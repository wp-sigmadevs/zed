use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    time::Duration,
};

use editor::{Editor, EditorEvent, HighlightKey, SelectionEffects, scroll::Autoscroll};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, HighlightStyle, IntoElement, ParentElement, Pixels, Render, Styled, Task,
    WeakEntity, Window, actions, px,
};
use language::{Buffer, BufferEvent, BufferSnapshot};
use project::{
    Project, ProjectItem as _, ProjectPath, SearchResults,
    buffer_store::BufferStoreEvent,
    search::{SearchQuery, SearchResult},
};
use text::{Anchor, Point, ToOffset as _, ToPoint as _};
use ui::{ListItem, ListItemSpacing, prelude::*};
use regex::{Regex, RegexBuilder};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
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

const DEFAULT_PATTERNS: [&str; 6] = [
    r"\bTODO\b",
    r"\bFIXME\b",
    r"\bHACK\b",
    r"\bXXX\b",
    "@todo",
    "shortcut:",
];

// Files larger than this are not coloured; scanning them on every edit would lag.
const MAX_HIGHLIGHT_BYTES: usize = 1_000_000;

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TodoSettings {
    pub patterns: Vec<String>,
    pub highlight: bool,
}

impl Settings for TodoSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let todo = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.todo.as_ref());
        Self {
            patterns: todo
                .and_then(|todo| todo.patterns.clone())
                .unwrap_or_default(),
            highlight: todo.and_then(|todo| todo.highlight).unwrap_or(true),
        }
    }
}

/// One regex for all patterns; an empty list falls back to the defaults.
fn combined_pattern(patterns: &[String]) -> String {
    let patterns: Vec<&str> = patterns
        .iter()
        .map(String::as_str)
        .filter(|pattern| !pattern.trim().is_empty())
        .collect();
    let patterns = if patterns.is_empty() {
        DEFAULT_PATTERNS.to_vec()
    } else {
        patterns
    };
    patterns
        .iter()
        .map(|pattern| format!("(?:{pattern})"))
        .collect::<Vec<_>>()
        .join("|")
}

pub fn init(cx: &mut App) {
    TodoSettings::register(cx);
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if editor.mode().is_full() && editor.project().is_some() {
            install_highlighter(cx);
        }
    })
    .detach();
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

/// Whether a match at `offset` sits in a comment: the grammar's comment scope, or a
/// comment marker before it on the line.
fn is_comment(snapshot: &BufferSnapshot, offset: usize, line_prefix: &str) -> bool {
    snapshot
        .language_scope_at(offset)
        .and_then(|scope| {
            scope
                .override_name()
                .map(|name| name.starts_with("comment"))
        })
        .unwrap_or(false)
        || has_comment_marker(line_prefix)
}

/// Theme colour slot per marker: 0 error, 1 warning, 2 info, 3 hint.
fn colour_class(label: &str) -> usize {
    match label {
        "FIXME" | "XXX" => 0,
        "HACK" => 1,
        "TODO" | "@TODO" => 2,
        _ => 3,
    }
}

/// Byte ranges and colour classes of the markers inside comments.
fn find_markers(snapshot: &BufferSnapshot, regex: &Regex) -> Vec<(Range<usize>, usize)> {
    let text = snapshot.text();
    regex
        .find_iter(&text)
        .filter_map(|found| {
            let row = snapshot.offset_to_point(found.start()).row;
            let line_start = snapshot.point_to_offset(Point::new(row, 0));
            let prefix = text.get(line_start..found.start()).unwrap_or_default();
            is_comment(snapshot, found.start(), prefix)
                .then(|| (found.range(), colour_class(&pattern_label(found.as_str()))))
        })
        .collect()
}

fn build_regex(pattern: &str) -> Option<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .log_err()
}

/// Colours TODO markers in a full editor; reruns on edits, reparse and settings changes.
fn install_highlighter(cx: &mut Context<Editor>) {
    let mut pending = Task::ready(());
    cx.subscribe_self(move |_, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited | EditorEvent::Reparsed(_)) {
            // Replacing the task drops, and so cancels, the previous run.
            drop(std::mem::replace(&mut pending, schedule_highlight(cx)));
        }
    })
    .detach();
    let mut last_settings = TodoSettings::get_global(cx).clone();
    let mut pending_settings = Task::ready(());
    cx.observe_global::<SettingsStore>(move |_, cx| {
        let settings = TodoSettings::get_global(cx);
        if *settings != last_settings {
            last_settings = settings.clone();
            drop(std::mem::replace(&mut pending_settings, schedule_highlight(cx)));
        }
    })
    .detach();
    schedule_highlight(cx).detach();
}

