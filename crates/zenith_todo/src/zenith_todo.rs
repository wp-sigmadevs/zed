use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, Styled, Task, WeakEntity, Window, actions, px,
};
use language::{Buffer, BufferEvent};
use project::{
    Project, SearchResults,
    buffer_store::BufferStoreEvent,
    search::{SearchQuery, SearchResult},
};
use text::{Anchor, Point, ToOffset as _, ToPoint as _};
use ui::{IconName, Label, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathMatcher};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    zenith_todo,
    [
        /// Toggles focus on the TODO panel.
        ToggleFocus,
        /// Scans the project again for TODO comments.
        Refresh,
    ]
);

// shortcut: fixed patterns, no `zenith.todo` setting yet; add one to settings_content in slice 2.
const PATTERN: &str = r"\bTODO\b|\bFIXME\b|\bHACK\b|\bXXX\b|@todo|shortcut:";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TodoPanel>(window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| TodoPanel::new(workspace, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

/// Text before a match that marks it as a comment, for languages whose grammar has no
/// comment override scope (e.g. the PHP extension).
fn has_comment_marker(prefix: &str) -> bool {
    prefix.trim_start().starts_with('*')
        || ["//", "#", "/*", "<!--", "{#"]
            .iter()
            .any(|marker| prefix.contains(marker))
}

struct TodoItem {
    anchor: Anchor,
    row: u32,
    text: SharedString,
}

struct FileGroup {
    path: SharedString,
    buffer: Entity<Buffer>,
    items: Vec<TodoItem>,
}

pub struct TodoPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    groups: Vec<FileGroup>,
    scanned: bool,
    scanning: bool,
    limit_reached: bool,
    error: Option<SharedString>,
    scan_task: Task<()>,
}

impl TodoPanel {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                cx.subscribe(buffer, |this: &mut Self, _, event, cx| {
                    if matches!(event, BufferEvent::Saved) && this.scanned {
                        this.rescan(cx);
                    }
                })
                .detach();
            }
        })
        .detach();
        Self {
            project,
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Bottom,
            groups: Vec::new(),
            scanned: false,
            scanning: false,
            limit_reached: false,
            error: None,
            scan_task: Task::ready(()),
        }
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.scanned = true;
        let query = match SearchQuery::regex(
            PATTERN,
            false,
            false,
            false,
            true,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        ) {
            Ok(query) => query,
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let SearchResults { task_handle, rx } = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        self.scanning = true;
        self.error = None;
        cx.notify();
        // shortcut: rescans the whole project on every save; scan just the saved file if it gets slow on big repos.
        self.scan_task = cx.spawn(async move |this, cx| {
            let _search = task_handle;
            let mut found = Vec::new();
            let mut limit_reached = false;
            while let Ok(result) = rx.recv().await {
                match result {
                    SearchResult::Buffer { buffer, ranges } => found.push((buffer, ranges)),
                    SearchResult::LimitReached => limit_reached = true,
                    SearchResult::WaitingForScan | SearchResult::Searching => {}
                }
            }
            this.update(cx, |this, cx| {
                this.groups = found
                    .into_iter()
                    .filter_map(|(buffer, ranges)| file_group(buffer, ranges, cx))
                    .collect();
                this.groups.sort_by(|a, b| a.path.cmp(&b.path));
                this.limit_reached = limit_reached;
                this.scanning = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn open(&self, buffer: Entity<Buffer>, anchor: Anchor, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let editor = workspace
                .open_project_item::<Editor>(None, buffer, true, true, true, true, window, cx);
            editor.update(cx, |editor, cx| {
                let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(anchor) else {
                    return;
                };
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()).unfold(),
                    window,
                    cx,
                    |selections| selections.select_ranges([anchor..anchor]),
                );
            });
        });
    }
}

