use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    ops::Range,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use editor::{Editor, EditorEvent, HighlightKey, SelectionEffects, scroll::Autoscroll};
use file_icons::FileIcons;
use fs::Fs;
use git::{blame::BlameEntry, repository::DiffType};
use gpui::{AnyElement, 
    Action, App, AppContext as _, ClickEvent, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, FontWeight, Global, HighlightStyle, IntoElement, ParentElement, Pixels, PromptLevel,
    Render, Role, ScrollStrategy, Styled, Task, UniformListScrollHandle, WeakEntity, Window,
    actions, px, uniform_list,
};
use language::{Buffer, BufferEvent, BufferSnapshot};
use project::{
    Project, ProjectItem as _, ProjectPath, SearchResults,
    buffer_store::BufferStoreEvent,
    git_store::Repository,
    search::{SearchQuery, SearchResult},
};
use regex::{Regex, RegexBuilder};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore, ZenithTodoScope};
use text::{Anchor, Point, ToOffset as _, ToPoint as _};
use ui::{
    ButtonLike, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, ListItem, ListItemSpacing,
    Tooltip, prelude::*,
};
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
        /// Opens the next TODO item.
        NextTodo,
        /// Opens the previous TODO item.
        PreviousTodo,
        /// Expands every group in the TODO tool window.
        ExpandAll,
        /// Collapses every group in the TODO tool window.
        CollapseAll,
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
    pub scope: Scope,
    pub dock: DockPosition,
    pub enabled: bool,
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
            scope: match todo.and_then(|todo| todo.scope) {
                Some(ZenithTodoScope::CurrentFile) => Scope::CurrentFile,
                Some(ZenithTodoScope::ChangedFiles) => Scope::ChangedFiles,
                Some(ZenithTodoScope::Project) | None => Scope::Project,
            },
            dock: match todo.and_then(|todo| todo.dock) {
                Some(settings::DockPosition::Left) => DockPosition::Left,
                Some(settings::DockPosition::Right) => DockPosition::Right,
                Some(settings::DockPosition::Bottom) | None => DockPosition::Bottom,
            },
            enabled: todo.and_then(|todo| todo.enabled).unwrap_or(true),
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

/// Marks that `init` ran, so the commit hook stays off when the feature is compiled out of `zenith`.
struct Installed;

impl Global for Installed {}

pub fn init(cx: &mut App) {
    TodoSettings::register(cx);
    cx.set_global(Installed);
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if editor.mode().is_full() && editor.project().is_some() {
            install_highlighter(editor, cx);
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
        // shortcut: read once per workspace, so toggling `enabled` shows or hides the panel in new windows only; add/remove on settings change if that bites.
        if !TodoSettings::get_global(cx).enabled {
            return;
        }
        let panel = cx.new(|cx| TodoPanel::new(workspace, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

/// Text before a match that marks it as a comment, for languages whose grammar has no
/// comment override scope (e.g. the PHP extension).
fn has_comment_marker(prefix: &str) -> bool {
    if prefix.trim_start().starts_with('*') {
        return true;
    }
    ["//", "#", "/*", "<!--", "{#"].iter().any(|marker| {
        prefix
            .match_indices(marker)
            .any(|(index, _)| !inside_string(&prefix[..index]))
    })
}

/// Whether text ending here leaves a `"` or `'` string open; backslash escapes are skipped.
fn inside_string(text: &str) -> bool {
    let mut open: Option<char> = None;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if let Some(quote) = open {
            if character == quote {
                open = None;
            }
        } else if character == '"' || character == '\'' {
            open = Some(character);
        }
    }
    open.is_some()
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
fn install_highlighter(editor: &Editor, cx: &mut Context<Editor>) {
    let mut pending = Task::ready(());
    cx.subscribe_self(move |editor, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::BufferEdited | EditorEvent::Reparsed(_)) {
            // Replacing the task drops, and so cancels, the previous run.
            drop(std::mem::replace(
                &mut pending,
                schedule_highlight(editor, cx),
            ));
        }
    })
    .detach();
    let mut last_settings = TodoSettings::get_global(cx).clone();
    let mut pending_settings = Task::ready(());
    cx.observe_global::<SettingsStore>(move |editor, cx| {
        let settings = TodoSettings::get_global(cx);
        if *settings != last_settings {
            last_settings = settings.clone();
            drop(std::mem::replace(
                &mut pending_settings,
                schedule_highlight(editor, cx),
            ));
        }
    })
    .detach();
    schedule_highlight(editor, cx).detach();
}

// Takes the editor by reference: during `observe_new` and its own events it is mid-update,
// so reading it back through `cx.entity()` panics.
fn schedule_highlight(editor: &Editor, cx: &mut Context<Editor>) -> Task<()> {
    let settings = TodoSettings::get_global(cx);
    let buffer = editor.buffer().read(cx).as_singleton();
    let regex = (settings.enabled && settings.highlight)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Project,
    CurrentFile,
    ChangedFiles,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupBy {
    File,
    Pattern,
}

/// A node of the tree that can be collapsed.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Node {
    Summary,
    Project,
    Group(SharedString),
}

/// One visible line of the tree; `Item` indexes `TodoPanel::items`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Row {
    Summary,
    Project,
    Group(SharedString, usize),
    Item(usize),
}

const HEADER_HEIGHT: f32 = 38.;
const ROW_HEIGHT: f32 = 26.;
const STRIP_WIDTH: f32 = 36.;
const ITEM_INDENT: f32 = 20.;

/// The top row: "Found 12 TODO items in 2 files", or what replaces it while scanning or
/// when there is nothing to scan.
fn summary_text(
    items: usize,
    files: usize,
    limit_reached: bool,
    scanning: bool,
    message: Option<&str>,
) -> String {
    if scanning {
        return "Scanning…".to_string();
    }
    if let Some(message) = message {
        return message.to_string();
    }
    let plural = |count: usize, one: &str, many: &str| {
        if count == 1 {
            one.to_string()
        } else {
            many.to_string()
        }
    };
    format!(
        "Found {items} TODO {} in {files} {}{}",
        plural(items, "item", "items"),
        plural(files, "file", "files"),
        if limit_reached {
            " (limit reached)"
        } else {
            ""
        }
    )
}

/// "1 item" / "12 items".
fn items_label(count: usize) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    }
}

