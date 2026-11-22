//! JetBrains scratch files: global, project-independent files of any language, kept in one
//! folder under the data dir, listed in a "Scratches" panel and opened as ordinary files.
//! They work with no project open: the file is opened through an invisible worktree, so the
//! scratches folder never shows up as a project root. `project_node` also keeps that folder open
//! as an invisible worktree, which the project panel lists under "Scratches and Consoles".

mod project_node;
pub use project_node::{is_store_worktree, store_worktree};

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Result, ensure};
use collections::HashMap;
use editor::{Addon, Editor};
use file_icons::FileIcons;
use fs::{CreateOptions, Fs, RemoveOptions, RenameOptions};
use futures::StreamExt as _;
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, KeyContext, MouseDownEvent, ParentElement, Pixels, Point, Render, Role,
    SharedString, Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions,
    anchored, deferred, px, uniform_list,
};
use picker::{Picker, PickerDelegate};
use project::{Project, TaskSourceKind};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use std::{any::Any, ops::Range};
use task::{TaskContext, TaskTemplate, TaskVariables, VariableName};
use ui::{ContextMenu, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ModalView, OpenOptions, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{DetachAndPromptErr as _, NotificationId},
};
use worktree::Worktree;

actions!(
    zenith_scratches,
    [
        /// Creates a scratch file: pick a language, and it opens in the editor.
        NewScratchFile,
        /// Toggles focus on the Scratches tool window.
        ToggleFocus,
        /// Renames the selected scratch file.
        RenameScratch,
        /// Moves the selected scratch file to the Trash.
        DeleteScratch,
        /// Shows the selected scratch file in the file manager.
        RevealScratch,
        /// Runs the scratch file in the terminal, with the runner for its extension.
        RunScratch,
    ]
);

const HEADER_HEIGHT: f32 = 38.;
const ROW_HEIGHT: f32 = 28.;

/// Language picker entries: label and file extension.
const KINDS: [(&str, &str); 11] = [
    ("PHP", "php"),
    ("JavaScript", "js"),
    ("TypeScript", "ts"),
    ("JSON", "json"),
    ("HTML", "html"),
    ("CSS", "css"),
    ("Markdown", "md"),
    ("Plain Text", "txt"),
    ("SQL", "sql"),
    ("Shell", "sh"),
    ("Python", "py"),
];

/// Built-in run commands by extension, as program and arguments. The task system substitutes
/// `$ZED_FILE` and quotes each argument for the shell, so a path is never spliced into a
/// command line by hand.
const RUNNERS: [(&str, &[&str]); 6] = [
    ("php", &["php", "$ZED_FILE"]),
    ("js", &["node", "$ZED_FILE"]),
    ("mjs", &["node", "$ZED_FILE"]),
    ("ts", &["npx", "tsx", "$ZED_FILE"]),
    ("py", &["python3", "$ZED_FILE"]),
    ("sh", &["bash", "$ZED_FILE"]),
];

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ScratchesSettings {
    pub enabled: bool,
    /// The configured folder, `None` when unset or not absolute.
    pub directory: Option<PathBuf>,
    pub runners: HashMap<String, Vec<String>>,
    pub use_project_cwd: bool,
    /// List the scratches under a "Scratches and Consoles" node in the project panel.
    pub project_panel_node: bool,
}

impl Settings for ScratchesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let scratches = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.scratches.as_ref());
        Self {
            enabled: scratches
                .and_then(|scratches| scratches.enabled)
                .unwrap_or(true),
            directory: absolute_dir(scratches.and_then(|scratches| scratches.directory.as_deref())),
            runners: scratches
                .and_then(|scratches| scratches.runners.clone())
                .unwrap_or_default(),
            use_project_cwd: scratches
                .and_then(|scratches| scratches.use_project_cwd)
                .unwrap_or(true),
            project_panel_node: scratches
                .and_then(|scratches| scratches.project_panel_node)
                .unwrap_or(true),
        }
    }
}

/// A relative folder would depend on the process's cwd, so it is ignored (with a log).
fn absolute_dir(directory: Option<&str>) -> Option<PathBuf> {
    let directory = Path::new(directory?);
    if directory.is_absolute() {
        Some(directory.to_path_buf())
    } else {
        log::warn!(
            "zenith.scratches.directory must be an absolute path, ignoring {}",
            directory.display()
        );
        None
    }
}

fn default_store_dir() -> PathBuf {
    paths::data_dir().join("scratches")
}

/// Where scratch files live: global, so one folder for every window and project.
pub fn store_dir(cx: &App) -> PathBuf {
    ScratchesSettings::get_global(cx)
        .directory
        .clone()
        .unwrap_or_else(default_store_dir)
}

/// Marks an editor showing a scratch file, so `ctrl-r` (context `zenith_scratch`) runs it.
struct ScratchEditor;

