//! PhpStorm's Usages popup (⌘⌥F7): the places the symbol at the cursor is used, one
//! row per line with the code coloured, from the language server's "find all
//! references". Enter opens a row; "Open in Tab" hands the whole result to the
//! editor's own references tab. An "N implementations" code lens opens the same popup
//! as PhpStorm's Choose Implementation.

use std::ops::Range;

use editor::{CodeLensOpenInTab, Editor, actions::FindAllReferences};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, IntoElement, ParentElement, Render, Role, ScrollStrategy, SharedString, Styled,
    StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, div, prelude::*,
    px, uniform_list,
};
use language::CharKind;
use project::{CodeAction, Location};
use settings::SeedQuerySetting;
use text::{PointUtf16, ToOffset as _, ToPoint as _, Unclipped};
use ui::{
    Button, ButtonSize, ButtonStyle, Label, ScrollAxes, Scrollbars, Tooltip, WithScrollbar,
    prelude::*,
};
use workspace::{ModalView, Workspace, searchable::SearchableItem};

use super::{
    OpenUsagesInTab,
    find_in_files::{
        Hit, HitSource, SearchContext, is_comment_line, open_hit, row_highlights, watch_syntax,
    },
    popup::{Frame, PopupKind},
    style::{
        FOOTER_HEIGHT, HEADER_GAP, HEADER_HEIGHT, PADDING, POPUP_BAND, POPUP_FOOTER, ROW_HEIGHT,
        VISIBLE_ROWS,
    },
};

/// More usages than this say "300+", not a longer list.
const MAX_USAGES: usize = 300;

/// What the popup lists: usages, or the implementations a lens offers to choose from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Usages,
    Implementations,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Kind::Usages => "Usages",
            Kind::Implementations => "Implementations",
        }
    }

    /// Singular and plural.
    fn nouns(self) -> (&'static str, &'static str) {
        match self {
            Kind::Usages => ("usage", "usages"),
            Kind::Implementations => ("implementation", "implementations"),
        }
    }
}

/// What the popup is waiting for or showing.
#[derive(Debug, PartialEq)]
enum State {
    Searching,
    Done,
    Failed(String),
}

pub(crate) struct UsagesModal {
    focus: FocusHandle,
    /// The editor the popup was opened from: "Open in Tab" asks it for the references tab.
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    kind: Kind,
    /// The word under the cursor (or a lens's origin) when the popup opened.
    symbol: SharedString,
    rows: Vec<Hit>,
    /// The list stopped at [`MAX_USAGES`].
    capped: bool,
    selected: usize,
    state: State,
    scroll: UniformListScrollHandle,
    frame: Frame,
    search: Task<()>,
    _syntax_watch: Vec<Subscription>,
    /// Set when a code lens opened the popup: "Open in Tab" lists the lens's locations
    /// instead of asking the language server about the cursor.
    open_lens_in_tab: Option<CodeLensOpenInTab>,
}

impl ModalView for UsagesModal {}
impl EventEmitter<DismissEvent> for UsagesModal {}