struct TodoItem {
    buffer: Entity<Buffer>,
    path: SharedString,
    anchor: Anchor,
    row: u32,
    pattern: SharedString,
    text: SharedString,
    /// Author and unix time of the line, once the file is blamed.
    blame: Option<(SharedString, i64)>,
}

/// How recently a TODO line was last changed, for the age chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Age {
    Any,
    Week,
    Month,
    Year,
}

impl Age {
    const CHIPS: [(Age, &'static str); 4] = [
        (Age::Any, "Any"),
        (Age::Week, "7 d"),
        (Age::Month, "30 d"),
        (Age::Year, "1 y"),
    ];

    fn max_seconds(self) -> Option<i64> {
        const DAY: i64 = 24 * 60 * 60;
        match self {
            Age::Any => None,
            Age::Week => Some(7 * DAY),
            Age::Month => Some(30 * DAY),
            Age::Year => Some(365 * DAY),
        }
    }
}

const NOT_COMMITTED: &str = "Not Committed Yet";

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Author and time of `row`; blame leaves uncommitted lines out, so those are new.
fn line_blame(entries: &[BlameEntry], row: u32, now: i64) -> (SharedString, i64) {
    match entries.iter().find(|entry| entry.range.contains(&row)) {
        Some(entry) => (
            entry.author.clone().unwrap_or_default().into(),
            entry.author_time.unwrap_or(now),
        ),
        None => (NOT_COMMITTED.into(), now),
    }
}

/// Whether an item passes the author and age filters; an unblamed item passes only when
/// no filter is set.
fn passes_filters(
    blame: Option<&(SharedString, i64)>,
    author: Option<&str>,
    age: Age,
    now: i64,
) -> bool {
    if author.is_none() && age == Age::Any {
        return true;
    }
    let Some((item_author, time)) = blame else {
        return false;
    };
    author.is_none_or(|author| item_author.as_ref() == author)
        && age.max_seconds().is_none_or(|max| now - time <= max)
}

/// A file's blame and the buffer version it was taken at; `None` entries mean the file
/// has no blame (not in a repository, or blame failed).
struct FileBlame {
    version: clock::Global,
    entries: Option<Vec<BlameEntry>>,
}

pub struct TodoPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    scope: Scope,
    group_by: GroupBy,
    items: Vec<TodoItem>,
    rows: Vec<Row>,
    collapsed: HashSet<Node>,
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    scanned: bool,
    scanning: bool,
    limit_reached: bool,
    message: Option<SharedString>,
    scan_task: Task<()>,
    buffer_tasks: HashMap<EntityId, Task<()>>,
    blame: HashMap<EntityId, FileBlame>,
    blame_task: Task<()>,
    author_filter: Option<SharedString>,
    age_filter: Age,
}