impl Addon for ScratchEditor {
    fn extend_key_context(&self, key_context: &mut KeyContext, _: &App) {
        key_context.add("zenith_scratch");
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

fn editor_file_path(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    Some(buffer.read(cx).file()?.as_local()?.abs_path(cx))
}

fn is_scratch(path: &Path, cx: &App) -> bool {
    path.parent() == Some(store_dir(cx).as_path())
}

pub fn init(cx: &mut App) {
    ScratchesSettings::register(cx);
    // shortcut: `enabled` is read when a window or editor opens, so toggling it needs a restart.
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if !ScratchesSettings::get_global(cx).enabled || !editor.mode().is_full() {
            return;
        }
        if editor_file_path(editor, cx).is_some_and(|path| is_scratch(&path, cx)) {
            editor.register_addon(ScratchEditor);
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if !ScratchesSettings::get_global(cx).enabled {
            return;
        }
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ScratchesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RunScratch, window, cx| {
            let path = workspace
                .active_item_as::<Editor>(cx)
                .and_then(|editor| editor_file_path(editor.read(cx), cx))
                .filter(|path| is_scratch(path, cx));
            match path {
                Some(path) => run_scratch(workspace, &path, window, cx),
                None => show_toast(workspace, "The active file is not a scratch file.", cx),
            }
        });
        workspace.register_action(|workspace, _: &NewScratchFile, window, cx| {
            let fs = workspace.app_state().fs.clone();
            let workspace_handle = workspace.weak_handle();
            workspace.toggle_modal(window, cx, move |window, cx| {
                KindPicker::new(fs, workspace_handle, window, cx)
            });
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| ScratchesPanel::new(workspace, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

// ---- run ----

/// The run command for `ext`: the user's runner when set (an empty one disables it), else the
/// built-in one.
fn runner(ext: &str, user_runners: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    let ext = ext.to_lowercase();
    let argv = match user_runners.get(&ext) {
        Some(argv) => argv.clone(),
        None => RUNNERS
            .iter()
            .find(|(runner_ext, _)| *runner_ext == ext)?
            .1
            .iter()
            .map(|arg| arg.to_string())
            .collect(),
    };
    argv.first()
        .is_some_and(|program| !program.trim().is_empty())
        .then_some(argv)
}

/// The task for `path`: program and arguments stay separate, so the task system quotes them.
fn run_task(path: &Path, argv: &[String], cwd: Option<PathBuf>) -> Option<task::ResolvedTask> {
    let (command, args) = argv.split_first()?;
    let template = TaskTemplate {
        label: format!(
            "Run {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        command: command.clone(),
        args: args.to_vec(),
        ..TaskTemplate::default()
    };
    let mut task_variables = TaskVariables::default();
    task_variables.insert(VariableName::File, path.to_string_lossy().into_owned());
    if let Some(name) = path.file_name() {
        task_variables.insert(VariableName::Filename, name.to_string_lossy().into_owned());
    }
    if let Some(stem) = path.file_stem() {
        task_variables.insert(VariableName::Stem, stem.to_string_lossy().into_owned());
    }
    if let Some(dir) = path.parent() {
        task_variables.insert(VariableName::Dirname, dir.to_string_lossy().into_owned());
    }
    let context = TaskContext {
        cwd,
        task_variables,
        ..TaskContext::default()
    };
    template.resolve_task("zenith_scratch", &context)
}

fn run_scratch(
    workspace: &mut Workspace,
    path: &Path,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = ScratchesSettings::get_global(cx);
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    let Some(argv) = runner(ext, &settings.runners) else {
        let message = format!(
            "No runner for .{ext} files. Add one under zenith.scratches.runners in settings."
        );
        show_toast(workspace, message, cx);
        return;
    };
    let project_root = settings
        .use_project_cwd
        .then(|| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .find_map(|worktree| worktree.read(cx).root_dir())
        })
        .flatten();
    let cwd = project_root
        .map(|root| root.to_path_buf())
        .or_else(|| path.parent().map(Path::to_path_buf));
    match run_task(path, &argv, cwd) {
        Some(task) => {
            workspace.schedule_resolved_task(TaskSourceKind::UserInput, task, true, window, cx)
        }
        None => show_toast(
            workspace,
            format!("Could not build the run command for {}.", path.display()),
            cx,
        ),
    }
}

fn show_toast(
    workspace: &mut Workspace,
    message: impl Into<std::borrow::Cow<'static, str>>,
    cx: &mut Context<Workspace>,
) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<RunScratch>(), message),
        cx,
    );
}

// ---- files: pure of any UI so the rules are testable on their own ----

#[derive(Clone, Debug, PartialEq)]
struct Scratch {
    name: String,
    path: PathBuf,
    mtime: SystemTime,
}

/// `scratch.php`, then `scratch_1.php`, `scratch_2.php`, ...
fn next_free_name(ext: &str, taken: &HashSet<String>) -> String {
    (0..)
        .map(|n| match n {
            0 => format!("scratch.{ext}"),
            n => format!("scratch_{n}.{ext}"),
        })
        .find(|name| !taken.contains(name))
        .expect("an unbounded range has a free name")
}

async fn read_paths(fs: &dyn Fs, dir: &Path) -> Result<Vec<PathBuf>> {
    if fs.metadata(dir).await?.is_none() {
        return Ok(Vec::new());
    }
    let mut stream = fs.read_dir(dir).await?;
    let mut paths = Vec::new();
    while let Some(path) = stream.next().await {
        paths.push(path?);
    }
    Ok(paths)
}

/// Files directly in `dir`, newest first. Hidden files and folders are not scratches.
async fn list(fs: &dyn Fs, dir: &Path) -> Result<Vec<Scratch>> {
    let mut items = Vec::new();
    for path in read_paths(fs, dir).await? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Some(metadata) = fs.metadata(&path).await.ok().flatten() else {
            continue;
        };
        if metadata.is_dir {
            continue;
        }
        items.push(Scratch {
            name: name.to_string(),
            mtime: metadata.mtime.timestamp_for_user(),
            path,
        });
    }
    items.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
    Ok(items)
}

/// A row of the panel list: a language section header, or a scratch (index into the items).
#[derive(Clone, Debug, PartialEq)]
enum Row {
    Header(SharedString),
    Scratch(usize),
}

/// The section a scratch is listed under: its language, by extension.
fn language_label(name: &str) -> SharedString {
    let Some(ext) = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
    else {
        return "Other".into();
    };
    KINDS
        .iter()
        .find(|(_, kind_ext)| *kind_ext == ext)
        .map_or_else(|| ext.to_uppercase().into(), |(label, _)| (*label).into())
}

/// Orders `items` (newest first) by language, keeping newest first inside each language,
/// and returns them with the rows to draw: a header before each language's scratches.
fn grouped(mut items: Vec<Scratch>) -> (Vec<Scratch>, Vec<Row>) {
    // A stable sort, so the newest-first order holds within a language.
    items.sort_by_cached_key(|item| language_label(&item.name));
    let mut rows = Vec::with_capacity(items.len());
    let mut current = None;
    for (ix, item) in items.iter().enumerate() {
        let label = language_label(&item.name);
        if current.as_ref() != Some(&label) {
            rows.push(Row::Header(label.clone()));
            current = Some(label);
        }
        rows.push(Row::Scratch(ix));
    }
    (items, rows)
}

/// Creates an empty `scratch*.<ext>` in `dir` and returns its path. Never overwrites.
async fn create(fs: &dyn Fs, dir: &Path, ext: &str) -> Result<PathBuf> {
    fs.create_dir(dir).await?;
    let taken = read_paths(fs, dir)
        .await?
        .iter()
        .filter_map(|path| Some(path.file_name()?.to_str()?.to_string()))
        .collect();
    let path = dir.join(next_free_name(ext, &taken));
    fs.create_file(
        &path,
        CreateOptions {
            overwrite: false,
            ignore_if_exists: false,
        },
    )
    .await?;
    Ok(path)
}

/// The canonical path of a file directly inside `dir`; an error for anything else
/// (outside the folder, the folder itself, a symlink pointing out of it).
async fn inside_store(fs: &dyn Fs, dir: &Path, path: &Path) -> Result<PathBuf> {
    let dir = fs.canonicalize(dir).await?;
    let path = fs.canonicalize(path).await?;
    ensure!(
        path.parent() == Some(dir.as_path()),
        "{} is not in the scratches folder",
        path.display()
    );
    Ok(path)
}

/// Moves a scratch to the system Trash. Refuses anything outside the store.
async fn delete(fs: &dyn Fs, dir: &Path, path: &Path) -> Result<()> {
    let path = inside_store(fs, dir, path).await?;
    fs.trash(&path, RemoveOptions::default()).await?;
    Ok(())
}

/// A new file name must be one plain path component.
fn validate_name(name: &str) -> Result<&str> {
    let name = name.trim();
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']),
        "\"{name}\" is not a valid file name"
    );
    Ok(name)
}

/// Renames a scratch inside the store; never overwrites and never leaves the folder.
async fn rename(fs: &dyn Fs, dir: &Path, from: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = validate_name(new_name)?;
    let from = inside_store(fs, dir, from).await?;
    let to = from.with_file_name(new_name);
    if to == from {
        return Ok(to);
    }
    ensure!(
        fs.metadata(&to).await?.is_none(),
        "{new_name} already exists"
    );
    fs.rename(&from, &to, RenameOptions::default()).await?;
    Ok(to)
}

fn age_label(then: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(then)
        .map_or(0, |age| age.as_secs());
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

// ---- panel ----

pub struct ScratchesPanel {
    fs: Arc<dyn Fs>,
    project: Entity<Project>,
    dir: PathBuf,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    items: Vec<Scratch>,
    rows: Vec<Row>,
    selected: Option<PathBuf>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    load_task: Task<()>,
    watch_task: Task<()>,
    /// The store as an invisible worktree of `project` (held here: the project keeps those
    /// weakly), and the folder it was opened for.
    store_worktree: Option<Entity<Worktree>>,
    store_worktree_dir: Option<PathBuf>,
    store_worktree_task: Task<()>,
    _settings_subscription: Subscription,
    _project_subscription: Subscription,
}

impl ScratchesPanel {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            let dir = store_dir(cx);
            if dir != this.dir {
                this.dir = dir;
                this.selected = None;
                this.watch(cx);
            }
            this.sync_store_worktree(cx);
        });
        let project = workspace.project().clone();
        let project_subscription = cx.subscribe(&project, |this, _, event, cx| {
            if let project::Event::WorktreeRemoved(id) = event
                && this
                    .store_worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.read(cx).id() == *id)
            {
                this.store_worktree = None;
                this.store_worktree_dir = None;
                this.sync_store_worktree(cx);
            }
        });
        let mut this = Self {
            fs: workspace.app_state().fs.clone(),
            project,
            dir: store_dir(cx),
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            items: Vec::new(),
            rows: Vec::new(),
            selected: None,
            context_menu: None,
            load_task: Task::ready(()),
            watch_task: Task::ready(()),
            store_worktree: None,
            store_worktree_dir: None,
            store_worktree_task: Task::ready(()),
            _settings_subscription: settings_subscription,
            _project_subscription: project_subscription,
        };
        this.watch(cx);
        this.sync_store_worktree(cx);
        this
    }

    /// Lists `self.dir` and follows it: scratches are global, so another window may add one.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let dir = self.dir.clone();
        self.watch_task = cx.spawn(async move |this, cx| {
            fs.create_dir(&dir).await.log_err();
            let (mut events, _watcher) = fs.watch(&dir, Duration::from_millis(100)).await;
            while events.next().await.is_some() {
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let dir = self.dir.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            let items = list(fs.as_ref(), &dir).await.log_err().unwrap_or_default();
            this.update(cx, |this, cx| {
                if this
                    .selected
                    .as_ref()
                    .is_some_and(|path| !items.iter().any(|item| &item.path == path))
                {
                    this.selected = None;
                }
                (this.items, this.rows) = grouped(items);
                cx.notify();
            })
            .ok();
        });
    }

    fn selected_ix(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.items.iter().position(|item| &item.path == selected)
    }

    fn select_ix(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.items.get(ix) {
            self.selected = Some(item.path.clone());
            if let Some(row_ix) = self.rows.iter().position(|row| *row == Row::Scratch(ix)) {
                self.scroll_handle
                    .scroll_to_item(row_ix, gpui::ScrollStrategy::Center);
            }
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_ix().map_or(0, |ix| ix + 1);
        self.select_ix(ix.min(self.items.len().saturating_sub(1)), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.selected_ix().map_or(0, |ix| ix.saturating_sub(1));
        self.select_ix(ix, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_ix(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_ix(self.items.len().saturating_sub(1), cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.selected.clone() {
            self.open(path, window, cx);
        }
    }

    fn open(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
        });
        cx.background_spawn(async move { open.await.map(|_| ()) })
            .detach_and_prompt_err("Failed to open the scratch file", window, cx, |_, _, _| {
                None
            });
    }

    fn rename(&mut self, _: &RenameScratch, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(workspace)) = (self.selected.clone(), self.workspace.upgrade())
        else {
            return;
        };
        let (fs, dir, panel) = (self.fs.clone(), self.dir.clone(), cx.entity().downgrade());
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                RenameModal::new(fs, dir, path, panel, window, cx)
            });
        });
    }

    fn delete(&mut self, _: &DeleteScratch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let (fs, dir) = (self.fs.clone(), self.dir.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = delete(fs.as_ref(), &dir, &path).await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
            result
        })
        .detach_and_prompt_err(
            "Failed to delete the scratch file",
            window,
            cx,
            |_, _, _| None,
        );
    }

    fn run(&mut self, _: &RunScratch, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(workspace)) = (self.selected.clone(), self.workspace.upgrade())
        else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            run_scratch(workspace, &path, window, cx)
        });
    }

    fn reveal(&mut self, _: &RevealScratch, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = &self.selected {
            cx.reveal_path(path);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(path);
        let focus = self.focus_handle.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus)
                .action("Run", RunScratch.boxed_clone())
                .separator()
                .action("Rename…", RenameScratch.boxed_clone())
                .action("Delete", DeleteScratch.boxed_clone())
                .separator()
                .action(
                    ui::utils::reveal_in_file_manager_label(false),
                    RevealScratch.boxed_clone(),
                )
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn render_row(&self, row_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(row_ix) {
            Some(Row::Scratch(ix)) => self.render_scratch(*ix, cx),
            Some(Row::Header(label)) => h_flex()
                .id(("zenith-scratch-group", row_ix))
                .role(Role::Heading)
                .h(px(ROW_HEIGHT))
                .px_2()
                .child(
                    Label::new(label.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            None => gpui::Empty.into_any_element(),
        }
    }

    fn render_scratch(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.items.get(ix) else {
            return gpui::Empty.into_any_element();
        };
        let age = age_label(item.mtime);
        let icon = FileIcons::get_icon(Path::new(&item.name), cx)
            .map(|path| Icon::from_path(path).size(IconSize::Small));
        let (open_path, menu_path) = (item.path.clone(), item.path.clone());
        let panel = cx.entity().downgrade();
        ListItem::new(("zenith-scratch-row", ix))
            .spacing(ListItemSpacing::Dense)
            .height(px(ROW_HEIGHT))
            .rounded()
            .toggle_state(self.selected.as_ref() == Some(&item.path))
            .aria_role(Role::ListItem)
            .aria_label(format!("{}, {age}", item.name))
            .start_slot::<Icon>(icon)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected = Some(open_path.clone());
                this.open(open_path.clone(), window, cx);
                cx.notify();
            }))
            .on_secondary_mouse_down(move |event: &MouseDownEvent, window, cx| {
                panel
                    .update(cx, |this, cx| {
                        this.deploy_context_menu(event.position, menu_path.clone(), window, cx)
                    })
                    .ok();
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(Label::new(item.name.clone()).truncate())
                    .child(Label::new(age).color(Color::Muted).size(LabelSize::Small)),
            )
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new("Scratches").weight(gpui::FontWeight::BOLD))
            .child(div().flex_1())
            .child(
                IconButton::new("zenith-scratches-new", IconName::ZenithPlus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("New Scratch File"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(NewScratchFile.boxed_clone(), cx)
                    }),
            )
            .child(
                div()
                    .debug_selector(|| "zenith-scratches-hide".into())
                    .child(
                        IconButton::new("zenith-scratches-hide", IconName::ZenithMinus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Hide"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(workspace::ToggleLeftDock.boxed_clone(), cx)
                            }),
                    ),
            )
    }
}

