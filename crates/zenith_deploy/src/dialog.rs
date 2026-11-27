//! PhpStorm's Deployment dialog, Connection tab (plan 37 M1b): the server list with add, remove
//! and "use as default", the connection form, Test Connection, OK and Cancel. Servers go to the
//! user data dir (`servers.json`), passwords to the Keychain; nothing here writes a project file.

use std::collections::HashMap;

use editor::{Editor, EditorEvent};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Role, SharedString, Stateful, Subscription, Task, Toggled, WeakEntity,
    Window, actions,
};
use ui::{Callout, Checkbox, IconButton, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

use crate::{
    Password, ServerConfig, ServerStore, ServerType, Servers, config::Mapping, on_save, secrets,
    spawn_operation,
};

actions!(
    zenith_deploy,
    [
        /// Opens the Deployment dialog: servers and their connection settings.
        OpenDeployment,
        /// Moves focus to the next control of the Deployment dialog.
        FocusNextControl,
        /// Moves focus to the previous control of the Deployment dialog.
        FocusPreviousControl,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenDeployment, window, cx| {
            DeploymentDialog::open(workspace, None, window, cx);
        });
    })
    .detach();
}

/// The first visible folder of the workspace: the key for project defaults and for
/// "Visible only for this project".
pub(crate) fn project_root(workspace: &Workspace, cx: &App) -> Option<String> {
    let project = workspace.project().read(cx);
    let worktree = project.visible_worktrees(cx).next()?;
    Some(worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

pub(crate) const INSECURE_FTP: &str = "Insecure: plain FTP sends the password and files in clear text. Use FTPS or SFTP when the server supports it.";
pub(crate) const PASSWORD_REMOVED_NOTE: &str =
    "The password saved in the Keychain for this server will be removed when you press OK.";
const PASSWORD_NOTE_ID: &str = "zenith-deploy-password-removed";
const OUTDATED: &str = "The saved servers changed while this dialog was open (another save finished). Close it and reopen Deployment settings to continue from the saved list.";
const PASSWORD_CLEARED: &str = "Saved. The saved password was removed because the host, port, type or username changed; type a new one.";
const PORT_INVALID: &str = "Port must be a number from 1 to 65535.";
pub(crate) const NO_USERNAME: &str = "Username is not specified";

struct Fields {
    name: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    username: Entity<Editor>,
    password: Entity<Editor>,
    root_path: Entity<Editor>,
    web_url: Entity<Editor>,
}

impl Fields {
    fn all(&self) -> [&Entity<Editor>; 7] {
        [
            &self.name,
            &self.host,
            &self.port,
            &self.username,
            &self.password,
            &self.root_path,
            &self.web_url,
        ]
    }
}

/// The tabs of the right-hand side: each edits the selected server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogTab {
    Connection,
    Mappings,
    ExcludedPaths,
}

impl DialogTab {
    const ALL: [Self; 3] = [Self::Connection, Self::Mappings, Self::ExcludedPaths];

    fn label(self) -> &'static str {
        match self {
            Self::Connection => "Connection",
            Self::Mappings => "Mappings",
            Self::ExcludedPaths => "Excluded Paths",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Connection => "zenith-deploy-tab-connection",
            Self::Mappings => "zenith-deploy-tab-mappings",
            Self::ExcludedPaths => "zenith-deploy-tab-excluded",
        }
    }
}

/// One mapping of the selected server, as two editors. Dropping a row drops its subscriptions.
struct MappingRow {
    local: Entity<Editor>,
    remote: Entity<Editor>,
    _subscriptions: [Subscription; 2],
}

#[derive(Clone)]
enum TestStatus {
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

pub struct DeploymentDialog {
    store: Entity<ServerStore>,
    /// The working copy; the store changes only on OK.
    servers: Servers,
    /// The saved list this dialog was opened on (or last saved): OK is refused when the store
    /// holds another one, so a dialog never undoes a save that finished after it opened.
    base: Servers,
    loaded: bool,
    /// Passwords typed in this dialog, by server id; written on OK.
    passwords: HashMap<String, String>,
    removed: Vec<String>,
    selected: Option<usize>,
    project: Option<String>,
    fields: Fields,
    tab: DialogTab,
    /// The dialog's own focus handle: tells whether focus is inside it, and takes it when the
    /// control that had it is gone.
    focus_handle: FocusHandle,
    /// The Add Mapping button: where focus goes when the last mapping row is removed.
    add_mapping_focus: FocusHandle,
    /// The Add Server button: where Tab starts when there is no server to edit.
    add_server_focus: FocusHandle,
    /// Counts removed mappings: a Browse answer for a row that has since moved is dropped.
    mapping_removals: u64,
    /// The selected server's mappings, rebuilt when the selection or the row count changes.
    mapping_rows: Vec<MappingRow>,
    /// The selected server's `excluded_local`, one pattern per line.
    excluded: Entity<Editor>,
    port_invalid: bool,
    advanced_open: bool,
    test_status: Option<TestStatus>,
    error: Option<SharedString>,
    /// A save is running: OK does nothing until it ends.
    saving: bool,
    workspace: WeakEntity<Workspace>,
    _test: Option<Task<()>>,
    _save: Option<Task<()>>,
    _browse: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for DeploymentDialog {}
impl EventEmitter<DismissEvent> for DeploymentDialog {}

impl Drop for DeploymentDialog {
    /// Esc, a click outside or a closing window must not cancel a save halfway (a dropped
    /// task stops at its next await): let it finish; only its result is no longer shown.
    fn drop(&mut self) {
        if let Some(save) = self._save.take() {
            save.detach();
        }
    }
}

/// A password is kept in the Keychain only for servers that log in with one and have the
/// option on.
fn stores_password(server: &ServerConfig) -> bool {
    server.save_password && server.uses_password()
}

/// The error of a failed Keychain change; `applied` earlier changes already happened and
/// stay, so the text must not claim that nothing changed.
fn keychain_error(applied: usize, what: &str, error: anyhow::Error) -> anyhow::Error {
    error.context(if applied == 0 {
        format!("Nothing was saved: {what}")
    } else {
        format!(
            "The servers were not saved, but {applied} Keychain change(s) were already made: {what}"
        )
    })
}

impl Focusable for DeploymentDialog {
    /// The name field when the form is drawn; else the dialog itself, so that Tab and Escape
    /// still reach its key context (an undrawn field is not in the dispatch path).
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.selected.is_some() {
            self.fields.name.focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl DeploymentDialog {
    /// Opens the dialog, selecting `server_id` (or the project's default server).
    pub fn open(
        workspace: &mut Workspace,
        server_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = project_root(workspace, cx);
        let store = ServerStore::global(cx);
        let this_workspace = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(store, project, server_id, this_workspace, window, cx)
        });
    }