impl TodoPanel {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                cx.subscribe(buffer, |this: &mut Self, buffer, event, cx| {
                    if matches!(event, BufferEvent::Saved) && this.scanned {
                        this.rescan_buffer(buffer, cx);
                    }
                })
                .detach();
            }
        })
        .detach();
        let settings = TodoSettings::get_global(cx).clone();
        let mut last_settings = settings.clone();
        cx.observe_global::<SettingsStore>(move |this, cx| {
            let settings = TodoSettings::get_global(cx).clone();
            let rescan = settings.patterns != last_settings.patterns
                || settings.scope != last_settings.scope;
            if settings.scope != last_settings.scope {
                this.scope = settings.scope;
            }
            last_settings = settings;
            if rescan && this.scanned {
                this.rescan(cx);
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
            fs: workspace.app_state().fs.clone(),
            focus_handle: cx.focus_handle(),
            scope: settings.scope,
            group_by: GroupBy::File,
            items: Vec::new(),
            rows: vec![Row::Summary],
            collapsed: HashSet::new(),
            selected: Some(0),
            scroll_handle: UniformListScrollHandle::new(),
            scanned: false,
            scanning: false,
            limit_reached: false,
            message: None,
            scan_task: Task::ready(()),
            buffer_tasks: HashMap::default(),
            blame: HashMap::default(),
            blame_task: Task::ready(()),
            author_filter: None,
            age_filter: Age::Any,
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

    /// Starts a project search for the configured patterns, limited to `buffers` when given.
    fn search(
        &self,
        buffers: Option<Vec<Entity<Buffer>>>,
        cx: &mut Context<Self>,
    ) -> Result<
        impl Future<Output = (Vec<(Entity<Buffer>, Vec<Range<Anchor>>)>, bool)> + use<>,
        SharedString,
    > {
        let pattern = combined_pattern(&TodoSettings::get_global(cx).patterns);
        let query = SearchQuery::regex(
            pattern,
            false,
            false,
            false,
            true,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            buffers,
        )
        .map_err(|error| SharedString::from(error.to_string()))?;
        let SearchResults { task_handle, rx } = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        Ok(async move {
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
            (found, limit_reached)
        })
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        self.scanned = true;
        self.message = None;
        self.buffer_tasks.clear();
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
        let search = match self.search(buffers, cx) {
            Ok(search) => search,
            Err(error) => {
                self.finish(Vec::new(), false, Some(error), cx);
                return;
            }
        };
        self.scanning = true;
        cx.notify();
        self.scan_task = cx.spawn(async move |this, cx| {
            let (found, limit_reached) = search.await;
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

    /// After a save, searches only the saved file and swaps its rows in place.
    fn rescan_buffer(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.entity_id();
        let in_scope = match self.scope {
            Scope::Project => true,
            Scope::CurrentFile => self.active_buffer(cx).as_ref() == Some(&buffer),
            // shortcut: git status may lag the save, so a newly changed file shows after Refresh; rescan on git events if that bites.
            Scope::ChangedFiles => buffer
                .read(cx)
                .project_path(cx)
                .is_some_and(|path| self.changed_paths(cx).contains(&path)),
        };
        if !in_scope {
            self.items
                .retain(|item| item.buffer.entity_id() != buffer_id);
            cx.notify();
            return;
        }
        let Ok(search) = self.search(Some(vec![buffer]), cx) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let (found, _) = search.await;
            this.update(cx, |this, cx| {
                let mut items: Vec<TodoItem> = std::mem::take(&mut this.items)
                    .into_iter()
                    .filter(|item| item.buffer.entity_id() != buffer_id)
                    .collect();
                items.extend(
                    found
                        .into_iter()
                        .flat_map(|(buffer, ranges)| todo_items(buffer, ranges, cx)),
                );
                let (limit_reached, message) = (this.limit_reached, this.message.clone());
                this.finish(items, limit_reached, message, cx);
                this.buffer_tasks.remove(&buffer_id);
            })
            .log_err();
        });
        self.buffer_tasks.insert(buffer_id, task);
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
        self.apply_blame(cx);
        self.refresh_blame(cx);
        self.rebuild_rows(cx);
    }

    /// Fills each item's author and time from blame taken at the buffer's current version.
    fn apply_blame(&mut self, cx: &App) {
        let now = unix_now();
        for item in &mut self.items {
            item.blame = self
                .blame
                .get(&item.buffer.entity_id())
                .filter(|blame| blame.version == item.buffer.read(cx).version())
                .and_then(|blame| blame.entries.as_deref())
                .map(|entries| line_blame(entries, item.row, now));
        }
    }

    /// Blames, one file after another in the background, the result files whose blame is
    /// missing or older than the buffer.
    fn refresh_blame(&mut self, cx: &mut Context<Self>) {
        let mut seen = HashSet::new();
        let stale: Vec<Entity<Buffer>> = self
            .items
            .iter()
            .map(|item| &item.buffer)
            .filter(|buffer| seen.insert(buffer.entity_id()))
            .filter(|buffer| {
                self.blame
                    .get(&buffer.entity_id())
                    .is_none_or(|blame| blame.version != buffer.read(cx).version())
            })
            .cloned()
            .collect();
        self.blame.retain(|buffer_id, _| seen.contains(buffer_id));
        if stale.is_empty() {
            return;
        }
        let project = self.project.clone();
        self.blame_task = cx.spawn(async move |this, cx| {
            for buffer in stale {
                let (version, blame) = cx.update(|cx| {
                    let version = buffer.read(cx).version();
                    let in_repository = project
                        .read(cx)
                        .git_store()
                        .read(cx)
                        .repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
                        .is_some();
                    let blame = if in_repository {
                        project.update(cx, |project, cx| project.blame_buffer(&buffer, None, cx))
                    } else {
                        Task::ready(Ok(None))
                    };
                    (version, blame)
                });
                let entries = blame.await.log_err().flatten().map(|blame| blame.entries);
                let updated = this.update(cx, |this, cx| {
                    this.blame
                        .insert(buffer.entity_id(), FileBlame { version, entries });
                    this.apply_blame(cx);
                    this.rebuild_rows(cx);
                });
                if updated.is_err() {
                    return;
                }
            }
        });
    }

    fn set_author_filter(&mut self, author: Option<SharedString>, cx: &mut Context<Self>) {
        self.author_filter = author;
        self.rebuild_rows(cx);
    }

    fn set_age_filter(&mut self, age: Age, cx: &mut Context<Self>) {
        self.age_filter = age;
        self.rebuild_rows(cx);
    }

    /// Indices of the items that pass the author and age filters.
    fn visible(&self) -> Vec<usize> {
        let now = unix_now();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                passes_filters(
                    item.blame.as_ref(),
                    self.author_filter.as_deref(),
                    self.age_filter,
                    now,
                )
            })
            .map(|(ix, _)| ix)
            .collect()
    }

    fn summary(&self) -> String {
        let visible = self.visible();
        let files = visible
            .iter()
            .map(|&ix| &self.items[ix].path)
            .collect::<HashSet<_>>()
            .len();
        summary_text(
            visible.len(),
            files,
            self.limit_reached,
            self.scanning,
            self.message.as_deref(),
        )
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let mut rows = vec![Row::Summary];
        let groups = self.groups();
        if !groups.is_empty() && !self.collapsed.contains(&Node::Summary) {
            rows.push(Row::Project);
            if !self.collapsed.contains(&Node::Project) {
                for (key, indices) in groups {
                    rows.push(Row::Group(key.clone(), indices.len()));
                    if !self.collapsed.contains(&Node::Group(key)) {
                        rows.extend(indices.into_iter().map(Row::Item));
                    }
                }
            }
        }
        self.rows = rows;
        let last = self.rows.len() - 1;
        self.selected = self.selected.map(|ix| ix.min(last));
        cx.notify();
    }

    fn toggle_node(&mut self, node: Node, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&node) {
            self.collapsed.insert(node);
        }
        self.rebuild_rows(cx);
    }

    fn expand_all(&mut self, _: &ExpandAll, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.rebuild_rows(cx);
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsed = self.groups().into_keys().map(Node::Group).collect();
        self.rebuild_rows(cx);
    }

    fn set_group_by(&mut self, group_by: GroupBy, cx: &mut Context<Self>) {
        self.group_by = group_by;
        self.collapsed.clear();
        self.rebuild_rows(cx);
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Selects and opens the next or previous item row after the selection.
    fn step_item(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.selected.unwrap_or(0);
        let target = if forward {
            (from + 1..self.rows.len()).find(|&ix| matches!(self.rows[ix], Row::Item(_)))
        } else {
            (0..from)
                .rev()
                .find(|&ix| matches!(self.rows[ix], Row::Item(_)))
        };
        if let Some(ix) = target {
            self.activate(ix, window, cx);
        }
    }

    /// Click: a node toggles, an item is selected and opened.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.select(ix, cx);
        match self.rows.get(ix).cloned() {
            Some(Row::Summary) => self.toggle_node(Node::Summary, cx),
            Some(Row::Project) => self.toggle_node(Node::Project, cx),
            Some(Row::Group(key, _)) => self.toggle_node(Node::Group(key), cx),
            Some(Row::Item(item_ix)) => {
                if let Some(item) = self.items.get(item_ix) {
                    self.open(item.buffer.clone(), item.anchor, window, cx);
                }
            }
            None => {}
        }
    }

    fn project_name(&self, cx: &App) -> SharedString {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).root_name_str().to_string().into())
            .unwrap_or_else(|| "Project".into())
    }

    /// Plain text of a row: also its accessible label.
    fn row_text(&self, row: &Row, cx: &App) -> String {
        match row {
            Row::Summary => self.summary(),
            Row::Project => format!(
                "{} - {}",
                self.project_name(cx),
                items_label(self.visible().len())
            ),
            Row::Group(key, count) => {
                format!("{} {}", group_name(key, self.group_by), items_label(*count))
            }
            Row::Item(ix) => {
                let item = &self.items[*ix];
                format!("{} {}", item.row + 1, item.text)
            }
        }
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

    /// Item indices per group, in item order (path, then line).
    fn groups(&self) -> BTreeMap<SharedString, Vec<usize>> {
        let mut groups: BTreeMap<SharedString, Vec<usize>> = BTreeMap::new();
        for ix in self.visible() {
            let item = &self.items[ix];
            let key = match self.group_by {
                GroupBy::File => item.path.clone(),
                GroupBy::Pattern => item.pattern.clone(),
            };
            groups.entry(key).or_default().push(ix);
        }
        groups
    }
}

