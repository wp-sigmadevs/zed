//! PhpStorm's Find in Files / Replace in Files popup (⌘⇧F / ⌘⇧R): a header with the
//! live match count and the file mask, a query box with case, word and regex toggles,
//! an In Project / Directory scope, the matching lines, a code preview of the picked
//! one, and a hand-off to the project search tab ("Open in Find Window") that does
//! the replacing.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, IntoElement, ParentElement, Render, ScrollStrategy, SharedString, Styled,
    StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, div, prelude::*,
    px, rems, uniform_list,
};
use picker::{MatchLocation, PreviewBackend, PreviewLayout, PreviewUpdate};
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use settings::SeedQuerySetting;
use text::{Point, ToOffset as _, ToPoint as _};
use ui::{
    Button, ButtonSize, ButtonStyle, Checkbox, IconButton, Label, ScrollAxes, Scrollbars,
    TintColor, Tooltip, WithScrollbar, prelude::*,
};
use util::paths::PathMatcher;
use workspace::{ModalView, Workspace, pane::DeploySearch, searchable::SearchableItem};

use super::{
    POPUP_BAND, POPUP_FOOTER,
    popup::{Frame, PopupKind},
};

/// More hits than this say "refine the query", not "scroll".
const MAX_HITS: usize = 300;
const ROW_HEIGHT: f32 = 26.;
/// The list shows this many rows; more scroll.
const VISIBLE_ROWS: f32 = 7.;
/// About nine lines of the preview's code.
const PREVIEW_HEIGHT: f32 = 210.;

/// Where the search looks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    Project,
    Directory,
}