impl Render for ScratchesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.items.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new("No scratch files.").color(Color::Muted))
                .into_any_element()
        } else {
            uniform_list(
                "zenith-scratches-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .key_context("ScratchesPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::reveal))
            .on_action(cx.listener(Self::run))
            .size_full()
            .child(self.render_header(cx))
            .child(body)
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for ScratchesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ScratchesPanel {}

impl Panel for ScratchesPanel {
    fn persistent_name() -> &'static str {
        "Scratches Panel"
    }

    fn panel_key() -> &'static str {
        "ScratchesPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }

    // ponytail: left only; a dock setting needs `settings_content`, a full rebuild.
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::FileCode)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Scratches")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

// ---- language picker (New Scratch File) ----

struct KindPicker {
    picker: Entity<Picker<KindDelegate>>,
}

impl KindPicker {
    fn new(
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = KindDelegate {
            owner: cx.entity().downgrade(),
            fs,
            workspace,
            matches: (0..KINDS.len()).collect(),
            selected: 0,
        };
        Self {
            picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
        }
    }
}

impl Render for KindPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ScratchKindPicker")
            .child(self.picker.clone())
    }
}

impl Focusable for KindPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for KindPicker {}
impl ModalView for KindPicker {}

struct KindDelegate {
    owner: WeakEntity<KindPicker>,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    /// Indices into `KINDS` that match the query.
    matches: Vec<usize>,
    selected: usize,
}