/// A group's heading: the file name for files, the marker for patterns.
fn group_name(key: &str, group_by: GroupBy) -> String {
    match group_by {
        GroupBy::File => Path::new(key).file_name().map_or_else(
            || key.to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        GroupBy::Pattern => key.to_string(),
    }
}

fn todo_items(buffer: Entity<Buffer>, ranges: Vec<Range<Anchor>>, cx: &App) -> Vec<TodoItem> {
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
                blame: None,
            })
        })
        .collect()
}

impl TodoPanel {
    fn scope_chip(
        &self,
        id: &'static str,
        label: &'static str,
        scope: Scope,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Self::chip(
            id,
            label,
            self.scope == scope,
            Role::Tab,
            cx,
            move |this, cx| this.set_scope(scope, cx),
        )
    }

    /// A rounded chip for the scope tabs and age filters; the active one is outlined and tinted.
    fn chip(
        id: impl Into<ElementId>,
        label: &'static str,
        active: bool,
        role: Role,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let border = if active {
            cx.theme().colors().border_focused
        } else {
            gpui::transparent_black()
        };
        div().rounded(px(6.)).border_1().border_color(border).child(
            ButtonLike::new(id)
                .style(ButtonStyle::Subtle)
                .toggle_state(active)
                .aria_role(role)
                .aria_label(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx)))
                .child(Label::new(label)),
        )
        .into_any_element()
    }

    /// Author dropdown (authors seen in the results) and age chips.
    fn render_filters(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let authors: BTreeSet<SharedString> = self
            .items
            .iter()
            .filter_map(|item| item.blame.as_ref().map(|(author, _)| author.clone()))
            .collect();
        let selected = self.author_filter.clone();
        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for author in std::iter::once(None).chain(authors.into_iter().map(Some)) {
                let label = author.clone().unwrap_or_else(|| "All Authors".into());
                let panel = panel.clone();
                menu = menu.toggleable_entry(
                    label,
                    author == selected,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        panel
                            .update(cx, |panel, cx| panel.set_author_filter(author.clone(), cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        let author_label = self
            .author_filter
            .clone()
            .unwrap_or_else(|| "All Authors".into());
        h_flex()
            .gap_1()
            .child(
                DropdownMenu::new("zenith-todo-author", author_label, menu)
                    .style(DropdownStyle::Subtle)
                    .aria_label("Filter by author"),
            )
            .children(Age::CHIPS.into_iter().map(|(age, label)| {
                Self::chip(
                    ("zenith-todo-age", age as usize),
                    label,
                    self.age_filter == age,
                    Role::Button,
                    cx,
                    move |this, cx| this.set_age_filter(age, cx),
                )
            }))
    }

    fn strip_button(
        &self,
        id: &'static str,
        icon: IconName,
        tooltip: &'static str,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        IconButton::new(id, icon)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    /// The left icon strip: previous/next, expand/collapse, group by, refresh.
    fn render_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (group_tooltip, next_group) = match self.group_by {
            GroupBy::File => ("Group by Pattern", GroupBy::Pattern),
            GroupBy::Pattern => ("Group by File", GroupBy::File),
        };
        v_flex()
            .w(px(STRIP_WIDTH))
            .flex_none()
            .items_center()
            .pt_2()
            .gap_1()
            .child(self.strip_button(
                "zenith-todo-previous",
                IconName::ArrowUp,
                "Previous TODO",
                |this, window, cx| this.step_item(false, window, cx),
                cx,
            ))
            .child(self.strip_button(
                "zenith-todo-next",
                IconName::ArrowDown,
                "Next TODO",
                |this, window, cx| this.step_item(true, window, cx),
                cx,
            ))
            .child(self.strip_button(
                "zenith-todo-expand-all",
                IconName::ZenithExpandAll,
                "Expand All",
                |this, window, cx| this.expand_all(&ExpandAll, window, cx),
                cx,
            ))
            .child(self.strip_button(
                "zenith-todo-collapse-all",
                IconName::ZenithCollapseAll,
                "Collapse All",
                |this, window, cx| this.collapse_all(&CollapseAll, window, cx),
                cx,
            ))
            .child(self.strip_button(
                "zenith-todo-group-by",
                IconName::ListTree,
                group_tooltip,
                move |this, _, cx| this.set_group_by(next_group, cx),
                cx,
            ))
            .child(self.strip_button(
                "zenith-todo-refresh",
                IconName::RotateCw,
                "Refresh",
                |this, _, cx| this.rescan(cx),
                cx,
            ))
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let row = self.rows[ix].clone();
        let status = cx.theme().status();
        let item = ListItem::new(("zenith-todo-row", ix))
            .spacing(ListItemSpacing::Dense)
            .height(px(ROW_HEIGHT))
            .rounded()
            .toggle_state(self.selected == Some(ix))
            .aria_role(Role::TreeItem)
            .aria_label(self.row_text(&row, cx))
            .indent_step_size(px(ITEM_INDENT))
            .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)));
        let bold = |text: String| Label::new(text).weight(FontWeight::BOLD);
        match row {
            Row::Summary => item
                .toggle(Some(!self.collapsed.contains(&Node::Summary)))
                .child(Label::new(self.summary()))
                .into_any_element(),
            Row::Project => item
                .indent_level(1)
                .toggle(Some(!self.collapsed.contains(&Node::Project)))
                .start_slot(Icon::new(IconName::Folder).size(IconSize::Small))
                .child(
                    h_flex()
                        .gap_2()
                        .child(bold(self.project_name(cx).to_string()))
                        .child(
                            Label::new(format!("- {}", items_label(self.visible().len())))
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
            Row::Group(key, count) => {
                let icon = (self.group_by == GroupBy::File)
                    .then(|| FileIcons::get_icon(Path::new(key.as_ref()), cx))
                    .flatten()
                    .map(|path| Icon::from_path(path).size(IconSize::Small));
                item.indent_level(2)
                    .toggle(Some(!self.collapsed.contains(&Node::Group(key.clone()))))
                    .start_slot::<Icon>(icon)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(group_name(&key, self.group_by)))
                            .child(Label::new(items_label(count)).color(Color::Muted)),
                    )
                    .into_any_element()
            }
            Row::Item(item_ix) => {
                let todo = &self.items[item_ix];
                // The marker's colour, with TODO in orange as PhpStorm draws it.
                let colour = match colour_class(&todo.pattern) {
                    0 => status.error,
                    1 | 2 => status.warning,
                    _ => status.hint,
                };
                let location = if self.group_by == GroupBy::Pattern {
                    format!("{}:{}", todo.path, todo.row + 1)
                } else {
                    (todo.row + 1).to_string()
                };
                item.indent_level(3)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(location).color(Color::Muted))
                            .child(
                                Label::new(todo.text.clone())
                                    .color(Color::Custom(colour))
                                    .truncate(),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for TodoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new("TODO").weight(FontWeight::BOLD))
            .child(self.scope_chip("zenith-todo-project", "Project", Scope::Project, cx))
            .child(self.scope_chip(
                "zenith-todo-current",
                "Current File",
                Scope::CurrentFile,
                cx,
            ))
            .child(self.scope_chip(
                "zenith-todo-changed",
                "Changed Files",
                Scope::ChangedFiles,
                cx,
            ))
            .child(div().flex_1())
            .child(self.render_filters(window, cx));
        let list = uniform_list(
            "zenith-todo-rows",
            self.rows.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll_handle)
        .flex_1();
        v_flex()
            .key_context("TodoPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.rescan(cx)))
            .on_action(
                cx.listener(|this, _: &NextTodo, window, cx| this.step_item(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousTodo, window, cx| this.step_item(false, window, cx)),
            )
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(Self::collapse_all))
            .size_full()
            .child(header)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_strip(cx))
                    .child(v_flex().flex_1().min_w_0().h_full().child(list)),
            )
    }
}