impl Focusable for UsagesModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl UsagesModal {
    /// Asks the language server for the usages of the symbol at the active editor's
    /// cursor and shows them. Does nothing without an editor.
    pub(crate) fn open(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
            return;
        };
        let project = workspace.project().clone();
        // The selection, else the word at the caret.
        let (symbol, locations) = editor.update(cx, |editor, cx| {
            let symbol = SearchableItem::query_suggestion(
                editor,
                Some(SeedQuerySetting::Always),
                window,
                cx,
            );
            (symbol, editor.find_all_references_locations(&project, cx))
        });
        let symbol = if symbol.contains('\n') {
            String::new()
        } else {
            symbol
        };
        Self::present(
            workspace,
            editor,
            Kind::Usages,
            symbol,
            locations,
            None,
            window,
            cx,
        );
    }

    /// A code lens click ("3 references", "2 implementations"): lists the lens's own
    /// locations. Returns false without a workspace, so the editor falls back to its own
    /// list, and for a single implementation, which the editor jumps to as PhpStorm does.
    pub(crate) fn open_for_code_lens(
        lens: &CodeAction,
        locations: Task<anyhow::Result<Vec<Location>>>,
        open_in_tab: CodeLensOpenInTab,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> bool {
        let Some(workspace) = editor.workspace() else {
            return false;
        };
        let arguments = lens
            .lsp_action
            .command()
            .and_then(|command| command.arguments.as_deref())
            .unwrap_or_default();
        let kind = if lens
            .lsp_action
            .title()
            .to_lowercase()
            .contains("implementation")
        {
            Kind::Implementations
        } else {
            Kind::Usages
        };
        if kind == Kind::Implementations
            && arguments
                .get(2)
                .and_then(|locations| locations.as_array())
                .is_some_and(|locations| locations.len() == 1)
        {
            return false;
        }
        let origin = arguments.get(1).and_then(|position| {
            Some(PointUtf16::new(
                u32::try_from(position.get("line")?.as_u64()?).ok()?,
                u32::try_from(position.get("character")?.as_u64()?).ok()?,
            ))
        });
        let symbol = lens_symbol(lens, origin, editor, cx);
        let editor = cx.entity();
        // The editor is mid-update here; open the popup once it is released.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                Self::present(
                    workspace,
                    editor,
                    kind,
                    symbol,
                    Some(locations),
                    Some(open_in_tab),
                    window,
                    cx,
                )
            });
        });
        true
    }

    fn present(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        kind: Kind,
        symbol: String,
        locations: Option<Task<anyhow::Result<Vec<Location>>>>,
        open_lens_in_tab: Option<CodeLensOpenInTab>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let mut modal = Self {
                focus: cx.focus_handle(),
                editor,
                workspace: workspace_handle,
                kind,
                symbol: symbol.into(),
                rows: Vec::new(),
                capped: false,
                selected: 0,
                state: State::Searching,
                scroll: UniformListScrollHandle::new(),
                frame: Frame::default(),
                search: Task::ready(()),
                _syntax_watch: Vec::new(),
                open_lens_in_tab,
            };
            match locations {
                Some(locations) => {
                    modal.search = cx.spawn_in(window, async move |this, cx| {
                        let result = locations.await;
                        this.update(cx, |this: &mut Self, cx| this.show(result, cx))
                            .ok();
                    });
                }
                // No symbol at the cursor, so nothing to look up.
                None => modal.state = State::Done,
            }
            modal
        });
    }

    fn show(&mut self, result: anyhow::Result<Vec<Location>>, cx: &mut Context<Self>) {
        match result {
            Ok(locations) => {
                (self.rows, self.capped) = usage_rows(locations, cx);
                self._syntax_watch = watch_syntax(&self.rows, cx);
                self.state = State::Done;
            }
            Err(error) => {
                self.state = State::Failed(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix;
        if !self.rows.is_empty() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select((self.selected + 1) % self.rows.len(), cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.rows.is_empty() {
            let len = self.rows.len();
            self.select((self.selected + len - 1) % len, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.rows.len().saturating_sub(1), cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Opens the picked row and closes the popup.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        open_hit(self.workspace.clone(), row, window, cx).detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_in_tab(&OpenUsagesInTab, window, cx);
    }

    /// Closes the popup and lets the editor open its own references tab, which asks the
    /// language server again (`Editor::find_all_references`, `navigation.rs`). A popup a
    /// code lens opened lists that lens's locations instead, as the lens itself would.
    fn open_in_tab(&mut self, _: &OpenUsagesInTab, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let open_lens_in_tab = self.open_lens_in_tab.take();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(open) = open_lens_in_tab {
                    open(editor, window, cx);
                } else if let Some(task) =
                    editor.find_all_references(&FindAllReferences::default(), window, cx)
                {
                    task.detach_and_log_err(cx);
                }
            });
        });
    }

    /// What the list says while it has no rows.
    fn empty_text(&self) -> String {
        match &self.state {
            State::Searching => "Searching…".to_string(),
            State::Done => summary(self.kind, 0, false),
            State::Failed(error) => format!("Could not find {}: {error}", self.kind.nouns().1),
        }
    }

    fn render_header(&self) -> impl IntoElement + use<> {
        h_flex()
            .debug_selector(|| "zenith-popup-header".into())
            .h(HEADER_HEIGHT)
            .px(PADDING)
            .gap(HEADER_GAP)
            .bg(gpui::rgb(POPUP_BAND))
            .child(
                Label::new(if self.symbol.is_empty() {
                    SharedString::from(self.kind.title())
                } else {
                    format!("{} of {}", self.kind.title(), self.symbol).into()
                })
                .weight(gpui::FontWeight::SEMIBOLD),
            )
            .when(!self.rows.is_empty(), |header| {
                header.child(
                    Label::new(summary(self.kind, self.rows.len(), self.capped))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }

    fn render_body(
        &self,
        sized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let list = div()
            .id("zenith-usages-list")
            .role(Role::List)
            .aria_label(self.kind.title())
            .relative()
            .when(sized, |list| list.flex_1().min_h_0())
            .when(!sized, |list| list.h(px(ROW_HEIGHT * VISIBLE_ROWS)));
        if self.rows.is_empty() {
            return list
                .px_4()
                .py_1()
                .child(Label::new(self.empty_text()).color(Color::Muted))
                .into_any_element();
        }
        list.child(
            uniform_list(
                "zenith-usages",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .size_full(),
        )
        .custom_scrollbars(
            Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.scroll),
            window,
            cx,
        )
        .into_any_element()
    }

    /// One usage: the code with the symbol in a chip, the file and line on the right.
    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let row = &self.rows[ix];
        let colors = cx.theme().colors();
        let chip = HighlightStyle {
            background_color: Some(colors.search_match_background),
            ..Default::default()
        };
        let syntax = row.syntax(cx);
        let syntax = cx.theme().syntax().resolve_runs(&syntax);
        let highlights = row_highlights(&row.line, syntax, &row.matches, chip);
        let text_color = if is_comment_line(&row.line) {
            colors.text_muted
        } else {
            colors.text
        };
        let place = format!("{}:{}", row.file_name, row.point.row + 1);
        let path = if row.directory.is_empty() {
            row.file_name.to_string()
        } else {
            format!("{}/{}", row.directory, row.file_name)
        };
        let label = format!("{path}, line {}: {}", row.point.row + 1, row.line);
        h_flex()
            .id(("zenith-usage", ix))
            .debug_selector(move || format!("zenith-usage-{ix}"))
            .role(Role::ListItem)
            .aria_label(label)
            .aria_selected(ix == self.selected)
            .aria_position_in_set(ix + 1)
            .aria_size_of_set(self.rows.len())
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_4()
            .gap_3()
            .flex_none()
            .justify_between()
            .when(ix == self.selected, |row| {
                row.bg(colors.element_selection_background)
            })
            .hover(|row| row.bg(colors.ghost_element_hover))
            .tooltip(Tooltip::text(path))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(ix, cx);
                if event.click_count() > 1 {
                    this.confirm(&menu::Confirm, window, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ui_sm(cx)
                    .text_color(text_color)
                    .child(StyledText::new(row.line.clone()).with_highlights(highlights)),
            )
            .child(Label::new(place).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open_all = format!("Open all {} in a new tab", self.kind.nouns().1);
        h_flex()
            .debug_selector(|| "zenith-popup-footer".into())
            .h(FOOTER_HEIGHT)
            .px(PADDING)
            .gap_2()
            .justify_end()
            .bg(gpui::rgb(POPUP_FOOTER))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(if cfg!(target_os = "macos") {
                    "⌘↵"
                } else {
                    "Ctrl+Enter"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                Button::new("zenith-usages-tab", "Open in Tab")
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Compact)
                    .label_size(LabelSize::Small)
                    .aria_label(open_all.clone())
                    .tooltip(Tooltip::for_action_title_in(
                        open_all,
                        &OpenUsagesInTab,
                        &self.focus,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_in_tab(&OpenUsagesInTab, window, cx)
                    })),
            )
    }
}

impl Render for UsagesModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, height) = self.frame.size(PopupKind::Usages, window, cx);
        let header = self.render_header();
        let body = v_flex()
            .size_full()
            .child(self.render_body(height.is_some(), window, cx))
            .child(v_flex().flex_none().child(self.render_footer(cx)));
        let root = v_flex()
            .key_context("Usages")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::open_in_tab));
        self.frame.render(
            PopupKind::Usages,
            root,
            header,
            body,
            |this: &mut Self| &mut this.frame,
            window,
            cx,
        )
    }
}