fn schedule_highlight(cx: &mut Context<Editor>) -> Task<()> {
    let settings = TodoSettings::get_global(cx);
    let buffer = cx.entity().read(cx).buffer().read(cx).as_singleton();
    let regex = settings
        .highlight
        .then(|| build_regex(&combined_pattern(&settings.patterns)))
        .flatten();
    let (Some(buffer), Some(regex)) = (buffer, regex) else {
        return cx.spawn(async move |editor, cx| {
            editor
                .update(cx, |editor, cx| apply_markers(editor, Vec::new(), cx))
                .log_err();
        });
    };
    let snapshot = buffer.read(cx).snapshot();
    cx.spawn(async move |editor, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(300))
            .await;
        let (snapshot, markers) = cx
            .background_spawn(async move {
                let markers = if snapshot.len() > MAX_HIGHLIGHT_BYTES {
                    Vec::new()
                } else {
                    find_markers(&snapshot, &regex)
                };
                (snapshot, markers)
            })
            .await;
        editor
            .update(cx, |editor, cx| {
                let markers = markers
                    .into_iter()
                    .map(|(range, class)| {
                        (
                            snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
                            class,
                        )
                    })
                    .collect();
                apply_markers(editor, markers, cx)
            })
            .log_err();
    })
}

fn apply_markers(
    editor: &mut Editor,
    markers: Vec<(Range<Anchor>, usize)>,
    cx: &mut Context<Editor>,
) {
    let status = cx.theme().status();
    let colours = [status.error, status.warning, status.info, status.hint];
    let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
    for (class, colour) in colours.into_iter().enumerate() {
        let ranges: Vec<_> = markers
            .iter()
            .filter(|(_, marker_class)| *marker_class == class)
            .filter_map(|(range, _)| {
                Some(
                    multibuffer_snapshot.anchor_in_buffer(range.start)?
                        ..multibuffer_snapshot.anchor_in_buffer(range.end)?,
                )
            })
            .collect();
        let key = HighlightKey::ZenithTodo(class);
        if ranges.is_empty() {
            editor.clear_highlights(key, cx);
        } else {
            editor.highlight_text(
                key,
                ranges,
                HighlightStyle {
                    color: Some(colour),
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
                cx,
            );
        }
    }
}

/// The group name for a matched marker: `todo:` and `Todo` both group under `TODO`.
fn pattern_label(matched: &str) -> SharedString {
    matched.trim_end_matches(':').to_uppercase().into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Project,
    CurrentFile,
    ChangedFiles,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupBy {
    File,
    Pattern,
}

struct TodoItem {
    buffer: Entity<Buffer>,
    path: SharedString,
    anchor: Anchor,
    row: u32,
    pattern: SharedString,
    text: SharedString,
}

pub struct TodoPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    scope: Scope,
    group_by: GroupBy,
    items: Vec<TodoItem>,
    scanned: bool,
    scanning: bool,
    limit_reached: bool,
    message: Option<SharedString>,
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
        let mut last_patterns = TodoSettings::get_global(cx).patterns.clone();
        cx.observe_global::<SettingsStore>(move |this, cx| {
            let patterns = &TodoSettings::get_global(cx).patterns;
            if *patterns != last_patterns {
                last_patterns = patterns.clone();
                if this.scanned {
                    this.rescan(cx);
                }
            }
        })
        .detach();
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            cx.subscribe(&workspace_entity, |this, _, event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged)
                    && this.scanned
                    && this.scope == Scope::CurrentFile
                {
                    this.rescan(cx);
                }
            })
            .detach();
        }
        Self {
            project,
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Bottom,
            scope: Scope::Project,
            group_by: GroupBy::File,
            items: Vec::new(),
            scanned: false,
            scanning: false,
            limit_reached: false,
            message: None,
            scan_task: Task::ready(()),
        }
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.rescan(cx);
    }

    fn active_buffer(&self, cx: &App) -> Option<Entity<Buffer>> {
        self.workspace
            .upgrade()?
            .read(cx)
            .active_item_as::<Editor>(cx)?
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
    }

    fn changed_paths(&self, cx: &App) -> HashSet<ProjectPath> {
        let git_store = self.project.read(cx).git_store().read(cx);
        git_store
            .repositories()
            .values()
            .flat_map(|repository| {
                let repository = repository.read(cx);
                repository
                    .cached_status()
                    .filter(|entry| !entry.status.is_deleted())
                    .filter_map(|entry| repository.repo_path_to_project_path(&entry.repo_path, cx))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.scanned = true;
        self.message = None;
        let buffers = match self.scope {
            Scope::CurrentFile => match self.active_buffer(cx) {
                Some(buffer) => Some(vec![buffer]),
                None => {
                    self.finish(Vec::new(), false, Some("No file is open.".into()), cx);
                    return;
                }
            },
            Scope::Project | Scope::ChangedFiles => None,
        };
        let changed_paths = (self.scope == Scope::ChangedFiles).then(|| self.changed_paths(cx));
        if changed_paths.as_ref().is_some_and(|paths| paths.is_empty()) {
            self.finish(Vec::new(), false, Some("No changed files.".into()), cx);
            return;
        }
        let pattern = combined_pattern(&TodoSettings::get_global(cx).patterns);
        let query = match SearchQuery::regex(
            pattern,
            false,
            false,
            false,
            true,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            buffers,
        ) {
            Ok(query) => query,
            Err(error) => {
                self.finish(Vec::new(), false, Some(error.to_string().into()), cx);
                return;
            }
        };
        let SearchResults { task_handle, rx } = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        self.scanning = true;
        cx.notify();
        // shortcut: rescans the whole scope on every save; scan just the saved file if it gets slow on big repos.
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
                let items = found
                    .into_iter()
                    .filter(|(buffer, _)| {
                        changed_paths.as_ref().is_none_or(|paths| {
                            buffer
                                .read(cx)
                                .project_path(cx)
                                .is_some_and(|path| paths.contains(&path))
                        })
                    })
                    .flat_map(|(buffer, ranges)| todo_items(buffer, ranges, cx))
                    .collect();
                this.finish(items, limit_reached, None, cx);
            })
            .log_err();
        });
    }

    fn finish(
        &mut self,
        mut items: Vec<TodoItem>,
        limit_reached: bool,
        message: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        items.sort_by(|a, b| a.path.cmp(&b.path).then(a.row.cmp(&b.row)));
        self.items = items;
        self.limit_reached = limit_reached;
        self.message = message;
        self.scanning = false;
        cx.notify();
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

    fn groups(&self) -> BTreeMap<SharedString, Vec<&TodoItem>> {
        let mut groups: BTreeMap<SharedString, Vec<&TodoItem>> = BTreeMap::new();
        for item in &self.items {
            let key = match self.group_by {
                GroupBy::File => item.path.clone(),
                GroupBy::Pattern => item.pattern.clone(),
            };
            groups.entry(key).or_default().push(item);
        }
        groups
    }
}

