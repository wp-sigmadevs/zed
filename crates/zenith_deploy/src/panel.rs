//! PhpStorm's Remote Host tool window (plan 37 M1b): a host dropdown with `<None>`, a toolbar
//! (collapse, refresh, disconnect, settings), and a lazy tree of the server's root. A directory
//! is listed only when expanded; a file opens as an editable cached copy whose saves upload to
//! the host after the user confirms (plan 37 M4a).

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::anyhow;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, PromptButton,
    PromptLevel, Render, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, actions, px, uniform_list,
};
use language::{Buffer, BufferEvent};
use settings::Settings as _;
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconButton, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenOptions, OpenVisible, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::{
    CopyState, DeploySettings, EntryKind, RemoteCopy, RemoteEntry, RemoteStat, ServerConfig,
    ServerRefused, ServerStore, ServerStoreEvent, ServerType, Transport, UploadDecision,
    UploadOutcome, UploadStart, cache_path_for,
    dialog::{DeploymentDialog, project_root},
    download_entry,
    remote_path::{join_remote, validate_name, validate_remote_path},
    secrets, spawn_operation, upload_warnings,
};

actions!(
    zenith_deploy,
    [
        /// Toggles focus on the Remote Host tool window.
        ToggleFocus,
        /// Lists the expanded remote folders again.
        Refresh,
        /// Closes the connection to the remote host.
        Disconnect,
        /// Collapses every folder in the Remote Host tool window.
        CollapseAll,
        /// Uploads the saved text of the open remote file to its host.
        UploadRemoteCopy,
    ]
);

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;

