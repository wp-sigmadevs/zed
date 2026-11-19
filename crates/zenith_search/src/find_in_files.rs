//! PhpStorm's Find in Files / Replace in Files popup (⌘⇧F / ⌘⇧R): a query box with
//! case, word and regex toggles, a file mask, live results, and a hand-off to the
//! project search tab ("Open in Find Window") that does the replacing.

use std::ops::Range;

use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Task, WeakEntity, Window, div, prelude::*, px,
    rems,
};
use picker::{Picker, PickerDelegate};
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use text::{Point, ToPoint as _};
use ui::{
    Button, ButtonStyle, Checkbox, HighlightedLabel, Label, ListItem, ListItemSpacing, TintColor,
    prelude::*,
};
use util::paths::PathMatcher;
use workspace::{ModalView, Workspace, pane::DeploySearch};

/// More hits than this say "refine the query", not "scroll".
const MAX_HITS: usize = 300;
/// PhpStorm's title band over the list.
const BAND: u32 = 0x393b40;

pub struct FindInFilesModal {
    picker: Entity<Picker<FindDelegate>>,
    mask: Entity<Editor>,
    replace: Option<Entity<Editor>>,
    open_in_tab: bool,
}

impl ModalView for FindInFilesModal {}
impl EventEmitter<DismissEvent> for FindInFilesModal {}

impl Focusable for FindInFilesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
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
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(
                workspace_handle,
                project,
                replace,
                previous_focus,
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
        let mask = field("*.php, *.js", window, cx);
        let replace_field = replace.then(|| field("Replace with", window, cx));
        let delegate = FindDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            project,
            mask: mask.clone(),
            hits: Vec::new(),
            selected: 0,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            query: String::new(),
            previous_focus,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        cx.subscribe_in(&mask, window, |this, _, event: &EditorEvent, window, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.picker
                    .update(cx, |picker, cx| picker.refresh(window, cx));
            }
        })
        .detach();
        Self {
            picker,
            mask,
            replace: replace_field,
            open_in_tab: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn picker(&self) -> &Entity<Picker<FindDelegate>> {
        &self.picker
    }

    #[cfg(test)]
    pub(crate) fn set_mask(&mut self, mask: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.mask
            .update(cx, |editor, cx| editor.set_text(mask, window, cx));
    }

    fn toggle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        flip: impl FnOnce(&mut FindDelegate),
    ) {
        self.picker.update(cx, |picker, cx| {
            flip(&mut picker.delegate);
            picker.refresh(window, cx);
        });
        cx.notify();
    }

    /// Hands the query to the project search tab, which also does replacing.
    fn open_in_find_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = &self.picker.read(cx).delegate;
        let mask = self.mask.read(cx).text(cx);
        let action = DeploySearch {
            replace_enabled: self.replace.is_some(),
            included_files: (!mask.trim().is_empty()).then_some(mask),
            query: Some(delegate.query.clone()),
            regex: Some(delegate.regex),
            case_sensitive: Some(delegate.case_sensitive),
            whole_word: Some(delegate.whole_word),
            ..Default::default()
        };
        let focus = delegate.previous_focus.clone();
        cx.emit(DismissEvent);
        window.focus(&focus, cx);
        window.dispatch_action(Box::new(action), cx);
    }
}

impl Render for FindInFilesModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let delegate = &self.picker.read(cx).delegate;
        let (case, word, regex) = (delegate.case_sensitive, delegate.whole_word, delegate.regex);
        let border = cx.theme().colors().border_variant;
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .toggle_state(on)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        };
        v_flex()
            .key_context("FindInFiles")
            .w(rems(44.))
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .h_11()
                    .px_4()
                    .gap_3()
                    .justify_between()
                    .bg(gpui::rgb(BAND))
                    .child(
                        Label::new(if self.replace.is_some() {
                            "Replace in Files"
                        } else {
                            "Find in Files"
                        })
                        .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new("File mask:").size(LabelSize::Small))
                            .child(
                                div()
                                    .w(rems(9.))
                                    .px_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(border)
                                    .child(self.mask.clone()),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(border)
                    .child(toggle("case", "Aa", case).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.toggle(window, cx, |d| d.case_sensitive = !d.case_sensitive)
                        },
                    )))
                    .child(toggle("word", "W", word).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.toggle(window, cx, |d| d.whole_word = !d.whole_word)
                        },
                    )))
                    .child(toggle("regex", ".*", regex).on_click(cx.listener(
                        |this, _, window, cx| this.toggle(window, cx, |d| d.regex = !d.regex),
                    ))),
            )
            .children(self.replace.clone().map(|replace| {
                div()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .child(replace)
            }))
            .child(div().h(px(360.)).child(self.picker.clone()))
            .child(
                h_flex()
                    .h_10()
                    .px_3()
                    .justify_between()
                    .bg(gpui::rgb(0x303236))
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Checkbox::new("zenith-find-new-tab", self.open_in_tab.into())
                            .label("Open results in new tab")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_in_tab = !this.open_in_tab;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("zenith-find-window", "Open in Find Window  ⌘↵")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_find_window(window, cx)
                            })),
                    ),
            )
    }
}