/// The rows for `locations`: files in path order, lines in file order, one row per line.
/// Past [`MAX_USAGES`] the rest is left out and the flag says so.
fn usage_rows(locations: Vec<Location>, cx: &App) -> (Vec<Hit>, bool) {
    let mut files: Vec<(HitSource, Vec<Range<text::Anchor>>)> = Vec::new();
    for location in locations {
        match files
            .iter_mut()
            .find(|(source, _)| source.buffer == location.buffer)
        {
            Some((_, ranges)) => ranges.push(location.range),
            // A buffer without a file has no path to list.
            None => files.extend(
                HitSource::new(&location.buffer, cx).map(|source| (source, vec![location.range])),
            ),
        }
    }
    files.sort_by(|(a, _), (b, _)| a.project_path.cmp(&b.project_path));
    let mut rows = Vec::new();
    for (source, mut ranges) in files {
        ranges.sort_by_key(|range| range.start.to_point(&source.snapshot));
        ranges.dedup();
        // Past the cap the rest is dropped anyway.
        ranges.truncate(MAX_USAGES + 1);
        rows.extend(source.hits(&ranges, SearchContext::Anywhere));
        if rows.len() > MAX_USAGES {
            break;
        }
    }
    let capped = cap(&mut rows, MAX_USAGES);
    (rows, capped)
}