fn todo_items(
    buffer: Entity<Buffer>,
    ranges: Vec<Range<Anchor>>,
    cx: &App,
) -> Vec<TodoItem> {
    let snapshot = buffer.read(cx).snapshot();
    let Some(file) = buffer.read(cx).file() else {
        return Vec::new();
    };
    let path: SharedString = file.full_path(cx).to_string_lossy().into_owned().into();
    ranges
        .into_iter()
        .filter_map(|range| {
            let point = range.start.to_point(&snapshot);
            let line: String = snapshot
                .text_for_range(
                    Point::new(point.row, 0)..Point::new(point.row, snapshot.line_len(point.row)),
                )
                .collect();
            let prefix = line.get(..point.column as usize).unwrap_or_default();
            if !is_comment(&snapshot, range.start.to_offset(&snapshot), prefix) {
                return None;
            }
            let matched: String = snapshot.text_for_range(range.clone()).collect();
            Some(TodoItem {
                buffer: buffer.clone(),
                path: path.clone(),
                anchor: range.start,
                row: point.row,
                pattern: pattern_label(&matched),
                text: line.trim().to_string().into(),
            })
        })
        .collect()
}

impl Render for TodoPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = self.groups();
        let file_count = self
            .items
            .iter()
            .map(|item| &item.path)
            .collect::<HashSet<_>>()
            .len();
        let status = if self.scanning {
            "Scanning…".to_string()
        } else if let Some(message) = &self.message {
            message.to_string()
        } else {
            let limit = if self.limit_reached {
                " (limit reached)"
            } else {
                ""
            };
            format!(
                "{} TODO items in {file_count} files{limit}",
                self.items.len()
            )
        };
        let scope_button = |id: &'static str, label: &'static str, scope: Scope| {
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.scope == scope)
                .on_click(cx.listener(move |this, _, _, cx| this.set_scope(scope, cx)))
        };
        let group_button = |id: &'static str, label: &'static str, group_by: GroupBy| {
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.group_by == group_by)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.group_by = group_by;
                    cx.notify();
                }))
        };
        let toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .child(scope_button(
                "zenith-todo-project",
                "Project",
                Scope::Project,
            ))
            .child(scope_button(
                "zenith-todo-current",
                "Current File",
                Scope::CurrentFile,
            ))
            .child(scope_button(
                "zenith-todo-changed",
                "Changed Files",
                Scope::ChangedFiles,
            ))
            .child(div().w_2())
            .child(group_button(
                "zenith-todo-by-file",
                "By File",
                GroupBy::File,
            ))
            .child(group_button(
                "zenith-todo-by-pattern",
                "By Pattern",
                GroupBy::Pattern,
            ))
            .child(div().flex_1())
            .child(Label::new(status).color(Color::Muted))
            .child(
                IconButton::new("zenith-todo-refresh", IconName::RotateCw)
                    .tooltip(ui::Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.rescan(cx))),
            );

        let show_path = self.group_by == GroupBy::Pattern;
        let mut list = v_flex()
            .id("zenith-todo-items")
            .flex_1()
            .overflow_y_scroll();
        let mut row_index = 0usize;
        for (key, items) in &groups {
            list = list.child(
                ListItem::new(("todo-group", row_index))
                    .spacing(ListItemSpacing::Dense)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(key.clone()))
                            .child(Label::new(items.len().to_string()).color(Color::Muted)),
                    ),
            );
            row_index += 1;
            for item in items {
                let buffer = item.buffer.clone();
                let anchor = item.anchor;
                let location = if show_path {
                    format!("{}:{}", item.path, item.row + 1)
                } else {
                    (item.row + 1).to_string()
                };
                list = list.child(
                    ListItem::new(("todo-item", row_index))
                        .spacing(ListItemSpacing::Dense)
                        .indent_level(1)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open(buffer.clone(), anchor, window, cx)
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(location).color(Color::Muted))
                                .child(Label::new(item.text.clone()).truncate()),
                        ),
                );
                row_index += 1;
            }
        }
        v_flex()
            .key_context("TodoPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.rescan(cx)))
            .size_full()
            .child(toolbar)
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
        let regex = build_regex(&combined_pattern(&[])).expect("valid pattern");
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

    #[test]
    fn pattern_labels_group_case_and_colon() {
        assert_eq!(pattern_label("todo"), pattern_label("TODO"));
        assert_eq!(pattern_label("Fixme"), "FIXME");
        assert_eq!(pattern_label("shortcut:"), "SHORTCUT");
        assert_eq!(pattern_label("@todo"), "@TODO");
    }

    #[test]
    fn custom_patterns_replace_defaults() {
        let regex =
            build_regex(&combined_pattern(&["NOTE".into(), " ".into()])).expect("valid pattern");
        assert!(regex.is_match("note: x"));
        assert!(!regex.is_match("TODO"));
        assert!(!regex.is_match(" "));
    }

    #[test]
    fn colour_per_marker() {
        assert_eq!(colour_class("FIXME"), 0);
        assert_eq!(colour_class("HACK"), 1);
        assert_eq!(colour_class("TODO"), 2);
        assert_eq!(colour_class("SHORTCUT"), 3);
    }

    #[gpui::test]
    fn markers_only_inside_comments(cx: &mut gpui::TestAppContext) {
        let buffer = cx.new(|cx| {
            Buffer::local(
                "let todo = 1; // TODO: one\n/* FIXME two */\nlet s = \"HACK\";\n",
                cx,
            )
        });
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let regex = build_regex(&combined_pattern(&[])).expect("valid pattern");
        let found: Vec<(String, usize)> = find_markers(&snapshot, &regex)
            .into_iter()
            .map(|(range, class)| (snapshot.text_for_range(range).collect(), class))
            .collect();
        assert_eq!(found, [("TODO".into(), 2), ("FIXME".into(), 0)]);
    }
}