pub struct FindInFilesModal {
    query: Entity<Editor>,
    mask: Entity<Editor>,
    directory: Entity<Editor>,
    replace: Option<Entity<Editor>>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    previous_focus: FocusHandle,
    /// The picked hit's code, drawn under the list.
    preview: Arc<dyn PreviewBackend>,
    scroll: UniformListScrollHandle,
    hits: Vec<Hit>,
    selected: usize,
    /// The search stopped at [`MAX_HITS`].
    capped: bool,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    /// The file mask only narrows the search while its box is ticked.
    mask_enabled: bool,
    scope: Scope,
    /// Enter sends the results to a project search tab instead of opening one hit.
    open_in_tab: bool,
    search: Task<()>,
    /// The card: dragging the header moves it, the corner grip resizes it.
    frame: Frame,
    #[cfg(test)]
    previewed: Option<(ProjectPath, u32)>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for FindInFilesModal {}
impl EventEmitter<DismissEvent> for FindInFilesModal {}

impl Focusable for FindInFilesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

impl FindInFilesModal {
    pub fn open(
        workspace: &mut Workspace,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let previous_focus = window
            .focused(cx)
            .unwrap_or_else(|| workspace.focus_handle(cx));
        let workspace_handle = cx.entity().downgrade();
        let project = workspace.project().clone();
        // The selection, else the word at the caret, like PhpStorm's Find in Files.
        let seed = workspace
            .active_item_as::<Editor>(cx)
            .map(|editor| {
                editor.update(cx, |editor, cx| {
                    SearchableItem::query_suggestion(
                        editor,
                        Some(SeedQuerySetting::Always),
                        window,
                        cx,
                    )
                })
            })
            .filter(|text| !text.contains('\n'))
            .unwrap_or_default();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(
                workspace_handle,
                project,
                replace,
                previous_focus,
                seed,
                window,
                cx,
            )
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        replace: bool,
        previous_focus: FocusHandle,
        seed: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let query = field("Search all files…", window, cx);
        let mask = field("*.php, *.js", window, cx);
        let directory = field("Directory, relative to the project", window, cx);
        let replace_field = replace.then(|| field("Replace with", window, cx));
        let preview = picker_preview::editor_preview(project.clone(), window, cx);

        let mut subscriptions = Vec::new();
        for editor in [&query, &mask, &directory] {
            let is_mask = editor == &mask;
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                move |this, editor, event: &EditorEvent, window, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        // Typing a mask switches it on, as in PhpStorm.
                        if is_mask && !editor.read(cx).text(cx).trim().is_empty() {
                            this.mask_enabled = true;
                        }
                        this.refresh(window, cx);
                    }
                },
            ));
        }
        if !seed.is_empty() {
            query.update(cx, |editor, cx| {
                editor.set_text(seed, window, cx);
                // Selected, so typing replaces it.
                editor.select_all(&editor::actions::SelectAll, window, cx);
            });
        }
        Self {
            query,
            mask,
            directory,
            replace: replace_field,
            workspace,
            project,
            previous_focus,
            preview,
            scroll: UniformListScrollHandle::new(),
            hits: Vec::new(),
            selected: 0,
            capped: false,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            mask_enabled: false,
            scope: Scope::Project,
            open_in_tab: false,
            search: Task::ready(()),
            frame: Frame::default(),
            #[cfg(test)]
            previewed: None,
            _subscriptions: subscriptions,
        }
    }

    /// The include globs: the mask when it is ticked, inside the directory when that
    /// scope is picked.
    fn include_globs(&self, cx: &App) -> Vec<String> {
        let mask = if self.mask_enabled {
            self.mask.read(cx).text(cx)
        } else {
            String::new()
        };
        let directory = if self.scope == Scope::Directory {
            let roots: Vec<PathBuf> = self
                .project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect();
            relative_directory(&self.directory.read(cx).text(cx), &roots)
        } else {
            String::new()
        };
        include_globs(&mask, &directory)
    }

    /// Searches again with the current query, options and scope.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let path_style = self.project.read(cx).path_style(cx);
        let globs = self.include_globs(cx);
        // The hits of the previous query stay until the new ones arrive.
        self.search = Task::ready(());
        let built = (!query.trim().is_empty())
            .then(|| PathMatcher::new(globs, path_style).ok())
            .flatten()
            .and_then(|include| {
                if self.regex {
                    SearchQuery::regex(
                        query,
                        self.whole_word,
                        self.case_sensitive,
                        false,
                        false,
                        include,
                        PathMatcher::default(),
                        false,
                        None,
                    )
                } else {
                    SearchQuery::text(
                        query,
                        self.whole_word,
                        self.case_sensitive,
                        false,
                        include,
                        PathMatcher::default(),
                        false,
                        None,
                    )
                }
                // An unfinished regex is not an error worth showing while typing.
                .ok()
            });
        let Some(built) = built else {
            self.set_hits(Vec::new(), false, window, cx);
            return;
        };
        let results = self
            .project
            .update(cx, |project, cx| project.search(built, cx));
        self.search = cx.spawn_in(window, async move |this, cx| {
            let _search = results.task_handle;
            let mut hits: Vec<Hit> = Vec::new();
            let mut capped = false;
            while let Ok(result) = results.rx.recv().await {
                let SearchResult::Buffer { buffer, ranges } = result else {
                    continue;
                };
                let found = cx.update(|_, cx| Hit::from_buffer(&buffer, &ranges, cx));
                let Ok(found) = found else { return };
                hits.extend(found);
                if hits.len() >= MAX_HITS {
                    hits.truncate(MAX_HITS);
                    capped = true;
                    break;
                }
            }
            // Files arrive in any order; list them by path.
            hits.sort_by(|a, b| {
                a.project_path
                    .cmp(&b.project_path)
                    .then(a.point.row.cmp(&b.point.row))
            });
            this.update_in(cx, |this, window, cx| {
                this.set_hits(hits, capped, window, cx)
            })
            .ok();
        });
    }

    fn set_hits(
        &mut self,
        hits: Vec<Hit>,
        capped: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hits = hits;
        self.capped = capped;
        self.select(0, window, cx);
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        if !self.hits.is_empty() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.update_preview(window, cx);
        cx.notify();
    }

    fn update_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(self.selected) else {
            self.preview.clear(cx);
            #[cfg(test)]
            {
                self.previewed = None;
            }
            return;
        };
        #[cfg(test)]
        {
            self.previewed = Some((hit.project_path.clone(), hit.point.row));
        }
        self.preview.update(
            PreviewUpdate::from_buffer(
                hit.buffer.clone(),
                MatchLocation {
                    anchor_range: hit.anchors.clone(),
                    range: hit.offsets.clone(),
                },
            ),
            window,
            cx,
        );
        // The editor knows its height only after it is drawn; then centre the match.
        let preview = self.preview.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| preview.adjust_to_new_size(window, cx));
        });
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if !self.hits.is_empty() {
            self.select((self.selected + 1) % self.hits.len(), window, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.hits.is_empty() {
            let len = self.hits.len();
            self.select((self.selected + len - 1) % len, window, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select(0, window, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.hits.len().saturating_sub(1), window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_in_find_window(window, cx);
    }

    /// Tab moves between the boxes, since the search box has no use for it.
    fn focus_next_field(
        &mut self,
        _: &picker::ConfirmCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut fields = vec![self.query.read(cx).focus_handle(cx)];
        fields.extend(self.replace.iter().map(|e| e.read(cx).focus_handle(cx)));
        if self.scope == Scope::Directory {
            fields.push(self.directory.read(cx).focus_handle(cx));
        }
        fields.push(self.mask.read(cx).focus_handle(cx));
        let current = fields.iter().position(|field| field.is_focused(window));
        let next = current.map_or(0, |ix| (ix + 1) % fields.len());
        window.focus(&fields[next], cx);
    }

    /// Enter opens the picked line; with "Open results in new tab" ticked it sends the
    /// whole result to a project search tab instead.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_in_tab {
            self.open_in_find_window(window, cx);
            return;
        }
        let Some(hit) = self.hits.get(self.selected).cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let open = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_path(hit.project_path.clone(), None, true, window, cx)
            })?;
            let item = open.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()).unfold(),
                        window,
                        cx,
                        |selections| selections.select_ranges([hit.point..hit.point]),
                    );
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn toggle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        flip: impl FnOnce(&mut Self),
    ) {
        flip(self);
        self.refresh(window, cx);
        cx.notify();
    }

    /// Hands the query to the project search tab, which also does replacing.
    fn open_in_find_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let included = self.include_globs(cx).join(",");
        let action = DeploySearch {
            replace_enabled: self.replace.is_some(),
            included_files: (!included.is_empty()).then_some(included),
            query: Some(self.query.read(cx).text(cx)),
            regex: Some(self.regex),
            case_sensitive: Some(self.case_sensitive),
            whole_word: Some(self.whole_word),
            ..Default::default()
        };
        let focus = self.previous_focus.clone();
        cx.emit(DismissEvent);
        window.focus(&focus, cx);
        window.dispatch_action(Box::new(action), cx);
    }

    /// "4 matches in 1 file"; empty until something is searched for.
    fn summary(&self, cx: &App) -> String {
        if self.query.read(cx).text(cx).trim().is_empty() {
            return String::new();
        }
        let matches = self.hits.iter().map(|hit| hit.matches.len()).sum();
        let mut files: Vec<&ProjectPath> = self.hits.iter().map(|hit| &hit.project_path).collect();
        files.dedup();
        summary(matches, files.len(), self.capped)
    }

    #[cfg(test)]
    pub(crate) fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query
            .update(cx, |editor, cx| editor.set_text(query, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn query_text(&self, cx: &App) -> String {
        self.query.read(cx).text(cx)
    }

    #[cfg(test)]
    pub(crate) fn set_mask(&mut self, mask: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.mask
            .update(cx, |editor, cx| editor.set_text(mask, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn set_directory(
        &mut self,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scope = Scope::Directory;
        self.directory
            .update(cx, |editor, cx| editor.set_text(directory, window, cx));
        cx.notify();
    }

    /// A grip drag from `from` to `to`, which may be outside the window.
    #[cfg(test)]
    pub(crate) fn resize_between(
        &mut self,
        from: gpui::Point<Pixels>,
        to: gpui::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.frame.resize_to(PopupKind::Find, from, window, cx);
        self.frame.resize_to(PopupKind::Find, to, window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn dragged(&self) -> bool {
        self.frame.moved()
    }

    #[cfg(test)]
    pub(crate) fn set_open_in_tab(&mut self, on: bool) {
        self.open_in_tab = on;
    }

    #[cfg(test)]
    pub(crate) fn match_count(&self) -> usize {
        self.hits.len()
    }

    /// The hit lines as "file:row", in list order.
    #[cfg(test)]
    pub(crate) fn hit_labels(&self) -> Vec<String> {
        self.hits
            .iter()
            .map(|hit| format!("{}:{}", hit.file_name, hit.point.row + 1))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn summary_text(&self, cx: &App) -> String {
        self.summary(cx)
    }

    /// The file and row the preview was last pointed at.
    #[cfg(test)]
    pub(crate) fn previewed(&self) -> Option<(ProjectPath, u32)> {
        self.previewed.clone()
    }

    /// A rounded box round an editor whose border turns blue while it has focus.
    fn field(&self, editor: &Entity<Editor>, window: &Window, cx: &App) -> Div {
        let colors = cx.theme().colors();
        let focused = editor.read(cx).focus_handle(cx).is_focused(window);
        h_flex()
            .h(px(30.))
            .px_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(if focused {
                colors.border_focused
            } else {
                colors.border
            })
            .bg(colors.editor_background)
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let title = if self.replace.is_some() {
            "Replace in Files"
        } else {
            "Find in Files"
        };
        let summary = self.summary(cx);
        let header = h_flex()
            .debug_selector(|| "zenith-popup-header".into())
            .h_11()
            .px_4()
            .gap_3()
            .justify_between()
            .bg(gpui::rgb(POPUP_BAND))
            .child(
                h_flex()
                    .gap_3()
                    .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("zenith-find-mask-on", self.mask_enabled.into())
                            .label("File mask:")
                            .label_size(LabelSize::Small)
                            .label_color(Color::Default)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle(window, cx, |this| {
                                    this.mask_enabled = !this.mask_enabled
                                })
                            })),
                    )
                    .child(
                        self.field(&self.mask, window, cx)
                            .w(rems(10.))
                            .child(div().flex_1().child(self.mask.clone())),
                    ),
            );
        header
    }

    fn render_search_box(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let chip = |id: &'static str, label: &'static str, tip: &'static str, on: bool| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .toggle_state(on)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .tooltip(Tooltip::text(tip))
                .aria_label(tip)
        };
        let has_query = !self.query.read(cx).text(cx).is_empty();
        v_flex()
            .gap_2()
            .child(
                self.field(&self.query, window, cx)
                    .debug_selector(|| "zenith-find-query".into())
                    .h(px(36.))
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().child(self.query.clone()))
                    .when(has_query, |this| {
                        this.child(
                            IconButton::new("zenith-find-clear", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Clear"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.query
                                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                                    window.focus(&this.query.read(cx).focus_handle(cx), cx);
                                })),
                        )
                    })
                    .child(
                        chip("zenith-find-case", "Aa", "Match Case", self.case_sensitive).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.toggle(window, cx, |this| {
                                    this.case_sensitive = !this.case_sensitive
                                })
                            }),
                        ),
                    )
                    .child(
                        chip("zenith-find-word", "W", "Words", self.whole_word).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.toggle(window, cx, |this| this.whole_word = !this.whole_word)
                            }),
                        ),
                    )
                    .child(
                        chip("zenith-find-regex", ".*", "Regex", self.regex).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.toggle(window, cx, |this| this.regex = !this.regex)
                            },
                        )),
                    ),
            )
            .children(self.replace.as_ref().map(|replace| {
                self.field(replace, window, cx)
                    .h(px(36.))
                    .child(div().flex_1().child(replace.clone()))
            }))
    }

    fn render_scope(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let segment = |id: &'static str, label: &'static str, scope: Scope| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.scope == scope)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.toggle(window, cx, |this| this.scope = scope);
                    if scope == Scope::Directory {
                        window.focus(&this.directory.read(cx).focus_handle(cx), cx);
                    }
                }))
        };
        h_flex()
            .gap_2()
            .child(segment(
                "zenith-find-in-project",
                "In Project",
                Scope::Project,
            ))
            .child(segment(
                "zenith-find-in-directory",
                "Directory",
                Scope::Directory,
            ))
            .when(self.scope == Scope::Directory, |this| {
                this.child(
                    self.field(&self.directory, window, cx)
                        .debug_selector(|| "zenith-find-directory".into())
                        .flex_1()
                        .child(div().flex_1().child(self.directory.clone())),
                )
            })
    }

    fn render_results(
        &self,
        sized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Until the popup is resized the list shows a fixed number of rows;
        // after, it takes whatever height the body has left.
        let list = div()
            .id("zenith-find-results")
            .relative()
            .when(sized, |list| list.flex_1().min_h_0())
            .when(!sized, |list| list.h(px(ROW_HEIGHT * VISIBLE_ROWS)));
        if self.hits.is_empty() {
            let text = if self.query.read(cx).text(cx).trim().is_empty() {
                "Type search query to find in files"
            } else {
                "No matches"
            };
            return list
                .px_4()
                .py_1()
                .child(Label::new(text).color(Color::Muted))
                .into_any_element();
        }
        list.child(
            uniform_list(
                "zenith-find-hits",
                self.hits.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_hit(ix, cx)).collect::<Vec<_>>()
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

    /// One matching line: the code with the match in a chip, the file and line on the right.
    fn render_hit(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hit = &self.hits[ix];
        let colors = cx.theme().colors();
        let chip = HighlightStyle {
            background_color: Some(colors.search_match_background),
            ..Default::default()
        };
        let highlights: Vec<_> = hit
            .matches
            .iter()
            .map(|range| clamp_range(&hit.line, range))
            .filter(|range| !range.is_empty())
            .map(|range| (range, chip))
            .collect();
        let text_color = if is_comment_line(&hit.line) {
            colors.text_muted
        } else {
            colors.text
        };
        h_flex()
            .id(("zenith-find-hit", ix))
            .debug_selector(move || format!("zenith-find-hit-{ix}"))
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
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(ix, window, cx);
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
                    .child(StyledText::new(hit.line.clone()).with_highlights(highlights)),
            )
            .child(
                Label::new(format!("{} {}", hit.file_name, hit.point.row + 1))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let border = cx.theme().colors().border_variant;
        let hit = self.hits.get(self.selected);
        v_flex()
            .border_t_1()
            .border_color(border)
            .child(h_flex().h_8().px_4().gap_2().when_some(hit, |this, hit| {
                this.child(Label::new(hit.file_name.clone())).child(
                    Label::new(hit.directory.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .child(
                div()
                    .debug_selector(|| "zenith-find-preview".into())
                    .h(px(PREVIEW_HEIGHT))
                    .overflow_hidden()
                    .child(self.preview.render(PreviewLayout::Below, cx)),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        h_flex()
            .debug_selector(|| "zenith-popup-footer".into())
            .h_12()
            .px_4()
            .gap_2()
            .justify_between()
            .bg(gpui::rgb(POPUP_FOOTER))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Checkbox::new("zenith-find-new-tab", self.open_in_tab.into())
                    .label("Open results in new tab")
                    .label_size(LabelSize::Small)
                    .label_color(Color::Default)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_in_tab = !this.open_in_tab;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
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
                        Button::new("zenith-find-window", "Open in Find Window")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_find_window(window, cx)
                            })),
                    ),
            )
    }
}

impl Render for FindInFilesModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, height) = self.frame.size(PopupKind::Find, window, cx);
        let header = self.render_header(window, cx);
        let search = self.render_search_box(window, cx);
        let scope = self.render_scope(window, cx);
        let results = self.render_results(height.is_some(), window, cx);
        let preview = self.render_preview(cx);
        let footer = self.render_footer(cx);
        let body = v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_none()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .gap_2()
                    .child(search)
                    .child(scope),
            )
            .child(results)
            .child(v_flex().flex_none().child(preview))
            .child(v_flex().flex_none().child(footer));
        let root = v_flex()
            // "Picker" makes the keymap's up, down, escape and tab work in the search box.
            .key_context("FindInFiles Picker")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_next_field));
        self.frame.render(
            PopupKind::Find,
            root,
            header,
            body,
            |this: &mut Self| &mut this.frame,
            window,
            cx,
        )
    }
}