fn file_group(
    buffer: Entity<Buffer>,
    ranges: Vec<std::ops::Range<Anchor>>,
    cx: &App,
) -> Option<FileGroup> {
    let snapshot = buffer.read(cx).snapshot();
    let path = buffer
        .read(cx)
        .file()?
        .full_path(cx)
        .to_string_lossy()
        .into_owned();
    let items: Vec<TodoItem> = ranges
        .into_iter()
        .filter_map(|range| {
            let point = range.start.to_point(&snapshot);
            let line: String = snapshot
                .text_for_range(
                    Point::new(point.row, 0)..Point::new(point.row, snapshot.line_len(point.row)),
                )
                .collect();
            let prefix = line.get(..point.column as usize).unwrap_or_default();
            let in_comment_scope = snapshot
                .language_scope_at(range.start.to_offset(&snapshot))
                .and_then(|scope| {
                    scope
                        .override_name()
                        .map(|name| name.starts_with("comment"))
                })
                .unwrap_or(false);
            (in_comment_scope || has_comment_marker(prefix)).then(|| TodoItem {
                anchor: range.start,
                row: point.row,
                text: line.trim().to_string().into(),
            })
        })
        .collect();
    (!items.is_empty()).then(|| FileGroup {
        path: path.into(),
        buffer,
        items,
    })
}

impl Render for TodoPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count: usize = self.groups.iter().map(|group| group.items.len()).sum();
        let status = if self.scanning {
            "Scanning…".to_string()
        } else if let Some(error) = &self.error {
            error.to_string()
        } else if self.limit_reached {
            format!(
                "{count} TODO items in {} files (limit reached)",
                self.groups.len()
            )
        } else {
            format!("{count} TODO items in {} files", self.groups.len())
        };
        let mut list = v_flex()
            .id("zenith-todo-items")
            .flex_1()
            .overflow_y_scroll();
        for (group_index, group) in self.groups.iter().enumerate() {
            list = list.child(
                ListItem::new(("todo-file", group_index))
                    .spacing(ListItemSpacing::Dense)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(group.path.clone()))
                            .child(Label::new(group.items.len().to_string()).color(Color::Muted)),
                    ),
            );
            for (item_index, item) in group.items.iter().enumerate() {
                let buffer = group.buffer.clone();
                let anchor = item.anchor;
                list = list.child(
                    ListItem::new(("todo-item", group_index * 100_000 + item_index))
                        .spacing(ListItemSpacing::Dense)
                        .indent_level(1)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(buffer.clone(), anchor, window, cx)
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(format!("{}", item.row + 1)).color(Color::Muted))
                                .child(Label::new(item.text.clone()).truncate()),
                        ),
                );
            }
        }
        v_flex()
            .key_context("TodoPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.rescan(cx)))
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(Label::new(status).color(Color::Muted))
                    .child(
                        IconButton::new("zenith-todo-refresh", IconName::RotateCw)
                            .tooltip(ui::Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
                    ),
            )
            .child(list)
    }
}

impl Focusable for TodoPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TodoPanel {}

impl Panel for TodoPanel {
    fn persistent_name() -> &'static str {
        "TODO Panel"
    }

    fn panel_key() -> &'static str {
        "TodoPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    // shortcut: position is not saved across restarts; persist it when the setting lands.
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("TODO")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active && !self.scanned {
            self.rescan(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_markers() {
        for prefix in ["// ", "    # ", "/* ", " * ", "<!-- ", "{# ", "$a = 1; // "] {
            assert!(has_comment_marker(prefix), "{prefix:?}");
        }
        for prefix in ["", "let todo_list = ", "fn "] {
            assert!(!has_comment_marker(prefix), "{prefix:?}");
        }
    }

    #[test]
    fn pattern_matches_markers_only() {
        let regex = regex::RegexBuilder::new(PATTERN)
            .case_insensitive(true)
            .build()
            .expect("valid pattern");
        for text in [
            "TODO: x",
            "fixme",
            "HACK",
            "xxx",
            "@todo",
            "shortcut: limit",
        ] {
            assert!(regex.is_match(text), "{text:?}");
        }
        for text in ["TODO_LIST", "todos", "hacker"] {
            assert!(!regex.is_match(text), "{text:?}");
        }
    }
}