impl PickerDelegate for KindDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "scratch language picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "New scratch file in…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
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
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = KINDS
            .iter()
            .enumerate()
            .filter(|(_, (label, ext))| {
                label.to_lowercase().contains(&query) || ext.contains(&query)
            })
            .map(|(ix, _)| ix)
            .collect();
        self.selected = 0;
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(&ix) = self.matches.get(self.selected) {
            let (fs, workspace, ext) = (self.fs.clone(), self.workspace.clone(), KINDS[ix].1);
            let dir = store_dir(cx);
            window
                .spawn(cx, async move |cx| {
                    let path = create(fs.as_ref(), &dir, ext).await?;
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                        })?
                        .await?;
                    anyhow::Ok(())
                })
                .detach_and_prompt_err(
                    "Failed to create the scratch file",
                    window,
                    cx,
                    |_, _, _| None,
                );
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.owner.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let (label, ext) = KINDS[*self.matches.get(ix)?];
        let icon = FileIcons::get_icon(Path::new(&format!("scratch.{ext}")), cx)
            .map(|path| Icon::from_path(path).size(IconSize::Small));
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot::<Icon>(icon)
                .child(Label::new(label)),
        )
    }
}

// ---- rename ----

struct RenameModal {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    from: PathBuf,
    panel: WeakEntity<ScratchesPanel>,
    editor: Entity<Editor>,
    error: Option<SharedString>,
}