/// Added lines of a unified diff that carry a marker inside a comment, as (path, line).
/// A marker line that the same diff also removes was moved, not added.
fn new_todo_lines(diff: &str, regex: &Regex) -> Vec<(String, String)> {
    let has_marker = |line: &str| {
        regex
            .find_iter(line)
            .any(|found| has_comment_marker(line.get(..found.start()).unwrap_or_default()))
    };
    let mut path = String::new();
    let mut in_header = false;
    let mut added = Vec::new();
    let mut removed = HashSet::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
        } else if line.starts_with("@@") {
            in_header = false;
        } else if in_header {
            if let Some(new_path) = line.strip_prefix("+++ ") {
                path = new_path.strip_prefix("b/").unwrap_or(new_path).to_string();
            }
        } else if let Some(text) = line.strip_prefix('+') {
            if has_marker(text) {
                added.push((path.clone(), text.trim().to_string()));
            }
        } else if let Some(text) = line.strip_prefix('-') {
            if has_marker(text) {
                removed.insert(text.trim().to_string());
            }
        }
    }
    added.retain(|(_, text)| !removed.contains(text));
    added
}

thread_local! {
    /// Set while `hold_commit` resumes a commit the user confirmed, so it is not held again.
    static COMMIT_CONFIRMED: Cell<bool> = const { Cell::new(false) };
}