pub(crate) fn init(cx: &mut App) {
    // Every editor of a cached copy, however it was made (a split clones the editor with
    // serialization on), keeps its text out of the editor database: hot exit would store the
    // dirty text of a cached `wp-config.php`.
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if is_cache_editor(editor, cx) {
            editor.set_should_serialize(false, cx);
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RemoteHostPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &UploadRemoteCopy, window, cx| {
            let Some(panel) = workspace.panel::<RemoteHostPanel>(cx) else {
                return;
            };
            let buffer = workspace
                .active_item_as::<Editor>(cx)
                .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
            panel.update(cx, |panel, cx| panel.upload_active(buffer, window, cx));
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| RemoteHostPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

type Session = Arc<smol::lock::Mutex<Box<dyn Transport>>>;
type SharedSession = Shared<Task<Result<Session, SharedString>>>;

#[derive(Clone, Debug)]
struct Row {
    /// The remote folder the entry is in.
    directory: String,
    entry: RemoteEntry,
    depth: usize,
    /// The "Empty" line under a listed, empty folder. Never selectable, never acted on.
    placeholder: bool,
}

impl Row {
    fn path(&self) -> Option<String> {
        if self.placeholder {
            return None;
        }
        join_remote(&self.directory, &self.entry.name).ok()
    }

    fn placeholder(directory: &str, depth: usize) -> Self {
        Self {
            directory: directory.to_string(),
            entry: RemoteEntry {
                name: "Empty".into(),
                kind: EntryKind::File,
                size: 0,
                modified: None,
            },
            depth,
            placeholder: true,
        }
    }
}

pub struct RemoteHostPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    store: Entity<ServerStore>,
    project: Option<String>,
    server_id: Option<String>,
    /// The config the open session was made with; a change in the dialog reconnects.
    connected_config: Option<ServerConfig>,
    picked_default: bool,
    session: Option<SharedSession>,
    /// Listed folders by remote path. A folder absent here has never been listed.
    children: HashMap<String, Vec<RemoteEntry>>,
    expanded: HashSet<String>,
    loading: HashSet<String>,
    rows: Vec<Row>,
    selected: Option<usize>,
    status: Option<SharedString>,
    /// The host dropdown's menu; dropped when the servers or the selection change.
    host_menu: Option<Entity<ContextMenu>>,
    active: bool,
    scroll_handle: UniformListScrollHandle,
    /// Running operations by key; dropping one cancels it.
    operations: HashMap<String, Task<()>>,
    /// Remote copies this window opened, by cache path. An entry stays after its tab closes so a
    /// late upload result is still reported here; its subscriptions go with the buffer.
    owned: HashMap<PathBuf, OwnedCopy>,
    /// `is_local_unshared` of this window's project. A closure, not a workspace read: the
    /// palette action runs while the workspace is being updated.
    local_unshared: Box<dyn Fn(&App) -> bool>,
    /// A status line that is not an error ("Uploaded …").
    notice: Option<SharedString>,
    /// A prompt of this window is on screen: a second one would replace it (window.rs), so
    /// later questions become toasts until it is answered.
    prompt_open: Rc<Cell<bool>>,
    /// The tasks waiting for those answers. Not cleared by Disconnect: the prompt stays up.
    prompts: HashMap<PathBuf, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// A remote copy's open buffer in this window.
struct OwnedCopy {
    buffer: WeakEntity<Buffer>,
    /// The `Saved` subscription; dropped when the buffer is released.
    _saved: Option<Subscription>,
}

impl RemoteHostPanel {
    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ServerStore::global(cx);
        let subscriptions = vec![
            cx.observe_in(&store, window, |this, _, _, cx| {
                this.store_changed(cx);
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, _, event: &ServerStoreEvent, window, cx| match event {
                    ServerStoreEvent::UploadFinished {
                        cache_path,
                        outcome,
                    } => this.upload_finished(cache_path, outcome, window, cx),
                },
            ),
        ];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.weak_handle(),
            store,
            project: project_root(workspace, cx),
            server_id: None,
            connected_config: None,
            picked_default: false,
            session: None,
            children: HashMap::new(),
            expanded: HashSet::new(),
            loading: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            status: None,
            host_menu: None,
            active: false,
            scroll_handle: UniformListScrollHandle::new(),
            operations: HashMap::new(),
            owned: HashMap::new(),
            local_unshared: {
                let project = workspace.project().clone();
                Box::new(move |cx| {
                    let project = project.read(cx);
                    is_local_unshared(project.is_local(), project.is_shared())
                })
            },
            notice: None,
            prompt_open: Rc::default(),
            prompts: HashMap::new(),
            _subscriptions: subscriptions,
        };
        this.store_changed(cx);
        this
    }

    /// The servers the dropdown offers after `<None>`.
    pub(crate) fn host_options(&self, cx: &App) -> Vec<ServerConfig> {
        self.store.read(cx).servers_for(self.project.as_deref())
    }

    fn server(&self, cx: &App) -> Option<ServerConfig> {
        let id = self.server_id.as_ref()?;
        self.store.read(cx).server_for(self.project.as_deref(), id)
    }

    fn root(&self, cx: &App) -> Option<String> {
        let server = self.server(cx)?;
        let root = if server.root_path.trim().is_empty() {
            "/".to_string()
        } else {
            server.root_path.trim().to_string()
        };
        Some(root)
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.host_menu = None;
        let store = self.store.read(cx);
        if !store.is_loaded() {
            return;
        }
        if !self.picked_default {
            self.picked_default = true;
            self.server_id = self
                .project
                .as_deref()
                .and_then(|project| store.default_for(project))
                .map(String::from);
        }
        let current = self.server(cx);
        match (&current, &self.connected_config) {
            (None, _) if self.server_id.is_some() => self.select_server(None, cx),
            (Some(current), Some(connected)) if current != connected => {
                self.reset(cx);
                if self.active {
                    self.list_root(cx);
                }
            }
            _ => cx.notify(),
        }
    }

    /// Picks a host from the dropdown; `None` is `<None>`.
    pub(crate) fn select_server(&mut self, server_id: Option<String>, cx: &mut Context<Self>) {
        self.host_menu = None;
        self.reset(cx);
        self.server_id = server_id;
        if self.server_id.is_some() {
            self.list_root(cx);
        }
        cx.notify();
    }

    /// Drops the session and the tree. Running operations are cancelled with their tasks.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.operations.clear();
        if let Some(session) = self.session.take() {
            // Only a session that finished connecting is told to quit.
            if let Some(Ok(session)) = session.peek().cloned() {
                spawn_operation(cx, async move { session.lock().await.disconnect().await })
                    .detach_and_log_err(cx);
            }
        }
        self.connected_config = None;
        self.children.clear();
        self.expanded.clear();
        self.loading.clear();
        self.rows.clear();
        self.selected = None;
        self.status = None;
        cx.notify();
    }

    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        if let Some(server_id) = &self.server_id {
            let kept = self
                .store
                .update(cx, |store, cx| store.clear_cache(server_id, cx));
            if kept > 0 {
                self.notice = Some(
                    format!(
                        "Disconnected. Kept {kept} open file{} with unsaved changes on this computer.",
                        if kept == 1 { "" } else { "s" }
                    )
                    .into(),
                );
            }
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            return;
        };
        let expanded: Vec<String> = self
            .expanded
            .iter()
            .filter(|path| self.children.contains_key(*path))
            .cloned()
            .collect();
        self.list(root, cx);
        for path in expanded {
            self.list(path, cx);
        }
    }

    pub(crate) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild_rows(cx);
    }

    fn list_root(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.root(cx) {
            self.list(root, cx);
        }
    }

    fn session(&mut self, cx: &mut Context<Self>) -> Option<SharedSession> {
        if let Some(session) = &self.session {
            return Some(session.clone());
        }
        let server = self.server(cx)?;
        self.connected_config = Some(server.clone());
        let open = self.store.read(cx).open_session(server, cx);
        let session = cx
            .spawn(async move |_, _| {
                open.await
                    .map(|transport| Arc::new(smol::lock::Mutex::new(transport)))
                    .map_err(|error| SharedString::from(format!("{error:#}")))
            })
            .shared();
        self.session = Some(session.clone());
        Some(session)
    }

    fn fail(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        // A refusal (550 on one folder) leaves the connection good. A timeout or I/O error can
        // leave it mid-command: start over.
        if error.downcast_ref::<ServerRefused>().is_none() {
            self.session = None;
            self.connected_config = None;
        }
        self.status = Some(plain_text(&format!("{error:#}"), MAX_STATUS_CHARS).into());
        cx.notify();
    }

    fn list(&mut self, path: String, cx: &mut Context<Self>) {
        if validate_remote_path(&path).is_err() {
            self.status = Some(
                plain_text(
                    &format!("{path:?} is not an absolute remote path"),
                    MAX_STATUS_CHARS,
                )
                .into(),
            );
            cx.notify();
            return;
        }
        let Some(session) = self.session(cx) else {
            return;
        };
        self.loading.insert(path.clone());
        cx.notify();
        let task = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let result = async {
                    let session = session.await.map_err(|error| anyhow!(error))?;
                    let path = path.clone();
                    cx.update(|cx| {
                        spawn_operation(
                            cx,
                            async move { session.lock().await.list_dir(&path).await },
                        )
                    })
                    .await
                }
                .await;
                this.update(cx, |this, cx| {
                    this.loading.remove(&path);
                    match result {
                        Ok(mut entries) => {
                            entries.sort_by(|a, b| {
                                (a.kind != EntryKind::Directory, &a.name)
                                    .cmp(&(b.kind != EntryKind::Directory, &b.name))
                            });
                            this.children.insert(path, entries);
                            this.status = None;
                            this.rebuild_rows(cx);
                        }
                        Err(error) => {
                            this.expanded.remove(&path);
                            // A stale listing would be shown again when the folder is re-expanded.
                            // The root has no row to re-expand: keep its listing and show the
                            // error instead of blanking the tree.
                            if this.root(cx).as_deref() != Some(path.as_str()) {
                                this.children.remove(&path);
                            }
                            this.rebuild_rows(cx);
                            this.fail(error, cx);
                        }
                    }
                })
                .log_err();
            }
        });
        self.operations.insert(format!("list:{path}"), task);
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        if let Some(root) = self.root(cx) {
            self.push_rows(&root, 0, &mut rows);
        }
        self.rows = rows;
        if self
            .selected
            .is_some_and(|selected| selected >= self.rows.len())
        {
            self.selected = self.rows.len().checked_sub(1);
        }
        // The line a selection index now points at may be a placeholder: step back to the
        // folder it belongs to (a placeholder always follows a real row).
        while let Some(selected) = self.selected
            && self.rows[selected].placeholder
        {
            self.selected = selected.checked_sub(1);
        }
        cx.notify();
    }

    fn push_rows(&self, directory: &str, depth: usize, rows: &mut Vec<Row>) {
        let Some(entries) = self.children.get(directory) else {
            return;
        };
        for entry in entries {
            let row = Row {
                directory: directory.to_string(),
                entry: entry.clone(),
                depth,
                placeholder: false,
            };
            let path = row.path();
            rows.push(row);
            if let Some(path) = path
                && entry.kind == EntryKind::Directory
                && self.expanded.contains(&path)
            {
                self.push_rows(&path, depth + 1, rows);
            }
        }
        // A finished listing with nothing in it. The root has no row to sit under, and a
        // folder being listed again shows "Loading…" instead.
        if entries.is_empty() && depth > 0 && !self.loading.contains(directory) {
            rows.push(Row::placeholder(directory, depth));
        }
    }

    /// Expands or collapses a folder (listing it the first time), or opens a file.
    pub(crate) fn activate_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        if row.placeholder {
            return;
        }
        self.selected = Some(index);
        let Some(path) = row.path() else {
            return;
        };
        match row.entry.kind {
            EntryKind::Directory => {
                if !self.expanded.remove(&path) {
                    self.expanded.insert(path.clone());
                    if !self.children.contains_key(&path) {
                        self.list(path, cx);
                    }
                }
                self.rebuild_rows(cx);
            }
            EntryKind::File => self.open_file(row, window, cx),
            EntryKind::Symlink => {
                self.status = Some("Symbolic links are not followed.".into());
                cx.notify();
            }
        }
    }

    fn open_file(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.server(cx) else {
            return;
        };
        if validate_name(&server.id).is_err() {
            self.status = Some("The server has an invalid id.".into());
            cx.notify();
            return;
        }
        let cache_root = self.store.read(cx).cache_dir.clone();
        // The one builder of cache paths: the upload index accepts no other layout.
        let remote_path = row.path();
        let cache_path = remote_path
            .as_ref()
            .and_then(|remote_path| cache_path_for(&cache_root, &server.id, remote_path).ok());
        let (Some(remote_path), Some(cache_path)) = (remote_path, cache_path) else {
            self.status = Some("This file cannot be opened from the cache.".into());
            cx.notify();
            return;
        };
        let Some(local_directory) = cache_path.parent().map(PathBuf::from) else {
            self.status = Some("This file cannot be opened from the cache.".into());
            cx.notify();
            return;
        };
        let Some(session) = self.session(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let store = self.store.clone();
        let server_id = server.id.clone();
        let server_name = shown(&server.name, 60);
        let file_label = shown(&row.entry.name, 100);
        let key = format!("open:{remote_path}");
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                // A copy that is still open (here, in another window, or restored by hot exit
                // without being ours) is never downloaded over: its edits were made against the
                // old baseline.
                let state = cx.update(|_, cx| open_state(&workspace, &store, &cache_path, cx))?;
                match state {
                    OpenState::Closed => {}
                    OpenState::Here => {
                        let item = workspace
                            .update_in(cx, |workspace, window, cx| {
                                workspace.open_abs_path(
                                    cache_path.clone(),
                                    OpenOptions {
                                        visible: Some(OpenVisible::None),
                                        ..OpenOptions::default()
                                    },
                                    window,
                                    cx,
                                )
                            })?
                            .await?;
                        // A restored copy that is not in the index must not be edited.
                        let orphan = cx.update(|_, cx| store.read(cx).is_orphan(&cache_path))?;
                        if let (true, Some(editor)) = (orphan, item.downcast::<Editor>()) {
                            cx.update(|_, cx| {
                                editor.update(cx, |editor, _| editor.set_read_only(true))
                            })?;
                        }
                        return anyhow::Ok(None);
                    }
                    OpenState::Elsewhere => {
                        return Err(anyhow!("This file is already open in another window."));
                    }
                }
                // One download into this path at a time, across windows; held until the buffer
                // is attached (it then counts as open).
                let _reservation = cx
                    .update(|_, cx| store.read(cx).reserve_download(&cache_path))?
                    .ok_or_else(|| anyhow!("This file is already being opened."))?;
                let session = session.await.map_err(|error| anyhow!(error))?;
                // Into a fresh directory: a saved copy that is still in the cache (its tab was
                // closed with changes that never uploaded) is only replaced after the user says so.
                let incoming = local_directory
                    .join(format!(".zenith-incoming-{}", uuid::Uuid::new_v4()));
                // Removed on every exit, including a task dropped by a reset or a closing window.
                let _incoming_guard = IncomingDir(incoming.clone());
                let fetched = cx
                    .update(|_, cx| {
                        let remote_path = remote_path.clone();
                        let cache_path = cache_path.clone();
                        let incoming = incoming.clone();
                        spawn_operation(cx, async move {
                            let fetched = fetch_copy(
                                session,
                                &cache_root,
                                &incoming,
                                &cache_path,
                                &remote_path,
                                &row,
                            )
                            .await;
                            if fetched.is_err() {
                                std::fs::remove_dir_all(&incoming).ok();
                            }
                            fetched
                        })
                    })?
                    .await?;
                let mut stat = fetched.stat.clone();
                let mut choice = ReopenAnswer::UseServer;
                if fetched.differs {
                    // Freed when this task ends, however it ends.
                    let slot = this.update(cx, |this, _| claim_prompt(&this.prompt_open))?;
                    let busy = slot.is_none();
                    choice = if busy {
                        ReopenAnswer::Cancel
                    } else {
                        let message = format!(
                            "The copy of {file_label} on this computer differs from the one on {server_name}"
                        );
                        let answers = [
                            PromptButton::ok(REOPEN_BUTTONS[0]),
                            PromptButton::new(REOPEN_BUTTONS[1]),
                            PromptButton::cancel(REOPEN_BUTTONS[2]),
                        ];
                        let answer = cx.update(|window, cx| {
                            window.prompt(
                                PromptLevel::Warning,
                                &message,
                                Some(REOPEN_DETAIL),
                                &answers,
                                cx,
                            )
                        })?;
                        let answer = answer.await.unwrap_or(2);
                        drop(slot);
                        reopen_answer(answer)
                    };
                    if choice == ReopenAnswer::Keep {
                        // Your version, compared with the server as it was when you got it.
                        stat = cx.update(|_, cx| {
                            store.read(cx).baseline_for(&cache_path, &server_id, &remote_path)
                        })?;
                    }
                    if busy {
                        cx.update(|_, cx| {
                            spawn_operation(cx, async move { place_copy(&fetched, &cache_path, false) })
                        })?
                        .await?;
                        return Err(anyhow!(
                            "Answer the open question first, then open the file again."
                        ));
                    }
                }
                let use_server = choice == ReopenAnswer::UseServer;
                cx.update(|_, cx| {
                    let cache_path = cache_path.clone();
                    spawn_operation(cx, async move { place_copy(&fetched, &cache_path, use_server) })
                })?
                .await?;
                if choice == ReopenAnswer::Cancel {
                    return anyhow::Ok(None);
                }
                let registered = cx.update(|_, cx| {
                    store.update(cx, |store, _| {
                        store.register_download(cache_path.clone(), &server_id, &remote_path, stat)
                    })
                })?;
                let open = workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        cache_path.clone(),
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..OpenOptions::default()
                        },
                        window,
                        cx,
                    )
                })?;
                let item = open.await?;
                if let Some(editor) = item.downcast::<Editor>() {
                    // Read-only until the buffer is attached to the index.
                    cx.update(|_, cx| editor.update(cx, |editor, _| editor.set_read_only(true)))?;
                    // A copy the index refused stays read-only and is never attached.
                    if registered.is_ok() {
                        this.update_in(cx, |this, window, cx| {
                            this.attach_editor(&cache_path, &editor, window, cx)
                        })?;
                    }
                }
                if let Err(error) = registered {
                    return Err(error.context("This copy is read-only"));
                }
                anyhow::Ok(Some(()))
            }
            .await;
            match result {
                Ok(Some(())) => {
                    this.update(cx, |this, cx| {
                        this.status = None;
                        cx.notify();
                    })
                    .log_err();
                }
                Ok(None) => {}
                Err(error) => {
                    this.update(cx, |this, cx| this.fail(error, cx)).log_err();
                }
            }
        });
        self.operations.insert(key, task);
    }

    /// Makes the editor of a freshly opened copy editable and wires its saves to the uploader.
    /// A file that is not in the index (`is_orphan`) or whose buffer the index refuses stays
    /// read-only and never uploads.
    fn attach_editor(
        &mut self,
        cache_path: &Path,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        if self.store.read(cx).is_orphan(cache_path) {
            return;
        }
        let attached = self
            .store
            .update(cx, |store, cx| store.attach_buffer(cache_path, &buffer, cx));
        if !attached {
            return;
        }
        // Hot exit would store the dirty text of a cached `wp-config.php` in the editor database.
        editor.update(cx, |editor, cx| {
            editor.set_should_serialize(false, cx);
            editor.set_read_only(false);
        });
        let saved = cx.subscribe_in(
            &buffer,
            window,
            |this, buffer, event: &BufferEvent, window, cx| {
                if matches!(event, BufferEvent::Saved) {
                    this.buffer_saved(buffer, window, cx);
                }
            },
        );
        // Finishes a Disconnect that kept this copy; held by the buffer itself, not this panel.
        let store = self.store.clone();
        let app: &mut App = cx;
        app.observe_release(&buffer, move |_, cx| {
            store.update(cx, |store, cx| store.buffer_released(cx));
        })
        .detach();
        self.owned.insert(
            cache_path.to_path_buf(),
            OwnedCopy {
                buffer: buffer.downgrade(),
                _saved: Some(saved),
            },
        );
        self.status = None;
        cx.notify();
    }

    fn buffer_saved(
        &mut self,
        buffer: &Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_upload(buffer, false, false, window, cx);
    }

    /// The palette action: uploads the saved text of the active editor's buffer when it is one
    /// of this window's remote copies.
    fn upload_active(
        &mut self,
        buffer: Option<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owned = buffer.filter(|buffer| {
            self.owned
                .values()
                .any(|copy| copy.buffer.entity_id() == buffer.entity_id())
        });
        match owned {
            Some(buffer) => self.start_upload(&buffer, false, false, window, cx),
            None => self.toast(
                "The active file is not a remote copy opened from the Remote Host panel.",
                cx,
            ),
        }
    }

    fn start_upload(
        &mut self,
        buffer: &Entity<Buffer>,
        force: bool,
        consent_once: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local_unshared = (self.local_unshared)(cx);
        let start = self.store.update(cx, |store, cx| {
            store.upload(buffer, local_unshared, force, consent_once, cx)
        });
        match start {
            UploadStart::Started | UploadStart::Queued => {}
            UploadStart::NeedsConfirmation => self.confirm_upload(buffer, window, cx),
            UploadStart::Refused(message) => self.toast(&message, cx),
            UploadStart::NotRemote => {}
        }
        cx.notify();
    }

    /// What a prompt or toast names for the copy `cache_path`. Every shown piece comes from a
    /// server's listing or the user's config, so it goes through [`plain_text`].
    fn describe(&self, cache_path: &Path, cx: &App) -> Option<Described> {
        let store = self.store.read(cx);
        let copy = store.copy_for_buffer_path(cache_path)?;
        let server = store
            .servers
            .servers
            .iter()
            .find(|server| server.id == copy.server_id)?;
        let file = copy.remote_path.rsplit('/').next().unwrap_or_default();
        Some(Described {
            file: shown(file, 100),
            name: shown(&server.name, 60),
            host: shown(&server.host, 100),
            server_type: server.server_type,
            target: secrets::target_account(server),
        })
    }

    fn buffer_path(&self, buffer: &Entity<Buffer>) -> Option<PathBuf> {
        self.owned.iter().find_map(|(path, copy)| {
            (copy.buffer.entity_id() == buffer.entity_id()).then(|| path.clone())
        })
    }

    /// Asks before the first upload to a server: Upload / Always / Cancel / Never.
    fn confirm_upload(
        &mut self,
        buffer: &Entity<Buffer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cache_path) = self.buffer_path(buffer) else {
            return;
        };
        let Some(described) = self.describe(&cache_path, cx) else {
            return;
        };
        let Some(slot) = claim_prompt(&self.prompt_open) else {
            self.saved_here_only(&described.file, buffer, cx);
            return;
        };
        let message = format!(
            "Upload {} to {} ({})?",
            described.file, described.name, described.host
        );
        let detail = upload_detail(described.server_type);
        let answers = [
            PromptButton::ok(UPLOAD_BUTTONS[0]),
            PromptButton::new(UPLOAD_BUTTONS[1]),
            PromptButton::cancel(UPLOAD_BUTTONS[2]),
            PromptButton::new(UPLOAD_BUTTONS[3]),
        ];
        let answer = window.prompt(PromptLevel::Warning, &message, Some(&detail), &answers, cx);
        let buffer = buffer.downgrade();
        let path = cache_path.clone();
        // The host the user is looking at in this prompt: an answer is recorded for it only.
        let target = described.target;
        let task = cx.spawn_in(window, async move |this, cx| {
            let answer = upload_answer(answer.await.unwrap_or(2));
            drop(slot);
            this.update_in(cx, |this, window, cx| {
                let Some(buffer) = buffer.upgrade() else {
                    return;
                };
                match answer {
                    UploadAnswer::Once => this.start_upload(&buffer, false, true, window, cx),
                    UploadAnswer::Always => {
                        if this.set_decision(&path, &target, UploadDecision::Allow, cx) {
                            this.start_upload(&buffer, false, false, window, cx);
                        }
                    }
                    UploadAnswer::Never => {
                        this.set_decision(&path, &target, UploadDecision::Deny, cx);
                    }
                    UploadAnswer::NotNow => {}
                }
            })
            .log_err();
        });
        self.prompts.insert(cache_path, task);
    }

    /// A second question while one is on screen would replace it: say where the file stands.
    fn saved_here_only(&mut self, file: &str, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        self.toast_with(
            &format!(
                "{file} is saved on this computer only. Run \"{UPLOAD_COMMAND}\" to upload it."
            ),
            Some(buffer.downgrade()),
            false,
            cx,
        );
    }

    /// Records the answer for the server of `cache_path` if it is still the host the prompt
    /// named (`target`); otherwise nothing is recorded and the user is told. Only button clicks
    /// reach this.
    fn set_decision(
        &mut self,
        cache_path: &Path,
        target: &str,
        decision: UploadDecision,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(server_id) = self
            .store
            .read(cx)
            .copy_for_buffer_path(cache_path)
            .map(|copy| copy.server_id.clone())
        else {
            return false;
        };
        let recorded = self.store.update(cx, |store, _| {
            store.set_upload_decision_for_target(&server_id, target, decision)
        });
        if !recorded {
            self.toast(
                "The server's address or login changed while the question was open, so your answer was not saved and nothing was uploaded.",
                cx,
            );
        }
        recorded
    }

    /// What the uploader reported for a copy of this window.
    fn upload_finished(
        &mut self,
        cache_path: &Path,
        outcome: &UploadOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.owned.contains_key(cache_path) {
            return;
        }
        cx.notify();
        let Some(Described { file, name, .. }) = self.describe(cache_path, cx) else {
            return;
        };
        match outcome {
            UploadOutcome::Uploaded => {
                let line = format!("Uploaded {file} to {name}");
                self.notice = Some(line.clone().into());
                // gpui has no live regions: the status line is silent for a screen reader, so
                // the notification area records every result too (follow-up in gpui).
                self.toast_with(&line, None, true, cx);
            }
            UploadOutcome::Conflict => self.confirm_force(
                cache_path,
                format!("{file} changed on {name} since you opened it"),
                "Overwrite replaces the server's version with yours. Cancel keeps your version on this computer only.",
                "Overwrite",
                window,
                cx,
            ),
            UploadOutcome::AskFirst => self.confirm_force(
                cache_path,
                format!("{file} is missing or cannot be verified on {name}: upload anyway?"),
                "The server did not report a version to compare with, so the upload could replace a file you have not seen. Cancel keeps your version on this computer only.",
                "Upload anyway",
                window,
                cx,
            ),
            UploadOutcome::Blocked(message) => {
                self.toast(&plain_text(message, MAX_TOAST_CHARS), cx)
            }
            UploadOutcome::Failed(message) => {
                let retry = self
                    .owned
                    .get(cache_path)
                    .map(|copy| copy.buffer.clone())
                    .filter(|buffer| buffer.upgrade().is_some());
                self.toast_with(
                    &format!("{} {FAILED_HINT}", plain_text(message, MAX_TOAST_CHARS)),
                    retry,
                    false,
                    cx,
                );
            }
        }
    }

    /// Cancel (the default, so a stray Return does not overwrite) or `yes`; yes uploads again
    /// with `force`.
    fn confirm_force(
        &mut self,
        cache_path: &Path,
        message: String,
        detail: &str,
        yes: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let buffer = self
            .owned
            .get(cache_path)
            .and_then(|copy| copy.buffer.upgrade());
        let Some(buffer) = buffer else {
            // The tab is closed. The saved text stays in the cache; opening the file again asks
            // before it replaces that text, and the next upload checks the server again.
            self.toast(
                &format!(
                    "{} Your changes are saved on this computer only. To upload them, open the file again, choose \"{}\", then run \"{UPLOAD_COMMAND}\".",
                    end_sentence(&message),
                    REOPEN_BUTTONS[0]
                ),
                cx,
            );
            return;
        };
        let Some(slot) = claim_prompt(&self.prompt_open) else {
            let file = self
                .describe(cache_path, cx)
                .map(|described| described.file)
                .unwrap_or_default();
            self.saved_here_only(&file, &buffer, cx);
            return;
        };
        let answers = force_buttons(yes);
        let answer = window.prompt(PromptLevel::Warning, &message, Some(detail), &answers, cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await.unwrap_or(0);
            drop(slot);
            this.update_in(cx, |this, window, cx| {
                // The button names the server and the file, and a stop only follows an upload the
                // user already allowed: this call is consent as well as force.
                if confirmed(answer) {
                    this.start_upload(&buffer, true, true, window, cx);
                }
            })
            .log_err();
        });
        self.prompts.insert(cache_path.to_path_buf(), task);
    }

    /// Plain text only: a server's reply may be in `message`.
    fn toast(&mut self, message: &str, cx: &mut Context<Self>) {
        self.toast_with(message, None, false, cx);
    }

    /// A toast; `retry` adds a "Retry upload" button that uploads that buffer again, `autohide`
    /// removes it after a while.
    fn toast_with(
        &mut self,
        message: &str,
        retry: Option<WeakEntity<Buffer>>,
        autohide: bool,
        cx: &mut Context<Self>,
    ) {
        let message = plain_text(message, 2 * MAX_TOAST_CHARS);
        let workspace = self.workspace.clone();
        let panel = cx.weak_entity();
        // Deferred: the palette action runs while the workspace is being updated.
        cx.defer(move |cx| {
            let mut toast = Toast::new(NotificationId::unique::<RemoteHostPanel>(), message);
            if autohide {
                toast = toast.autohide();
            }
            if let Some(buffer) = retry {
                toast = toast.on_click("Retry upload", move |window, cx| {
                    let Some(buffer) = buffer.upgrade() else {
                        return;
                    };
                    panel
                        .update(cx, |panel, cx| {
                            panel.start_upload(&buffer, false, false, window, cx)
                        })
                        .log_err();
                });
            }
            workspace
                .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
                .log_err();
        });
    }

    /// An upload of this panel's server is running.
    fn is_uploading(&self, cx: &App) -> bool {
        let Some(server_id) = &self.server_id else {
            return false;
        };
        self.store
            .read(cx)
            .copies
            .values()
            .any(|copy| &copy.server_id == server_id && copy.state == CopyState::Uploading)
    }

    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let server_id = self.server_id.clone();
        // The click arrives while the panel renders inside the workspace; open after it.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    DeploymentDialog::open(workspace, server_id, window, cx)
                })
                .log_err();
        });
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let from = self.selected.map_or(0, |selected| selected + 1);
        let Some(next) = (from..self.rows.len()).find(|&index| !self.rows[index].placeholder)
        else {
            return;
        };
        self.selected = Some(next);
        self.scroll_handle
            .scroll_to_item(next, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(previous) = (0..self.selected.unwrap_or(1))
            .rev()
            .find(|&index| self.rows.get(index).is_some_and(|row| !row.placeholder))
        else {
            return;
        };
        self.selected = Some(previous);
        self.scroll_handle
            .scroll_to_item(previous, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected {
            self.activate_row(selected, window, cx);
        }
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        if row.placeholder {
            // Muted text, no icon, no click handler, no hover: nothing to act on.
            return div()
                .debug_selector(|| "remote-host-empty".into())
                .child(
                    ListItem::new(SharedString::from(format!(
                        "zenith-deploy-empty-{}",
                        row.directory
                    )))
                    .height(px(ROW_HEIGHT))
                    .indent_level(row.depth)
                    .indent_step_size(px(INDENT))
                    .selectable(false)
                    .aria_label("Empty folder")
                    .child(
                        Label::new("Empty")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
                )
                .into_any_element();
        }
        let path = row.path().unwrap_or_default();
        let expanded = self.expanded.contains(&path);
        let (icon, kind) = match row.entry.kind {
            EntryKind::Directory if expanded => (IconName::FolderOpen, "folder"),
            EntryKind::Directory => (IconName::Folder, "folder"),
            EntryKind::File => (IconName::File, "file"),
            EntryKind::Symlink => (IconName::Link, "symbolic link"),
        };
        let loading = self.loading.contains(&path);
        ListItem::new(SharedString::from(format!("zenith-deploy-row-{path}")))
            .height(px(ROW_HEIGHT))
            .indent_level(row.depth)
            .indent_step_size(px(INDENT))
            .toggle_state(self.selected == Some(index))
            .aria_label(SharedString::from(format!(
                "{} ({kind})",
                shown(&row.entry.name, 200)
            )))
            .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .child(Label::new(shown(&row.entry.name, 200)))
            .when(loading, |item| {
                item.end_slot(
                    Label::new("Loading…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.activate_row(index, window, cx)))
            .into_any_element()
    }

    fn host_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        if let Some(menu) = &self.host_menu {
            return menu.clone();
        }
        let options = self.host_options(cx);
        let selected = self.server_id.clone();
        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let choices = std::iter::once((None, "<None>".to_string())).chain(
                options
                    .into_iter()
                    .map(|server| (Some(server.id), server.name)),
            );
            for (id, name) in choices {
                let panel = panel.clone();
                let toggled = id == selected;
                menu = menu.toggleable_entry(
                    name,
                    toggled,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        panel
                            .update(cx, |panel, cx| panel.select_server(id.clone(), cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        self.host_menu = Some(menu.clone());
        menu
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .server(cx)
            .map(|server| server.name)
            .unwrap_or_else(|| "<None>".into());
        let menu = self.host_menu(window, cx);
        let connected = self.session.is_some();
        let has_server = self.server_id.is_some();
        h_flex()
            .p_1()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                DropdownMenu::new("zenith-deploy-host", label, menu)
                    .style(DropdownStyle::Subtle)
                    .aria_label("Remote host"),
            )
            .child(
                div()
                    .debug_selector(|| "zenith-deploy-open-dialog".into())
                    .child(
                        IconButton::new("zenith-deploy-open-dialog", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Deployment Settings…"))
                            .aria_label("Deployment Settings…")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_dialog(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("zenith-deploy-collapse", IconName::ListCollapse)
                    .icon_size(IconSize::Small)
                    .disabled(self.expanded.is_empty())
                    .tooltip(Tooltip::for_action_title("Collapse All", &CollapseAll))
                    .aria_label("Collapse All")
                    .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(!has_server)
                    .tooltip(Tooltip::for_action_title("Refresh", &Refresh))
                    .aria_label("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-disconnect", IconName::Disconnected)
                    .icon_size(IconSize::Small)
                    .disabled(!connected)
                    .tooltip(Tooltip::for_action_title("Disconnect", &Disconnect))
                    .aria_label("Disconnect")
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Deployment Settings…"))
                    .aria_label("Deployment Settings…")
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
    }
}

/// The cache holds copies of remote files (maybe `wp-config.php`): owner-only on unix.
fn create_private_dir(cache_root: &Path, directory: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(cache_root, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = cache_root;
    Ok(())
}

/// Longest piece of server text in a toast, and in the status line, in characters.
const MAX_TOAST_CHARS: usize = 300;
const MAX_STATUS_CHARS: usize = 300;

/// The palette name of [`UploadRemoteCopy`].
const UPLOAD_COMMAND: &str = "zenith deploy: upload remote copy";

const FAILED_HINT: &str = "Your changes are saved on this computer. Choose Retry upload, or run \"zenith deploy: upload remote copy\".";

/// The shown names of one copy and its server, and the host the user is being asked about.
struct Described {
    file: String,
    name: String,
    host: String,
    server_type: ServerType,
    target: String,
}

/// Whether `editor` shows a file in the remote-copy cache.
fn is_cache_editor(editor: &Editor, cx: &App) -> bool {
    let Some(store) = cx.try_global::<crate::GlobalServerStore>() else {
        return false;
    };
    editor
        .buffer()
        .read(cx)
        .as_singleton()
        .and_then(|buffer| crate::remote_edit::buffer_path(buffer.read(cx), cx))
        .is_some_and(|path| path.starts_with(&store.0.read(cx).cache_dir))
}

/// What the dialog asks when a saved copy is still in the cache and differs from the server's.
const REOPEN_BUTTONS: [&str; 3] = ["Keep my saved copy", "Use the server's version", "Cancel"];
const REOPEN_DETAIL: &str = "It may hold changes you saved here that were never uploaded, or the file on the server may have changed. Keep my saved copy opens your version, and the next upload checks the server first. Use the server's version replaces your saved copy.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReopenAnswer {
    Keep,
    UseServer,
    Cancel,
}

/// A dismissed prompt is Cancel.
fn reopen_answer(index: usize) -> ReopenAnswer {
    match index {
        0 => ReopenAnswer::Keep,
        1 => ReopenAnswer::UseServer,
        _ => ReopenAnswer::Cancel,
    }
}

/// A downloaded file waiting in `incoming` beside the cache path it would replace.
struct Fetched {
    incoming: PathBuf,
    downloaded: PathBuf,
    /// A file is already at the cache path and its bytes differ from the download.
    differs: bool,
    stat: Option<RemoteStat>,
}

/// Stat (before the download: what the server looked like), download into `incoming`, and
/// compare with the file already in the cache.
async fn fetch_copy(
    session: Session,
    cache_root: &Path,
    incoming: &Path,
    cache_path: &Path,
    remote_path: &str,
    row: &Row,
) -> anyhow::Result<Fetched> {
    create_private_dir(cache_root, incoming)?;
    let mut transport = session.lock().await;
    // No stamp is allowed; the first upload then asks first.
    let stat = transport.stat(remote_path).await.ok().flatten();
    let downloaded =
        download_entry(transport.as_mut(), &row.directory, &row.entry, incoming).await?;
    drop(transport);
    anyhow::ensure!(
        downloaded.parent() == Some(incoming) && downloaded.file_name() == cache_path.file_name(),
        "The download landed outside the cache."
    );
    let existing = match std::fs::read(cache_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let differs = match existing {
        Some(existing) => existing != std::fs::read(&downloaded)?,
        None => false,
    };
    Ok(Fetched {
        incoming: incoming.to_path_buf(),
        downloaded,
        differs,
        stat,
    })
}

/// Moves the download over the cache path (`use_server`) or drops it, and removes `incoming`.
fn place_copy(fetched: &Fetched, cache_path: &Path, use_server: bool) -> anyhow::Result<()> {
    let placed = if use_server {
        std::fs::rename(&fetched.downloaded, cache_path).map_err(anyhow::Error::from)
    } else {
        Ok(())
    };
    std::fs::remove_dir_all(&fetched.incoming).ok();
    placed
}

/// `message` as one sentence: ends in `.`, `?` or `!`.
fn end_sentence(message: &str) -> String {
    if message.ends_with(['.', '?', '!']) {
        message.to_string()
    } else {
        format!("{message}.")
    }
}

/// `project.is_local() && !project.is_shared()`: a collab guest's save would upload with the
/// host's credentials, so only a local, unshared project uploads.
fn is_local_unshared(is_local: bool, is_shared: bool) -> bool {
    is_local && !is_shared
}

/// The first-upload prompt's buttons, in order; [`upload_answer`] maps the clicked index back.
const UPLOAD_BUTTONS: [&str; 4] = [
    "Upload",
    "Always upload to this server until I quit",
    "Cancel",
    "Never upload to this server until I quit",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadAnswer {
    /// Upload this save only.
    Once,
    /// Upload this and later saves to the server until the app quits.
    Always,
    /// Keep the change local.
    NotNow,
    /// No uploads to the server until the app quits.
    Never,
}

/// An unknown index (a dismissed prompt) is "Not now": never an upload.
fn upload_answer(index: usize) -> UploadAnswer {
    match index {
        0 => UploadAnswer::Once,
        1 => UploadAnswer::Always,
        3 => UploadAnswer::Never,
        _ => UploadAnswer::NotNow,
    }
}

/// Cancel first, so it is the default; `yes` is never the button Return presses.
fn force_buttons(yes: &'static str) -> [PromptButton; 2] {
    [PromptButton::cancel("Cancel"), PromptButton::new(yes)]
}

/// Overwrite / Upload anyway is the second button (Cancel is the default); anything else
/// cancels.
fn confirmed(index: usize) -> bool {
    index == 1
}

/// What the first-upload prompt says besides the question.
fn upload_detail(server_type: ServerType) -> String {
    let warnings = upload_warnings(server_type);
    let mut lines = vec!["Each save writes the file over the one on the server.".to_string()];
    if warnings.clear_text {
        lines.push(
            "FTP sends your password and the file unencrypted: others on the network can read them."
                .into(),
        );
    }
    if warnings.follows_symlinks {
        lines.push(
            "If this file is a link (symlink) on the server, the upload may change the file it points to."
                .into(),
        );
    }
    lines.push(
        "Zenith checks the server for changes first, but can miss a change made in the same second that kept the size the same."
            .into(),
    );
    lines.join("\n")
}

/// `plain_text`, or "(unnamed)" when nothing visible is left: a prompt must never name a file
/// or server with an empty string.
fn shown(text: &str, max: usize) -> String {
    let text = plain_text(text, max);
    if text.is_empty() {
        "(unnamed)".into()
    } else {
        text
    }
}

/// The right to show a prompt in this window; a second prompt would replace the first, so only
/// one holder exists. Dropping the slot (answer handled, or the task that holds it cancelled by a
/// reset or a closing window) frees it.
struct PromptSlot(Rc<Cell<bool>>);

impl Drop for PromptSlot {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn claim_prompt(flag: &Rc<Cell<bool>>) -> Option<PromptSlot> {
    if flag.replace(true) {
        None
    } else {
        Some(PromptSlot(flag.clone()))
    }
}

/// The directory a download waits in; removed when dropped, whatever ends the task. The bytes
/// are the server's (maybe `wp-config.php`).
struct IncomingDir(PathBuf);

impl Drop for IncomingDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Characters that show nothing or reorder text: format characters (zero-width, bidi
/// overrides and isolates, tags, soft hyphen, BOM).
fn is_invisible(c: char) -> bool {
    matches!(u32::from(c),
        0xAD | 0x34F | 0x61C | 0x115F | 0x1160 | 0x180E | 0x200B | 0x200E..=0x200F
        | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFEFF | 0xFFA0 | 0xFFF9..=0xFFFB
        | 0xE0000..=0xE007F)
}

/// A mark that attaches to the character before it: cutting between them, or starting a text
/// with one, would attach it to whatever is shown next to the text.
fn is_combining(c: char) -> bool {
    matches!(u32::from(c),
        0x300..=0x36F | 0x483..=0x489 | 0x591..=0x5BD | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF
        | 0x200D | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F)
}

/// Text from a server or a listing, made safe to show: ANSI escape sequences removed whole,
/// control and invisible/bidi format characters dropped, whitespace (newlines, U+2028/2029)
/// collapsed to single spaces, at most `max` characters (then `…`).
fn plain_text(text: &str, max: usize) -> String {
    let mut kept = String::new();
    // A cap on the work for a huge reply; escape sequences are short.
    let mut chars = text.chars().take(max.saturating_mul(16).max(64)).peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                // CSI: parameters, then a final byte in @..~.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if is_invisible(c) {
            continue;
        }
        kept.push(if c.is_control() { ' ' } else { c });
    }
    let line = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars: Vec<char> = line.chars().skip_while(|c| is_combining(*c)).collect();
    if chars.len() > max {
        let mut cut = max;
        // Never between a character and its marks: the whole cluster goes.
        while cut > 0 && is_combining(chars[cut]) {
            cut -= 1;
        }
        chars.truncate(cut);
        chars.push('…');
    }
    chars.into_iter().collect()
}

/// Where a cache file is open.
#[derive(Debug, PartialEq, Eq)]
enum OpenState {
    Closed,
    /// In this window's project: its tab can be focused.
    Here,
    /// In another window (or kept by the index with no buffer found): never downloaded over.
    Elsewhere,
}

/// Whether `path` is open: in the index as an open copy, or as a buffer of any project, such as
/// one hot exit restored that is not ours.
fn open_state(
    workspace: &WeakEntity<Workspace>,
    store: &Entity<ServerStore>,
    path: &Path,
    cx: &App,
) -> OpenState {
    // By the buffer's own file path: an invisible worktree has no project path to look up.
    let here = |workspace: &Entity<Workspace>| {
        workspace
            .read(cx)
            .project()
            .read(cx)
            .opened_buffers(cx)
            .iter()
            .any(|buffer| {
                crate::remote_edit::buffer_path(buffer.read(cx), cx).as_deref() == Some(path)
            })
    };
    let own = workspace.upgrade();
    if own.as_ref().is_some_and(here) {
        return OpenState::Here;
    }
    // The window being updated cannot be read: it was covered by `own` above.
    let elsewhere = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok())
        .any(|multi| {
            multi
                .workspaces()
                .any(|workspace| Some(workspace) != own.as_ref() && here(workspace))
        });
    let indexed = store
        .read(cx)
        .copy_for_buffer_path(path)
        .is_some_and(RemoteCopy::is_open);
    if elsewhere || indexed {
        OpenState::Elsewhere
    } else {
        OpenState::Closed
    }
}

impl Render for RemoteHostPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.rows.is_empty() {
            let message: SharedString = if self.server_id.is_none() {
                "Select a host to browse it.".into()
            } else if !self.loading.is_empty() {
                "Connecting…".into()
            } else {
                "Nothing to show. Refresh to list the root again.".into()
            };
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element()
        } else {
            uniform_list(
                "zenith-deploy-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range
                        .map(|index| this.render_row(index, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .key_context("RemoteHostPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &Disconnect, _, cx| this.disconnect(cx)))
            .on_action(cx.listener(|this, _: &CollapseAll, _, cx| this.collapse_all(cx)))
            .on_action(cx.listener(|this, _: &UploadRemoteCopy, window, cx| {
                // From the panel: the editor focused last, not the panel, owns the file.
                let buffer = this.workspace.upgrade().and_then(|workspace| {
                    workspace
                        .read(cx)
                        .active_item_as::<Editor>(cx)
                        .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
                });
                this.upload_active(buffer, window, cx);
            }))
            .size_full()
            .child(self.render_toolbar(window, cx))
            .when_some(self.status.clone(), |panel, status| {
                // Wrapped so a screen reader can reach the text (a bare `Label` has no id).
                panel.child(
                    div()
                        .id("remote-host-error")
                        .role(gpui::Role::Status)
                        .aria_label(SharedString::from(format!("Error: {status}")))
                        .debug_selector(|| "remote-host-error".into())
                        .child(
                            h_flex()
                                .px_2()
                                .gap_1()
                                .child(
                                    div().flex_none().child(
                                        Icon::new(IconName::Warning)
                                            .color(Color::Error)
                                            .size(IconSize::Small),
                                    ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "remote-host-error-text".into())
                                        .min_w_0()
                                        .child(
                                            Label::new(status)
                                                .color(Color::Error)
                                                .size(LabelSize::Small),
                                        ),
                                ),
                        ),
                )
            })
            .when_some(self.upload_line(cx), |panel, line| {
                panel.child(
                    div()
                        .id("remote-host-upload-status")
                        .role(gpui::Role::Status)
                        .aria_label(line.clone())
                        .debug_selector(|| "remote-host-upload-status".into())
                        .child(
                            h_flex()
                                .px_2()
                                .gap_1()
                                .child(
                                    div().flex_none().child(
                                        Icon::new(IconName::ArrowUp)
                                            .color(Color::Muted)
                                            .size(IconSize::Small),
                                    ),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "remote-host-upload-text".into())
                                        .min_w_0()
                                        .child(
                                            Label::new(line)
                                                .color(Color::Muted)
                                                .size(LabelSize::Small),
                                        ),
                                ),
                        ),
                )
            })
            .child(body)
            .when(self.server_id.is_some(), |panel| {
                panel.child(
                    div()
                        .id("remote-host-hint")
                        .role(gpui::Role::Label)
                        .aria_label(HINT)
                        .debug_selector(|| "remote-host-hint".into())
                        .px_2()
                        .py_1()
                        .child(Label::new(HINT).color(Color::Muted).size(LabelSize::XSmall)),
                )
            })
    }
}

const HINT: &str = "Files you open here are editable. Saving asks before uploading to the server, unless you chose Always.";

impl RemoteHostPanel {
    /// "Uploading…" while any copy of the shown server uploads, else the last result.
    fn upload_line(&self, cx: &App) -> Option<SharedString> {
        if self.is_uploading(cx) {
            Some("Uploading…".into())
        } else {
            self.notice.clone()
        }
    }
}

impl Focusable for RemoteHostPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RemoteHostPanel {}

impl Panel for RemoteHostPanel {
    fn persistent_name() -> &'static str {
        "Remote Host Panel"
    }

    fn panel_key() -> &'static str {
        "RemoteHostPanel"
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        DeploySettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    // ponytail: the dock comes from `zenith.deploy.dock`; dragging the panel does not write it.
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Remote Host")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active && self.rows.is_empty() && self.loading.is_empty() {
            // The dock calls this inside a workspace update; list after it.
            cx.defer_in(window, |this, _, cx| this.list_root(cx));
        }
    }

    fn activation_priority(&self) -> u32 {
        15
    }

    fn enabled(&self, cx: &App) -> bool {
        DeploySettings::get_global(cx).enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RemoteStat, Servers,
        test_support::{FakeRemote, init, server, workspace},
    };
    use gpui::{TestAppContext, VisualTestContext};
    use workspace::item::SerializableItem as _;

    struct Fixture<'a> {
        workspace: Entity<Workspace>,
        panel: Entity<RemoteHostPanel>,
        cx: &'a mut VisualTestContext,
        servers: Vec<ServerConfig>,
        cache: tempfile::TempDir,
    }

    fn remote() -> FakeRemote {
        FakeRemote::new(&[
            (
                "/www",
                &[
                    ("index.php", EntryKind::File),
                    ("wp-content", EntryKind::Directory),
                    ("vendor", EntryKind::Directory),
                    ("evil", EntryKind::Symlink),
                ],
            ),
            (
                "/www/wp-content",
                &[("themes", EntryKind::Directory), ("a.css", EntryKind::File)],
            ),
        ])
    }

    async fn fixture<'a>(remote: &FakeRemote, cx: &'a mut TestAppContext) -> Fixture<'a> {
        // Anonymous FTP: no Keychain lookup, so the fake sees every connect.
        let servers: Vec<_> = [
            server("Everywhere", None),
            server("Here", Some("/p")),
            server("Elsewhere", Some("/other")),
        ]
        .into_iter()
        .map(|mut server| {
            server.anonymous = true;
            server
        })
        .collect();
        let mut saved = Servers {
            servers: servers.clone(),
            ..Servers::default()
        };
        saved
            .project_defaults
            .insert("/p".into(), servers[1].id.clone());
        let cache = tempfile::tempdir().expect("tempdir");
        let app_state = init(cx, saved, remote, cache.path().to_path_buf());
        let (workspace, cx) = workspace(&app_state, cx).await;
        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<RemoteHostPanel>(cx))
            .expect("the panel is registered");
        Fixture {
            workspace,
            panel,
            cx,
            servers,
            cache,
        }
    }

    fn row_names(fixture: &mut Fixture) -> Vec<(String, usize)> {
        fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .rows
                .iter()
                .filter(|row| !row.placeholder)
                .map(|row| (row.entry.name.clone(), row.depth))
                .collect()
        })
    }

    fn row_index(fixture: &mut Fixture, name: &str) -> usize {
        fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .rows
                .iter()
                .position(|row| !row.placeholder && row.entry.name == name)
                .expect("row is shown")
        })
    }

    #[gpui::test]
    async fn dropdown_offers_none_and_this_projects_servers_and_starts_on_the_default(
        cx: &mut TestAppContext,
    ) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        let (options, selected) = fixture.panel.read_with(fixture.cx, |panel, cx| {
            let names: Vec<_> = panel
                .host_options(cx)
                .into_iter()
                .map(|server| server.name)
                .collect();
            (names, panel.server_id.clone())
        });
        assert_eq!(
            options,
            ["Everywhere", "Here"],
            "Elsewhere is limited to /other"
        );
        assert_eq!(selected, Some(fixture.servers[1].id.clone()));
        assert!(
            remote.log().is_empty(),
            "nothing connects before the panel is used"
        );

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(None, cx));
        fixture.cx.run_until_parked();
        assert_eq!(
            fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.server_id.clone()),
            None,
            "<None>"
        );
    }

    #[gpui::test]
    async fn tree_lists_a_folder_only_when_it_is_expanded(cx: &mut TestAppContext) {
        let remote = remote();
        let mut fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        assert_eq!(
            remote.log(),
            ["connect everywhere.example.com password=None", "list /www"]
        );
        assert_eq!(
            row_names(&mut fixture),
            [
                ("vendor".to_string(), 0),
                ("wp-content".to_string(), 0),
                ("evil".to_string(), 0),
                ("index.php".to_string(), 0)
            ]
        );

        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        assert_eq!(
            remote.log().last().map(String::as_str),
            Some("list /www/wp-content")
        );
        assert!(row_names(&mut fixture).contains(&("themes".to_string(), 1)));

        // Collapsing and expanding again uses the listing already fetched.
        for _ in 0..2 {
            let wp_content = row_index(&mut fixture, "wp-content");
            fixture.panel.update_in(fixture.cx, |panel, window, cx| {
                panel.activate_row(wp_content, window, cx)
            });
            fixture.cx.run_until_parked();
        }
        let lists: Vec<_> = remote
            .log()
            .into_iter()
            .filter(|line| line.starts_with("list"))
            .collect();
        assert_eq!(
            lists,
            ["list /www", "list /www/wp-content"],
            "vendor is never listed"
        );

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.collapse_all(cx));
        assert!(!row_names(&mut fixture).contains(&("themes".to_string(), 1)));

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.disconnect(cx));
        fixture.cx.run_until_parked();
        assert_eq!(remote.log().last().map(String::as_str), Some("disconnect"));
        assert!(row_names(&mut fixture).is_empty());
    }

    #[gpui::test]
    async fn a_refused_folder_keeps_the_session_and_the_rest_of_the_tree(cx: &mut TestAppContext) {
        let remote = remote();
        remote
            .refusing
            .lock()
            .expect("lock")
            .push("/www/wp-content".into());
        let mut fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        remote.refusing.lock().expect("lock").clear();
        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        let connects = remote
            .log()
            .iter()
            .filter(|line| line.starts_with("connect"))
            .count();
        assert_eq!(connects, 1, "a refusal keeps the session");
        let names = row_names(&mut fixture);
        assert!(names.contains(&("index.php".to_string(), 0)), "{names:?}");
        assert!(names.contains(&("themes".to_string(), 1)), "{names:?}");
    }

    #[gpui::test]
    async fn disconnect_deletes_the_servers_cached_copies(cx: &mut TestAppContext) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        let cached = fixture.cache.path().join(&id);
        std::fs::create_dir_all(&cached).expect("mkdir");
        std::fs::write(cached.join("wp-config.php"), "<?php").expect("write");
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.disconnect(cx));
        fixture.cx.run_until_parked();
        assert!(!cached.exists());
    }

    #[gpui::test]
    async fn a_failed_refresh_of_the_root_keeps_the_tree_and_shows_the_error(
        cx: &mut TestAppContext,
    ) {
        let remote = remote();
        let mut fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        remote.refusing.lock().expect("lock").push("/www".into());
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.refresh(cx));
        fixture.cx.run_until_parked();
        assert!(row_names(&mut fixture).contains(&("index.php".to_string(), 0)));
        let status = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.status.clone());
        assert!(status.is_some_and(|status| status.contains("550")));
    }

    #[gpui::test]
    async fn a_failed_refresh_drops_the_stale_listing(cx: &mut TestAppContext) {
        let remote = remote();
        let mut fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        assert!(row_names(&mut fixture).contains(&("themes".to_string(), 1)));

        remote
            .refusing
            .lock()
            .expect("lock")
            .push("/www/wp-content".into());
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.refresh(cx));
        fixture.cx.run_until_parked();
        let (expanded, listed) = fixture.panel.read_with(fixture.cx, |panel, _| {
            (
                panel.expanded.contains("/www/wp-content"),
                panel.children.contains_key("/www/wp-content"),
            )
        });
        assert!(!expanded && !listed, "expanded={expanded} listed={listed}");

        remote.refusing.lock().expect("lock").clear();
        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        assert!(
            row_names(&mut fixture).contains(&("themes".to_string(), 1)),
            "re-expanding lists the folder again"
        );
    }

    const INDEX: &str = "/www/index.php";

    /// A server the fixture can address by id and the shared state of one opened copy.
    struct Opened {
        editor: Entity<Editor>,
        buffer: Entity<Buffer>,
        cached: PathBuf,
        server: ServerConfig,
    }

    /// Selects the first server, mirrors the cache file onto the project's fake file system
    /// (the panel writes the real cache) and opens `index.php` through its row.
    async fn open_index(fixture: &mut Fixture<'_>) -> Opened {
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        fixture.cx.run_until_parked();
        let cached = cache_path_for(fixture.cache.path(), &server.id, INDEX).expect("cache path");
        mirror_cache(fixture, &cached).await;
        click_row(fixture, "index.php");
        wait_for_editor(fixture).await;
        let editor = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx)
            })
            .expect("the file opens in an editor");
        let buffer = editor.read_with(fixture.cx, |editor, cx| {
            editor.buffer().read(cx).as_singleton().expect("singleton")
        });
        Opened {
            editor,
            buffer,
            cached,
            server,
        }
    }

    async fn mirror_cache(fixture: &Fixture<'_>, cached: &Path) {
        let fs = fixture.workspace.read_with(fixture.cx, |workspace, cx| {
            workspace.project().read(cx).fs().clone()
        });
        fs.as_fake()
            .insert_tree(
                cached.parent().expect("parent"),
                serde_json::json!({ "index.php": "<?php" }),
            )
            .await;
    }

    fn click_row(fixture: &mut Fixture<'_>, name: &str) {
        let index = row_index(fixture, name);
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(index, window, cx)
        });
    }

    /// The download writes the real cache on smol's blocking pool, outside the test scheduler:
    /// give it real time.
    async fn wait_for_editor(fixture: &mut Fixture<'_>) {
        fixture.cx.executor().allow_parking();
        for _ in 0..200 {
            fixture.cx.run_until_parked();
            let attached = fixture.panel.read_with(fixture.cx, |panel, _| {
                panel
                    .owned
                    .values()
                    .any(|copy| copy.buffer.upgrade().is_some())
            });
            if attached {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fixture.cx.run_until_parked();
    }

    fn store(fixture: &mut Fixture<'_>) -> Entity<ServerStore> {
        fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.store.clone())
    }

    fn set_decision(fixture: &mut Fixture<'_>, opened: &Opened, decision: UploadDecision) {
        let store = store(fixture);
        store.update(fixture.cx, |store, _| {
            store.set_upload_decision(&opened.server.id, decision)
        });
    }

    /// Replaces the text and saves through the project, like Save does.
    async fn edit_and_save(fixture: &mut Fixture<'_>, opened: &Opened, text: &str) {
        edit(fixture, opened, text);
        let project = fixture
            .workspace
            .read_with(fixture.cx, |workspace, _| workspace.project().clone());
        let buffer = opened.buffer.clone();
        project
            .update(fixture.cx, |project, cx| project.save_buffer(buffer, cx))
            .await
            .expect("saved");
        fixture.cx.run_until_parked();
    }

    fn edit(fixture: &mut Fixture<'_>, opened: &Opened, text: &str) {
        opened.editor.update_in(fixture.cx, |editor, window, cx| {
            editor.set_text(text.to_string(), window, cx)
        });
    }

    fn puts(remote: &FakeRemote) -> Vec<(String, Vec<u8>)> {
        remote.puts.lock().expect("lock").clone()
    }

    fn remote_with_baseline() -> FakeRemote {
        let remote = remote();
        remote.stats.lock().expect("lock").insert(
            INDEX.into(),
            RemoteStat {
                size: 5,
                stamp: Some("s1".into()),
            },
        );
        remote
    }

    fn gets(remote: &FakeRemote) -> usize {
        remote
            .log()
            .iter()
            .filter(|line| line.starts_with("get "))
            .count()
    }

    #[gpui::test]
    async fn opening_a_remote_file_opens_an_editable_attached_copy(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;

        let log = remote.log();
        let stat = log.iter().position(|line| line == "stat /www/index.php");
        let get = log.iter().position(|line| line == "get /www/index.php");
        assert!(
            stat < get && get.is_some(),
            "stat before the download: {log:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&opened.cached).expect("cached copy"),
            "<?php"
        );
        assert!(
            !opened
                .editor
                .read_with(fixture.cx, |editor, cx| editor.read_only(cx)),
            "the copy is editable"
        );
        assert!(
            !opened.editor.read_with(fixture.cx, |editor, _| {
                editor.should_serialize(&editor::EditorEvent::Saved)
            }),
            "hot exit must not store the text"
        );
        let store = store(&mut fixture);
        let (attached, baseline) = store.read_with(fixture.cx, |store, _| {
            let copy = store.copy_for_buffer_path(&opened.cached).expect("indexed");
            (
                copy.buffers
                    .iter()
                    .any(|weak| weak.entity_id() == opened.buffer.entity_id()),
                copy.stat.clone(),
            )
        });
        assert!(attached);
        assert_eq!(
            baseline,
            Some(RemoteStat {
                size: 5,
                stamp: Some("s1".into())
            }),
            "the stat taken before the download is the baseline"
        );
        assert_eq!(
            fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.status.clone()),
            None
        );

        click_row(&mut fixture, "evil");
        fixture.cx.run_until_parked();
        assert!(
            !remote.log().iter().any(|line| line == "get /www/evil"),
            "symlinks are never downloaded"
        );
    }

    #[gpui::test]
    async fn a_save_with_allow_uploads_exactly_the_saved_bytes_and_records_the_baseline(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        set_decision(&mut fixture, &opened, UploadDecision::Allow);

        edit_and_save(&mut fixture, &opened, "<?php echo 1;").await;
        assert_eq!(
            puts(&remote),
            [(INDEX.to_string(), b"<?php echo 1;".to_vec())]
        );
        let store = store(&mut fixture);
        let baseline = store.read_with(fixture.cx, |store, _| {
            store
                .copy_for_buffer_path(&opened.cached)
                .and_then(|copy| copy.stat.clone())
        });
        assert_eq!(
            baseline,
            Some(RemoteStat {
                size: 13,
                stamp: Some("put-1".into())
            })
        );
        let notice = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.notice.clone());
        assert_eq!(
            notice.as_deref(),
            Some("Uploaded index.php to Everywhere"),
            "no toast, a status line"
        );
        // The second save compares with the new baseline: no conflict.
        edit_and_save(&mut fixture, &opened, "<?php echo 2;").await;
        assert_eq!(puts(&remote).len(), 2);
    }

    #[gpui::test]
    async fn a_save_with_ask_uploads_nothing_until_a_button_consents(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;

        edit_and_save(&mut fixture, &opened, "one").await;
        assert!(fixture.cx.has_pending_prompt(), "the first save asks");
        let (message, detail) = fixture.cx.pending_prompt().expect("prompt");
        assert!(message.contains("Everywhere") && message.contains("everywhere.example.com"));
        assert!(detail.contains("same second"), "{detail}");
        assert!(puts(&remote).is_empty());

        fixture.cx.simulate_prompt_answer("Cancel");
        fixture.cx.run_until_parked();
        assert!(puts(&remote).is_empty(), "saved locally only");

        edit_and_save(&mut fixture, &opened, "two").await;
        fixture.cx.simulate_prompt_answer("Upload");
        fixture.cx.run_until_parked();
        assert_eq!(puts(&remote), [(INDEX.to_string(), b"two".to_vec())]);
        let store = store(&mut fixture);
        assert_eq!(
            store.read_with(fixture.cx, |store, _| store
                .upload_decision(&opened.server.id)),
            UploadDecision::Ask,
            "Upload is for this save only"
        );

        edit_and_save(&mut fixture, &opened, "three").await;
        assert!(fixture.cx.has_pending_prompt(), "asks again");
        fixture
            .cx
            .simulate_prompt_answer("Always upload to this server until I quit");
        fixture.cx.run_until_parked();
        assert_eq!(puts(&remote).len(), 2, "the answer uploads this save too");
        edit_and_save(&mut fixture, &opened, "four").await;
        assert!(!fixture.cx.has_pending_prompt());
        assert_eq!(puts(&remote).len(), 3);
    }

    #[gpui::test]
    async fn never_stops_uploads_for_the_session(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        edit_and_save(&mut fixture, &opened, "one").await;
        fixture
            .cx
            .simulate_prompt_answer("Never upload to this server until I quit");
        fixture.cx.run_until_parked();
        edit_and_save(&mut fixture, &opened, "two").await;
        assert!(!fixture.cx.has_pending_prompt());
        assert!(puts(&remote).is_empty());
        let store = store(&mut fixture);
        assert_eq!(
            store.read_with(fixture.cx, |store, _| store
                .upload_decision(&opened.server.id)),
            UploadDecision::Deny
        );
    }

    #[gpui::test]
    async fn a_conflict_uploads_only_after_overwrite(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        set_decision(&mut fixture, &opened, UploadDecision::Allow);
        // Someone else changed the file on the server after the download.
        remote.stats.lock().expect("lock").insert(
            INDEX.into(),
            RemoteStat {
                size: 9,
                stamp: Some("other".into()),
            },
        );

        edit_and_save(&mut fixture, &opened, "mine").await;
        let (message, _) = fixture.cx.pending_prompt().expect("conflict prompt");
        assert!(message.contains("changed on Everywhere"), "{message}");
        assert!(puts(&remote).is_empty());
        fixture.cx.simulate_prompt_answer("Cancel");
        fixture.cx.run_until_parked();
        assert!(puts(&remote).is_empty(), "Cancel uploads nothing");

        edit_and_save(&mut fixture, &opened, "mine again").await;
        fixture.cx.simulate_prompt_answer("Overwrite");
        fixture.cx.run_until_parked();
        assert_eq!(puts(&remote), [(INDEX.to_string(), b"mine again".to_vec())]);
    }

    #[gpui::test]
    async fn an_unverifiable_remote_asks_before_uploading_anyway(cx: &mut TestAppContext) {
        let remote = remote(); // no stat for the file: no baseline
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        set_decision(&mut fixture, &opened, UploadDecision::Allow);

        edit_and_save(&mut fixture, &opened, "x").await;
        let (message, _) = fixture.cx.pending_prompt().expect("ask-first prompt");
        assert!(message.contains("cannot be verified"), "{message}");
        assert!(puts(&remote).is_empty());
        fixture.cx.simulate_prompt_answer("Upload anyway");
        fixture.cx.run_until_parked();
        assert_eq!(puts(&remote), [(INDEX.to_string(), b"x".to_vec())]);
    }

    #[gpui::test]
    async fn a_restored_copy_that_is_not_in_the_index_stays_read_only_and_never_uploads(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        fixture.cx.run_until_parked();
        let cached = cache_path_for(fixture.cache.path(), &server.id, INDEX).expect("cache path");
        mirror_cache(&fixture, &cached).await;
        // What hot exit does: a buffer at the cache path, with nothing in the index.
        let restored = fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    cached.clone(),
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..OpenOptions::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .expect("opened");
        let editor = restored.downcast::<Editor>().expect("editor");
        let store = store(&mut fixture);
        let orphan = store.read_with(fixture.cx, |store, _| store.is_orphan(&cached));
        assert!(orphan);

        click_row(&mut fixture, "index.php");
        fixture.cx.run_until_parked();
        assert_eq!(
            gets(&remote),
            0,
            "a fresh download would hide the stale text"
        );
        assert!(editor.read_with(fixture.cx, |editor, cx| editor.read_only(cx)));
        let buffer = editor.read_with(fixture.cx, |editor, cx| {
            editor.buffer().read(cx).as_singleton().expect("singleton")
        });
        store.update(fixture.cx, |store, _| {
            store.set_upload_decision(&server.id, UploadDecision::Allow)
        });
        let project = fixture
            .workspace
            .read_with(fixture.cx, |workspace, _| workspace.project().clone());
        buffer.update(fixture.cx, |buffer, cx| buffer.set_text("stale", cx));
        project
            .update(fixture.cx, |project, cx| project.save_buffer(buffer, cx))
            .await
            .expect("saved");
        fixture.cx.run_until_parked();
        assert!(puts(&remote).is_empty());
    }

    #[gpui::test]
    async fn saving_a_buffer_outside_the_cache_uploads_nothing(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        set_decision(&mut fixture, &opened, UploadDecision::Allow);
        let project = fixture
            .workspace
            .read_with(fixture.cx, |workspace, _| workspace.project().clone());
        let path = project.read_with(fixture.cx, |project, cx| {
            project
                .find_project_path("/p/a.txt", cx)
                .expect("project file")
        });
        let local = project
            .update(fixture.cx, |project, cx| project.open_buffer(path, cx))
            .await
            .expect("opened");
        local.update(fixture.cx, |buffer, cx| buffer.set_text("local", cx));
        project
            .update(fixture.cx, |project, cx| project.save_buffer(local, cx))
            .await
            .expect("saved");
        fixture.cx.run_until_parked();
        assert!(puts(&remote).is_empty());
        assert!(!remote.log().iter().any(|line| line.starts_with("put")));
    }

    #[gpui::test]
    async fn opening_an_open_copy_again_does_not_download_and_focuses_the_tab(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        assert_eq!(gets(&remote), 1);
        // Unsaved edits are in the buffer; a second download would replace their baseline.
        edit(&mut fixture, &opened, "unsaved");
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/p/a.txt"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("another tab");
        let other = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx)
            })
            .expect("editor");
        assert_ne!(
            other.entity_id(),
            opened.editor.entity_id(),
            "another tab is active"
        );

        click_row(&mut fixture, "index.php");
        fixture.cx.run_until_parked();
        assert_eq!(gets(&remote), 1, "no second download");
        let active = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx)
            })
            .expect("editor");
        assert_eq!(
            active.entity_id(),
            opened.editor.entity_id(),
            "the tab is focused"
        );
        assert_eq!(
            opened
                .buffer
                .read_with(fixture.cx, |buffer, _| buffer.text()),
            "unsaved"
        );
    }

    #[gpui::test]
    async fn disconnect_keeps_the_cache_of_a_dirty_copy(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        edit(&mut fixture, &opened, "dirty");
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.disconnect(cx));
        fixture.cx.run_until_parked();
        assert!(opened.cached.exists(), "the open copy's file stays");
        let status = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.status.clone());
        assert_eq!(status, None, "not an error");
        let notice = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.notice.clone());
        assert_eq!(
            notice.as_deref(),
            Some("Disconnected. Kept 1 open file with unsaved changes on this computer.")
        );
    }

    #[gpui::test]
    async fn the_upload_action_uploads_the_saved_text_of_the_active_remote_copy(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        edit_and_save(&mut fixture, &opened, "kept local").await;
        fixture.cx.simulate_prompt_answer("Cancel");
        fixture.cx.run_until_parked();
        assert!(puts(&remote).is_empty());

        // The editor has focus: the action reaches the workspace through the key path.
        opened.editor.update_in(fixture.cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx)
        });
        fixture.cx.dispatch_action(UploadRemoteCopy);
        fixture.cx.run_until_parked();
        assert!(fixture.cx.has_pending_prompt(), "still asks first");
        fixture.cx.simulate_prompt_answer("Upload");
        fixture.cx.run_until_parked();
        assert_eq!(puts(&remote), [(INDEX.to_string(), b"kept local".to_vec())]);

        // Another file is not a remote copy: nothing is asked or sent.
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/p/a.txt"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("another tab");
        fixture.cx.dispatch_action(UploadRemoteCopy);
        fixture.cx.run_until_parked();
        assert!(!fixture.cx.has_pending_prompt());
        assert_eq!(puts(&remote).len(), 1);
    }

    #[gpui::test]
    async fn closing_the_last_tab_finishes_a_disconnect_that_kept_the_copy(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let Opened {
            editor,
            buffer,
            cached,
            ..
        } = open_index(&mut fixture).await;
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.disconnect(cx));
        fixture.cx.run_until_parked();
        assert!(cached.exists(), "kept while the tab is open");

        drop((editor, buffer));
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                })
            })
            .await
            .expect("closed");
        fixture.cx.run_until_parked();
        // The release callbacks run at the next effect flush; the app flushes constantly.
        fixture.cx.update(|_, _| ());
        fixture.cx.run_until_parked();
        assert!(!cached.exists(), "the release finishes the Disconnect");
    }

    /// Runs `step` (with real time for the smol pool) until `done` holds.
    async fn poll_until(fixture: &mut Fixture<'_>, mut done: impl FnMut(&mut Fixture<'_>) -> bool) {
        fixture.cx.executor().allow_parking();
        for _ in 0..300 {
            fixture.cx.run_until_parked();
            if done(fixture) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the condition never held");
    }

    /// Closes the copy's tab and drops every handle to its buffer.
    async fn close_tab(fixture: &mut Fixture<'_>, opened: Opened) -> (PathBuf, ServerConfig) {
        let Opened {
            editor,
            buffer,
            cached,
            server,
        } = opened;
        drop((editor, buffer));
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
                })
            })
            .await
            .expect("closed");
        fixture.cx.update(|_, _| ());
        fixture.cx.run_until_parked();
        (cached, server)
    }

    fn incoming_leftovers(cached: &Path) -> Vec<String> {
        std::fs::read_dir(cached.parent().expect("parent"))
            .expect("read_dir")
            .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|name| name != "index.php")
            .collect()
    }

    /// Closes the tab, puts `saved` in the cache file (a copy saved locally, never uploaded),
    /// changes the server file and clicks the row; returns when the question is up.
    async fn reopen_with_saved_copy(
        fixture: &mut Fixture<'_>,
        remote: &FakeRemote,
        saved: &str,
    ) -> PathBuf {
        let opened = open_index(fixture).await;
        let (cached, _) = close_tab(fixture, opened).await;
        std::fs::write(&cached, saved).expect("saved copy");
        remote.stats.lock().expect("lock").insert(
            INDEX.into(),
            RemoteStat {
                size: 9,
                stamp: Some("s2".into()),
            },
        );
        click_row(fixture, "index.php");
        poll_until(fixture, |fixture| fixture.cx.has_pending_prompt()).await;
        cached
    }

    async fn editor_opens(fixture: &mut Fixture<'_>) {
        poll_until(fixture, |fixture| {
            fixture.panel.read_with(fixture.cx, |panel, _| {
                panel
                    .owned
                    .values()
                    .any(|copy| copy.buffer.upgrade().is_some())
            })
        })
        .await;
    }

    #[gpui::test]
    async fn reopening_asks_before_replacing_a_saved_copy_and_keep_leaves_it_with_the_old_baseline(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let cached = reopen_with_saved_copy(&mut fixture, &remote, "mine").await;
        let (message, detail) = fixture.cx.pending_prompt().expect("prompt");
        assert!(
            message.contains("index.php") && message.contains("differs"),
            "{message}"
        );
        assert!(detail.contains("never uploaded"), "{detail}");
        assert_eq!(
            std::fs::read_to_string(&cached).expect("cache"),
            "mine",
            "the saved copy is untouched while the question is open"
        );

        fixture.cx.simulate_prompt_answer("Keep my saved copy");
        editor_opens(&mut fixture).await;
        assert_eq!(std::fs::read_to_string(&cached).expect("cache"), "mine");
        assert!(
            incoming_leftovers(&cached).is_empty(),
            "{:?}",
            incoming_leftovers(&cached)
        );
        let store = store(&mut fixture);
        let baseline = store.read_with(fixture.cx, |store, _| {
            store
                .copy_for_buffer_path(&cached)
                .and_then(|copy| copy.stat.clone())
        });
        assert_eq!(
            baseline,
            Some(RemoteStat {
                size: 5,
                stamp: Some("s1".into())
            }),
            "your version is compared with the server as it was when you got it"
        );
    }

    #[gpui::test]
    async fn reopening_with_use_the_servers_version_replaces_the_copy(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let cached = reopen_with_saved_copy(&mut fixture, &remote, "mine").await;
        fixture
            .cx
            .simulate_prompt_answer("Use the server's version");
        editor_opens(&mut fixture).await;
        assert_eq!(std::fs::read_to_string(&cached).expect("cache"), "<?php");
        assert!(incoming_leftovers(&cached).is_empty());
        let store = store(&mut fixture);
        let stamp = store.read_with(fixture.cx, |store, _| {
            store
                .copy_for_buffer_path(&cached)
                .and_then(|copy| copy.stat.clone())
                .and_then(|stat| stat.stamp)
        });
        assert_eq!(stamp.as_deref(), Some("s2"));
    }

    #[gpui::test]
    async fn cancelling_the_reopen_question_keeps_the_copy_and_opens_nothing(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let cached = reopen_with_saved_copy(&mut fixture, &remote, "mine").await;
        fixture.cx.simulate_prompt_answer("Cancel");
        poll_until(&mut fixture, |_| incoming_leftovers(&cached).is_empty()).await;
        assert_eq!(std::fs::read_to_string(&cached).expect("cache"), "mine");
        let open = fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .owned
                .values()
                .any(|copy| copy.buffer.upgrade().is_some())
        });
        assert!(!open);
        assert!(
            !fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.prompt_open.get())
        );
    }

    #[gpui::test]
    async fn reopening_an_unchanged_copy_asks_nothing(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        let (cached, _) = close_tab(&mut fixture, opened).await;
        click_row(&mut fixture, "index.php");
        editor_opens(&mut fixture).await;
        assert!(!fixture.cx.has_pending_prompt());
        assert_eq!(std::fs::read_to_string(&cached).expect("cache"), "<?php");
        assert!(incoming_leftovers(&cached).is_empty());
    }

    #[gpui::test]
    async fn a_download_into_a_claimed_path_is_refused_and_the_claim_ends_with_its_guard(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        fixture.cx.run_until_parked();
        let cached = cache_path_for(fixture.cache.path(), &server.id, INDEX).expect("cache path");
        mirror_cache(&fixture, &cached).await;
        let store = store(&mut fixture);
        // Another window is in the middle of downloading this file.
        let claim = store.read_with(fixture.cx, |store, _| store.reserve_download(&cached));
        assert!(claim.is_some());
        assert!(
            store.read_with(fixture.cx, |store, _| store
                .reserve_download(&cached)
                .is_none()),
            "a second claim is refused"
        );

        click_row(&mut fixture, "index.php");
        fixture.cx.run_until_parked();
        assert_eq!(gets(&remote), 0, "no second download into the claimed path");
        let status = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.status.clone());
        assert!(status.is_some_and(|status| status.contains("already being opened")));

        drop(claim);
        click_row(&mut fixture, "index.php");
        editor_opens(&mut fixture).await;
        assert_eq!(gets(&remote), 1);
        assert!(
            store.read_with(fixture.cx, |store, _| store
                .reserve_download(&cached)
                .is_none()),
            "an open copy cannot be claimed either"
        );
    }

    #[gpui::test]
    async fn every_editor_of_a_cached_copy_stays_out_of_the_editor_database(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        // Splitting a pane clones the editor with serialization on.
        let split = opened.editor.update_in(fixture.cx, |editor, window, cx| {
            cx.new(|cx| editor.clone(window, cx))
        });
        assert!(
            !split.read_with(fixture.cx, |editor, _| {
                editor.should_serialize(&editor::EditorEvent::Saved)
            }),
            "the clone must not store dirty text"
        );
        // A file outside the cache still serializes.
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/p/a.txt"),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("another tab");
        let other = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx)
            })
            .expect("editor");
        assert!(other.read_with(fixture.cx, |editor, _| {
            editor.should_serialize(&editor::EditorEvent::Saved)
        }));
    }

    #[gpui::test]
    async fn an_answer_is_recorded_only_for_the_host_the_prompt_named(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        edit_and_save(&mut fixture, &opened, "one").await;
        assert!(fixture.cx.has_pending_prompt());
        // The server's address changes while the question is up.
        let store = store(&mut fixture);
        store.update(fixture.cx, |store, _| {
            store.servers.servers[0].host = "elsewhere.example.net".into();
        });
        fixture
            .cx
            .simulate_prompt_answer("Always upload to this server until I quit");
        fixture.cx.run_until_parked();
        assert_eq!(
            store.read_with(fixture.cx, |store, _| store
                .upload_decision(&opened.server.id)),
            UploadDecision::Ask,
            "nothing is remembered for the new host"
        );
        assert!(puts(&remote).is_empty());
    }

    #[gpui::test]
    async fn a_second_question_while_a_prompt_is_up_does_not_replace_it(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        edit_and_save(&mut fixture, &opened, "one").await;
        edit_and_save(&mut fixture, &opened, "two").await;
        fixture.cx.simulate_prompt_answer("Cancel");
        fixture.cx.run_until_parked();
        assert!(
            !fixture.cx.has_pending_prompt(),
            "the second save became a toast, not another prompt"
        );
        assert!(puts(&remote).is_empty());
        let open = fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.prompt_open.get());
        assert!(!open);
    }

    #[gpui::test]
    async fn the_hint_and_the_upload_line_are_elements_a_screen_reader_can_reach(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.focus_panel::<RemoteHostPanel>(window, cx);
            });
        set_decision(&mut fixture, &opened, UploadDecision::Allow);
        edit_and_save(&mut fixture, &opened, "x").await;
        fixture.cx.run_until_parked();
        for selector in ["remote-host-hint", "remote-host-upload-status"] {
            assert!(fixture.cx.debug_bounds(selector).is_some(), "{selector}");
        }
    }

    fn tree_with_an_empty_folder() -> FakeRemote {
        FakeRemote::new(&[
            (
                "/www",
                &[
                    ("empty", EntryKind::Directory),
                    ("full", EntryKind::Directory),
                    ("z.php", EntryKind::File),
                ],
            ),
            ("/www/empty", &[]),
            ("/www/full", &[("a.css", EntryKind::File)]),
        ])
    }

    /// Connects to the first server and lists the root of `tree_with_an_empty_folder`.
    async fn connect(fixture: &mut Fixture<'_>) {
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.focus_panel::<RemoteHostPanel>(window, cx);
            });
        fixture.cx.run_until_parked();
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        fixture.cx.run_until_parked();
    }

    /// (folder, depth) of every "Empty" placeholder row.
    fn placeholders(fixture: &mut Fixture<'_>) -> Vec<(String, usize)> {
        fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .rows
                .iter()
                .filter(|row| row.placeholder)
                .map(|row| (row.directory.clone(), row.depth))
                .collect()
        })
    }

    fn selected(fixture: &mut Fixture<'_>) -> Option<usize> {
        fixture
            .panel
            .read_with(fixture.cx, |panel, _| panel.selected)
    }

    #[gpui::test]
    async fn an_empty_expanded_folder_shows_one_empty_row_under_it(cx: &mut TestAppContext) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        assert!(placeholders(&mut fixture).is_empty(), "nothing is expanded");

        click_row(&mut fixture, "empty");
        fixture.cx.run_until_parked();
        assert_eq!(
            placeholders(&mut fixture),
            [("/www/empty".to_string(), 1)],
            "one level deeper than the folder"
        );
        fixture.panel.read_with(fixture.cx, |panel, _| {
            let kinds: Vec<_> = panel
                .rows
                .iter()
                .map(|row| (row.entry.name.as_str(), row.placeholder))
                .collect();
            assert_eq!(
                kinds,
                [
                    ("empty", false),
                    ("Empty", true),
                    ("full", false),
                    ("z.php", false)
                ]
            );
        });
        assert!(fixture.cx.debug_bounds("remote-host-empty").is_some());
    }

    #[gpui::test]
    async fn a_folder_with_entries_has_no_empty_row(cx: &mut TestAppContext) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        click_row(&mut fixture, "full");
        fixture.cx.run_until_parked();
        assert!(row_names(&mut fixture).contains(&("a.css".to_string(), 1)));
        assert!(placeholders(&mut fixture).is_empty());
        assert!(fixture.cx.debug_bounds("remote-host-empty").is_none());
    }

    #[gpui::test]
    async fn a_folder_still_loading_has_no_empty_row(cx: &mut TestAppContext) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        click_row(&mut fixture, "empty");
        assert!(
            placeholders(&mut fixture).is_empty(),
            "the listing has not come back yet"
        );
        fixture.cx.run_until_parked();
        assert_eq!(placeholders(&mut fixture).len(), 1);

        // A re-list keeps the old (empty) listing but is loading again: "Loading…", not "Empty".
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.refresh(cx));
        fixture.panel.update(fixture.cx, |panel, cx| {
            let loading = panel.loading.contains("/www/empty");
            assert!(loading, "the folder is being listed again");
            panel.rebuild_rows(cx);
        });
        assert!(placeholders(&mut fixture).is_empty());
        fixture.cx.run_until_parked();
        assert_eq!(placeholders(&mut fixture).len(), 1);
    }

    #[gpui::test]
    async fn collapsing_hides_the_empty_row(cx: &mut TestAppContext) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        click_row(&mut fixture, "empty");
        fixture.cx.run_until_parked();
        assert_eq!(placeholders(&mut fixture).len(), 1);

        click_row(&mut fixture, "empty");
        assert!(placeholders(&mut fixture).is_empty(), "collapsed");
        assert!(fixture.cx.debug_bounds("remote-host-empty").is_none());

        click_row(&mut fixture, "empty");
        assert_eq!(placeholders(&mut fixture).len(), 1, "expanded again");
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.collapse_all(cx));
        assert!(placeholders(&mut fixture).is_empty(), "collapse all");
    }

    #[gpui::test]
    async fn the_empty_row_cannot_be_selected_clicked_or_stepped_onto(cx: &mut TestAppContext) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        click_row(&mut fixture, "empty");
        fixture.cx.run_until_parked();
        let folder = row_index(&mut fixture, "empty");
        assert_eq!(selected(&mut fixture), Some(folder));
        let placeholder = folder + 1;
        let lists = remote.log().len();

        // Enter on it (a stale selection) and a mouse click do nothing.
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(placeholder, window, cx)
        });
        let bounds = fixture
            .cx
            .debug_bounds("remote-host-empty")
            .expect("the empty row is drawn");
        fixture
            .cx
            .simulate_click(bounds.center(), gpui::Modifiers::none());
        fixture.cx.run_until_parked();
        assert_eq!(selected(&mut fixture), Some(folder));
        assert_eq!(placeholders(&mut fixture).len(), 1);
        assert_eq!(remote.log().len(), lists, "nothing was listed or opened");
        fixture.panel.read_with(fixture.cx, |panel, _| {
            assert!(panel.expanded.contains("/www/empty"), "still expanded")
        });

        // The arrow keys step over it.
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx)
        });
        assert_eq!(
            selected(&mut fixture),
            Some(row_index(&mut fixture, "full"))
        );
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.select_previous(&menu::SelectPrevious, window, cx)
        });
        assert_eq!(selected(&mut fixture), Some(folder));

        // A selection index that lands on it after a rebuild moves to the folder.
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.selected = Some(placeholder);
            panel.rebuild_rows(cx);
        });
        assert_eq!(selected(&mut fixture), Some(folder));
    }

    #[gpui::test]
    async fn a_folder_that_becomes_empty_after_a_refresh_gets_the_empty_row(
        cx: &mut TestAppContext,
    ) {
        let remote = tree_with_an_empty_folder();
        let mut fixture = fixture(&remote, cx).await;
        connect(&mut fixture).await;
        click_row(&mut fixture, "full");
        fixture.cx.run_until_parked();
        assert!(placeholders(&mut fixture).is_empty());

        remote
            .emptied
            .lock()
            .expect("lock")
            .push("/www/full".into());
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.refresh(cx));
        fixture.cx.run_until_parked();
        assert_eq!(placeholders(&mut fixture), [("/www/full".to_string(), 1)]);
        assert!(!row_names(&mut fixture).contains(&("a.css".to_string(), 1)));
    }

    #[gpui::test]
    async fn a_long_error_or_upload_line_stays_inside_the_panel(cx: &mut TestAppContext) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.focus_panel::<RemoteHostPanel>(window, cx);
            });
        fixture.cx.run_until_parked();
        let long = "Connection failed: the server closed the connection while listing a folder, "
            .repeat(4);
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.status = Some(long.clone().into());
            panel.notice = Some(long.into());
            cx.notify();
        });
        fixture.cx.run_until_parked();
        let row = fixture
            .cx
            .debug_bounds("remote-host-error")
            .expect("the error row is drawn");
        for selector in ["remote-host-error-text", "remote-host-upload-text"] {
            let text = fixture
                .cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is drawn"));
            assert!(
                text.right() <= row.right(),
                "{selector} right {:?} (width {:?}) is past the panel edge {:?}",
                text.right(),
                text.size.width,
                row.right()
            );
        }
    }

    #[gpui::test]
    async fn a_reset_frees_the_prompt_slot_and_removes_the_waiting_download(
        cx: &mut TestAppContext,
    ) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let cached = reopen_with_saved_copy(&mut fixture, &remote, "mine").await;
        assert!(
            fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.prompt_open.get())
        );
        assert_eq!(
            incoming_leftovers(&cached).len(),
            1,
            "the server's bytes wait beside the cache while the question is up"
        );

        // Another window edits the server: this panel resets and drops the task that waits.
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.reset(cx));
        // A dropped task releases its future (and what it holds) when the executor next runs it.
        fixture.cx.run_until_parked();
        assert!(
            !fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.prompt_open.get()),
            "the slot is freed with the task"
        );
        assert!(
            incoming_leftovers(&cached).is_empty(),
            "{:?}",
            incoming_leftovers(&cached)
        );

        // The next question still appears.
        fixture.cx.simulate_prompt_answer("Cancel"); // the stale prompt on screen
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        poll_until(&mut fixture, |fixture| !row_names(fixture).is_empty()).await;
        click_row(&mut fixture, "index.php");
        poll_until(&mut fixture, |fixture| fixture.cx.has_pending_prompt()).await;
        let (message, _) = fixture.cx.pending_prompt().expect("prompt");
        assert!(message.contains("differs"), "{message}");
    }

    #[gpui::test]
    async fn keeping_a_saved_copy_does_not_reuse_a_stat_from_another_host(cx: &mut TestAppContext) {
        let remote = remote_with_baseline();
        let mut fixture = fixture(&remote, cx).await;
        let opened = open_index(&mut fixture).await;
        let (cached, server) = close_tab(&mut fixture, opened).await;
        std::fs::write(&cached, "mine").expect("saved copy");
        let store = store(&mut fixture);
        store.update(fixture.cx, |store, _| {
            store.servers.servers[0].host = "moved.example.net".into();
        });
        let id = server.id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        poll_until(&mut fixture, |fixture| !row_names(fixture).is_empty()).await;
        click_row(&mut fixture, "index.php");
        poll_until(&mut fixture, |fixture| fixture.cx.has_pending_prompt()).await;
        fixture.cx.simulate_prompt_answer("Keep my saved copy");
        editor_opens(&mut fixture).await;
        let baseline = store.read_with(fixture.cx, |store, _| {
            store
                .copy_for_buffer_path(&cached)
                .map(|copy| copy.stat.clone())
        });
        assert_eq!(
            baseline,
            Some(None),
            "no stamp of the old host: the first upload asks first"
        );
    }

    #[test]
    fn only_a_local_unshared_project_uploads() {
        assert!(is_local_unshared(true, false));
        assert!(!is_local_unshared(true, true));
        assert!(!is_local_unshared(false, false));
        assert!(!is_local_unshared(false, true));
    }

    #[test]
    fn prompt_buttons_map_to_the_actions_they_name() {
        let by_label = |label: &str| {
            upload_answer(
                UPLOAD_BUTTONS
                    .iter()
                    .position(|b| *b == label)
                    .expect("button"),
            )
        };
        assert_eq!(by_label("Upload"), UploadAnswer::Once);
        assert_eq!(
            by_label("Always upload to this server until I quit"),
            UploadAnswer::Always
        );
        assert_eq!(by_label("Cancel"), UploadAnswer::NotNow);
        assert_eq!(
            by_label("Never upload to this server until I quit"),
            UploadAnswer::Never
        );
        assert_eq!(
            upload_answer(99),
            UploadAnswer::NotNow,
            "a dismissed prompt uploads nothing"
        );
        assert!(confirmed(1));
        assert!(!confirmed(0) && !confirmed(7), "Cancel is the default");
        let buttons = force_buttons("Overwrite");
        assert!(matches!(&buttons[0], PromptButton::Cancel(label) if label.as_ref() == "Cancel"));
        assert!(matches!(&buttons[1], PromptButton::Other(_)));
        assert_eq!(
            UPLOAD_BUTTONS[2], "Cancel",
            "the in-app prompt needs the literal label"
        );
        assert_eq!(reopen_answer(0), ReopenAnswer::Keep);
        assert_eq!(reopen_answer(1), ReopenAnswer::UseServer);
        assert_eq!(reopen_answer(2), ReopenAnswer::Cancel);
        assert_eq!(reopen_answer(9), ReopenAnswer::Cancel);
        assert_eq!(REOPEN_BUTTONS[2], "Cancel");
    }

    #[test]
    fn the_prompt_states_what_the_protocol_risks() {
        let ftp = upload_detail(ServerType::Ftp);
        assert!(
            ftp.contains("unencrypted") && ftp.contains("symlink"),
            "{ftp}"
        );
        let ftps = upload_detail(ServerType::Ftps);
        assert!(
            !ftps.contains("unencrypted") && ftps.contains("symlink"),
            "{ftps}"
        );
        let sftp = upload_detail(ServerType::Sftp);
        assert!(!sftp.contains("symlink"), "{sftp}");
        for detail in [ftp, ftps, sftp] {
            assert!(detail.contains("same second"));
        }
    }

    #[test]
    fn server_text_is_one_short_plain_line() {
        assert_eq!(
            plain_text("a\nb\r\nc\td", 50),
            "a b c d",
            "whitespace collapses"
        );
        assert_eq!(
            plain_text("\x1b[31mred\x1b[0m!", 50),
            "red!",
            "the whole sequence goes"
        );
        assert_eq!(plain_text("a\x1b]0;title\x07b", 50), "ab", "OSC");
        assert_eq!(
            plain_text("a\x1b]0;title\x1b\\b", 50),
            "ab",
            "OSC ending in ST"
        );
        assert_eq!(plain_text("x\x1b", 50), "x", "a lone ESC");
        assert_eq!(
            plain_text("invoice\u{202E}fdp.php", 50),
            "invoicefdp.php",
            "a bidi override cannot flip the name"
        );
        assert_eq!(
            plain_text("a\u{2066}b\u{2069}c\u{200B}d\u{FEFF}e", 50),
            "abcde"
        );
        assert_eq!(plain_text("a\u{2028}b\u{2029}c", 50), "a b c");
        assert_eq!(plain_text("a\u{E0041}b", 50), "ab", "tag characters");
        let long = "x".repeat(100_000);
        let short = plain_text(&long, 300);
        assert_eq!(short.chars().count(), 301);
        assert!(short.ends_with('…'));
        assert_eq!(plain_text("héllo", 5), "héllo");
        assert_eq!(plain_text("日本語.php", 20), "日本語.php");
        assert_eq!(
            plain_text("a\u{3164}b\u{FFA0}c\u{115F}d\u{1160}e\u{34F}f", 50),
            "abcdef"
        );
        assert_eq!(
            plain_text("\u{1F468}\u{200D}\u{1F469}", 50),
            "\u{1F468}\u{200D}\u{1F469}",
            "a joiner is not a reordering control"
        );
        assert_eq!(plain_text("\u{301}abc", 50), "abc", "no leading mark");
        // A cut never separates a character from its mark: the whole cluster goes.
        assert_eq!(plain_text("abe\u{301}f", 3), "ab…");
        assert_eq!(plain_text("abce\u{301}", 4), "abc…");
        assert_eq!(shown("\u{200B}\u{202E}", 50), "(unnamed)");
        assert_eq!(shown("a.php", 50), "a.php");
    }

    #[test]
    fn a_message_ends_as_one_sentence() {
        assert_eq!(end_sentence("a changed"), "a changed.");
        assert_eq!(end_sentence("upload anyway?"), "upload anyway?");
    }

    #[gpui::test]
    async fn the_ellipsis_button_opens_the_deployment_dialog(cx: &mut TestAppContext) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.focus_panel::<RemoteHostPanel>(window, cx);
            });
        fixture.cx.run_until_parked();
        let bounds = fixture
            .cx
            .debug_bounds("zenith-deploy-open-dialog")
            .expect("the toolbar draws the … button");
        fixture
            .cx
            .simulate_click(bounds.center(), gpui::Modifiers::none());
        fixture.cx.run_until_parked();
        let dialog = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens");
        let selected = dialog.read_with(fixture.cx, |dialog, _| {
            dialog.selected_server().map(|server| server.id.clone())
        });
        assert_eq!(selected, Some(fixture.servers[1].id.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_root_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let cache = root.path().join("cache");
        create_private_dir(&cache, &cache.join("server").join("www")).expect("created");
        let mode = std::fs::metadata(&cache)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