/// One matching line.
#[derive(Clone)]
pub struct Hit {
    buffer: Entity<language::Buffer>,
    /// The first match, for the preview.
    anchors: Range<text::Anchor>,
    offsets: Range<usize>,
    project_path: ProjectPath,
    file_name: SharedString,
    directory: SharedString,
    point: Point,
    /// The line with leading space removed, and the byte ranges that matched in it.
    line: String,
    matches: Vec<Range<usize>>,
}

impl Hit {
    fn from_buffer(
        buffer: &Entity<language::Buffer>,
        ranges: &[Range<text::Anchor>],
        cx: &App,
    ) -> Vec<Hit> {
        let buffer_entity = buffer;
        let buffer = buffer.read(cx);
        let Some(file) = buffer.file() else {
            return Vec::new();
        };
        let project_path = ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        };
        let file_name: SharedString = file
            .path()
            .file_name()
            .unwrap_or_default()
            .to_string()
            .into();
        let directory: SharedString = file
            .path()
            .parent()
            .map(|parent| parent.as_unix_str().to_string())
            .unwrap_or_default()
            .into();
        let snapshot = buffer.snapshot();
        let mut hits: Vec<Hit> = Vec::new();
        for range in ranges {
            let start = range.start.to_point(&snapshot);
            let end = range.end.to_point(&snapshot);
            let line_len = snapshot.line_len(start.row);
            let text: String = snapshot
                .text_for_range(Point::new(start.row, 0)..Point::new(start.row, line_len))
                .collect();
            let lead = text.len() - text.trim_start().len();
            let column_end = if end.row == start.row {
                end.column
            } else {
                line_len
            };
            let matched = (start.column as usize).saturating_sub(lead)
                ..(column_end as usize).saturating_sub(lead).max(1);
            // Several matches on one line make one row.
            if let Some(last) = hits.last_mut()
                && last.point.row == start.row
            {
                last.matches.push(matched);
                continue;
            }
            hits.push(Hit {
                buffer: buffer_entity.clone(),
                anchors: range.clone(),
                offsets: range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot),
                project_path: project_path.clone(),
                file_name: file_name.clone(),
                directory: directory.clone(),
                point: start,
                line: text[lead..].to_string(),
                matches: vec![matched],
            });
        }
        hits
    }
}