/// Commit hook for the git panel: returns `true` when it holds the commit to check the
/// changes for new TODOs. It then calls `resume` when none are found or the user picks
/// "Commit Anyway", and opens the TODO panel on "Review".
pub fn hold_commit(
    repository: &Entity<Repository>,
    staged: bool,
    window: &mut Window,
    cx: &mut App,
    resume: impl FnOnce(&mut Window, &mut App) + 'static,
) -> bool {
    if COMMIT_CONFIRMED.get() || !cx.has_global::<Installed>() {
        return false;
    }
    let settings = TodoSettings::get_global(cx);
    if !settings.enabled {
        return false;
    }
    let Some(regex) = build_regex(&combined_pattern(&settings.patterns)) else {
        return false;
    };
    let diff_type = if staged {
        DiffType::HeadToIndex
    } else {
        DiffType::HeadToWorktree
    };
    let diff = repository.update(cx, |repository, cx| repository.diff(diff_type, cx));
    window
        .spawn(cx, async move |cx| {
            // A failed diff must not block the commit; it is logged and the commit goes on.
            let new_todos = diff
                .await
                .ok()
                .and_then(|diff| diff.log_err())
                .map(|diff| new_todo_lines(&diff, &regex))
                .unwrap_or_default();
            let answer = if new_todos.is_empty() {
                Some(0)
            } else {
                let message = match new_todos.len() {
                    1 => "This commit adds a TODO comment.".to_string(),
                    count => format!("This commit adds {count} TODO comments."),
                };
                let detail = new_todos
                    .iter()
                    .take(5)
                    .map(|(path, text)| format!("{path}: {text}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let prompt = cx.update(|window, cx| {
                    window.prompt(
                        PromptLevel::Warning,
                        &message,
                        Some(&detail),
                        &["Commit Anyway", "Review"],
                        cx,
                    )
                })?;
                prompt.await.ok()
            };
            cx.update(|window, cx| match answer {
                Some(0) => {
                    COMMIT_CONFIRMED.set(true);
                    resume(window, cx);
                    COMMIT_CONFIRMED.set(false);
                }
                Some(_) => review_changed_todos(window, cx),
                None => {}
            })
        })
        .detach_and_log_err(cx);
    true
}