    fn new(
        store: Entity<ServerStore>,
        project: Option<String>,
        server_id: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let fields = Fields {
            name: field("Server name", window, cx),
            host: field("ftp.example.com", window, cx),
            port: field("21", window, cx),
            username: field("User name", window, cx),
            password: field("Leave empty to keep the saved password", window, cx),
            root_path: field("/", window, cx),
            web_url: field("https://example.com", window, cx),
        };
        fields
            .password
            .update(cx, |editor, cx| editor.set_masked(true, cx));
        let mut subscriptions: Vec<Subscription> = fields
            .all()
            .into_iter()
            .map(|editor| {
                cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.read_fields(cx);
                    }
                })
            })
            .collect();
        let excluded = cx.new(|cx| {
            // The feedback editor of `edit_prediction_ui`'s rate modal, minus its completions.
            let mut editor = Editor::multi_line(window, cx);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_placeholder_text("vendor\n*.log", window, cx);
            editor
        });
        subscriptions.push(cx.subscribe(&excluded, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.read_excluded(cx);
            }
        }));
        subscriptions.push(cx.observe_in(&store, window, |this, store, window, cx| {
            if !this.loaded && store.read(cx).is_loaded() {
                this.copy_from_store(None, window, cx);
            }
        }));
        let mut this = Self {
            store,
            servers: Servers::default(),
            base: Servers::default(),
            loaded: false,
            passwords: HashMap::new(),
            removed: Vec::new(),
            selected: None,
            project,
            fields,
            tab: DialogTab::Connection,
            focus_handle: cx.focus_handle(),
            // Tracked by a button, so the handle itself must be a tab stop.
            add_mapping_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            add_server_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            mapping_removals: 0,
            mapping_rows: Vec::new(),
            excluded,
            port_invalid: false,
            advanced_open: false,
            test_status: None,
            error: None,
            saving: false,
            workspace,
            _test: None,
            _save: None,
            _browse: None,
            _subscriptions: subscriptions,
        };
        if this.store.read(cx).is_loaded() {
            this.copy_from_store(server_id, window, cx);
        }
        this
    }

    fn copy_from_store(
        &mut self,
        server_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        self.servers = store.servers().clone();
        self.base = self.servers.clone();
        self.error = store.load_error().map(|error| {
            SharedString::from(format!("The saved servers could not be read: {error}"))
        });
        self.loaded = true;
        let wanted = server_id.or_else(|| {
            self.project
                .as_deref()
                .and_then(|project| self.servers.project_defaults.get(project).cloned())
        });
        let index = wanted
            .and_then(|id| {
                self.servers
                    .servers
                    .iter()
                    .position(|server| server.id == id)
            })
            .or_else(|| (!self.servers.servers.is_empty()).then_some(0));
        self.select(index, window, cx);
        // Opened before the servers were read, focus is on the dialog itself: now there is a
        // form, start in its name field.
        if self.selected.is_some() && self.focus_handle.is_focused(window) {
            window.focus(&self.fields.name.focus_handle(cx), cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn servers(&self) -> &Servers {
        &self.servers
    }

    pub(crate) fn selected_server(&self) -> Option<&ServerConfig> {
        self.servers.servers.get(self.selected?)
    }

    fn selected_server_mut(&mut self) -> Option<&mut ServerConfig> {
        self.servers.servers.get_mut(self.selected?)
    }

    pub(crate) fn select(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = index.filter(|index| *index < self.servers.servers.len());
        self.test_status = None;
        self.port_invalid = false;
        let server = self.selected_server().cloned().unwrap_or_default();
        let password = self.passwords.get(&server.id).cloned().unwrap_or_default();
        let texts = [
            (&self.fields.name, server.name.clone()),
            (&self.fields.host, server.host.clone()),
            (&self.fields.port, server.port.to_string()),
            (&self.fields.username, server.username.clone()),
            (&self.fields.password, password),
            (&self.fields.root_path, server.root_path.clone()),
            (&self.fields.web_url, server.web_url),
        ];
        for (editor, text) in texts {
            editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
        }
        let excluded = server.excluded_local.join("\n");
        self.excluded
            .update(cx, |editor, cx| editor.set_text(excluded, window, cx));
        self.rebuild_mapping_rows(window, cx);
        cx.notify();
    }

    fn set_tab(&mut self, tab: DialogTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn mapping_row(mapping: &Mapping, window: &mut Window, cx: &mut Context<Self>) -> MappingRow {
        let editor =
            |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(text, window, cx);
                    editor
                })
            };
        let local = editor(&mapping.local, "/path/to/folder", window, cx);
        let remote = editor(&mapping.remote, "Relative to the root path", window, cx);
        let on_edit = |this: &mut Self, _, event: &EditorEvent, cx: &mut Context<Self>| {
            if matches!(event, EditorEvent::BufferEdited) {
                this.read_mappings(cx);
            }
        };
        let _subscriptions = [
            cx.subscribe(&local, on_edit),
            cx.subscribe(&remote, on_edit),
        ];
        MappingRow {
            local,
            remote,
            _subscriptions,
        }
    }

    /// One row per mapping of the selected server (none without one).
    fn rebuild_mapping_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mappings = self
            .selected_server()
            .map(|server| server.mappings.clone())
            .unwrap_or_default();
        self.mapping_rows = mappings
            .iter()
            .map(|mapping| Self::mapping_row(mapping, window, cx))
            .collect();
        cx.notify();
    }

    /// Copies the mapping editors into the selected server.
    fn read_mappings(&mut self, cx: &mut Context<Self>) {
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx).trim().to_string();
        let mappings: Vec<Mapping> = self
            .mapping_rows
            .iter()
            .map(|row| Mapping {
                local: text(&row.local),
                remote: text(&row.remote),
            })
            .collect();
        if let Some(server) = self.selected_server_mut() {
            server.mappings = mappings;
            cx.notify();
        }
    }

    /// Copies the excluded-paths editor into the selected server: one pattern per line, blank
    /// lines dropped.
    fn read_excluded(&mut self, cx: &mut Context<Self>) {
        let text = self.excluded.read(cx).text(cx);
        let patterns: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();
        if let Some(server) = self.selected_server_mut() {
            // Showing a server's patterns fires this too: while the box holds exactly what the
            // server has, a hand-edited list with blank or padded entries must stay as it is, or
            // OK would count it as changed and ask for the upload-on-save confirmation again.
            // Any other text is the user's: store it tidied (a padded pattern matches nothing).
            if text != server.excluded_local.join("\n") {
                server.excluded_local = patterns;
            }
            cx.notify();
        }
    }

    /// A new mapping starts at the project folder; the remote folder is the one to type.
    pub(crate) fn add_mapping(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let local = self.project.clone().unwrap_or_default();
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        server.mappings.push(Mapping {
            local,
            remote: String::new(),
        });
        self.rebuild_mapping_rows(window, cx);
        if let Some(row) = self.mapping_rows.last() {
            window.focus(&row.remote.focus_handle(cx), cx);
        }
    }

    pub(crate) fn remove_mapping(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        if index < server.mappings.len() {
            server.mappings.remove(index);
            self.mapping_removals += 1;
            self.rebuild_mapping_rows(window, cx);
            // The row that had focus is gone: hand it to the one that took its place (or the
            // one before it), else to Add Mapping, so the keyboard stays inside the dialog.
            let neighbour = self
                .mapping_rows
                .get(index)
                .or_else(|| self.mapping_rows.last());
            let target = match neighbour {
                Some(row) => row.remote.focus_handle(cx),
                None => self.add_mapping_focus.clone(),
            };
            window.focus(&target, cx);
        }
    }

    /// Asks for a local folder with the system's folder dialog and types it into the row. Not
    /// `Workspace::prompt_for_open_path`: its in-app fallback is a modal, which would replace
    /// this dialog and drop what is typed in it.
    pub(crate) fn browse_mapping(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server_id) = self.selected_server().map(|server| server.id.clone()) else {
            return;
        };
        if index >= self.mapping_rows.len() {
            return;
        }
        let removals = self.mapping_removals;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        self._browse = Some(cx.spawn_in(window, async move |this, cx| {
            let folder = paths
                .await
                .ok()
                .and_then(|result| result.log_err())
                .flatten()
                .and_then(|paths| paths.into_iter().next());
            let Some(folder) = folder else {
                return;
            };
            // The rows may have been rebuilt while the prompt was open: find the row again, and
            // drop the answer if another server is showing or a mapping was removed (the index
            // may now name another row).
            this.update_in(cx, |this, window, cx| {
                if this.selected_server().map(|server| &server.id) != Some(&server_id)
                    || this.mapping_removals != removals
                {
                    return;
                }
                if let Some(row) = this.mapping_rows.get(index) {
                    row.local.update(cx, |editor, cx| {
                        editor.set_text(folder.to_string_lossy().into_owned(), window, cx)
                    });
                }
            })
            .log_err();
        }));
    }

    /// Copies the editors into the selected server.
    fn read_fields(&mut self, cx: &mut Context<Self>) {
        let text = |editor: &Entity<Editor>, cx: &App| editor.read(cx).text(cx);
        let name = text(&self.fields.name, cx);
        let host = text(&self.fields.host, cx).trim().to_string();
        let port = text(&self.fields.port, cx);
        let username = text(&self.fields.username, cx).trim().to_string();
        let password = text(&self.fields.password, cx);
        let root_path = text(&self.fields.root_path, cx).trim().to_string();
        let web_url = text(&self.fields.web_url, cx).trim().to_string();
        let parsed_port = port.trim().parse::<u16>().ok().filter(|port| *port > 0);
        self.port_invalid = parsed_port.is_none();
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        server.name = name;
        server.host = host;
        if let Some(port) = parsed_port {
            server.port = port;
        }
        server.username = username;
        server.root_path = root_path;
        server.web_url = web_url;
        let id = server.id.clone();
        if password.is_empty() {
            self.passwords.remove(&id);
        } else {
            self.passwords.insert(id, password);
        }
        cx.notify();
    }

    pub(crate) fn add_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut server = ServerConfig::new();
        let mut number = self.servers.servers.len() + 1;
        while self
            .servers
            .servers
            .iter()
            .any(|existing| existing.name == format!("Server {number}"))
        {
            number += 1;
        }
        server.name = format!("Server {number}");
        self.servers.servers.push(server);
        self.tab = DialogTab::Connection;
        self.select(Some(self.servers.servers.len() - 1), window, cx);
        window.focus(&self.fields.host.focus_handle(cx), cx);
    }

    pub(crate) fn remove_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        let removed = self.servers.servers.remove(index);
        self.servers
            .project_defaults
            .retain(|_, server_id| *server_id != removed.id);
        self.passwords.remove(&removed.id);
        self.removed.push(removed.id);
        let next = if self.servers.servers.is_empty() {
            None
        } else {
            Some(index.min(self.servers.servers.len() - 1))
        };
        self.select(next, window, cx);
    }

    /// Marks the selected server as this project's default (PhpStorm's ✓).
    pub(crate) fn set_default(&mut self, cx: &mut Context<Self>) {
        let (Some(project), Some(server)) = (self.project.clone(), self.selected_server()) else {
            return;
        };
        let id = server.id.clone();
        self.servers.project_defaults.insert(project, id);
        cx.notify();
    }

    fn is_default(&self, server: &ServerConfig) -> bool {
        self.project
            .as_deref()
            .is_some_and(|project| self.servers.project_defaults.get(project) == Some(&server.id))
    }

    pub(crate) fn set_type(
        &mut self,
        server_type: ServerType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.selected_server_mut() else {
            return;
        };
        if server.port == server.server_type.default_port() {
            server.port = server_type.default_port();
        }
        server.server_type = server_type;
        if server_type == ServerType::Sftp {
            server.anonymous = false;
        }
        let port = server.port.to_string();
        self.fields
            .port
            .update(cx, |editor, cx| editor.set_text(port, window, cx));
        self.port_invalid = false;
        self.test_status = None;
        cx.notify();
    }

    fn update_selected(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut ServerConfig)) {
        if let Some(server) = self.selected_server_mut() {
            update(server);
            cx.notify();
        }
    }

    /// The warnings shown under the form, in order.
    pub(crate) fn warnings(&self) -> Vec<&'static str> {
        let Some(server) = self.selected_server() else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        if server.username.is_empty() && !server.anonymous {
            warnings.push(NO_USERNAME);
        }
        if server.server_type == ServerType::Ftp {
            warnings.push(INSECURE_FTP);
        }
        warnings
    }

    /// True when OK would delete the Keychain password of the selected server: one was stored
    /// and the form no longer keeps one (Anonymous login, SFTP, or "Save password" off).
    pub(crate) fn removes_saved_password(&self, cx: &App) -> bool {
        let Some(server) = self.selected_server() else {
            return false;
        };
        !stores_password(server)
            && self
                .store
                .read(cx)
                .servers()
                .servers
                .iter()
                .any(|old| old.id == server.id && stores_password(old))
    }

    pub(crate) fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.selected_server().cloned() else {
            return;
        };
        let typed = self.passwords.get(&server.id).cloned();
        let store = self.store.read(cx);
        let needs_password = server.uses_password();
        // The stored password belongs to the saved host, port, type and username: never send it
        // to another.
        let saved_target = store
            .servers()
            .servers
            .iter()
            .any(|saved| saved.id == server.id && !saved.credentials_target_changed(&server));
        if needs_password && typed.is_none() && !saved_target {
            self.test_status = Some(TestStatus::Failed(
                "Type the password to test a changed host, port, type or username; the saved one is not sent to it."
                    .into(),
            ));
            cx.notify();
            return;
        }
        let session = match typed {
            Some(password) if server.uses_password() => {
                let mut server = server;
                server.password = Some(Password(password));
                spawn_operation(cx, (store.connect)(server))
            }
            _ => store.open_session(server, cx),
        };
        self.test_status = Some(TestStatus::Running);
        cx.notify();
        self._test = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let mut transport = session.await?;
                cx.update(|cx| {
                    spawn_operation(cx, async move {
                        let directory = transport.pwd().await;
                        transport.disconnect().await.log_err();
                        directory
                    })
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.test_status = Some(match result {
                    Ok(directory) => TestStatus::Passed(
                        format!("Connected. Initial directory: {directory}").into(),
                    ),
                    Err(error) => {
                        TestStatus::Failed(format!("Connection failed: {error:#}").into())
                    }
                });
                cx.notify();
            })
            .log_err();
        }));
    }

    pub(crate) fn can_save(&self, cx: &App) -> bool {
        self.loaded
            && !self.saving
            && !self.port_invalid
            && self.store.read(cx).load_error().is_none()
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        if !self.loaded
            || self.saving
            || self.port_invalid
            || self.store.read(cx).load_error().is_some()
        {
            return;
        }
        // Before any Keychain work: a dialog on an outdated list must not change the Keychain.
        if self.store.read(cx).servers() != &self.base {
            self.error = Some(OUTDATED.into());
            cx.notify();
            return;
        }
        // Also before any Keychain work: the store refuses these too, but by then a delete or
        // write would have changed the Keychain for a save that never happens.
        if let Err(error) = crate::validate_changed(self.store.read(cx).servers(), &self.servers) {
            self.error = Some(format!("{error:#}").into());
            cx.notify();
            return;
        }
        // Held until the task ends, so a save cannot start while another one is still on the
        // Keychain (see `ServerStore::begin_save`).
        if let Err(error) = self.store.update(cx, |store, _| store.begin_save()) {
            self.error = Some(format!("Saving failed: {error:#}").into());
            cx.notify();
            return;
        }
        let keychain = self.store.read(cx).keychain.clone();
        let mut deletes: Vec<Task<anyhow::Result<bool>>> = Vec::new();
        let mut writes: Vec<(ServerConfig, String)> = Vec::new();
        let mut session_changes: Vec<(ServerConfig, Option<Password>)> = Vec::new();
        let previous = self.store.read(cx).servers().clone();
        let mut cleared_credentials = false;
        for server in &self.servers.servers {
            let typed = self.passwords.get(&server.id).cloned();
            let saves_password = stores_password(server);
            let old = previous.servers.iter().find(|old| old.id == server.id);
            let was_saved = old.is_some_and(stores_password);
            // Nothing typed for a new target: the old password must not follow the server there.
            let target_changed =
                typed.is_none() && old.is_some_and(|old| old.credentials_target_changed(server));
            match typed {
                Some(password) if saves_password => {
                    writes.push((server.clone(), password));
                    session_changes.push((server.clone(), None));
                }
                Some(password) => session_changes.push((server.clone(), Some(Password(password)))),
                None => {}
            }
            if target_changed && server.uses_password() {
                // Only tell the user when a password was actually there to lose.
                let had_session =
                    old.is_some_and(|old| self.store.read(cx).session_password_for(old).is_some());
                cleared_credentials |= was_saved || had_session;
            }
            if was_saved && (!saves_password || target_changed) {
                deletes.push(secrets::delete_password(&keychain, &server.id, cx));
            }
        }
        for id in &self.removed {
            deletes.push(secrets::delete_password(&keychain, id, cx));
        }
        let servers = self.servers.clone();
        let store = self.store.clone();
        let base = self.base.clone();
        let workspace = self.workspace.clone();
        self.saving = true;
        self._save = Some(cx.spawn(async move |this, cx| {
            // Deletes first, then writes, and the new servers last: a failure leaves the saved
            // servers as they were and the saved password (if its delete did not run) intact.
            // The session passwords change together with the servers, never before. The
            // Keychain changes already made are not rolled back; a retry repeats them.
            let result: anyhow::Result<()> = async {
                let mut applied = 0;
                let mut failure = None;
                for delete in futures::future::join_all(deletes).await {
                    match delete {
                        Ok(removed) => applied += usize::from(removed),
                        Err(error) => failure = failure.or(Some(error)),
                    }
                }
                if let Some(error) = failure {
                    return Err(keychain_error(
                        applied,
                        "the Keychain could not remove a saved password",
                        error,
                    ));
                }
                for (server, password) in writes {
                    let write = cx.update(|cx| {
                        secrets::save_password(keychain.as_ref(), &server, &password, cx)
                    });
                    write.await.map_err(|error| {
                        keychain_error(applied, "the Keychain could not store the password", error)
                    })?;
                    applied += 1;
                }
                let save = store.update(cx, |store, cx| {
                    store.save(base, servers, session_changes, &Default::default(), cx)
                });
                save.await.map_err(|error| {
                    if applied == 0 {
                        error
                    } else {
                        keychain_error(applied, "the servers could not be saved", error)
                    }
                })
            }
            .await;
            // On every path, whatever became of the dialog.
            store.update(cx, |store, _| store.end_save());
            let shown = this.update(cx, |this, cx| {
                this.saving = false;
                if result.is_ok() {
                    this.base = this.store.read(cx).servers().clone();
                }
                match &result {
                    Ok(()) if cleared_credentials => {
                        this.error = Some(PASSWORD_CLEARED.into());
                        cx.notify();
                    }
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("Saving failed: {error:#}").into());
                        cx.notify();
                    }
                }
            });
            if shown.is_err() {
                // The dialog was closed while saving: tell the user in the workspace.
                let message = match result {
                    Err(error) => {
                        log::error!(
                            "deploy: saving the servers after the dialog closed: {error:#}"
                        );
                        Some(format!("Deployment: saving failed: {error:#}"))
                    }
                    Ok(()) if cleared_credentials => {
                        Some(format!("Deployment: {PASSWORD_CLEARED}"))
                    }
                    Ok(()) => None,
                };
                if let Some(message) = message {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(NotificationId::unique::<DeploymentDialog>(), message),
                                cx,
                            )
                        })
                        .log_err();
                }
            }
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Tab and Shift-Tab. The tab order belongs to the window and does not stop at the dialog's
    /// edge, so a step that leaves the dialog wraps to its first (or, backwards, last) control.
    fn move_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        if self.focus_handle.contains_focused(window, cx) {
            return;
        }
        self.focus_handle.focus(window, cx);
        window.focus_next(cx);
        if !forward {
            // ponytail: walks forward to the last control; a dialog has far fewer than 256.
            for _ in 0..256 {
                window.focus_next(cx);
                if !self.focus_handle.contains_focused(window, cx) {
                    window.focus_prev(cx);
                    break;
                }
            }
        }
    }

    /// A button whose Enter does what its click does. Without this the base keymap's Enter
    /// (`menu::Confirm`) reaches the dialog and saves it. `button` receives the click handler.
    fn pressed<B: IntoElement>(
        press: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        button: impl FnOnce(Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>) -> B,
        cx: &mut Context<Self>,
    ) -> Div {
        let on_enter = press.clone();
        let on_click = cx.listener(move |this, _: &ClickEvent, window, cx| press(this, window, cx));
        div()
            .on_action(
                cx.listener(move |this, _: &menu::Confirm, window, cx| on_enter(this, window, cx)),
            )
            .child(button(Box::new(on_click)))
    }

    /// A focusable wrapper whose Enter and Space run `press`: for rows and checkboxes, which
    /// are not buttons. The wrapped element keeps handling the mouse.
    fn focusable_press(
        id: impl Into<gpui::ElementId>,
        enabled: bool,
        press: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let colors = cx.theme().colors();
        let on_enter = press.clone();
        div()
            .id(id)
            .border_1()
            .border_color(colors.border_transparent)
            .rounded_sm()
            .when(enabled, |this| {
                this.tab_index(0isize)
                    .focus_visible(|style| style.border_color(colors.border_focused))
                    .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                        on_enter(this, window, cx)
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        // A mouse click is the wrapped element's own business.
                        if event.is_keyboard() {
                            press(this, window, cx)
                        }
                    }))
            })
    }

    fn render_server_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_selection = self.selected.is_some();
        let can_default = has_selection && self.project.is_some();
        let toolbar = h_flex()
            .gap_0p5()
            .child(Self::pressed(
                |this, window, cx| this.add_server(window, cx),
                |on_click| {
                    IconButton::new("zenith-deploy-add", IconName::Plus)
                        .track_focus(&self.add_server_focus)
                        .icon_size(IconSize::Small)
                        .tab_index(0isize)
                        .tooltip(Tooltip::text("Add Server"))
                        .aria_label("Add Server")
                        .on_click(on_click)
                },
                cx,
            ))
            .child(Self::pressed(
                |this, window, cx| this.remove_server(window, cx),
                |on_click| {
                    IconButton::new("zenith-deploy-remove", IconName::Dash)
                        .icon_size(IconSize::Small)
                        .tab_index(0isize)
                        .disabled(!has_selection)
                        .tooltip(Tooltip::text("Remove Server"))
                        .aria_label("Remove Server")
                        .on_click(on_click)
                },
                cx,
            ))
            .child(Self::pressed(
                |this, _, cx| this.set_default(cx),
                |on_click| {
                    IconButton::new("zenith-deploy-default", IconName::Check)
                        .icon_size(IconSize::Small)
                        .tab_index(0isize)
                        .disabled(!can_default)
                        .tooltip(Tooltip::text("Use as Default for This Project"))
                        .aria_label("Use as Default for This Project")
                        .on_click(on_click)
                },
                cx,
            ));
        let border = cx.theme().colors().border;
        let server_count = self.servers.servers.len();
        let rows =
            self.servers
                .servers
                .iter()
                .enumerate()
                .map(|(index, server)| {
                    let is_default = self.is_default(server);
                    let label = if server.name.is_empty() {
                        "Unnamed".to_string()
                    } else {
                        server.name.clone()
                    };
                    let selected = self.selected == Some(index);
                    // Focusable so the keyboard can pick a server: Enter or Space selects it.
                    Self::focusable_press(
                        ("zenith-deploy-server-focus", index),
                        true,
                        move |this, window, cx| this.select(Some(index), window, cx),
                        cx,
                    )
                    .debug_selector(move || format!("zenith-deploy-server-{index}"))
                    .role(Role::ListBoxOption)
                    .aria_label(if is_default {
                        format!("{label}, default")
                    } else {
                        label.clone()
                    })
                    .aria_selected(selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(server_count)
                    .child(
                        ListItem::new(("zenith-deploy-server", index))
                            .toggle_state(selected)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(label).when(is_default, |label| {
                                        label.weight(gpui::FontWeight::BOLD)
                                    }))
                                    .when(is_default, |row| {
                                        row.child(Label::new("(default)").color(Color::Muted))
                                    }),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select(Some(index), window, cx)
                            })),
                    )
                });
        v_flex().w(rems(14.)).gap_1().child(toolbar).child(
            v_flex()
                .id("zenith-deploy-server-list")
                .role(Role::ListBox)
                .aria_label("Servers")
                .h(rems(22.))
                .overflow_y_scroll()
                .border_1()
                .border_color(border)
                .rounded_sm()
                .children(rows)
                .when(self.servers.servers.is_empty(), |list| {
                    list.child(
                        div()
                            .p_2()
                            .child(Label::new("No servers. Add one with +.").color(Color::Muted)),
                    )
                }),
        )
    }

    fn row(
        label: &'static str,
        role: Role,
        description: Option<&'static str>,
        editor: &Entity<Editor>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .debug_selector(|| format!("zenith-deploy-row-{label}"))
            .gap_2()
            .child(
                div()
                    .w(rems(7.))
                    .child(Label::new(label).color(Color::Muted)),
            )
            .child(
                Self::labeled_field(
                    SharedString::from(format!("zenith-deploy-field-{label}")),
                    label,
                    role,
                    editor,
                    window,
                    cx,
                )
                .debug_selector(|| format!("zenith-deploy-field-{label}"))
                .when_some(description, |field, text| field.aria_description(text))
                .flex_1(),
            )
    }

    /// A bordered editor that is a tab stop and that screen readers can name: the fork's
    /// editors expose no node of their own (the pattern of `settings_ui`'s input field). The
    /// value is read only while assistive technology is connected, and never for a password.
    fn labeled_field(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        role: Role,
        editor: &Entity<Editor>,
        window: &Window,
        cx: &App,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .role(role)
            .aria_label(label)
            .when(
                window.is_a11y_active() && role != Role::PasswordInput,
                |field| field.aria_value(editor.read(cx).text(cx)),
            )
            .track_focus(&editor.focus_handle(cx).tab_index(0).tab_stop(true))
            .min_w_0()
            .px_2()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_sm()
            .child(editor.clone())
    }

    /// A checkbox that is a tab stop (Enter and Space toggle it) and that screen readers
    /// announce with its state. `ui::Checkbox` has no focus handle or role of its own.
    fn checkbox(
        id: &'static str,
        label: &'static str,
        checked: bool,
        disabled: bool,
        description: Option<&'static str>,
        toggle: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let on_click = toggle.clone();
        Self::focusable_press(
            SharedString::from(format!("{id}-focus")),
            !disabled,
            toggle,
            cx,
        )
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .when_some(description, |this, text| this.aria_description(text))
        .child(
            Checkbox::new(id, checked.into())
                .label(label)
                .disabled(disabled)
                .on_click(
                    cx.listener(move |this, _: &ToggleState, window, cx| {
                        on_click(this, window, cx)
                    }),
                ),
        )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .id("zenith-deploy-tabs")
            .debug_selector(|| "zenith-deploy-tabs".to_string())
            .role(Role::TabList)
            .aria_label("Server settings")
            .gap_1()
            .children(DialogTab::ALL.into_iter().enumerate().map(|(index, tab)| {
                let selected = self.tab == tab;
                // The selected tab has a bar under it as well as a background, so the choice
                // does not rest on colour alone.
                v_flex()
                    .id(tab.id())
                    .debug_selector(|| tab.id().to_string())
                    .role(Role::Tab)
                    .aria_label(tab.label())
                    .aria_selected(selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(DialogTab::ALL.len())
                    .tab_index(0isize)
                    .px_2()
                    .pt_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_transparent)
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.element_selected))
                    .hover(|style| style.bg(colors.element_hover))
                    .focus_visible(|style| style.border_color(colors.border_focused))
                    .child(Label::new(tab.label()).color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .child(
                        div()
                            .h(px(2.))
                            .w_full()
                            .mt_0p5()
                            .rounded_sm()
                            .bg(if selected {
                                colors.text_accent
                            } else {
                                colors.border_transparent
                            }),
                    )
                    // Enter selects the tab (the base keymap's Enter would save the dialog).
                    .on_action(
                        cx.listener(move |this, _: &menu::Confirm, _, cx| this.set_tab(tab, cx)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            }))
    }

    fn render_mappings(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.mapping_rows.iter().enumerate().map(|(index, row)| {
            let number = index + 1;
            let side = |label: &'static str| {
                div()
                    .w(rems(5.))
                    .child(Label::new(label).color(Color::Muted))
            };
            let button_box = || div().w(rems(6.)).flex_none();
            v_flex()
                .id(("zenith-deploy-mapping", index))
                .debug_selector(move || format!("zenith-deploy-mapping-{index}"))
                .gap_1()
                .pb_1p5()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .gap_2()
                        .child(side("Local"))
                        .child(
                            Self::labeled_field(
                                ("zenith-deploy-map-local", index),
                                format!("Local folder, mapping {number}"),
                                Role::TextInput,
                                &row.local,
                                window,
                                cx,
                            )
                            .debug_selector(move || format!("zenith-deploy-map-local-{index}"))
                            .flex_1(),
                        )
                        .child(button_box().child(Self::pressed(
                            move |this, window, cx| this.browse_mapping(index, window, cx),
                            |on_click| {
                                Button::new(("zenith-deploy-map-browse", index), "Browse…")
                                    .full_width()
                                    .tab_index(0isize)
                                    .tooltip(Tooltip::text("Choose the local folder"))
                                    .aria_label(format!(
                                        "Browse for the local folder, mapping {number}"
                                    ))
                                    .on_click(on_click)
                            },
                            cx,
                        ))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(side("Remote"))
                        .child(
                            Self::labeled_field(
                                ("zenith-deploy-map-remote", index),
                                format!("Remote folder, mapping {number}"),
                                Role::TextInput,
                                &row.remote,
                                window,
                                cx,
                            )
                            .debug_selector(move || format!("zenith-deploy-map-remote-{index}"))
                            .flex_1(),
                        )
                        .child(button_box().child(Self::pressed(
                            move |this, window, cx| this.remove_mapping(index, window, cx),
                            |on_click| {
                                Button::new(("zenith-deploy-map-remove", index), "Remove")
                                    .full_width()
                                    .tab_index(0isize)
                                    .tooltip(Tooltip::text("Remove this mapping"))
                                    .aria_label(format!("Remove mapping {number}"))
                                    .on_click(on_click)
                            },
                            cx,
                        ))),
                )
        });
        v_flex()
            .debug_selector(|| "zenith-deploy-mappings".to_string())
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child(
                Label::new("A local folder and the folder under the root path it uploads to. A saved file goes by the longest mapping that contains it.")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .children(rows)
            .when(self.mapping_rows.is_empty(), |panel| {
                panel.child(Label::new("No mappings. Add one with Add Mapping.").color(Color::Muted))
            })
            .child(
                div().child(Self::pressed(
                    |this, window, cx| this.add_mapping(window, cx),
                    |on_click| {
                        Button::new("zenith-deploy-map-add", "Add Mapping")
                            .tab_index(0isize)
                            .track_focus(&self.add_mapping_focus)
                            .tooltip(Tooltip::text("Add a mapping, starting at the project folder"))
                            .on_click(on_click)
                    },
                    cx,
                )),
            )
    }

    fn render_excluded(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .debug_selector(|| "zenith-deploy-excluded".to_string())
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child(
                Label::new("One pattern per line, for example vendor, *.log or build/**. Matching local files and folders are not uploaded on save.")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                Self::labeled_field(
                    "zenith-deploy-excluded-field",
                    "Excluded paths, one pattern per line",
                    Role::MultilineTextInput,
                    &self.excluded,
                    window,
                    cx,
                )
                .debug_selector(|| "zenith-deploy-excluded-field".to_string())
                .h(rems(10.)),
            )
            .child(
                div()
                    .debug_selector(|| "zenith-deploy-excluded-fixed".to_string())
                    .child(
                        Label::new(format!(
                            "Always skipped, whatever is listed here: {}",
                            on_save::fixed_exclusions_text()
                        ))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    ),
            )
    }

    fn render_form(
        &self,
        server: &ServerConfig,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let types = h_flex()
            .gap_1()
            .child(
                div()
                    .w(rems(7.))
                    .child(Label::new("Type").color(Color::Muted)),
            )
            .children(
                [
                    (ServerType::Ftps, "FTPS"),
                    (ServerType::Ftp, "FTP"),
                    (ServerType::Sftp, "SFTP"),
                ]
                .into_iter()
                .map(|(server_type, label)| {
                    Self::pressed(
                        move |this, window, cx| this.set_type(server_type, window, cx),
                        |on_click| {
                            Button::new(
                                SharedString::from(format!("zenith-deploy-type-{label}")),
                                label,
                            )
                            .tab_index(0isize)
                            .toggle_state(server.server_type == server_type)
                            .on_click(on_click)
                        },
                        cx,
                    )
                }),
            );
        let is_ftp = server.server_type != ServerType::Sftp;
        let project_only = server.project.is_some();
        let has_project = self.project.is_some();
        let removes_password = self.removes_saved_password(cx);
        let advanced = &server.advanced;
        v_flex()
            .debug_selector(|| "zenith-deploy-form".to_string())
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child(Self::row("Name", Role::TextInput, None, &self.fields.name, window, cx))
            .child(types)
            .child(Self::row("Host", Role::TextInput, None, &self.fields.host, window, cx))
            .child(Self::row(
                "Port",
                Role::TextInput,
                self.port_invalid.then_some(PORT_INVALID),
                &self.fields.port,
                window,
                cx,
            ))
            .when(self.port_invalid, |form| {
                form.child(
                    div()
                        .id("zenith-deploy-port-error")
                        .role(Role::Alert)
                        .aria_label(PORT_INVALID)
                        .child(Label::new(PORT_INVALID).color(Color::Error)),
                )
            })
            .child(Self::row("Username", Role::TextInput, None, &self.fields.username, window, cx))
            .when(is_ftp, |form| {
                form.child(Self::checkbox(
                    "zenith-deploy-anonymous",
                    "Anonymous login",
                    server.anonymous,
                    false,
                    None,
                    |this, _, cx| {
                        this.update_selected(cx, |server| server.anonymous = !server.anonymous)
                    },
                    cx,
                ))
            })
            .when(is_ftp && !server.anonymous, |form| {
                form.child(Self::row("Password", Role::PasswordInput, None, &self.fields.password, window, cx))
                    .child(Self::checkbox(
                        "zenith-deploy-save-password",
                        "Save password in the Keychain",
                        server.save_password,
                        false,
                        None,
                        |this, _, cx| {
                            this.update_selected(cx, |server| {
                                server.save_password = !server.save_password
                            })
                        },
                        cx,
                    ))
            })
            .when(removes_password, |form| {
                form.child(div().debug_selector(|| PASSWORD_NOTE_ID.to_string()).child(
                    Callout::new()
                        .severity(Severity::Warning)
                        .icon(IconName::Warning)
                        .title(PASSWORD_REMOVED_NOTE),
                ))
            })
            .when(!is_ftp, |form| {
                form.child(
                    Label::new("SFTP signs in with your SSH key or agent; the host key must already be in known_hosts.")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
            })
            .child(Self::row("Root path", Role::TextInput, None, &self.fields.root_path, window, cx))
            .child(Self::row("Web URL", Role::TextInput, None, &self.fields.web_url, window, cx))
            .child(Self::checkbox(
                "zenith-deploy-project-only",
                "Visible only for this project",
                project_only,
                !has_project && !project_only,
                None,
                |this, _, cx| {
                    let project = this.project.clone();
                    this.update_selected(cx, |server| {
                        server.project = match server.project {
                            Some(_) => None,
                            None => project,
                        }
                    })
                },
                cx,
            ))
            .child(
                h_flex()
                    .gap_1()
                    .child(Self::pressed(
                        |this, _, cx| {
                            this.advanced_open = !this.advanced_open;
                            cx.notify();
                        },
                        |on_click| {
                            IconButton::new(
                                "zenith-deploy-advanced-toggle",
                                if self.advanced_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                },
                            )
                            .icon_size(IconSize::Small)
                            .tab_index(0isize)
                            .tooltip(Tooltip::text("Show or hide the advanced settings"))
                            .aria_label("Advanced")
                            .aria_expanded(self.advanced_open)
                            .on_click(on_click)
                        },
                        cx,
                    ))
                    .child(Label::new("Advanced")),
            )
            .when(self.advanced_open && is_ftp, |form| {
                form.child(
                    v_flex()
                        .pl_4()
                        .gap_1()
                        .child(Self::checkbox(
                            "zenith-deploy-passive",
                            "Passive mode",
                            advanced.passive,
                            false,
                            None,
                            |this, _, cx| {
                                this.update_selected(cx, |server| {
                                    server.advanced.passive = !server.advanced.passive
                                })
                            },
                            cx,
                        ))
                        .child(Self::checkbox(
                            "zenith-deploy-list",
                            "Use LIST instead of MLSD",
                            advanced.use_list,
                            false,
                            None,
                            |this, _, cx| {
                                this.update_selected(cx, |server| {
                                    server.advanced.use_list = !server.advanced.use_list
                                })
                            },
                            cx,
                        ))
                        .child(
                            h_flex()
                                .pl_4()
                                .gap_2()
                                .debug_selector(|| "zenith-deploy-list-compat".to_string())
                                .child(Self::checkbox(
                                    "zenith-deploy-list-compat",
                                    "Compatible with old version of listing children",
                                    advanced.list_compat,
                                    !advanced.use_list,
                                    (!advanced.use_list)
                                        .then_some("Needs Use LIST instead of MLSD to be on"),
                                    |this, _, cx| {
                                        this.update_selected(cx, |server| {
                                            server.advanced.list_compat =
                                                !server.advanced.list_compat
                                        })
                                    },
                                    cx,
                                ))
                                .when(!advanced.use_list, |row| {
                                    row.child(
                                        div()
                                            .debug_selector(|| "zenith-deploy-needs-list".to_string())
                                            .child(
                                                Label::new("(needs LIST)")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                }),
                        ),
                )
            })
            .when(self.advanced_open, |form| {
                form.child(
                    div().pl_4().child(
                        Self::checkbox(
                            "zenith-deploy-hidden",
                            "Show hidden files",
                            advanced.hidden_files,
                            false,
                            None,
                            |this, _, cx| {
                                this.update_selected(cx, |server| {
                                    server.advanced.hidden_files = !server.advanced.hidden_files
                                })
                            },
                            cx,
                        ),
                    ),
                )
            })
            .children(self.warnings().into_iter().map(|warning| {
                let (id, severity) = if warning == INSECURE_FTP {
                    ("zenith-deploy-insecure", Severity::Error)
                } else {
                    ("zenith-deploy-no-username", Severity::Warning)
                };
                div().debug_selector(|| id.to_string()).child(
                    Callout::new()
                        .severity(severity)
                        .icon(IconName::Warning)
                        .title(warning),
                )
            }))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.test_status {
            None => None,
            Some(TestStatus::Running) => Some((
                IconName::ArrowCircle,
                Color::Muted,
                SharedString::from("Connecting…"),
            )),
            Some(TestStatus::Passed(message)) => {
                Some((IconName::Check, Color::Success, message.clone()))
            }
            Some(TestStatus::Failed(message)) => {
                Some((IconName::Warning, Color::Error, message.clone()))
            }
        };
        let can_save = self.can_save(cx);
        v_flex()
            .gap_1()
            .when_some(status, |footer, (icon, color, message)| {
                // The icon says passed or failed by shape; the text says it in words.
                footer.child(
                    div()
                        .id("zenith-deploy-status-line")
                        .role(Role::Status)
                        .aria_label(message.clone())
                        .child(
                            h_flex()
                                .debug_selector(|| "zenith-deploy-status".to_string())
                                .gap_1()
                                .child(
                                    div()
                                        .flex_none()
                                        .child(Icon::new(icon).color(color).size(IconSize::Small)),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "zenith-deploy-status-text".to_string())
                                        .min_w_0()
                                        .child(Label::new(message).color(color)),
                                ),
                        ),
                )
            })
            .when_some(self.error.clone(), |footer, error| {
                footer.child(
                    div()
                        .id("zenith-deploy-error")
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .child(Label::new(error).color(Color::Error)),
                )
            })
            .when(self.saving, |footer| {
                footer.child(
                    div()
                        .id("zenith-deploy-saving-line")
                        .role(Role::Status)
                        .aria_label("Saving…")
                        .debug_selector(|| "zenith-deploy-saving".to_string())
                        .child(Label::new("Saving…").color(Color::Muted)),
                )
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(Self::pressed(
                        |this, _, cx| this.test_connection(cx),
                        |on_click| {
                            Button::new("zenith-deploy-test", "Test Connection")
                                .tab_index(0isize)
                                .disabled(self.selected.is_none())
                                .on_click(on_click)
                        },
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Self::pressed(
                                |this, window, cx| this.cancel(&menu::Cancel, window, cx),
                                |on_click| {
                                    Button::new("zenith-deploy-cancel", "Cancel")
                                        .tab_index(0isize)
                                        .on_click(on_click)
                                },
                                cx,
                            ))
                            // Enter on OK is the dialog's own confirm: it saves.
                            .child(
                                Button::new("zenith-deploy-ok", "OK")
                                    .style(ButtonStyle::Filled)
                                    .tab_index(0isize)
                                    .disabled(!can_save)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            )
    }
}

impl Render for DeploymentDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.selected_server().cloned() {
            Some(server) => match self.tab {
                DialogTab::Connection => self.render_form(&server, window, cx).into_any_element(),
                DialogTab::Mappings => self.render_mappings(window, cx).into_any_element(),
                DialogTab::ExcludedPaths => self.render_excluded(window, cx).into_any_element(),
            },
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new(if self.loaded {
                        "Select a server or add one."
                    } else {
                        "Loading servers…"
                    })
                    .color(Color::Muted),
                )
                .into_any_element(),
        };
        let panel = v_flex()
            .id("zenith-deploy-tab-panel")
            .role(Role::TabPanel)
            .aria_label(self.tab.label())
            .flex_1()
            .min_w_0()
            .child(content);
        // Never wider than the window: a percentage here would resolve against
        // the modal layer's shrink-wrapped row, not the window.
        let width = rems(48.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().width * 0.95);
        v_flex()
            .debug_selector(|| "zenith-deploy-dialog".to_string())
            .key_context("DeploymentDialog")
            // One tab group for the whole dialog: its controls (all tab index 0) go in document
            // order, and `tab_group` gives the container index 0, which is what keeps them
            // together in the window's tab order.
            .tab_group()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            // The base keymap's Tab is `menu::SelectNext`, which does nothing here:
            // `assets/keymaps/zenith.json` binds these two inside the dialog, editors included.
            .on_action(cx.listener(|this, _: &FocusNextControl, window, cx| {
                this.move_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPreviousControl, window, cx| {
                this.move_focus(false, window, cx)
            }))
            .elevation_3(cx)
            .w(width)
            .p_3()
            .gap_2()
            .child(Label::new("Deployment").weight(gpui::FontWeight::BOLD))
            .child(
                h_flex()
                    .items_start()
                    .gap_3()
                    .child(self.render_server_list(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(self.render_tabs(cx))
                            .child(panel),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeRemote, init, server, workspace};
    use gpui::{TestAppContext, VisualTestContext, size};

    async fn open_dialog<'a>(
        servers: Servers,
        remote: &FakeRemote,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        &'a mut VisualTestContext,
    ) {
        let cache = tempfile::tempdir().expect("tempdir");
        open_dialog_in(servers, remote, cache.path().to_path_buf(), cx).await
    }

    async fn open_dialog_in<'a>(
        servers: Servers,
        remote: &FakeRemote,
        cache_dir: std::path::PathBuf,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        &'a mut VisualTestContext,
    ) {
        let app_state = init(cx, servers, remote, cache_dir);
        let (workspace, cx) = workspace(&app_state, cx).await;
        cx.dispatch_action(OpenDeployment);
        cx.run_until_parked();
        let dialog = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens as a modal");
        (workspace, dialog, cx)
    }

    #[gpui::test]
    async fn add_remove_and_default_server(cx: &mut TestAppContext) {
        let (workspace, dialog, cx) =
            open_dialog(Servers::default(), &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.add_server(window, cx);
            dialog.add_server(window, cx);
        });
        let (names, selected) = dialog.read_with(cx, |dialog, _| {
            let names: Vec<_> = dialog
                .servers()
                .servers
                .iter()
                .map(|server| server.name.clone())
                .collect();
            (names, dialog.selected_server().cloned().expect("selected"))
        });
        assert_eq!(names, ["Server 1", "Server 2"]);
        assert_eq!(
            selected.server_type,
            ServerType::Ftps,
            "FTPS is the default"
        );
        assert_eq!(selected.port, 21);

        dialog.update(cx, |dialog, cx| dialog.set_default(cx));
        let defaults = dialog.read_with(cx, |dialog, _| dialog.servers().project_defaults.clone());
        assert_eq!(
            defaults.get("/p").map(String::as_str),
            Some(selected.id.as_str())
        );

        dialog.update_in(cx, |dialog, window, cx| dialog.remove_server(window, cx));
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.servers().servers.len(), 1);
            assert!(
                dialog.servers().project_defaults.is_empty(),
                "a removed server is no project's default"
            );
            assert_eq!(
                dialog.selected_server().map(|server| server.name.as_str()),
                Some("Server 1")
            );
        });

        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(saved.servers.len(), 1);
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none(),
            "OK closes the dialog"
        );
    }

    #[gpui::test]
    async fn username_warning_and_insecure_ftp_callout(cx: &mut TestAppContext) {
        let (_, dialog, cx) = open_dialog(Servers::default(), &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| dialog.add_server(window, cx));
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.warnings()),
            [NO_USERNAME]
        );
        assert!(cx.debug_bounds("zenith-deploy-no-username").is_some());
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_none());

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.set_type(ServerType::Ftp, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_some());
        assert!(
            dialog
                .read_with(cx, |dialog, _| dialog.warnings())
                .contains(&INSECURE_FTP)
        );

        let username = dialog.read_with(cx, |dialog, _| dialog.fields.username.clone());
        username.update_in(cx, |editor, window, cx| {
            editor.set_text("deploy", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog
                .selected_server()
                .map(|server| server.username.clone())),
            Some("deploy".to_string())
        );
        assert!(cx.debug_bounds("zenith-deploy-no-username").is_none());

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.set_type(ServerType::Sftp, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-deploy-insecure").is_none());
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.selected_server().map(|s| s.port)),
            Some(22),
            "the port follows the type while it is the default"
        );
    }

    #[gpui::test]
    async fn test_connection_uses_the_typed_password_and_reports_the_result(
        cx: &mut TestAppContext,
    ) {
        let remote = FakeRemote::default();
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        let password = dialog.read_with(cx, |dialog, _| dialog.fields.password.clone());
        password.update_in(cx, |editor, window, cx| {
            editor.set_text("s3cret", window, cx)
        });
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        assert_eq!(
            remote.log(),
            [
                "connect live.example.com password=Some(\"s3cret\")",
                "disconnect"
            ]
        );
        dialog.read_with(cx, |dialog, _| match &dialog.test_status {
            Some(TestStatus::Passed(message)) => assert!(message.contains("/www"), "{message}"),
            _ => panic!("expected a passed test"),
        });
    }

    /// Plainly fake; built here so no test spells a secret literal.
    fn fake_secret() -> String {
        format!("not-a-real-value-{}", 1)
    }

    #[gpui::test]
    async fn saving_a_changed_target_without_a_typed_secret_forgets_the_session_secret(
        cx: &mut TestAppContext,
    ) {
        let mut saved = server("Live", None);
        saved.save_password = false;
        let saved_server = saved.clone();
        let servers = Servers {
            servers: vec![saved],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| {
            store.set_session_password(&saved_server, Some(Password(fake_secret())))
        });
        let usable = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store.session_password_for(&store.servers().servers[0])
            })
        };

        // A rename keeps the target: the secret stays.
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "Renamed".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(usable(cx).is_some());

        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(
            usable(cx).is_none(),
            "the secret typed for the old host is not usable at the new one"
        );
        dialog.read_with(cx, |dialog, _| {
            let message = dialog.error.as_ref().map(|error| error.to_string());
            assert!(
                message.is_some_and(|message| message.contains("password was removed")),
                "the user is told why"
            );
        });
    }

    /// Two saved servers: `edited` (host changed, a password typed, saved with the given
    /// `save_password`) and `removed`, whose Keychain delete fails. Returns the Save outcome.
    async fn aborted_save(
        save_password: bool,
        cx: &mut TestAppContext,
    ) -> (
        Entity<DeploymentDialog>,
        Entity<ServerStore>,
        std::sync::Arc<crate::secrets::fake::FakeKeychain>,
        ServerConfig,
        &mut VisualTestContext,
    ) {
        let mut edited = server("Live", None);
        edited.save_password = save_password;
        let doomed = server("Doomed", None);
        let servers = Servers {
            servers: vec![edited.clone(), doomed.clone()],
            ..Servers::default()
        };
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        keychain.put(&edited, "not-a-real-value-0");
        keychain.put(&doomed, "not-a-real-value-0");
        *keychain.fail_delete.lock().expect("lock") = true;
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| store.keychain = keychain.clone());
        dialog.update(cx, |dialog, cx| {
            dialog
                .servers
                .servers
                .retain(|server| server.id != doomed.id);
            dialog.removed.push(doomed.id.clone());
            dialog.servers.servers[0].host = "elsewhere.example.com".into();
            dialog.passwords.insert(edited.id.clone(), fake_secret());
            dialog.save(cx);
        });
        cx.run_until_parked();
        (dialog, store, keychain, edited, cx)
    }

    #[gpui::test]
    async fn an_aborted_save_leaves_the_old_target_without_a_secret_for_the_new_one(
        cx: &mut TestAppContext,
    ) {
        let (dialog, store, _, edited, cx) = aborted_save(false, cx).await;
        let (host, usable) = store.read_with(cx, |store, _| {
            let saved = &store.servers().servers[0];
            (saved.host.clone(), store.session_password_for(saved))
        });
        assert_eq!(host, edited.host, "the old target is still the saved one");
        assert!(
            usable.is_none(),
            "the new target's secret is not used at the old one"
        );
        dialog.read_with(cx, |dialog, _| {
            let message = dialog.error.as_ref().map(|error| error.to_string());
            assert!(
                message.is_some_and(|message| message.contains("Nothing was saved")),
                "the user is told the save was aborted"
            );
        });
    }

    #[gpui::test]
    async fn a_save_with_an_unusable_mapping_never_touches_the_keychain(cx: &mut TestAppContext) {
        let mut saved = server("Live", None);
        saved.root_path = "/var/www".into();
        saved.mappings = vec![crate::config::Mapping {
            local: "/proj".into(),
            remote: "t".into(),
        }];
        let servers = Servers {
            servers: vec![saved.clone()],
            ..Servers::default()
        };
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        keychain.put(&saved, "not-a-real-value-0");
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| store.keychain = keychain.clone());
        // Each of these would change the Keychain before the store refuses the root: a typed
        // password writes an entry; a changed host (nothing typed) and a turned-off
        // "save password" delete the saved one.
        type Edit = fn(&mut DeploymentDialog);
        let cases: [(&str, Edit); 3] = [
            ("typed password", |dialog| {
                let id = dialog.servers.servers[0].id.clone();
                dialog.passwords.insert(id, fake_secret());
            }),
            ("host change, nothing typed", |dialog| {
                dialog.servers.servers[0].host = "elsewhere.example.com".into();
            }),
            ("save password off", |dialog| {
                dialog.servers.servers[0].save_password = false;
            }),
        ];
        for (case, edit) in cases {
            dialog.update(cx, |dialog, cx| {
                dialog.servers = dialog.base.clone();
                dialog.passwords.clear();
                dialog.servers.servers[0].root_path = "relative".into();
                edit(dialog);
                dialog.save(cx);
            });
            cx.run_until_parked();
            let message = error_text(&dialog, cx);
            assert!(
                message.contains("Live") && message.contains("root"),
                "{case}: {message}"
            );
            assert_eq!(
                keychain.account_of(&saved),
                Some(crate::secrets::target_account(&saved)),
                "{case}: the saved entry is untouched"
            );
            let entries = keychain.entries.lock().expect("lock");
            assert_eq!(entries.len(), 1, "{case}: no entry written");
            assert_eq!(
                entries.values().next().map(|(_, secret)| secret.as_slice()),
                Some(b"not-a-real-value-0".as_slice()),
                "{case}: the saved secret is untouched"
            );
        }
        dialog.update(cx, |dialog, _| {
            dialog.servers = dialog.base.clone();
            dialog.passwords.clear();
        });
        dialog.read_with(cx, |dialog, _| assert!(!dialog.saving));
        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].host, saved.host);
            // `begin_save` was never taken: a corrected save can start.
        });
        dialog.update(cx, |dialog, cx| {
            dialog.servers.servers[0].root_path = "/var/www2".into();
            dialog.save(cx);
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].root_path, "/var/www2");
        });
    }

    #[gpui::test]
    async fn an_aborted_save_leaves_the_saved_keychain_entry_alone(cx: &mut TestAppContext) {
        let (_, _, keychain, edited, _) = aborted_save(true, cx).await;
        assert_eq!(
            keychain.account_of(&edited),
            Some(crate::secrets::target_account(&edited)),
            "the working saved password still belongs to the saved target"
        );
    }

    #[gpui::test]
    async fn no_note_is_shown_when_there_was_no_password_to_remove(cx: &mut TestAppContext) {
        let mut unsaved = server("Live", None);
        unsaved.save_password = false;
        let servers = Servers {
            servers: vec![unsaved],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| assert!(dialog.error.is_none()));
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none()
        );
    }

    #[gpui::test]
    async fn changing_the_target_of_a_passwordless_server_saves_quietly(cx: &mut TestAppContext) {
        let mut anonymous = server("Live", None);
        anonymous.anonymous = true;
        let servers = Servers {
            servers: vec![anonymous],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| assert!(dialog.error.is_none()));
        assert!(
            workspace
                .read_with(cx, |workspace, cx| workspace
                    .active_modal::<DeploymentDialog>(cx))
                .is_none(),
            "no password was removed, so there is nothing to tell the user"
        );
    }

    #[gpui::test]
    async fn test_connection_never_sends_the_saved_secret_to_a_changed_target(
        cx: &mut TestAppContext,
    ) {
        let remote = FakeRemote::default();
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let saved_server = servers.servers[0].clone();
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| {
            store.set_session_password(&saved_server, Some(Password(fake_secret())))
        });
        let original = dialog.read_with(cx, |dialog, _| dialog.selected_server().cloned().unwrap());

        // Control: the saved target still gets the stored secret.
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        let log = remote.log();
        assert_eq!(log.len(), 2, "connect and disconnect");
        assert!(
            log[0].contains(&fake_secret()),
            "the connect carried the secret: {log:?}"
        );
        remote.log.lock().expect("lock").clear();

        let changes: [fn(&mut ServerConfig); 4] = [
            |server| server.host = "elsewhere.example.com".into(),
            |server| server.port += 1,
            |server| server.server_type = ServerType::Ftp,
            |server| server.username = "someone-else".into(),
        ];
        for change in changes {
            // Each case starts from the saved server: changes must not stack.
            dialog.update(cx, |dialog, cx| {
                dialog.update_selected(cx, |server| *server = original.clone());
                dialog.update_selected(cx, change);
                dialog.test_connection(cx);
            });
            cx.run_until_parked();
            dialog.read_with(cx, |dialog, _| match &dialog.test_status {
                Some(TestStatus::Failed(message)) => {
                    assert!(message.contains("Type the password"), "{message}")
                }
                _ => panic!("expected the changed target to be refused"),
            });
            assert!(
                remote.log().is_empty(),
                "nothing connects: {:?}",
                remote.log()
            );
        }
    }

    #[gpui::test]
    async fn removing_a_server_deletes_its_cached_copies(cx: &mut TestAppContext) {
        let cache = tempfile::tempdir().expect("tempdir");
        let live = server("Live", None);
        let cached = cache.path().join(&live.id);
        std::fs::create_dir_all(&cached).expect("mkdir");
        std::fs::write(cached.join("wp-config.php"), "<?php").expect("write");
        let servers = Servers {
            servers: vec![live],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog_in(
            servers,
            &FakeRemote::default(),
            cache.path().to_path_buf(),
            cx,
        )
        .await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_server(window, cx);
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(!cached.exists());
    }

    #[gpui::test]
    async fn a_failed_test_connection_shows_the_error(cx: &mut TestAppContext) {
        let remote = FakeRemote {
            fail_connect: true,
            ..FakeRemote::default()
        };
        let mut live = server("Live", None);
        live.anonymous = true;
        let servers = Servers {
            servers: vec![live],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &remote, cx).await;
        dialog.update(cx, |dialog, cx| dialog.test_connection(cx));
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| match &dialog.test_status {
            Some(TestStatus::Failed(message)) => {
                assert!(message.contains("530 Login incorrect"), "{message}")
            }
            _ => panic!("expected a failed test"),
        });
    }

    fn is_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .is_some()
    }

    fn error_text(dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext) -> String {
        dialog.read_with(cx, |dialog, _| {
            dialog
                .error
                .as_ref()
                .map(|error| error.to_string())
                .unwrap_or_default()
        })
    }

    /// Like [`open_dialog`], on a store the test builds (a real file, a failing disk).
    async fn open_dialog_on(
        servers: Servers,
        make_store: impl FnOnce(&mut Context<ServerStore>) -> ServerStore,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        Entity<ServerStore>,
        &mut VisualTestContext,
    ) {
        let app_state = init(
            cx,
            servers,
            &FakeRemote::default(),
            std::path::PathBuf::from("/nonexistent-cache"),
        );
        let store = cx.update(|cx| {
            let store = cx.new(make_store);
            cx.set_global(crate::GlobalServerStore(store.clone()));
            store
        });
        let (workspace, cx) = workspace(&app_state, cx).await;
        cx.dispatch_action(OpenDeployment);
        cx.run_until_parked();
        let dialog = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens as a modal");
        (workspace, dialog, store, cx)
    }

    /// An in-memory store whose servers.json path is `path`.
    fn store_at(
        servers: Servers,
        path: std::path::PathBuf,
    ) -> impl FnOnce(&mut Context<ServerStore>) -> ServerStore {
        move |_| {
            let mut store = ServerStore::in_memory(
                servers,
                std::path::PathBuf::from("/nonexistent-cache"),
                FakeRemote::default().connect_fn(),
            );
            store.path = Some(path);
            store
        }
    }

    /// A path whose parent is a file: writing servers.json there fails.
    fn unwritable_path(dir: &std::path::Path) -> std::path::PathBuf {
        let file = dir.join("not-a-directory");
        std::fs::write(&file, "x").expect("write");
        file.join("servers.json")
    }

    fn types_into(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    }

    fn text_of(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    #[gpui::test]
    async fn saving_with_a_damaged_servers_file_writes_nothing_and_says_why(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        std::fs::write(&path, "{not json").expect("write");
        let load_path = path.clone();
        let cache = dir.path().join("cache");
        let (workspace, dialog, store, cx) = open_dialog_on(
            Servers::default(),
            move |cx| ServerStore::load(load_path, cache, cx),
            cx,
        )
        .await;
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        store.update(cx, |store, _| store.keychain = keychain.clone());

        dialog.update_in(cx, |dialog, window, cx| {
            dialog.add_server(window, cx);
            let id = dialog.selected_server().expect("selected").id.clone();
            dialog.passwords.insert(id, fake_secret());
            dialog.save(cx);
        });
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "{not json",
            "the damaged file is left for the user to repair"
        );
        assert!(
            keychain.entries.lock().expect("lock").is_empty(),
            "no password is stored for a server that cannot be saved"
        );
        assert!(
            error_text(&dialog, cx).contains("could not be read"),
            "{}",
            error_text(&dialog, cx)
        );
        assert!(is_open(&workspace, cx), "the dialog stays open");
        store.read_with(cx, |store, _| assert!(store.servers().servers.is_empty()));
    }

    #[gpui::test]
    async fn the_store_never_overwrites_a_damaged_servers_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        std::fs::write(&path, "{not json").expect("write");
        let load_path = path.clone();
        let cache = dir.path().join("cache");
        let store = cx.new(|cx| ServerStore::load(load_path, cache, cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| assert!(store.load_error().is_some()));

        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let result = store
            .update(cx, |store, cx| {
                store.save(
                    Servers::default(),
                    servers,
                    Vec::new(),
                    &Default::default(),
                    cx,
                )
            })
            .await;
        let error = result.expect_err("saving over a damaged file must fail");
        assert!(error.to_string().contains("not overwritten"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{not json");
    }

    #[gpui::test]
    async fn switching_servers_keeps_each_servers_own_fields(cx: &mut TestAppContext) {
        let mut first = server("First", None);
        first.port = 2121;
        first.root_path = "/one".into();
        first.web_url = "https://one.example.com".into();
        let mut second = server("Second", None);
        second.server_type = ServerType::Sftp;
        second.port = 2222;
        second.username = "other".into();
        second.root_path = "/two".into();
        second.web_url = "https://two.example.com".into();
        let servers = Servers {
            servers: vec![first.clone(), second.clone()],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let fields = |dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext| {
            dialog.read_with(cx, |dialog, _| {
                let f = &dialog.fields;
                [
                    f.name.clone(),
                    f.host.clone(),
                    f.port.clone(),
                    f.username.clone(),
                    f.password.clone(),
                    f.root_path.clone(),
                    f.web_url.clone(),
                ]
            })
        };
        let shown = |dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext| {
            fields(dialog, cx).map(|editor| text_of(&editor, cx))
        };

        let [name, host, _, _, password, ..] = fields(&dialog, cx);
        types_into(&name, "First renamed", cx);
        types_into(&host, "edited.example.com", cx);
        types_into(&password, &fake_secret(), cx);

        dialog.update_in(cx, |dialog, window, cx| dialog.select(Some(1), window, cx));
        assert_eq!(
            shown(&dialog, cx),
            [
                "Second",
                "second.example.com",
                "2222",
                "other",
                "",
                "/two",
                "https://two.example.com"
            ],
            "the second server shows its own fields and no password of the first"
        );
        dialog.update_in(cx, |dialog, window, cx| dialog.select(Some(0), window, cx));
        assert_eq!(
            shown(&dialog, cx),
            [
                "First renamed",
                "edited.example.com",
                "2121",
                "deploy",
                &fake_secret(),
                "/one",
                "https://one.example.com"
            ],
            "the first server keeps what was typed for it"
        );
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.servers().servers[1], second, "never touched");
            let edited = &dialog.servers().servers[0];
            assert_eq!(edited.name, "First renamed");
            assert_eq!(edited.host, "edited.example.com");
            assert_eq!(edited.port, 2121);
            assert_eq!(edited.web_url, first.web_url);
            assert_eq!(dialog.passwords.get(&second.id), None);
        });
    }

    /// One saved server `Live` that the dialog renames and gives a typed password.
    async fn rename_and_type_a_password(
        save_password: bool,
        servers_path: std::path::PathBuf,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<DeploymentDialog>,
        Entity<ServerStore>,
        ServerConfig,
        std::sync::Arc<crate::secrets::fake::FakeKeychain>,
        &mut VisualTestContext,
    ) {
        let mut saved = server("Live", None);
        saved.save_password = save_password;
        let servers = Servers {
            servers: vec![saved.clone()],
            ..Servers::default()
        };
        let (workspace, dialog, store, cx) =
            open_dialog_on(servers.clone(), store_at(servers, servers_path), cx).await;
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        store.update(cx, |store, _| store.keychain = keychain.clone());
        dialog.update(cx, |dialog, _| {
            dialog.passwords.insert(saved.id.clone(), fake_secret());
        });
        (workspace, dialog, store, saved, keychain, cx)
    }

    #[gpui::test]
    async fn a_failing_keychain_write_reports_an_error_and_leaves_the_store(
        cx: &mut TestAppContext,
    ) {
        let (workspace, dialog, store, saved, keychain, cx) =
            rename_and_type_a_password(true, "/unused".into(), cx).await;
        *keychain.fail_write.lock().expect("lock") = true;
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "Renamed".into());
            dialog.save(cx);
        });
        cx.run_until_parked();

        let message = error_text(&dialog, cx);
        assert!(
            message.contains("Nothing was saved") && message.contains("could not store"),
            "{message}"
        );
        assert!(keychain.entries.lock().expect("lock").is_empty());
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.servers().servers[0].name,
                "Live",
                "the store is unchanged"
            );
            assert_eq!(store.session_password_for(&saved), None);
        });
        assert!(is_open(&workspace, cx), "the dialog stays open to retry");
        dialog.read_with(cx, |dialog, _| assert!(!dialog.saving, "OK works again"));
    }

    #[gpui::test]
    async fn a_failing_disk_write_after_the_keychain_writes_changes_nothing_but_the_keychain(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = unwritable_path(dir.path());
        let (workspace, dialog, store, saved, keychain, cx) =
            rename_and_type_a_password(true, bad, cx).await;
        let mut moved = saved.clone();
        moved.host = "elsewhere.example.com".into();
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();

        let message = error_text(&dialog, cx);
        assert!(message.contains("Saving failed"), "{message}");
        assert!(
            message.contains("1 Keychain change(s) were already made"),
            "the user learns the Keychain already changed: {message}"
        );
        assert!(is_open(&workspace, cx), "the dialog stays open to retry");
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.servers().servers[0].host,
                saved.host,
                "servers unchanged"
            );
        });
        assert_eq!(
            keychain.account_of(&saved),
            Some(crate::secrets::target_account(&moved)),
            "the Keychain write is not rolled back; it is the password for the retry"
        );

        // Retry once the disk works: the same dialog saves and closes.
        let good = dir.path().join("servers.json");
        store.update(cx, |store, _| store.path = Some(good.clone()));
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        assert!(!is_open(&workspace, cx));
        let on_disk = Servers::load(&good).expect("servers.json written");
        assert_eq!(on_disk.servers[0].host, "elsewhere.example.com");
    }

    #[gpui::test]
    async fn a_failed_save_keeps_the_session_password_of_the_old_target(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = unwritable_path(dir.path());
        let (_, dialog, store, saved, _, cx) = rename_and_type_a_password(false, bad, cx).await;
        let old_secret = Password("not-a-real-value-0".into());
        store.update(cx, |store, _| {
            store.set_session_password(&saved, Some(old_secret.clone()))
        });
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
        });
        cx.run_until_parked();

        assert!(error_text(&dialog, cx).contains("Saving failed"));
        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].host, saved.host);
            assert_eq!(
                store.session_password_for(&store.servers().servers[0]),
                Some(old_secret),
                "the password typed for the saved target still works there"
            );
        });

        // The session password of the new target is applied only by a save that worked.
        let good = dir.path().join("servers.json");
        store.update(cx, |store, _| store.path = Some(good));
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            let moved = &store.servers().servers[0];
            assert_eq!(moved.host, "elsewhere.example.com");
            assert_eq!(
                store.session_password_for(moved),
                Some(Password(fake_secret()))
            );
        });
    }

    #[gpui::test]
    async fn a_partly_applied_keychain_change_is_not_reported_as_nothing_saved(
        cx: &mut TestAppContext,
    ) {
        let kept = server("Kept", None);
        let gone = server("Gone", None);
        let stuck = server("Stuck", None);
        let servers = Servers {
            servers: vec![kept.clone(), gone.clone(), stuck.clone()],
            ..Servers::default()
        };
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        keychain.put(&gone, "not-a-real-value-0");
        keychain.put(&stuck, "not-a-real-value-0");
        keychain.fail_delete_of(&stuck);
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| store.keychain = keychain.clone());
        dialog.update(cx, |dialog, cx| {
            dialog.servers.servers.retain(|server| server.id == kept.id);
            dialog.removed = vec![gone.id.clone(), stuck.id.clone()];
            dialog.save(cx);
        });
        cx.run_until_parked();

        let message = error_text(&dialog, cx);
        assert!(!message.contains("Nothing was saved"), "{message}");
        assert!(message.contains("1 Keychain change(s)"), "{message}");
        assert!(message.contains("were not saved"), "{message}");
        assert!(
            keychain.account_of(&gone).is_none(),
            "the first delete happened"
        );
        assert!(keychain.account_of(&stuck).is_some());
        store.read_with(cx, |store, _| assert_eq!(store.servers().servers.len(), 3));
    }

    /// The form column must stay inside the dialog (it once ran ~67 pt past
    /// the right edge because a flex child could not shrink below its content).
    #[gpui::test]
    async fn the_form_stays_inside_the_dialog(cx: &mut TestAppContext) {
        const ROWS: [&str; 7] = [
            "Name",
            "Host",
            "Port",
            "Username",
            "Password",
            "Root path",
            "Web URL",
        ];
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;

        let failed: SharedString =
            format!("Connection failed: {}", "the server hung up; ".repeat(7)).into();
        let fixed: SharedString = "Type the password to test a changed host, port, type or username; the saved one is not sent to it.".into();
        // (variant, type, save password, advanced open, test status)
        let variants = [
            ("FTPS", ServerType::Ftps, true, false, None),
            ("FTP", ServerType::Ftp, true, false, None),
            ("FTP, password removed", ServerType::Ftp, false, false, None),
            ("FTP, advanced open", ServerType::Ftp, true, true, None),
            ("SFTP", ServerType::Sftp, true, true, None),
            (
                "FTP, long failure",
                ServerType::Ftp,
                true,
                false,
                Some(TestStatus::Failed(failed)),
            ),
            (
                "FTP, long fixed message",
                ServerType::Ftp,
                true,
                false,
                Some(TestStatus::Failed(fixed)),
            ),
        ];
        let mut problems = Vec::new();
        for window_size in [
            None,
            Some(size(px(900.), px(700.))),
            Some(size(px(600.), px(700.))),
        ] {
            if let Some(window_size) = window_size {
                cx.simulate_resize(window_size);
            }
            let viewport = cx.update(|window, _| window.viewport_size());
            for (variant, server_type, save_password, advanced, status) in variants.clone() {
                let has_status = status.is_some();
                dialog.update(cx, |dialog, cx| {
                    dialog.advanced_open = advanced;
                    dialog.test_status = status;
                    dialog.update_selected(cx, |server| {
                        server.server_type = server_type;
                        server.save_password = save_password;
                    })
                });
                cx.run_until_parked();
                let dialog_bounds = cx
                    .debug_bounds("zenith-deploy-dialog")
                    .expect("the dialog is drawn");
                let limit = dialog_bounds.right();
                if limit > viewport.width || dialog_bounds.left() < px(0.) {
                    problems.push(format!(
                        "window {:?} {variant}: dialog {:?}..{limit:?} is outside the window",
                        viewport.width,
                        dialog_bounds.left()
                    ));
                }
                // (selector, must be drawn): a renamed selector fails loudly.
                let mut ids: Vec<(String, bool)> = vec![("zenith-deploy-form".into(), true)];
                for label in ROWS {
                    // SFTP signs in with a key: no password row.
                    let drawn = !(label == "Password" && server_type == ServerType::Sftp);
                    ids.push((format!("zenith-deploy-row-{label}"), drawn));
                    ids.push((format!("zenith-deploy-field-{label}"), drawn));
                }
                ids.push((
                    "zenith-deploy-insecure".into(),
                    server_type == ServerType::Ftp,
                ));
                ids.push((
                    "zenith-deploy-password-removed".into(),
                    !save_password || server_type == ServerType::Sftp,
                ));
                // Drawn only while Advanced is open on an FTP or FTPS server.
                ids.push((
                    "zenith-deploy-list-compat".into(),
                    advanced && server_type != ServerType::Sftp,
                ));
                ids.push(("zenith-deploy-status".into(), has_status));
                ids.push(("zenith-deploy-status-text".into(), has_status));
                for (id, drawn) in ids {
                    // Selectors are 'static; leaking a few test strings is fine.
                    let bounds = cx.debug_bounds(Box::leak(id.clone().into_boxed_str()));
                    if bounds.is_some() != drawn {
                        problems.push(format!(
                            "{variant}: {id} drawn = {}, expected {drawn}",
                            bounds.is_some()
                        ));
                    }
                    let Some(bounds) = bounds else { continue };
                    if bounds.right() > limit + px(0.5) {
                        problems.push(format!(
                            "window {:?} {variant}: {id} right {:?} (width {:?}) is past the dialog edge {limit:?}",
                            viewport.width,
                            bounds.right(),
                            bounds.size.width
                        ));
                    }
                }
            }
            // The tabs: the strip is always drawn, each panel only on its own tab. Two mappings
            // with long folders must wrap or shrink, not run past the edge.
            dialog.update_in(cx, |dialog, window, cx| {
                dialog.tab = DialogTab::Connection;
                dialog.update_selected(cx, |server| server.server_type = ServerType::Ftps);
                for _ in 0..2 {
                    dialog.add_mapping(window, cx);
                }
                for row in &dialog.mapping_rows {
                    let long = "a-folder-with-a-rather-long-name/".repeat(6);
                    row.local.update(cx, |editor, cx| {
                        editor.set_text(format!("/p/{long}"), window, cx)
                    });
                    row.remote
                        .update(cx, |editor, cx| editor.set_text(long.clone(), window, cx));
                }
            });
            for tab in DialogTab::ALL {
                dialog.update(cx, |dialog, cx| dialog.set_tab(tab, cx));
                cx.run_until_parked();
                let limit = cx
                    .debug_bounds("zenith-deploy-dialog")
                    .expect("the dialog is drawn")
                    .right();
                let mut ids: Vec<(String, bool)> = vec![("zenith-deploy-tabs".into(), true)];
                for other in DialogTab::ALL {
                    ids.push((other.id().into(), true));
                }
                ids.push(("zenith-deploy-form".into(), tab == DialogTab::Connection));
                let mappings = tab == DialogTab::Mappings;
                ids.push(("zenith-deploy-mappings".into(), mappings));
                for index in 0..2 {
                    for part in ["mapping", "map-local", "map-remote"] {
                        ids.push((format!("zenith-deploy-{part}-{index}"), mappings));
                    }
                }
                let excluded = tab == DialogTab::ExcludedPaths;
                for id in ["excluded", "excluded-field", "excluded-fixed"] {
                    ids.push((format!("zenith-deploy-{id}"), excluded));
                }
                for (id, drawn) in ids {
                    let bounds = cx.debug_bounds(Box::leak(id.clone().into_boxed_str()));
                    if bounds.is_some() != drawn {
                        problems.push(format!(
                            "{tab:?}: {id} drawn = {}, expected {drawn}",
                            bounds.is_some()
                        ));
                    }
                    if let Some(bounds) = bounds
                        && bounds.right() > limit + px(0.5)
                    {
                        problems.push(format!(
                            "window {:?} {tab:?}: {id} right {:?} is past the dialog edge {limit:?}",
                            viewport.width,
                            bounds.right()
                        ));
                    }
                }
            }
            dialog.update_in(cx, |dialog, window, cx| {
                dialog.tab = DialogTab::Connection;
                dialog.update_selected(cx, |server| server.mappings.clear());
                dialog.rebuild_mapping_rows(window, cx);
            });
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    fn set_text(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
        editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn switching_tabs_keeps_edits(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let (host, excluded) = dialog.read_with(cx, |dialog, _| {
            (dialog.fields.host.clone(), dialog.excluded.clone())
        });
        set_text(&host, "edited.example.com", cx);
        dialog.update(cx, |dialog, cx| dialog.set_tab(DialogTab::Mappings, cx));
        dialog.update_in(cx, |dialog, window, cx| dialog.add_mapping(window, cx));
        cx.run_until_parked();
        let remote = dialog.read_with(cx, |dialog, _| dialog.mapping_rows[0].remote.clone());
        set_text(&remote, "sub", cx);
        dialog.update(cx, |dialog, cx| {
            dialog.set_tab(DialogTab::ExcludedPaths, cx)
        });
        set_text(&excluded, "vendor\n*.log", cx);

        for tab in [
            DialogTab::Connection,
            DialogTab::Mappings,
            DialogTab::ExcludedPaths,
            DialogTab::Connection,
        ] {
            dialog.update(cx, |dialog, cx| dialog.set_tab(tab, cx));
            cx.run_until_parked();
            assert_eq!(text_of(&host, cx), "edited.example.com", "{tab:?}");
            assert_eq!(text_of(&remote, cx), "sub", "{tab:?}");
            assert_eq!(text_of(&excluded, cx), "vendor\n*.log", "{tab:?}");
            dialog.read_with(cx, |dialog, _| {
                let server = dialog.selected_server().expect("selected");
                assert_eq!(server.host, "edited.example.com", "{tab:?}");
                assert_eq!(
                    server.mappings,
                    [Mapping {
                        local: "/p".into(),
                        remote: "sub".into()
                    }],
                    "{tab:?}: a new mapping starts at the project folder"
                );
                assert_eq!(server.excluded_local, ["vendor", "*.log"], "{tab:?}");
            });
        }
    }

    #[gpui::test]
    async fn mappings_and_exclusions_round_trip_to_the_store(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.add_mapping(window, cx);
            dialog.add_mapping(window, cx);
        });
        cx.run_until_parked();
        let (local, remote, second, excluded) = dialog.read_with(cx, |dialog, _| {
            (
                dialog.mapping_rows[0].local.clone(),
                dialog.mapping_rows[0].remote.clone(),
                dialog.mapping_rows[1].local.clone(),
                dialog.excluded.clone(),
            )
        });
        set_text(&local, "/p/theme", cx);
        set_text(&remote, " wp-content/themes/x ", cx);
        set_text(&second, "/p/plugin", cx);
        set_text(&excluded, "vendor\n\n  *.log \n", cx);
        // A row removed before OK is not saved.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_mapping(1, window, cx)
        });
        cx.run_until_parked();
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();

        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(
            saved.servers[0].mappings,
            [Mapping {
                local: "/p/theme".into(),
                remote: "wp-content/themes/x".into()
            }]
        );
        assert_eq!(saved.servers[0].excluded_local, ["vendor", "*.log"]);

        let dialog = reopen(&workspace, cx);
        let (rows, texts) = dialog.read_with(cx, |dialog, cx| {
            let rows = dialog.mapping_rows.len();
            let texts: Vec<String> = [
                &dialog.mapping_rows[0].local,
                &dialog.mapping_rows[0].remote,
                &dialog.excluded,
            ]
            .map(|editor| editor.read(cx).text(cx))
            .into();
            (rows, texts)
        });
        assert_eq!(rows, 1);
        assert_eq!(texts, ["/p/theme", "wp-content/themes/x", "vendor\n*.log"]);
    }

    #[gpui::test]
    async fn an_invalid_glob_is_refused_before_any_keychain_call(cx: &mut TestAppContext) {
        let saved = server("Live", None);
        let servers = Servers {
            servers: vec![saved.clone()],
            ..Servers::default()
        };
        let keychain = std::sync::Arc::new(crate::secrets::fake::FakeKeychain::default());
        keychain.put(&saved, "not-a-real-value-0");
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let store = cx.update(|_, cx| ServerStore::global(cx));
        store.update(cx, |store, _| store.keychain = keychain.clone());
        let excluded = dialog.read_with(cx, |dialog, _| dialog.excluded.clone());
        set_text(&excluded, "vendor\n[unclosed", cx);
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(
                dialog.servers.servers[0].excluded_local,
                ["vendor", "[unclosed"],
                "the editor feeds the server"
            );
        });
        // Each would change the Keychain (a write, a delete) if the pattern got that far.
        type Edit = fn(&mut DeploymentDialog);
        let cases: [(&str, Edit); 3] = [
            ("typed password", |dialog| {
                let id = dialog.servers.servers[0].id.clone();
                dialog.passwords.insert(id, fake_secret());
            }),
            ("host change, nothing typed", |dialog| {
                dialog.servers.servers[0].host = "elsewhere.example.com".into();
            }),
            ("save password off", |dialog| {
                dialog.servers.servers[0].save_password = false;
            }),
        ];
        for (case, edit) in cases {
            dialog.update(cx, |dialog, cx| {
                dialog.servers = dialog.base.clone();
                dialog.passwords.clear();
                dialog.servers.servers[0].excluded_local =
                    vec!["vendor".into(), "[unclosed".into()];
                edit(dialog);
                dialog.save(cx);
            });
            cx.run_until_parked();
            let message = error_text(&dialog, cx);
            assert!(
                message.contains("Live") && message.contains("[unclosed"),
                "{case}: {message}"
            );
            assert_eq!(
                keychain.account_of(&saved),
                Some(crate::secrets::target_account(&saved)),
                "{case}: the saved entry is untouched"
            );
            let entries = keychain.entries.lock().expect("lock");
            assert_eq!(entries.len(), 1, "{case}: no entry written");
            assert_eq!(
                entries.values().next().map(|(_, secret)| secret.as_slice()),
                Some(b"not-a-real-value-0".as_slice()),
                "{case}: the saved secret is untouched"
            );
        }
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.servers().servers[0].excluded_local,
                saved.excluded_local
            );
        });
        // Corrected, the same dialog saves: nothing was left locked.
        dialog.update(cx, |dialog, _| {
            dialog.servers = dialog.base.clone();
            dialog.passwords.clear();
        });
        set_text(&excluded, "vendor\n[ab]c", cx);
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.servers().servers[0].excluded_local,
                ["vendor", "[ab]c"]
            );
        });
    }

    #[gpui::test]
    async fn changing_the_exclusions_asks_for_the_upload_confirmation_again(
        cx: &mut TestAppContext,
    ) {
        let mut confirmed = server("Live", None);
        confirmed.upload_on_save = true;
        confirmed.upload_on_save_confirmed = true;
        let servers = Servers {
            servers: vec![confirmed],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let confirmed_in_store = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                ServerStore::global(cx).read(cx).servers().servers[0].upload_on_save_confirmed
            })
        };
        // Control: an edit that is not part of the fingerprint keeps the confirmation.
        let name = dialog.read_with(cx, |dialog, _| dialog.fields.name.clone());
        set_text(&name, "Renamed", cx);
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        assert!(confirmed_in_store(cx), "a rename needs no new confirmation");

        let dialog = reopen(&workspace, cx);
        let excluded = dialog.read_with(cx, |dialog, _| dialog.excluded.clone());
        let current = text_of(&excluded, cx);
        set_text(&excluded, &format!("{current}\nstorage"), cx);
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        assert!(
            !confirmed_in_store(cx),
            "other excluded paths: the confirmation is asked for again"
        );
    }

    #[gpui::test]
    async fn browse_asks_for_one_folder_and_fills_the_local_field(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.add_mapping(window, cx);
            dialog.browse_mapping(0, window, cx);
        });
        let mut asked = None;
        cx.simulate_path_prompt_response(|options| {
            asked = Some((options.files, options.directories, options.multiple));
            Some(vec!["/p/chosen".into()])
        });
        cx.run_until_parked();
        assert_eq!(asked, Some((false, true, false)), "one folder, no files");
        let local = dialog.read_with(cx, |dialog, _| dialog.mapping_rows[0].local.clone());
        assert_eq!(text_of(&local, cx), "/p/chosen");
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(
                dialog.selected_server().expect("selected").mappings[0].local,
                "/p/chosen"
            );
        });
    }

    /// The real keymaps, so the tests press real keys: the base keymap binds a context-less
    /// Tab to `menu::SelectNext` and Enter to `menu::Confirm`.
    fn load_keymaps(cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            for json in [
                include_str!("../../../assets/keymaps/default-macos.json"),
                include_str!("../../../assets/keymaps/zenith.json"),
            ] {
                match settings::KeymapFile::load(json, cx) {
                    settings::KeymapFileLoadResult::Success { key_bindings }
                    | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => {
                        cx.bind_keys(key_bindings)
                    }
                    settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                        panic!("{error:#}")
                    }
                }
            }
        });
    }

    fn focused(cx: &mut VisualTestContext) -> Option<FocusHandle> {
        cx.update(|window, cx| window.focused(cx))
    }

    fn focus(handle: &FocusHandle, cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.focus(handle, cx));
        cx.run_until_parked();
    }

    fn focus_is_inside(dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext) -> bool {
        let handle = dialog.read_with(cx, |dialog, _| dialog.focus_handle.clone());
        cx.update(|window, cx| handle.contains_focused(window, cx))
    }

    /// Clicks each tab in turn and returns their focus handles, leaving the first one focused.
    fn tab_handles(cx: &mut VisualTestContext) -> Vec<FocusHandle> {
        let mut handles = Vec::new();
        for tab in DialogTab::ALL {
            let bounds = cx.debug_bounds(tab.id()).expect("the tab is drawn");
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            handles.push(focused(cx).expect("a click on a tab focuses it"));
        }
        let first = cx.debug_bounds(DialogTab::Connection.id()).expect("drawn");
        cx.simulate_click(first.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        handles
    }

    async fn open_with_one_server(
        cx: &mut TestAppContext,
    ) -> (Entity<DeploymentDialog>, &mut VisualTestContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        load_keymaps(cx);
        cx.run_until_parked();
        (dialog, cx)
    }

    #[gpui::test]
    async fn tab_and_shift_tab_walk_the_controls_and_wrap_inside_the_dialog(
        cx: &mut TestAppContext,
    ) {
        let (dialog, cx) = open_with_one_server(cx).await;
        let tabs = tab_handles(cx);
        assert!(
            tabs[0] != tabs[1] && tabs[1] != tabs[2] && tabs[0] != tabs[2],
            "each tab has its own focus handle"
        );
        focus(&tabs[0], cx);
        for (key, expected) in [("tab", 1), ("tab", 2), ("shift-tab", 1), ("shift-tab", 0)] {
            cx.simulate_keystrokes(key);
            assert_eq!(focused(cx).as_ref(), Some(&tabs[expected]), "{key}");
        }

        // Forward from the first tab, every control is visited once and the walk comes back to
        // the tab: Tab wraps at the end instead of leaving the dialog.
        let mut forward = vec![tabs[0].clone()];
        loop {
            cx.simulate_keystrokes("tab");
            assert!(focus_is_inside(&dialog, cx), "Tab left the dialog");
            let now = focused(cx).expect("focused");
            if now == tabs[0] {
                break;
            }
            assert!(!forward.contains(&now), "a control was visited twice");
            forward.push(now);
            assert!(forward.len() < 80, "Tab never wraps");
        }
        assert!(
            forward.len() > 15,
            "the controls are tab stops, not just the tabs: {}",
            forward.len()
        );
        // And backwards, the same controls in the reverse order.
        let mut backward = vec![tabs[0].clone()];
        for _ in 1..forward.len() {
            cx.simulate_keystrokes("shift-tab");
            assert!(focus_is_inside(&dialog, cx), "Shift-Tab left the dialog");
            backward.push(focused(cx).expect("focused"));
        }
        cx.simulate_keystrokes("shift-tab");
        assert_eq!(focused(cx).as_ref(), Some(&tabs[0]), "back at the start");
        let mut expected = forward.clone();
        expected[1..].reverse();
        assert_eq!(backward, expected);
    }

    #[gpui::test]
    async fn tab_leaves_a_text_field_and_the_multi_line_excluded_box(cx: &mut TestAppContext) {
        let (dialog, cx) = open_with_one_server(cx).await;
        let (name, excluded) = dialog.read_with(cx, |dialog, _| {
            (dialog.fields.name.clone(), dialog.excluded.clone())
        });
        set_text(&excluded, "vendor", cx);
        for tab in [DialogTab::Connection, DialogTab::ExcludedPaths] {
            dialog.update(cx, |dialog, cx| dialog.set_tab(tab, cx));
            cx.run_until_parked();
            let editor = if tab == DialogTab::Connection {
                &name
            } else {
                &excluded
            };
            for key in ["tab", "shift-tab"] {
                let handle = cx.update(|_, cx| editor.focus_handle(cx));
                focus(&handle, cx);
                let before = text_of(editor, cx);
                cx.simulate_keystrokes(key);
                assert_ne!(
                    focused(cx).as_ref(),
                    Some(&handle),
                    "{tab:?}: {key} must leave the field"
                );
                assert!(focus_is_inside(&dialog, cx), "{key} stays in the dialog");
                assert_eq!(text_of(editor, cx), before, "{key} types nothing");
            }
        }
    }

    #[gpui::test]
    async fn enter_on_a_tab_selects_it_and_does_not_save(cx: &mut TestAppContext) {
        let (dialog, cx) = open_with_one_server(cx).await;
        let name = dialog.read_with(cx, |dialog, _| dialog.fields.name.clone());
        // A save would write this name to the store and close the dialog.
        set_text(&name, "Edited", cx);
        let tabs = tab_handles(cx);
        focus(&tabs[0], cx);
        cx.simulate_keystrokes("tab");
        assert_eq!(focused(cx).as_ref(), Some(&tabs[1]));
        cx.simulate_keystrokes("enter");
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.tab, DialogTab::Mappings, "Enter selected the tab");
            assert!(!dialog.saving, "Enter did not start a save");
        });
        cx.simulate_keystrokes("tab enter");
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.tab, DialogTab::ExcludedPaths);
        });
        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(saved.servers[0].name, "Live", "nothing was saved");
        assert!(
            cx.debug_bounds("zenith-deploy-dialog").is_some(),
            "the dialog is still open"
        );
    }

    #[gpui::test]
    async fn enter_presses_the_focused_button_and_checkbox_not_ok(cx: &mut TestAppContext) {
        let (dialog, cx) = open_with_one_server(cx).await;
        let name = dialog.read_with(cx, |dialog, _| dialog.fields.name.clone());
        set_text(&name, "Edited", cx);
        let assert_open_and_unsaved = |cx: &mut VisualTestContext, what: &str| {
            assert!(
                cx.debug_bounds("zenith-deploy-dialog").is_some(),
                "{what}: the dialog is still open"
            );
            let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
            assert_eq!(saved.servers[0].name, "Live", "{what}: nothing was saved");
        };

        // A checkbox: Enter and Space toggle it. The username field comes just before it.
        let anonymous = |dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext| {
            dialog.read_with(cx, |dialog, _| {
                dialog.selected_server().expect("selected").anonymous
            })
        };
        let username = dialog.read_with(cx, |dialog, cx| dialog.fields.username.focus_handle(cx));
        focus(&username, cx);
        cx.simulate_keystrokes("tab");
        assert!(!anonymous(&dialog, cx));
        cx.simulate_keystrokes("enter");
        assert!(anonymous(&dialog, cx), "Enter toggled Anonymous login");
        assert_open_and_unsaved(cx, "checkbox");
        let space = gpui::Keystroke::parse("space").expect("keystroke");
        cx.simulate_keystrokes("space");
        cx.simulate_event(gpui::KeyUpEvent { keystroke: space });
        assert!(!anonymous(&dialog, cx), "Space toggled it back");
        assert_open_and_unsaved(cx, "checkbox");

        // Buttons: Add Mapping, then (focus is in the new row) Remove, three stops later.
        dialog.update(cx, |dialog, cx| dialog.set_tab(DialogTab::Mappings, cx));
        cx.run_until_parked();
        let add = dialog.read_with(cx, |dialog, _| dialog.add_mapping_focus.clone());
        focus(&add, cx);
        cx.simulate_keystrokes("enter");
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.mapping_rows.len(), 1, "Enter pressed Add Mapping");
        });
        assert_open_and_unsaved(cx, "Add Mapping");
        // Focus is in the new row's remote field; Remove follows it.
        cx.simulate_keystrokes("tab enter");
        dialog.read_with(cx, |dialog, _| {
            assert!(dialog.mapping_rows.is_empty(), "Enter pressed Remove");
        });
        assert_open_and_unsaved(cx, "Remove");
    }

    #[gpui::test]
    async fn enter_on_a_server_row_selects_it(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("One", None), server("Two", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        load_keymaps(cx);
        cx.run_until_parked();
        let row = cx.debug_bounds("zenith-deploy-server-0").expect("row 0");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.selected), Some(0));
        cx.simulate_keystrokes("tab enter");
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.selected),
            Some(1),
            "Tab reached the second row and Enter selected it"
        );
        assert!(
            cx.debug_bounds("zenith-deploy-dialog").is_some(),
            "Enter did not close the dialog"
        );
    }

    #[gpui::test]
    async fn adding_a_server_from_another_tab_shows_its_host_field_focused(
        cx: &mut TestAppContext,
    ) {
        let (dialog, cx) = open_with_one_server(cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.set_tab(DialogTab::ExcludedPaths, cx)
        });
        cx.run_until_parked();
        dialog.update_in(cx, |dialog, window, cx| dialog.add_server(window, cx));
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.tab),
            DialogTab::Connection
        );
        assert!(cx.debug_bounds("zenith-deploy-field-Host").is_some());
        let host = dialog.read_with(cx, |dialog, cx| dialog.fields.host.focus_handle(cx));
        assert_eq!(focused(cx), Some(host), "the focused field is on screen");
    }

    #[gpui::test]
    async fn removing_a_mapping_hands_focus_to_a_neighbour(cx: &mut TestAppContext) {
        let (dialog, cx) = open_with_one_server(cx).await;
        dialog.update(cx, |dialog, cx| dialog.set_tab(DialogTab::Mappings, cx));
        dialog.update_in(cx, |dialog, window, cx| {
            for _ in 0..3 {
                dialog.add_mapping(window, cx);
            }
        });
        cx.run_until_parked();
        let remote_of = |index: usize, cx: &mut VisualTestContext| {
            dialog.read_with(cx, |dialog, _| dialog.mapping_rows[index].remote.clone())
        };
        let handle_of = |index: usize, cx: &mut VisualTestContext| {
            let remote = remote_of(index, cx);
            cx.update(|_, cx| remote.focus_handle(cx))
        };
        let first = handle_of(0, cx);
        focus(&first, cx);
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_mapping(0, window, cx)
        });
        cx.run_until_parked();
        let new_first = handle_of(0, cx);
        assert_eq!(
            focused(cx),
            Some(new_first),
            "the row that took index 0 has focus"
        );
        assert!(focus_is_inside(&dialog, cx));

        // The last row goes: the one before it takes focus.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_mapping(1, window, cx)
        });
        cx.run_until_parked();
        let remaining = handle_of(0, cx);
        assert_eq!(focused(cx), Some(remaining));

        // The only row goes: Add Mapping takes focus.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_mapping(0, window, cx)
        });
        cx.run_until_parked();
        let add = dialog.read_with(cx, |dialog, _| dialog.add_mapping_focus.clone());
        assert_eq!(focused(cx), Some(add));
        assert!(focus_is_inside(&dialog, cx));
    }

    #[gpui::test]
    async fn a_hand_edited_pattern_list_keeps_the_upload_confirmation(cx: &mut TestAppContext) {
        let mut confirmed = server("Live", None);
        confirmed.upload_on_save = true;
        confirmed.upload_on_save_confirmed = true;
        // Blank and padded entries, as a hand edit leaves them.
        confirmed.excluded_local = vec!["vendor".into(), String::new(), "  *.log ".into()];
        let servers = Servers {
            servers: vec![confirmed.clone()],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        let saved =
            cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().servers[0].clone());
        assert!(
            saved.upload_on_save_confirmed,
            "opening and pressing OK changed nothing, so nothing is asked again"
        );
        assert_eq!(saved.excluded_local, confirmed.excluded_local);
    }

    #[gpui::test]
    async fn trimming_a_padded_pattern_stores_it_trimmed_and_asks_again(cx: &mut TestAppContext) {
        let mut padded = server("Live", None);
        padded.upload_on_save = true;
        padded.upload_on_save_confirmed = true;
        padded.excluded_local = vec!["  *.log ".into()];
        let servers = Servers {
            servers: vec![padded],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let excluded = dialog.read_with(cx, |dialog, _| dialog.excluded.clone());
        assert_eq!(
            text_of(&excluded, cx),
            "  *.log ",
            "the box shows what is stored"
        );
        set_text(&excluded, "*.log", cx);
        dialog.update(cx, |dialog, cx| dialog.save(cx));
        cx.run_until_parked();
        let saved =
            cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().servers[0].clone());
        // A padded pattern matches nothing: the trimmed one must be what is stored.
        assert_eq!(saved.excluded_local, ["*.log"]);
        assert!(
            !saved.upload_on_save_confirmed,
            "the exclusions changed: the confirmation is asked for again"
        );
    }

    #[gpui::test]
    async fn without_servers_tab_and_escape_still_reach_the_dialog(cx: &mut TestAppContext) {
        let (workspace, dialog, cx) =
            open_dialog(Servers::default(), &FakeRemote::default(), cx).await;
        load_keymaps(cx);
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        let add = dialog.read_with(cx, |dialog, _| dialog.add_server_focus.clone());
        assert!(
            focus_is_inside(&dialog, cx),
            "Tab keeps focus in the dialog"
        );
        assert_eq!(focused(cx), Some(add), "Tab reached Add Server");
        cx.simulate_keystrokes("escape");
        assert!(!is_open(&workspace, cx), "Escape closes the dialog");
    }

    #[gpui::test]
    async fn a_browse_answer_is_dropped_when_a_mapping_was_removed(cx: &mut TestAppContext) {
        let mut one = server("One", None);
        one.mappings = ["/a", "/b", "/c"]
            .map(|local| Mapping {
                local: local.into(),
                remote: local.trim_start_matches('/').into(),
            })
            .into();
        let servers = Servers {
            servers: vec![one],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        // Browse on the second row; before the answer the first row is removed, so index 1 is
        // now the third mapping.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.browse_mapping(1, window, cx)
        });
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.remove_mapping(0, window, cx)
        });
        cx.simulate_path_prompt_response(|_| Some(vec!["/chosen".into()]));
        cx.run_until_parked();
        let locals = dialog.read_with(cx, |dialog, _| {
            dialog.servers.servers[0]
                .mappings
                .iter()
                .map(|mapping| mapping.local.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(locals, ["/b", "/c"], "no mapping took the answer");
    }

    #[gpui::test]
    async fn tab_reaches_add_mapping_after_the_tabs(cx: &mut TestAppContext) {
        let (dialog, cx) = open_with_one_server(cx).await;
        let tabs = tab_handles(cx);
        dialog.update(cx, |dialog, cx| dialog.set_tab(DialogTab::Mappings, cx));
        cx.run_until_parked();
        focus(&tabs[2], cx);
        cx.simulate_keystrokes("tab");
        let add = dialog.read_with(cx, |dialog, _| dialog.add_mapping_focus.clone());
        assert_eq!(focused(cx), Some(add), "no rows: Add Mapping is next");
    }

    #[gpui::test]
    async fn enter_on_ok_saves_and_closes(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        load_keymaps(cx);
        let name = dialog.read_with(cx, |dialog, _| dialog.fields.name.clone());
        set_text(&name, "Edited", cx);
        // Add Server is the first control, so Shift-Tab wraps to the last one: OK.
        let add = dialog.read_with(cx, |dialog, _| dialog.add_server_focus.clone());
        focus(&add, cx);
        cx.simulate_keystrokes("shift-tab enter");
        cx.run_until_parked();
        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(saved.servers[0].name, "Edited", "Enter on OK saved");
        assert!(!is_open(&workspace, cx), "and closed the dialog");
    }

    #[gpui::test]
    async fn enter_in_a_single_line_field_saves_and_closes(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (workspace, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        load_keymaps(cx);
        let name = dialog.read_with(cx, |dialog, _| dialog.fields.name.clone());
        set_text(&name, "Edited", cx);
        let handle = cx.update(|_, cx| name.focus_handle(cx));
        focus(&handle, cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let saved = cx.update(|_, cx| ServerStore::global(cx).read(cx).servers().clone());
        assert_eq!(saved.servers[0].name, "Edited", "Enter in a field saved");
        assert!(!is_open(&workspace, cx), "and closed the dialog");
    }

    #[gpui::test]
    async fn a_browse_answer_lands_in_the_same_servers_rebuilt_row(cx: &mut TestAppContext) {
        let mut one = server("One", None);
        one.mappings = vec![Mapping {
            local: "/one".into(),
            remote: "one".into(),
        }];
        let mut two = server("Two", None);
        two.mappings = vec![Mapping {
            local: "/two".into(),
            remote: "two".into(),
        }];
        let servers = Servers {
            servers: vec![one, two],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let local_of_server = |index: usize, cx: &mut VisualTestContext| {
            dialog.read_with(cx, |dialog, _| {
                dialog.servers.servers[index].mappings[0].local.clone()
            })
        };
        let select = |index: usize, cx: &mut VisualTestContext| {
            dialog.update_in(cx, |dialog, window, cx| {
                dialog.select(Some(index), window, cx)
            });
        };

        // The rows were rebuilt (another server shown, then this one again) while the prompt
        // was open: the answer still lands in this server's first row.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.browse_mapping(0, window, cx)
        });
        select(1, cx);
        select(0, cx);
        cx.simulate_path_prompt_response(|_| Some(vec!["/chosen".into()]));
        cx.run_until_parked();
        assert_eq!(local_of_server(0, cx), "/chosen");
        let shown = dialog.read_with(cx, |dialog, _| dialog.mapping_rows[0].local.clone());
        assert_eq!(text_of(&shown, cx), "/chosen");

        // Another server is showing when the answer comes: it is dropped, not typed into that
        // server's own first row.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.browse_mapping(0, window, cx)
        });
        select(1, cx);
        cx.simulate_path_prompt_response(|_| Some(vec!["/elsewhere".into()]));
        cx.run_until_parked();
        assert_eq!(local_of_server(1, cx), "/two");
        assert_eq!(local_of_server(0, cx), "/chosen");
    }

    #[gpui::test]
    async fn compat_listing_is_disabled_and_says_why_without_list(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        dialog.update(cx, |dialog, cx| {
            dialog.advanced_open = true;
            dialog.update_selected(cx, |server| {
                server.server_type = ServerType::Ftps;
                server.advanced.use_list = false;
                server.advanced.list_compat = true;
            })
        });
        cx.run_until_parked();
        let compat = |dialog: &Entity<DeploymentDialog>, cx: &mut VisualTestContext| {
            dialog.read_with(cx, |dialog, _| {
                dialog
                    .selected_server()
                    .expect("selected")
                    .advanced
                    .list_compat
            })
        };
        let click_checkbox = |cx: &mut VisualTestContext| {
            let row = cx
                .debug_bounds("zenith-deploy-list-compat")
                .expect("the compat row is drawn");
            // The checkbox sits at the left of the row, after its indent.
            let at = gpui::point(row.left() + rems(1.5).to_pixels(px(16.)), row.center().y);
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
        };
        assert!(
            cx.debug_bounds("zenith-deploy-needs-list").is_some(),
            "LIST is off: the row says what it needs"
        );
        click_checkbox(cx);
        assert!(
            compat(&dialog, cx),
            "a click on the disabled checkbox changes nothing"
        );

        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.advanced.use_list = true)
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-deploy-needs-list").is_none());
        click_checkbox(cx);
        assert!(
            !compat(&dialog, cx),
            "the same click works once LIST is on, so the click above did reach the checkbox"
        );
    }

    #[gpui::test]
    async fn dropping_a_saved_password_shows_a_note_before_ok(cx: &mut TestAppContext) {
        let servers = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let (_, dialog, cx) = open_dialog(servers, &FakeRemote::default(), cx).await;
        let note = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.debug_bounds("zenith-deploy-password-removed").is_some()
        };
        assert!(!note(cx), "nothing is removed yet");

        for (label, change) in [
            (
                "Anonymous",
                (|server: &mut ServerConfig| server.anonymous = true) as fn(&mut ServerConfig),
            ),
            ("Save password off", |server| server.save_password = false),
            ("SFTP", |server| server.server_type = ServerType::Sftp),
        ] {
            dialog.update(cx, |dialog, cx| dialog.update_selected(cx, change));
            assert!(
                note(cx),
                "{label}: the saved password is about to be removed"
            );
            dialog.read_with(cx, |dialog, cx| assert!(dialog.removes_saved_password(cx)));
            dialog.update(cx, |dialog, cx| {
                dialog.update_selected(cx, |server| {
                    server.anonymous = false;
                    server.save_password = true;
                    server.server_type = ServerType::Ftps;
                })
            });
            assert!(!note(cx), "{label}: undone, so the note goes");
        }

        // A server that never stored a password has nothing to remove.
        dialog.update_in(cx, |dialog, window, cx| dialog.add_server(window, cx));
        dialog.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.anonymous = true)
        });
        assert!(!note(cx), "a new server has no saved password");
    }

    #[gpui::test]
    async fn closing_the_dialog_during_a_save_still_writes_the_servers(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (workspace, dialog, _, _, _, cx) =
            rename_and_type_a_password(false, path.clone(), cx).await;
        // Save, then Esc before the save task has run.
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.update_selected(cx, |server| server.name = "Renamed".into());
            dialog.save(cx);
            dialog.cancel(&menu::Cancel, window, cx);
        });
        drop(dialog);
        cx.run_until_parked();

        assert!(!is_open(&workspace, cx), "Esc closed the dialog");
        let on_disk = Servers::load(&path).expect("servers.json written after Esc");
        assert_eq!(on_disk.servers[0].name, "Renamed");
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.notification_ids().is_empty(),
                "no toast for a plain save"
            );
        });
    }

    /// A store with one saved server `Live` that writes to `path`.
    fn store_with_live(
        path: std::path::PathBuf,
        cx: &mut TestAppContext,
    ) -> (Entity<ServerStore>, Servers) {
        let saved = Servers {
            servers: vec![server("Live", None)],
            ..Servers::default()
        };
        let store = cx.new(store_at(saved.clone(), path));
        (store, saved)
    }

    fn renamed(servers: &Servers, name: &str) -> Servers {
        let mut servers = servers.clone();
        servers.servers[0].name = name.into();
        servers
    }

    #[gpui::test]
    async fn a_save_from_an_outdated_list_is_refused_and_the_newer_one_survives(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (store, start) = store_with_live(path.clone(), cx);

        // Both saves started from `start`; B finishes first, then the stale A arrives.
        store
            .update(cx, |store, cx| {
                store.save(
                    start.clone(),
                    renamed(&start, "B"),
                    Vec::new(),
                    &Default::default(),
                    cx,
                )
            })
            .await
            .expect("B saves");
        let error = store
            .update(cx, |store, cx| {
                store.save(
                    start.clone(),
                    renamed(&start, "A"),
                    Vec::new(),
                    &Default::default(),
                    cx,
                )
            })
            .await
            .expect_err("A started from a list that is no longer the saved one");
        assert!(error.to_string().contains("changed while"), "{error}");

        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].name, "B");
        });
        assert_eq!(Servers::load(&path).expect("load").servers[0].name, "B");
    }

    #[gpui::test]
    async fn a_second_save_is_refused_while_the_first_is_writing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (store, start) = store_with_live(path.clone(), cx);

        let first = store.update(cx, |store, cx| {
            store.save(
                start.clone(),
                renamed(&start, "First"),
                Vec::new(),
                &Default::default(),
                cx,
            )
        });
        let second = store.update(cx, |store, cx| {
            store.save(
                start.clone(),
                renamed(&start, "Second"),
                Vec::new(),
                &Default::default(),
                cx,
            )
        });
        let error = second.await.expect_err("the first write is still running");
        assert!(error.to_string().contains("another save"), "{error}");
        first.await.expect("the first save is not harmed");
        assert_eq!(Servers::load(&path).expect("load").servers[0].name, "First");
        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].name, "First");
        });
    }

    /// Opens the dialog again (after Esc) and returns it.
    fn reopen(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<DeploymentDialog> {
        cx.dispatch_action(OpenDeployment);
        cx.run_until_parked();
        workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens again")
    }

    #[gpui::test]
    async fn a_save_is_refused_while_an_earlier_one_still_waits_on_the_keychain(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (workspace, dialog_a, store, _, keychain, cx) =
            rename_and_type_a_password(true, path.clone(), cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *keychain.write_gate.lock().expect("lock") = Some(gate);

        // A: Save waits on the Keychain, then Esc.
        dialog_a.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "From A".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog_a.read_with(cx, |dialog, cx| {
            assert!(dialog.saving);
            assert!(!dialog.can_save(cx), "OK is off while saving");
        });
        assert!(
            cx.debug_bounds("zenith-deploy-saving").is_some(),
            "a visible Saving label"
        );
        dialog_a.update_in(cx, |dialog, window, cx| {
            dialog.cancel(&menu::Cancel, window, cx)
        });
        drop(dialog_a);
        cx.run_until_parked();
        assert!(!is_open(&workspace, cx));

        // B opens on the same list and presses OK while A is still on the Keychain.
        let dialog_b = reopen(&workspace, cx);
        dialog_b.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "From B".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert!(
            error_text(&dialog_b, cx).contains("another save is still running"),
            "{}",
            error_text(&dialog_b, cx)
        );
        assert!(is_open(&workspace, cx), "B stays open");
        assert!(!path.exists(), "B wrote nothing");

        // A's Keychain wait ends: A saves, and B may not undo it.
        release.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            Servers::load(&path).expect("load").servers[0].name,
            "From A"
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.servers().servers[0].name, "From A");
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.notification_ids().is_empty(),
                "A succeeded quietly"
            );
        });

        // A new dialog on the saved list can save once A is done.
        dialog_b.update_in(cx, |dialog, window, cx| {
            dialog.cancel(&menu::Cancel, window, cx)
        });
        drop(dialog_b);
        cx.run_until_parked();
        let dialog_c = reopen(&workspace, cx);
        dialog_c.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "From C".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            Servers::load(&path).expect("load").servers[0].name,
            "From C"
        );
    }

    #[gpui::test]
    async fn a_dialog_opened_before_another_save_finished_cannot_undo_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (workspace, dialog_a, store, saved, keychain, cx) =
            rename_and_type_a_password(true, path.clone(), cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *keychain.write_gate.lock().expect("lock") = Some(gate);

        dialog_a.update(cx, |dialog, cx| {
            dialog.update_selected(cx, |server| server.name = "From A".into());
            dialog.save(cx);
        });
        cx.run_until_parked();
        dialog_a.update_in(cx, |dialog, window, cx| {
            dialog.cancel(&menu::Cancel, window, cx)
        });
        drop(dialog_a);
        cx.run_until_parked();

        // B opens on the old list; A finishes; only then B presses OK.
        let dialog_b = reopen(&workspace, cx);
        release.send(()).ok();
        cx.run_until_parked();
        assert_eq!(
            Servers::load(&path).expect("load").servers[0].name,
            "From A"
        );
        dialog_b.update(cx, |dialog, cx| {
            // Turning the password option off would delete A's Keychain entry.
            dialog.update_selected(cx, |server| {
                server.name = "From B".into();
                server.save_password = false;
            });
            dialog.save(cx);
        });
        cx.run_until_parked();

        assert!(
            error_text(&dialog_b, cx).contains("this dialog was open"),
            "{}",
            error_text(&dialog_b, cx)
        );
        assert!(is_open(&workspace, cx), "B stays open");
        assert_eq!(
            Servers::load(&path).expect("load").servers[0].name,
            "From A",
            "disk keeps A"
        );
        store.read_with(cx, |store, _| {
            let kept = &store.servers().servers[0];
            assert_eq!(kept.name, "From A", "memory keeps A");
            assert!(kept.save_password);
        });
        assert!(
            keychain.account_of(&saved).is_some(),
            "B changed nothing in the Keychain"
        );
    }

    #[gpui::test]
    async fn a_save_that_clears_a_password_after_its_dialog_closed_is_toasted(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("servers.json");
        let (workspace, dialog, _, saved, keychain, cx) =
            rename_and_type_a_password(true, path.clone(), cx).await;
        keychain.put(&saved, "not-a-real-value-0");
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.passwords.clear();
            dialog.update_selected(cx, |server| server.host = "elsewhere.example.com".into());
            dialog.save(cx);
            dialog.cancel(&menu::Cancel, window, cx);
        });
        drop(dialog);
        cx.run_until_parked();

        assert_eq!(
            Servers::load(&path).expect("load").servers[0].host,
            "elsewhere.example.com"
        );
        assert!(
            keychain.account_of(&saved).is_none(),
            "the password is gone"
        );
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.notification_ids().len(),
                1,
                "the user is told the saved password was removed"
            );
        });
    }
}