impl RenameModal {
    fn new(
        fs: Arc<dyn Fs>,
        dir: PathBuf,
        from: PathBuf,
        panel: WeakEntity<ScratchesPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = from
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(name, window, cx);
            editor
        });
        Self {
            fs,
            dir,
            from,
            panel,
            editor,
            error: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor.read(cx).text(cx);
        let (fs, dir, from) = (self.fs.clone(), self.dir.clone(), self.from.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = rename(fs.as_ref(), &dir, &from, &name).await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.panel.update(cx, |panel, cx| panel.refresh(cx)).ok();
                    cx.emit(DismissEvent);
                }
                Err(err) => {
                    this.error = Some(format!("{err:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for RenameModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ScratchRename")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(24.))
            .p_3()
            .gap_2()
            .child(Label::new("Rename Scratch File").weight(gpui::FontWeight::BOLD))
            .child(
                div()
                    .px_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(self.editor.clone()),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).color(Color::Error)),
            )
    }
}

impl Focusable for RenameModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RenameModal {}
impl ModalView for RenameModal {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, TestAppContext, VisualTestContext};
    use settings::{ZenithScratchesSettingsContent, ZenithSettingsContent};
    use workspace::{AppState, MultiWorkspace};

    struct Fixture {
        fs: Arc<fs::FakeFs>,
        workspace: Entity<Workspace>,
        panel: Entity<ScratchesPanel>,
    }

    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        fixture_with(cx, &[], None).await
    }

    /// A window on `roots` (empty folders), with the store folder set to `directory` first.
    async fn fixture_with<'a>(
        cx: &'a mut TestAppContext,
        roots: &[&str],
        directory: Option<&str>,
    ) -> (Fixture, &'a mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            // The global bindings from `default-macos.json` the modals rely on.
            cx.bind_keys([
                KeyBinding::new("down", menu::SelectNext, None),
                KeyBinding::new("enter", menu::Confirm, None),
                KeyBinding::new("escape", menu::Cancel, None),
            ]);
            app_state
        });
        if let Some(directory) = directory {
            cx.update(|cx| {
                set_scratches(
                    cx,
                    ZenithScratchesSettingsContent {
                        directory: Some(directory.to_string()),
                        ..Default::default()
                    },
                )
            });
        }
        for root in roots {
            app_state
                .fs
                .as_fake()
                .create_dir(Path::new(root))
                .await
                .unwrap();
        }
        // No worktree: scratches must work with no project open.
        let roots: Vec<&Path> = roots.iter().map(Path::new).collect();
        let project = project::Project::test(app_state.fs.clone(), roots, cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ScratchesPanel>(cx))
            .expect("panel is registered");
        let fixture = Fixture {
            fs: app_state.fs.as_fake(),
            workspace,
            panel,
        };
        (fixture, cx)
    }

    fn names(fixture: &Fixture, cx: &mut VisualTestContext) -> Vec<String> {
        fixture.panel.read_with(cx, |panel, _| {
            panel.items.iter().map(|item| item.name.clone()).collect()
        })
    }

    fn open_panel(fixture: &Fixture, cx: &mut VisualTestContext) {
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<ScratchesPanel>(window, cx);
        });
        cx.run_until_parked();
    }

    /// Runs `NewScratchFile` and picks the `down_presses`-th language with the keyboard.
    fn new_scratch(cx: &mut VisualTestContext, down_presses: usize) {
        cx.dispatch_action(NewScratchFile);
        cx.run_until_parked();
        for _ in 0..down_presses {
            cx.simulate_keystrokes("down");
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
    }

    fn scratch(name: &str, age_secs: u64) -> Scratch {
        Scratch {
            name: name.to_string(),
            path: PathBuf::from("/s").join(name),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn runner_resolution_built_in_override_and_unknown() {
        let mut user = HashMap::default();
        assert_eq!(runner("php", &user), Some(argv(&["php", "$ZED_FILE"])));
        assert_eq!(
            runner("TS", &user),
            Some(argv(&["npx", "tsx", "$ZED_FILE"]))
        );
        assert_eq!(runner("py", &user), Some(argv(&["python3", "$ZED_FILE"])));
        assert_eq!(runner("rb", &user), None, "no built-in runner");
        assert_eq!(runner("", &user), None);

        user.insert(
            "php".to_string(),
            argv(&["php8.3", "-d", "display_errors=1", "$ZED_FILE"]),
        );
        user.insert("rb".to_string(), argv(&["ruby", "$ZED_FILE"]));
        user.insert("sh".to_string(), Vec::new());
        user.insert("js".to_string(), argv(&["  "]));
        assert_eq!(
            runner("php", &user),
            Some(argv(&["php8.3", "-d", "display_errors=1", "$ZED_FILE"])),
            "the user's runner wins"
        );
        assert_eq!(runner("rb", &user), Some(argv(&["ruby", "$ZED_FILE"])));
        assert_eq!(
            runner("sh", &user),
            None,
            "an empty runner disables the built-in"
        );
        assert_eq!(runner("js", &user), None, "a blank program is no runner");
        assert_eq!(
            runner("ts", &user),
            Some(argv(&["npx", "tsx", "$ZED_FILE"]))
        );
    }

    #[test]
    fn run_task_keeps_the_path_one_argument() {
        let path = Path::new("/scratches/my scratch; rm -rf ~.php");
        let task = run_task(
            path,
            &argv(&["php", "-f", "$ZED_FILE"]),
            Some(PathBuf::from("/project")),
        )
        .expect("resolves");
        assert_eq!(task.resolved.command.as_deref(), Some("php"));
        assert_eq!(
            task.resolved.args,
            ["-f", "/scratches/my scratch; rm -rf ~.php"]
        );
        assert_eq!(task.resolved.cwd.as_deref(), Some(Path::new("/project")));
        assert!(run_task(path, &[], None).is_none());
    }

    #[test]
    fn directory_setting_must_be_absolute() {
        assert_eq!(absolute_dir(None), None);
        assert_eq!(absolute_dir(Some("")), None);
        assert_eq!(absolute_dir(Some("scratches")), None);
        assert_eq!(absolute_dir(Some("~/scratches")), None);
        assert_eq!(
            absolute_dir(Some("/tmp/scratches")),
            Some(PathBuf::from("/tmp/scratches"))
        );
    }

    #[test]
    fn grouping_by_language_newest_first_within() {
        // `list` hands them over newest first.
        let items = vec![
            scratch("b.php", 1),
            scratch("x.js", 2),
            scratch("a.php", 3),
            scratch("notes", 4),
            scratch("y.JS", 5),
            scratch("tool.rb", 6),
        ];
        let (items, rows) = grouped(items);
        let names: Vec<_> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            ["x.js", "y.JS", "notes", "b.php", "a.php", "tool.rb"]
        );
        assert_eq!(
            rows,
            [
                Row::Header("JavaScript".into()),
                Row::Scratch(0),
                Row::Scratch(1),
                Row::Header("Other".into()),
                Row::Scratch(2),
                Row::Header("PHP".into()),
                Row::Scratch(3),
                Row::Scratch(4),
                Row::Header("RB".into()),
                Row::Scratch(5),
            ]
        );
        assert_eq!(grouped(Vec::new()), (Vec::new(), Vec::new()));
    }

    fn set_scratches(cx: &mut App, scratches: ZenithScratchesSettingsContent) {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.zenith = Some(ZenithSettingsContent {
                    scratches: Some(scratches),
                    ..Default::default()
                });
            });
        });
    }

    fn set_directory(directory: &str, cx: &mut VisualTestContext) {
        let directory = directory.to_string();
        cx.update(|_, cx| {
            set_scratches(
                cx,
                ZenithScratchesSettingsContent {
                    directory: Some(directory),
                    ..Default::default()
                },
            )
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn directory_setting_moves_the_store(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        new_scratch(cx, 0);
        assert_eq!(names(&fixture, cx), ["scratch.php"]);

        let custom = PathBuf::from("/custom/scratches");
        set_directory("/custom/scratches", cx);
        assert_eq!(cx.update(|_, cx| store_dir(cx)), custom);
        assert!(
            names(&fixture, cx).is_empty(),
            "the panel lists the new folder"
        );
        new_scratch(cx, 1);
        assert!(fixture.fs.is_file(&custom.join("scratch.js")).await);
        assert_eq!(names(&fixture, cx), ["scratch.js"]);

        // A relative folder is ignored: back to the default store.
        set_directory("relative/scratches", cx);
        assert_eq!(cx.update(|_, cx| store_dir(cx)), default_store_dir());
        assert_eq!(names(&fixture, cx), ["scratch.php"]);
    }

    #[test]
    fn next_free_name_skips_taken_names() {
        let mut taken = HashSet::new();
        assert_eq!(next_free_name("php", &taken), "scratch.php");
        taken.insert("scratch.php".to_string());
        assert_eq!(next_free_name("php", &taken), "scratch_1.php");
        taken.insert("scratch_1.php".to_string());
        assert_eq!(next_free_name("php", &taken), "scratch_2.php");
        assert_eq!(next_free_name("js", &taken), "scratch.js");
    }

    #[gpui::test]
    async fn create_never_reuses_a_name(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let dir = default_store_dir();
        let first = create(fs.as_ref(), &dir, "php").await.unwrap();
        let second = create(fs.as_ref(), &dir, "php").await.unwrap();
        assert_eq!(first, dir.join("scratch.php"));
        assert_eq!(second, dir.join("scratch_1.php"));
    }

    #[gpui::test]
    async fn new_scratch_file_creates_and_opens_it_without_a_project(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        assert!(names(&fixture, cx).is_empty());

        new_scratch(cx, 0);
        let path = default_store_dir().join("scratch.php");
        assert!(fixture.fs.is_file(&path).await, "PHP is the first language");
        assert_eq!(names(&fixture, cx), ["scratch.php"]);
        let opened = fixture.workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .and_then(|editor| editor.read(cx).active_buffer(cx))
                .and_then(|buffer| buffer.read(cx).file().map(|file| file.full_path(cx)))
        });
        assert_eq!(
            opened.as_deref().and_then(|path| path.file_name()),
            Some("scratch.php".as_ref())
        );
        let is_scratch_editor = fixture.workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .is_some_and(|editor| editor.read(cx).addon::<ScratchEditor>().is_some())
        });
        assert!(
            is_scratch_editor,
            "ctrl-r runs it: key context zenith_scratch"
        );
        let visible_worktrees = fixture.workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).visible_worktrees(cx).count()
        });
        assert_eq!(visible_worktrees, 0, "the store is not a project folder");

        // The second language (JavaScript), and the panel follows the folder.
        new_scratch(cx, 1);
        assert!(
            fixture
                .fs
                .is_file(&default_store_dir().join("scratch.js"))
                .await
        );
        assert_eq!(names(&fixture, cx).len(), 2);
    }

    #[gpui::test]
    async fn delete_trashes_and_the_panel_list_updates(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        new_scratch(cx, 0);
        let path = default_store_dir().join("scratch.php");

        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.select_ix(0, cx);
            panel.delete(&DeleteScratch, window, cx);
        });
        cx.run_until_parked();
        assert!(!fixture.fs.is_file(&path).await);
        assert!(names(&fixture, cx).is_empty());
    }

    #[gpui::test]
    async fn delete_refuses_a_path_outside_the_store(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/elsewhere",
            gpui::private::serde_json::json!({"keep.php": ""}),
        )
        .await;
        let dir = default_store_dir();
        create(fs.as_ref(), &dir, "php").await.unwrap();

        let err = delete(fs.as_ref(), &dir, Path::new("/elsewhere/keep.php")).await;
        assert!(err.is_err());
        assert!(fs.is_file(Path::new("/elsewhere/keep.php")).await);
        // Nor the folder itself.
        assert!(delete(fs.as_ref(), &dir, &dir).await.is_err());
        assert!(fs.is_dir(&dir).await);
        // A scratch inside is fine.
        delete(fs.as_ref(), &dir, &dir.join("scratch.php"))
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn rename_refuses_traversal_and_overwrite(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let dir = default_store_dir();
        let a = create(fs.as_ref(), &dir, "php").await.unwrap();
        let b = create(fs.as_ref(), &dir, "php").await.unwrap();

        for bad in [
            "",
            "  ",
            ".",
            "..",
            "../evil.php",
            "a/b.php",
            "a\\b.php",
            "a\0b",
        ] {
            assert!(rename(fs.as_ref(), &dir, &a, bad).await.is_err(), "{bad:?}");
        }
        assert!(!fs.is_file(&dir.parent().unwrap().join("evil.php")).await);
        assert!(fs.is_file(&a).await);
        // Never overwrites an existing scratch.
        assert!(
            rename(fs.as_ref(), &dir, &a, "scratch_1.php")
                .await
                .is_err()
        );
        assert!(fs.is_file(&a).await && fs.is_file(&b).await);
        // Renaming a path outside the store is refused too.
        fs.insert_tree(
            "/elsewhere",
            gpui::private::serde_json::json!({"x.php": ""}),
        )
        .await;
        assert!(
            rename(fs.as_ref(), &dir, Path::new("/elsewhere/x.php"), "y.php")
                .await
                .is_err()
        );
        // A plain name works.
        let renamed = rename(fs.as_ref(), &dir, &a, "notes.md").await.unwrap();
        assert_eq!(renamed, dir.join("notes.md"));
        assert!(fs.is_file(&renamed).await && !fs.is_file(&a).await);
    }

    #[gpui::test]
    async fn rename_modal_shows_the_error_and_stays_open(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        new_scratch(cx, 0);
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.select_ix(0, cx);
            panel.rename(&RenameScratch, window, cx);
        });
        cx.run_until_parked();
        let modal = fixture
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<RenameModal>(cx)
            })
            .expect("rename modal is open");
        modal.update_in(cx, |modal, window, cx| {
            modal
                .editor
                .update(cx, |editor, cx| editor.set_text("../evil.php", window, cx));
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(modal.read_with(cx, |modal, _| modal.error.is_some()));
        assert!(
            fixture
                .fs
                .is_file(&default_store_dir().join("scratch.php"))
                .await
        );

        modal.update_in(cx, |modal, window, cx| {
            modal
                .editor
                .update(cx, |editor, cx| editor.set_text("notes.md", window, cx));
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(names(&fixture, cx), ["notes.md"]);
    }

    #[gpui::test]
    async fn header_hide_button_closes_the_left_dock(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        let is_open = |fixture: &Fixture, cx: &mut VisualTestContext| {
            fixture.workspace.read_with(cx, |workspace, cx| {
                workspace
                    .dock_at_position(DockPosition::Left)
                    .read(cx)
                    .is_open()
            })
        };
        assert!(is_open(&fixture, cx));
        let bounds = cx
            .debug_bounds("zenith-scratches-hide")
            .expect("the header draws a hide button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!is_open(&fixture, cx));
    }

    fn store_worktree_of(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
    ) -> Option<worktree::WorktreeId> {
        fixture.workspace.read_with(cx, |workspace, cx| {
            store_worktree(workspace.project().read(cx), cx).map(|worktree| worktree.read(cx).id())
        })
    }

    #[gpui::test]
    async fn the_store_is_an_invisible_worktree_the_file_finder_never_lists(
        cx: &mut TestAppContext,
    ) {
        let (fixture, cx) = fixture(cx).await;
        cx.run_until_parked();
        let store = default_store_dir();
        assert!(
            fixture.fs.is_dir(&store).await,
            "the folder is created for the worktree"
        );
        let (held, visible, finder_candidates, all) =
            fixture.workspace.read_with(cx, |workspace, cx| {
                let project = workspace.project().read(cx);
                let ids = |worktrees: Vec<Entity<Worktree>>| -> Vec<(PathBuf, bool)> {
                    worktrees
                        .iter()
                        .map(|worktree| {
                            (
                                worktree.read(cx).abs_path().to_path_buf(),
                                worktree.read(cx).is_visible(),
                            )
                        })
                        .collect()
                };
                (
                    store_worktree(project, cx).is_some(),
                    ids(project.visible_worktrees(cx).collect()),
                    // What the file finder searches (file_finder.rs `visible_worktrees_and_single_files`).
                    ids(project
                        .worktree_store()
                        .read(cx)
                        .visible_worktrees_and_single_files(cx)
                        .collect()),
                    ids(project.worktrees(cx).collect()),
                )
            });
        assert!(held, "the panel opened the store: {all:?}");
        assert_eq!(all, [(store, false)]);
        assert!(
            visible.is_empty() && finder_candidates.is_empty(),
            "{visible:?} {finder_candidates:?}"
        );
    }

    #[gpui::test]
    async fn the_setting_off_releases_the_worktree_and_on_reopens_it(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        cx.run_until_parked();
        assert!(store_worktree_of(&fixture, cx).is_some());

        cx.update(|_, cx| {
            set_scratches(
                cx,
                ZenithScratchesSettingsContent {
                    project_panel_node: Some(false),
                    ..Default::default()
                },
            )
        });
        cx.run_until_parked();
        assert!(store_worktree_of(&fixture, cx).is_none());
        let remaining = fixture.workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).worktrees(cx).count()
        });
        assert_eq!(remaining, 0, "nothing holds the worktree any more");

        cx.update(|_, cx| set_scratches(cx, ZenithScratchesSettingsContent::default()));
        cx.run_until_parked();
        assert!(store_worktree_of(&fixture, cx).is_some());
    }

    #[gpui::test]
    async fn a_store_inside_a_project_folder_is_not_held(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture_with(cx, &["/proj"], Some("/proj/scratches")).await;
        cx.run_until_parked();
        assert!(
            store_worktree_of(&fixture, cx).is_none(),
            "the project's own visible worktree contains the store"
        );
        assert!(
            fixture
                .panel
                .read_with(cx, |panel, _| panel.store_worktree.is_none())
        );
    }
}