/// Opens the TODO panel on the Changed Files scope.
fn review_changed_todos(window: &mut Window, cx: &mut App) {
    let Some(workspace) = Workspace::for_window(window, cx) else {
        return;
    };
    let panel = workspace.update(cx, |workspace, cx| {
        workspace.focus_panel::<TodoPanel>(window, cx)
    });
    if let Some(panel) = panel {
        panel.update(cx, |panel, cx| panel.set_scope(Scope::ChangedFiles, cx));
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

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        TodoSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => settings::DockPosition::Left,
                DockPosition::Bottom => settings::DockPosition::Bottom,
                DockPosition::Right => settings::DockPosition::Right,
            };
            settings
                .zenith
                .get_or_insert_default()
                .todo
                .get_or_insert_default()
                .dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZenithListChecks)
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
        for prefix in [
            "// ",
            "    # ",
            "/* ",
            " * ",
            "<!-- ",
            "{# ",
            "$a = 1; // ",
            "$s = 'x'; # ",
            "// don't ",
        ] {
            assert!(has_comment_marker(prefix), "{prefix:?}");
        }
        for prefix in [
            "",
            "let todo_list = ",
            "fn ",
            "$s = \"# ",
            "url = 'http://",
            "$s = \"a\\\" // ",
        ] {
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

    // Opening a file creates an editor inside `observe_new`; the highlighter must not read
    // that editor back (it panicked at startup in `3ed32bfd4e`).
    #[gpui::test]
    async fn opening_a_file_colours_its_markers(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/project",
                serde_json::json!({ "a.txt": "// TODO: one\nplain todo\n" }),
            )
            .await;
        let project =
            Project::test(app_state.fs.clone(), [std::path::Path::new("/project")], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    "/project/a.txt".into(),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            let (_, ranges) = editor
                .text_highlights(HighlightKey::ZenithTodo(2), cx)
                .expect("TODO coloured");
            assert_eq!(ranges.len(), 1);
        });
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

    #[test]
    fn summary_wording() {
        assert_eq!(
            summary_text(12, 2, false, false, None),
            "Found 12 TODO items in 2 files"
        );
        assert_eq!(
            summary_text(1, 1, false, false, None),
            "Found 1 TODO item in 1 file"
        );
        assert_eq!(
            summary_text(0, 0, true, false, None),
            "Found 0 TODO items in 0 files (limit reached)"
        );
        assert_eq!(summary_text(3, 1, false, true, Some("x")), "Scanning…");
        assert_eq!(
            summary_text(0, 0, false, false, Some("No file is open.")),
            "No file is open."
        );
    }

    async fn panel_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (
        Entity<workspace::Workspace>,
        Entity<TodoPanel>,
        &mut gpui::VisualTestContext,
    ) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/project",
                serde_json::json!({
                    "a.txt": "// TODO: one\n// FIXME: two\n",
                    "b.txt": "// TODO: three\n",
                }),
            )
            .await;
        let project =
            Project::test(app_state.fs.clone(), [std::path::Path::new("/project")], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let panel = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.focus_panel::<TodoPanel>(window, cx)
            })
            .expect("panel is registered");
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    fn texts(panel: &Entity<TodoPanel>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        panel.read_with(cx, |panel, cx| {
            panel
                .rows
                .iter()
                .map(|row| panel.row_text(row, cx))
                .collect()
        })
    }

    #[gpui::test]
    async fn tree_has_summary_project_and_file_nodes(cx: &mut gpui::TestAppContext) {
        let (_, panel, cx) = panel_fixture(cx).await;
        assert_eq!(
            texts(&panel, cx),
            [
                "Found 3 TODO items in 2 files",
                "project - 3 items",
                "a.txt 2 items",
                "1 // TODO: one",
                "2 // FIXME: two",
                "b.txt 1 item",
                "1 // TODO: three",
            ]
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.collapse_all(&CollapseAll, window, cx)
        });
        assert_eq!(
            texts(&panel, cx),
            [
                "Found 3 TODO items in 2 files",
                "project - 3 items",
                "a.txt 2 items",
                "b.txt 1 item"
            ]
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.expand_all(&ExpandAll, window, cx)
        });
        assert_eq!(texts(&panel, cx).len(), 7);
    }

    #[gpui::test]
    async fn current_file_tab_scopes_to_the_active_editor(cx: &mut gpui::TestAppContext) {
        let (workspace, panel, cx) = panel_fixture(cx).await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    "/project/a.txt".into(),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("file opens");
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.set_scope(Scope::CurrentFile, cx));
        cx.run_until_parked();
        assert_eq!(
            texts(&panel, cx),
            [
                "Found 2 TODO items in 1 file",
                "project - 2 items",
                "a.txt 2 items",
                "1 // TODO: one",
                "2 // FIXME: two",
            ]
        );
    }

    #[gpui::test]
    async fn next_todo_walks_the_item_rows(cx: &mut gpui::TestAppContext) {
        let (_, panel, cx) = panel_fixture(cx).await;
        let selected = |panel: &Entity<TodoPanel>, cx: &mut gpui::VisualTestContext| {
            panel.read_with(cx, |panel, _| panel.selected)
        };
        panel.update_in(cx, |panel, window, cx| panel.step_item(true, window, cx));
        cx.run_until_parked();
        assert_eq!(selected(&panel, cx), Some(3));
        panel.update_in(cx, |panel, window, cx| panel.step_item(true, window, cx));
        panel.update_in(cx, |panel, window, cx| panel.step_item(true, window, cx));
        cx.run_until_parked();
        assert_eq!(selected(&panel, cx), Some(6));
        panel.update_in(cx, |panel, window, cx| panel.step_item(false, window, cx));
        cx.run_until_parked();
        assert_eq!(selected(&panel, cx), Some(4));
    }

    #[test]
    fn filters_by_author_and_age() {
        let now = 1_000_000_000;
        let day = 24 * 60 * 60;
        let alice = Some((SharedString::from("Alice"), now - 3 * day));
        let bob = Some((SharedString::from("Bob"), now - 40 * day));
        assert!(passes_filters(None, None, Age::Any, now));
        assert!(!passes_filters(None, Some("Alice"), Age::Any, now));
        assert!(!passes_filters(None, None, Age::Year, now));
        assert!(passes_filters(
            alice.as_ref(),
            Some("Alice"),
            Age::Week,
            now
        ));
        assert!(!passes_filters(bob.as_ref(), Some("Alice"), Age::Any, now));
        assert!(!passes_filters(bob.as_ref(), None, Age::Month, now));
        assert!(passes_filters(bob.as_ref(), None, Age::Year, now));
        assert!(passes_filters(bob.as_ref(), Some("Bob"), Age::Any, now));
    }

    #[test]
    fn blame_lines_without_a_commit_are_new() {
        let entry = BlameEntry {
            sha: "0123456789abcdef0123456789abcdef01234567"
                .parse()
                .expect("valid sha"),
            range: 2..4,
            original_line_number: 3,
            author: Some("Alice".into()),
            author_mail: None,
            author_time: Some(500),
            author_tz: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            committer_tz: None,
            summary: None,
            previous: None,
            filename: "a.rs".into(),
            boundary: false,
        };
        let entries = [entry];
        assert_eq!(line_blame(&entries, 3, 900), ("Alice".into(), 500));
        assert_eq!(line_blame(&entries, 4, 900), (NOT_COMMITTED.into(), 900));
    }

    #[test]
    fn new_todos_in_a_diff() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n\
            index 1111111..2222222 100644\n\
            --- a/src/a.rs\n\
            +++ b/src/a.rs\n\
            @@ -1,2 +1,4 @@\n\
            \x20fn a() {} // TODO: old\n\
            +// TODO: new one\n\
            +let todo = 1;\n\
            -// FIXME: moved\n\
            +    // FIXME: moved\n\
            +++ y // HACK\n\
            diff --git a/new.php b/new.php\n\
            new file mode 100644\n\
            --- /dev/null\n\
            +++ b/new.php\n\
            @@ -0,0 +1 @@\n\
            +<?php # @todo x\n";
        let regex = build_regex(&combined_pattern(&[])).expect("valid pattern");
        let found = new_todo_lines(diff, &regex);
        let expected = [
            ("src/a.rs", "// TODO: new one"),
            ("src/a.rs", "++ y // HACK"),
            ("new.php", "<?php # @todo x"),
        ]
        .map(|(path, text)| (path.to_string(), text.to_string()));
        assert_eq!(found, expected);
    }

    #[gpui::test]
    async fn disabled_means_no_panel_and_no_colours(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content
                        .zenith
                        .get_or_insert_default()
                        .todo
                        .get_or_insert_default()
                        .enabled = Some(false);
                });
            });
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree("/project", serde_json::json!({ "a.txt": "// TODO: one\n" }))
            .await;
        let project =
            Project::test(app_state.fs.clone(), [std::path::Path::new("/project")], cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<TodoPanel>(window, cx)
        });
        assert!(panel.is_none());
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    "/project/a.txt".into(),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert!(
                editor
                    .text_highlights(HighlightKey::ZenithTodo(2), cx)
                    .is_none()
            );
        });
    }
}