/// Cuts `rows` to `max`; true when something was cut.
fn cap<T>(rows: &mut Vec<T>, max: usize) -> bool {
    let capped = rows.len() > max;
    rows.truncate(max);
    capped
}

/// The word a lens is about: at its origin position (`editor.action.showReferences`'s
/// second argument), else at the start of the lens.
// shortcut: assumes the origin is in the lens's own buffer (PHPantom's lenses); read the
// first argument's uri if a server points elsewhere.
fn lens_symbol(lens: &CodeAction, origin: Option<PointUtf16>, editor: &Editor, cx: &App) -> String {
    let Some(buffer) = editor.buffer().read(cx).buffer(lens.range.start.buffer_id) else {
        return String::new();
    };
    let snapshot = buffer.read(cx).snapshot();
    let origin = origin.map_or_else(
        || lens.range.start.to_offset(&snapshot),
        |origin| Unclipped(origin).to_offset(&snapshot),
    );
    match snapshot.surrounding_word(origin, None) {
        (range, Some(CharKind::Word)) => snapshot.text_for_range(range).collect(),
        _ => String::new(),
    }
}

/// "3 usages", "300+ usages" for a capped list, "No usages found" for none.
fn summary(kind: Kind, count: usize, capped: bool) -> String {
    let (one, many) = kind.nouns();
    match (count, capped) {
        (0, _) => format!("No {many} found"),
        (1, false) => format!("1 {one}"),
        (count, false) => format!("{count} {many}"),
        (count, true) => format!("{count}+ {many}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use settings::KeymapFile;

    impl UsagesModal {
        /// "file:line code" for every row.
        fn row_texts(&self) -> Vec<String> {
            self.rows
                .iter()
                .map(|row| format!("{}:{} {}", row.file_name, row.point.row + 1, row.line))
                .collect()
        }
    }

    /// `two` is used on rows 1, 7 and 8 (0-based) and declared on row 4.
    const SOURCE: &str = "fn one() {\n    let mut a = ˇtwo();\n}\n\nfn two() {}\n\nfn three() {\n    two();\n    two();\n}";

    fn references(uri: lsp::Uri, places: &[(u32, u32, u32)]) -> Vec<lsp::Location> {
        places
            .iter()
            .map(|&(row, start, end)| lsp::Location {
                uri: uri.clone(),
                range: lsp::Range::new(
                    lsp::Position::new(row, start),
                    lsp::Position::new(row, end),
                ),
            })
            .collect()
    }

    /// A Rust editor whose language server answers `textDocument/references` with `places`
    /// (copied from `lsp_locations`' tests), with Zenith's keymap and the menu keys bound.
    async fn usages_cx(
        source: &str,
        places: Vec<(u32, u32, u32)>,
        cx: &mut TestAppContext,
    ) -> EditorLspTestContext {
        // Before the workspace exists, so it gets the action handlers.
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            crate::init(cx);
        });
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.update(|_, cx| {
            cx.bind_keys(crate::tests::zenith_keymap(cx));
            cx.bind_keys(KeymapFile::load_panic_on_failure(
                r#"[{ "bindings": {
                    "enter": "menu::Confirm", "secondary-enter": "menu::SecondaryConfirm",
                    "up": "menu::SelectPrevious", "down": "menu::SelectNext",
                    "escape": "menu::Cancel" } }]"#,
                cx,
            ));
        });
        cx.set_state(source);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(move |params, _| {
                let places = places.clone();
                async move {
                    let uri = params.text_document_position.text_document.uri;
                    Ok(Some(references(uri, &places)))
                }
            });
        cx
    }

    fn open(cx: &mut EditorLspTestContext) {
        cx.simulate_keystrokes("secondary-alt-f7");
        cx.run_until_parked();
    }

    fn modal(cx: &mut EditorLspTestContext) -> Option<Entity<UsagesModal>> {
        let workspace = cx.workspace.clone();
        cx.update(|_, cx| workspace.read(cx).active_modal::<UsagesModal>(cx))
    }

    fn read<T>(
        cx: &mut EditorLspTestContext,
        modal: &Entity<UsagesModal>,
        read: impl FnOnce(&UsagesModal) -> T,
    ) -> T {
        cx.update(|_, cx| read(modal.read(cx)))
    }

    const FOUR: [(u32, u32, u32); 4] = [(1, 16, 19), (4, 3, 6), (7, 4, 7), (8, 4, 7)];

    #[gpui::test]
    async fn test_usages_popup_lists_the_lines_with_file_and_row(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, FOUR.to_vec(), cx).await;
        open(&mut cx);
        let modal = modal(&mut cx).expect("the usages popup opens on cmd-alt-f7");
        let (symbol, rows, capped) = read(&mut cx, &modal, |m| {
            (m.symbol.to_string(), m.row_texts(), m.capped)
        });
        assert_eq!(symbol, "two");
        assert_eq!(
            rows,
            [
                "file.rs:2 let mut a = two();",
                "file.rs:5 fn two() {}",
                "file.rs:8 two();",
                "file.rs:9 two();",
            ]
        );
        assert!(!capped);
    }

    #[gpui::test]
    async fn test_usages_popup_says_no_usages_found(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, Vec::new(), cx).await;
        open(&mut cx);
        let modal = modal(&mut cx).expect("the popup opens even with nothing to list");
        let (rows, text) = read(&mut cx, &modal, |m| (m.row_texts(), m.empty_text()));
        assert!(rows.is_empty());
        assert_eq!(text, "No usages found");
    }

    #[gpui::test]
    async fn test_usages_popup_stops_at_300_rows(cx: &mut TestAppContext) {
        let source = format!("ˇtwo();\n{}", "two();\n".repeat(309));
        let places = (0..310).map(|row| (row, 0, 3)).collect();
        let mut cx = usages_cx(&source, places, cx).await;
        open(&mut cx);
        let modal = modal(&mut cx).expect("popup");
        let (count, capped) = read(&mut cx, &modal, |m| (m.rows.len(), m.capped));
        assert_eq!((count, capped), (MAX_USAGES, true));
        assert_eq!(summary(Kind::Usages, count, capped), "300+ usages");
    }

    #[gpui::test]
    async fn test_arrow_keys_move_and_escape_closes(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, FOUR.to_vec(), cx).await;
        open(&mut cx);
        let modal = modal(&mut cx).expect("popup");
        let selected = |cx: &mut EditorLspTestContext| read(cx, &modal, |m| m.selected);
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&mut cx), 1);
        cx.simulate_keystrokes("up up");
        // Up from the first row wraps to the last.
        assert_eq!(selected(&mut cx), 3);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(self::modal(&mut cx).is_none(), "escape closes the popup");
    }

    #[gpui::test]
    async fn test_enter_opens_the_picked_usage_and_closes(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, FOUR.to_vec(), cx).await;
        open(&mut cx);
        // The second row is the declaration, `fn two() {}`.
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert!(modal(&mut cx).is_none(), "enter closes the popup");
        cx.assert_editor_state("fn one() {\n    let mut a = two();\n}\n\nfn ˇtwo() {}\n\nfn three() {\n    two();\n    two();\n}");
    }

    /// Every editor open in the workspace other than the test's own, with its text.
    fn other_editors(cx: &mut EditorLspTestContext) -> Vec<String> {
        let own = cx.editor.clone();
        cx.update_workspace(|workspace, _, cx| {
            workspace
                .items_of_type::<Editor>(cx)
                .filter(|editor| *editor != own)
                .map(|editor| editor.read(cx).text(cx))
                .collect()
        })
    }

    #[gpui::test]
    async fn test_open_in_tab_button_action_opens_a_references_tab(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, FOUR.to_vec(), cx).await;
        open(&mut cx);
        assert!(other_editors(&mut cx).is_empty());
        cx.dispatch_action(OpenUsagesInTab);
        cx.run_until_parked();
        assert!(modal(&mut cx).is_none(), "the popup closes");
        let tabs = other_editors(&mut cx);
        assert_eq!(tabs.len(), 1, "one new multibuffer tab");
        assert!(tabs[0].contains("fn three()"), "{:?}", tabs[0]);
    }

    #[gpui::test]
    async fn test_secondary_enter_opens_the_references_tab(cx: &mut TestAppContext) {
        let mut cx = usages_cx(SOURCE, FOUR.to_vec(), cx).await;
        open(&mut cx);
        cx.simulate_keystrokes("secondary-enter");
        cx.run_until_parked();
        assert!(modal(&mut cx).is_none());
        assert_eq!(other_editors(&mut cx).len(), 1);
    }

    /// A Rust editor whose server puts one "N {noun}" lens on `fn two`, with the
    /// command and arguments PHPantom sends (`locations_lens_command`: origin uri,
    /// origin position, `Location[]`). The lens shows in the code actions menu
    /// (`"code_lens": "menu"`), so the test can pick it without a mouse; picking it runs
    /// the same `try_handle_client_command` as a click on the inline lens.
    async fn lens_cx(
        noun: &'static str,
        places: Vec<(u32, u32, u32)>,
        cx: &mut TestAppContext,
    ) -> EditorLspTestContext {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            crate::init(cx);
        });
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: None,
                }),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.update(|_, cx| {
            <settings::SettingsStore as gpui::UpdateGlobal>::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.code_lens = Some(settings::CodeLens::Menu);
                });
            });
        });
        let mut lens_requests = cx
            .lsp
            .set_request_handler::<lsp::request::CodeLensRequest, _, _>(move |params, _| {
                let places = places.clone();
                async move {
                    let uri = params.text_document.uri;
                    let locations = references(uri.clone(), &places);
                    Ok(Some(vec![lsp::CodeLens {
                        range: lsp::Range::new(lsp::Position::new(4, 0), lsp::Position::new(4, 11)),
                        command: Some(lsp::Command {
                            title: format!("{} {noun}", locations.len()),
                            command: "editor.action.showReferences".to_string(),
                            arguments: Some(vec![
                                serde_json::json!(uri),
                                serde_json::json!(lsp::Position::new(4, 3)),
                                serde_json::json!(locations),
                            ]),
                        }),
                        data: None,
                    }]))
                }
            });
        cx.set_state("fn one() {\n    let mut a = two();\n}\n\nfn tˇwo() {}\n\nfn three() {\n    two();\n    two();\n}");
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        // The cursor's code actions carry the lens only once the server has answered.
        lens_requests.next().await;
        cx.run_until_parked();
        cx
    }

    /// The popup a lens opened, with its locations listed.
    fn lens_popup(cx: &mut EditorLspTestContext) -> Entity<UsagesModal> {
        let modal = modal(cx).expect("the lens opens the popup");
        assert!(read(cx, &modal, |m| m.state != State::Searching));
        modal
    }

    /// Picks the lens from the code actions menu.
    async fn click_lens(cx: &mut EditorLspTestContext) {
        cx.update_editor(|editor, window, cx| {
            editor.toggle_code_actions(
                &editor::actions::ToggleCodeActions {
                    deployed_from: None,
                    quick_launch: false,
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();
        cx.condition(|editor, _| editor.context_menu_visible())
            .await;
        cx.update_editor(|editor, window, cx| {
            editor
                .confirm_code_action(
                    &editor::actions::ConfirmCodeAction { item_ix: Some(0) },
                    window,
                    cx,
                )
                .expect("the lens is the first item")
                .detach_and_log_err(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_code_lens_references_open_the_usages_popup_with_the_lens_locations(
        cx: &mut TestAppContext,
    ) {
        let mut cx = lens_cx("references", FOUR.to_vec(), cx).await;
        click_lens(&mut cx).await;
        let modal = lens_popup(&mut cx);
        let (kind, symbol, rows) = read(&mut cx, &modal, |m| {
            (m.kind, m.symbol.to_string(), m.row_texts())
        });
        assert_eq!(kind, Kind::Usages);
        // The word at the lens's origin position, as the header says "Usages of two".
        assert_eq!(symbol, "two");
        assert_eq!(
            rows,
            [
                "file.rs:2 let mut a = two();",
                "file.rs:5 fn two() {}",
                "file.rs:8 two();",
                "file.rs:9 two();",
            ]
        );
        assert!(other_editors(&mut cx).is_empty(), "no references tab yet");
    }

    #[gpui::test]
    async fn test_open_in_tab_from_a_lens_lists_the_lens_locations(cx: &mut TestAppContext) {
        // No `textDocument/references` handler: the tab can only come from the lens.
        let mut cx = lens_cx("references", FOUR.to_vec(), cx).await;
        click_lens(&mut cx).await;
        lens_popup(&mut cx);
        cx.dispatch_action(OpenUsagesInTab);
        cx.run_until_parked();
        assert!(modal(&mut cx).is_none(), "the popup closes");
        let tabs = other_editors(&mut cx);
        assert_eq!(tabs.len(), 1, "one new multibuffer tab");
        assert!(tabs[0].contains("fn three()"), "{:?}", tabs[0]);
    }

    #[gpui::test]
    async fn test_implementations_lens_opens_the_implementation_chooser(cx: &mut TestAppContext) {
        let mut cx = lens_cx("implementations", vec![(4, 3, 6), (6, 3, 8)], cx).await;
        click_lens(&mut cx).await;
        let modal = lens_popup(&mut cx);
        let (kind, symbol, rows, count) = read(&mut cx, &modal, |m| {
            (
                m.kind,
                m.symbol.to_string(),
                m.row_texts(),
                summary(m.kind, m.rows.len(), m.capped),
            )
        });
        assert_eq!(kind, Kind::Implementations);
        assert_eq!(symbol, "two");
        assert_eq!(rows, ["file.rs:5 fn two() {}", "file.rs:7 fn three() {"]);
        assert_eq!(count, "2 implementations");
    }

    #[gpui::test]
    async fn test_a_single_implementation_lens_jumps_straight_to_it(cx: &mut TestAppContext) {
        let mut cx = lens_cx("implementations", vec![(6, 3, 8)], cx).await;
        click_lens(&mut cx).await;
        assert!(modal(&mut cx).is_none(), "nothing to choose from");
        let state = cx.editor_state();
        let line = state.lines().nth(6).unwrap_or_default();
        assert!(line.contains('ˇ'), "the cursor is on `fn three`: {state}");
    }

    #[test]
    fn test_code_lens_is_on_by_default() {
        let defaults: settings::SettingsContent =
            settings::parse_json_with_comments(&settings::default_settings()).unwrap();
        assert_eq!(defaults.editor.code_lens, Some(settings::CodeLens::On));
    }

    #[test]
    fn test_cap_cuts_to_the_limit_and_says_so() {
        let mut rows: Vec<usize> = (0..MAX_USAGES + 5).collect();
        assert!(cap(&mut rows, MAX_USAGES));
        assert_eq!(rows.len(), MAX_USAGES);
        // Exactly the limit is not capped: nothing was left out.
        let mut exact: Vec<usize> = (0..MAX_USAGES).collect();
        assert!(!cap(&mut exact, MAX_USAGES));
        assert_eq!(exact.len(), MAX_USAGES);
    }

    #[test]
    fn test_summary_counts_and_marks_a_capped_list() {
        assert_eq!(summary(Kind::Usages, 0, false), "No usages found");
        assert_eq!(summary(Kind::Usages, 1, false), "1 usage");
        assert_eq!(summary(Kind::Usages, 12, false), "12 usages");
        assert_eq!(summary(Kind::Usages, MAX_USAGES, true), "300+ usages");
        assert_eq!(
            summary(Kind::Implementations, 0, false),
            "No implementations found"
        );
        assert_eq!(summary(Kind::Implementations, 1, false), "1 implementation");
    }
}
