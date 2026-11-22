//! PhpStorm's Usages popup (⌘⌥F7): the places the symbol at the cursor is used, one
//! row per line with the code coloured, from the language server's "find all
//! references". Enter opens a row; "Open in Tab" hands the whole result to the
//! editor's own references tab.

use std::ops::Range;

use editor::{Editor, actions::FindAllReferences};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, IntoElement, ParentElement, Render, Role, ScrollStrategy, SharedString, Styled,
    StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, div, prelude::*,
    px, uniform_list,
};
use project::Location;
use settings::SeedQuerySetting;
use text::ToPoint as _;
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
    /// The word under the cursor when the popup opened.
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
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let mut modal = Self {
                focus: cx.focus_handle(),
                editor,
                workspace: workspace_handle,
                symbol: symbol.into(),
                rows: Vec::new(),
                capped: false,
                selected: 0,
                state: State::Searching,
                scroll: UniformListScrollHandle::new(),
                frame: Frame::default(),
                search: Task::ready(()),
                _syntax_watch: Vec::new(),
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
    /// language server again (`Editor::find_all_references`, `navigation.rs`).
    fn open_in_tab(&mut self, _: &OpenUsagesInTab, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            editor.update(cx, |editor, cx| {
                if let Some(task) =
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
            State::Done => summary(0, false),
            State::Failed(error) => format!("Could not find usages: {error}"),
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
                    SharedString::from("Usages")
                } else {
                    format!("Usages of {}", self.symbol).into()
                })
                .weight(gpui::FontWeight::SEMIBOLD),
            )
            .when(!self.rows.is_empty(), |header| {
                header.child(
                    Label::new(summary(self.rows.len(), self.capped))
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
            .aria_label("Usages")
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
                    .aria_label("Open all usages in a new tab")
                    .tooltip(Tooltip::for_action_title_in(
                        "Open all usages in a new tab",
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

/// "3 usages", "300+ usages" for a capped list, "No usages found" for none.
fn summary(count: usize, capped: bool) -> String {
    match (count, capped) {
        (0, _) => "No usages found".to_string(),
        (1, false) => "1 usage".to_string(),
        (count, false) => format!("{count} usages"),
        (count, true) => format!("{count}+ usages"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
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
        cx.update(crate::init);
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
        assert_eq!(summary(count, capped), "300+ usages");
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
        assert_eq!(summary(0, false), "No usages found");
        assert_eq!(summary(1, false), "1 usage");
        assert_eq!(summary(12, false), "12 usages");
        assert_eq!(summary(MAX_USAGES, true), "300+ usages");
    }
}