/// "4 matches in 1 file"; a capped search adds a plus, as there may be more.
fn summary(matches: usize, files: usize, capped: bool) -> String {
    let plus = if capped { "+" } else { "" };
    format!(
        "{matches}{plus} {} in {files}{plus} {}",
        if matches == 1 && !capped {
            "match"
        } else {
            "matches"
        },
        if files == 1 && !capped {
            "file"
        } else {
            "files"
        },
    )
}

/// The globs for the include side of the search: the mask's comma-separated globs,
/// inside `directory` when there is one.
fn include_globs(mask: &str, directory: &str) -> Vec<String> {
    let masks = mask
        .split(',')
        .map(str::trim)
        .filter(|glob| !glob.is_empty());
    if directory.is_empty() {
        return masks.map(str::to_string).collect();
    }
    let masks: Vec<&str> = masks.collect();
    if masks.is_empty() {
        vec![directory.to_string()]
    } else {
        masks
            .into_iter()
            .map(|glob| format!("{directory}/**/{glob}"))
            .collect()
    }
}

/// The directory as the search wants it: relative to a worktree root. An absolute
/// path inside a root loses the root; a root itself, or nothing, means everywhere.
fn relative_directory(input: &str, roots: &[PathBuf]) -> String {
    let input = input.trim();
    let relative = roots
        .iter()
        .find_map(|root| Path::new(input).strip_prefix(root).ok())
        .map_or(input, |rest| rest.to_str().unwrap_or(input));
    relative
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// Whether the line looks like a comment, which the list dims.
// ponytail: language-agnostic prefixes only; real comment spans would need the syntax tree.
fn is_comment_line(line: &str) -> bool {
    let line = line.trim_start();
    ["//", "/*", "*/", "* ", "<!--"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        || line == "*"
}

/// The range cut back to the line and to character boundaries.
fn clamp_range(line: &str, range: &Range<usize>) -> Range<usize> {
    let snap = |mut ix: usize| {
        ix = ix.min(line.len());
        while !line.is_char_boundary(ix) {
            ix -= 1;
        }
        ix
    };
    let start = snap(range.start);
    start..snap(range.end).max(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_is_singular_and_marks_a_capped_search() {
        assert_eq!(summary(4, 1, false), "4 matches in 1 file");
        assert_eq!(summary(1, 1, false), "1 match in 1 file");
        assert_eq!(summary(0, 0, false), "0 matches in 0 files");
        assert_eq!(summary(300, 12, true), "300+ matches in 12+ files");
    }

    #[test]
    fn test_include_globs_put_the_mask_inside_the_directory() {
        assert_eq!(include_globs("*.php, *.js", ""), ["*.php", "*.js"]);
        assert_eq!(include_globs("", "inc"), ["inc"]);
        assert_eq!(
            include_globs("*.php, ,*.js", "inc/App"),
            ["inc/App/**/*.php", "inc/App/**/*.js"]
        );
        assert!(include_globs(" , ", "").is_empty());
    }

    #[test]
    fn test_relative_directory_accepts_absolute_paths_inside_a_worktree() {
        let roots = [PathBuf::from("/work/site")];
        assert_eq!(relative_directory("/work/site/inc/", &roots), "inc");
        assert_eq!(relative_directory(" ./inc ", &roots), "inc");
        assert_eq!(relative_directory("/work/site", &roots), "");
        assert_eq!(relative_directory("inc/App", &roots), "inc/App");
    }

    #[test]
    fn test_comment_lines_are_told_apart() {
        assert!(is_comment_line("// note"));
        assert!(is_comment_line("* @param array $services"));
        assert!(is_comment_line("/** Docs */"));
        assert!(!is_comment_line("public $services = [];"));
        assert!(!is_comment_line("*ptr = 1;"));
    }

    #[test]
    fn test_clamp_range_stays_on_character_boundaries() {
        assert_eq!(clamp_range("héllo", &(1..2)), 1..1);
        assert_eq!(clamp_range("héllo", &(1..3)), 1..3);
        assert_eq!(clamp_range("abc", &(1..99)), 1..3);
        assert_eq!(clamp_range("abc", &(9..99)), 3..3);
    }
}