/// One matching line.
#[derive(Clone)]
pub struct Hit {
    project_path: ProjectPath,
    file_name: SharedString,
    directory: SharedString,
    point: Point,
    /// The line with leading space removed, and the byte ranges that matched in it.
    line: String,
    matches: Vec<Range<usize>>,
}

pub struct FindDelegate {
    modal: WeakEntity<FindInFilesModal>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    mask: Entity<Editor>,
    hits: Vec<Hit>,
    selected: usize,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    query: String,
    previous_focus: FocusHandle,
}

impl FindDelegate {
    fn hits_for(
        buffer: &Entity<language::Buffer>,
        ranges: &[Range<text::Anchor>],
        cx: &App,
    ) -> Vec<Hit> {
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

impl PickerDelegate for FindDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "Find in Files"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> std::sync::Arc<str> {
        "Search all files…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(if self.query.trim().is_empty() {
            "Type search query to find in files".into()
        } else {
            "No matches".into()
        })
    }

    fn match_count(&self) -> usize {
        self.hits.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        self.selected = 0;
        let clear = |this: &mut Self, cx: &mut Context<Picker<Self>>| {
            this.hits.clear();
            cx.notify();
        };
        if query.trim().is_empty() {
            clear(self, cx);
            return Task::ready(());
        }
        let path_style = self.project.read(cx).path_style(cx);
        let globs: Vec<String> = self
            .mask
            .read(cx)
            .text(cx)
            .split(',')
            .map(|glob| glob.trim().to_string())
            .filter(|glob| !glob.is_empty())
            .collect();
        let Ok(include) = PathMatcher::new(globs, path_style) else {
            clear(self, cx);
            return Task::ready(());
        };
        let built = if self.regex {
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
        };
        let Ok(built) = built else {
            // An unfinished regex is not an error worth showing while typing.
            clear(self, cx);
            return Task::ready(());
        };
        let results = self
            .project
            .update(cx, |project, cx| project.search(built, cx));
        cx.spawn_in(window, async move |picker, cx| {
            let _search = results.task_handle;
            let mut hits: Vec<Hit> = Vec::new();
            while let Ok(result) = results.rx.recv().await {
                let SearchResult::Buffer { buffer, ranges } = result else {
                    continue;
                };
                let found = cx.update(|_, cx| Self::hits_for(&buffer, &ranges, cx));
                let Ok(found) = found else { return };
                hits.extend(found);
                if hits.len() >= MAX_HITS {
                    hits.truncate(MAX_HITS);
                    break;
                }
            }
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.hits = hits;
                    picker.delegate.selected = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let open_in_tab = self
            .modal
            .read_with(cx, |modal, _| modal.open_in_tab)
            .unwrap_or(false);
        if secondary || open_in_tab {
            self.modal
                .update(cx, |modal, cx| modal.open_in_find_window(window, cx))
                .ok();
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
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.hits.get(ix)?;
        let positions: Vec<usize> = hit
            .line
            .char_indices()
            .map(|(offset, _)| offset)
            .filter(|offset| hit.matches.iter().any(|range| range.contains(offset)))
            .collect();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .min_w_0()
                        .child(
                            HighlightedLabel::new(hit.line.clone(), positions)
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .child(
                            Label::new(format!(
                                "{}:{}  {}",
                                hit.file_name,
                                hit.point.row + 1,
                                hit.directory
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                ),
        )
    }
}
