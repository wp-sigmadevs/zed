//! Deployment: server definitions and the FTP/FTPS/SFTP transports behind the Remote Host
//! tool window and the Deployment dialog (plan 37).

pub mod config;
mod dialog;
mod ftp;
pub mod on_save;
mod panel;
pub mod remote_edit;
pub mod remote_path;
pub mod secrets;
mod sftp;
pub mod transport;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::future::{BoxFuture, FutureExt as _, Shared};
use gpui::{App, AppContext as _, Context, Entity, FutureExt as _, Global, Task};
use settings::{DockPosition, RegisterSetting, Settings, SettingsContent};

pub use config::{Password, ServerConfig, ServerType, Servers};
pub use dialog::{DeploymentDialog, OpenDeployment};
pub use panel::{RemoteHostPanel, ToggleFocus};
pub use remote_edit::{
    CopyState, RemoteCopy, ServerStoreEvent, UploadDecision, UploadOutcome, UploadStart,
    UploadWarnings, cache_path_for, upload_warnings,
};
pub use transport::{
    EntryKind, RemoteEntry, RemoteStat, ServerRefused, Transport, connect, download_entry,
    upload_entry,
};

/// Every network operation gives up after this long.
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct DeploySettings {
    pub enabled: bool,
    pub dock: DockPosition,
}

impl Settings for DeploySettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let deploy = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.deploy.as_ref());
        Self {
            enabled: deploy.and_then(|deploy| deploy.enabled).unwrap_or(true),
            dock: deploy
                .and_then(|deploy| deploy.dock)
                .unwrap_or(DockPosition::Right),
        }
    }
}

pub fn init(cx: &mut App) {
    DeploySettings::register(cx);
    let store = cx.new(|cx| {
        ServerStore::load(
            config::servers_path(),
            paths::data_dir().join("deployment").join("cache"),
            cx,
        )
    });
    cx.set_global(GlobalServerStore(store));
    dialog::init(cx);
    panel::init(cx);
}

pub type ConnectFn =
    Arc<dyn Fn(ServerConfig) -> BoxFuture<'static, Result<Box<dyn Transport>>> + Send + Sync>;

/// The saved servers, shared by the Deployment dialog and every Remote Host panel.
pub struct ServerStore {
    servers: Servers,
    path: Option<PathBuf>,
    /// Where opened remote files are cached, one folder per server.
    cache_dir: PathBuf,
    loaded: bool,
    /// servers.json is being written; see [`Self::save`].
    writing: bool,
    /// A dialog save is running, Keychain phase included; see [`Self::begin_save`].
    busy: bool,
    /// A damaged servers.json is never overwritten: saving stays off until the user fixes it.
    load_error: Option<String>,
    /// Passwords of servers with "Save password" off: memory only, gone on quit. Each is kept
    /// with the target account it was typed for and only used for that target.
    session_passwords: HashMap<String, (String, Password)>,
    keychain: Arc<dyn secrets::Keychain>,
    /// The Keychain reads in flight, by (server id, target account): a retry while one still
    /// waits (a prompt, a stuck daemon) joins it instead of starting another read and prompt.
    /// An entry leaves the map when its read ends, by answer or by deadline.
    keychain_reads: Arc<std::sync::Mutex<HashMap<(String, String), KeychainRead>>>,
    connect: ConnectFn,
    /// Cached copies of remote files this app downloaded, by cache path (plan 37 M4a). The only
    /// source of an upload target.
    copies: HashMap<PathBuf, remote_edit::RemoteCopy>,
    /// "Upload without asking" per server for this session, with the target it was said for.
    /// Memory only: never read from settings or any file.
    upload_decisions: HashMap<String, (String, UploadDecision)>,
    /// Running uploads, owned here so a panel reset or window close cannot cancel them.
    uploads: HashMap<PathBuf, Task<()>>,
    /// Servers whose Disconnect (or deletion) left open copies' files in place; their cache is
    /// cleared when those copies are released ([`ServerStore::buffer_released`]).
    pending_clear: std::collections::HashSet<String>,
    /// Cache paths a panel is downloading into right now, across windows
    /// ([`ServerStore::reserve_download`]).
    downloading: Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
    /// Project files that auto-upload on save (plan 37 M4b), by absolute local path. Apart from
    /// `copies` on purpose: [`ServerStore::clear_cache`] deletes the file of every entry in
    /// `copies`, and these are the user's own project files. The stat is the session-only
    /// baseline (the verified stat after our last upload).
    mapped: HashMap<PathBuf, remote_edit::RemoteCopy>,
    /// The kill switch for auto-upload ([`ServerStore::set_auto_upload_paused`]).
    auto_upload_paused: bool,
    /// Servers whose first-save-without-baseline question the user answered "always" this
    /// session. Never skips a Conflict.
    /// Each with the [`on_save::fingerprint`] it was said for
    /// ([`ServerStore::set_skip_no_baseline_prompt`]).
    skip_no_baseline_prompt: HashMap<String, String>,
    /// Per server, the number of sessions auto-upload may hold at once (`advanced.connections`).
    connection_limits: HashMap<String, (usize, Arc<smol::lock::Semaphore>)>,
    _load: Task<()>,
}

type KeychainRead = Shared<BoxFuture<'static, Result<secrets::Lookup, String>>>;

/// Refuses `servers` when a new server, or one whose root folder or mappings changed against
/// `previous`, has unusable mappings ([`on_save::validate_mappings`]), or when one whose
/// excluded paths changed has an invalid pattern ([`on_save::validate_excluded`]). Only what the
/// user changes now is checked, so a mapping that is already saved does not block an edit of
/// something else; it does block every mapped upload of its server, because `on_save::resolve`
/// checks all of a server's mappings. The dialog calls this before it touches the Keychain: a refused save must
/// change nothing.
pub(crate) fn validate_changed(previous: &Servers, servers: &Servers) -> Result<()> {
    for server in &servers.servers {
        let old = previous.servers.iter().find(|old| old.id == server.id);
        let changed = old
            .is_none_or(|old| old.root_path != server.root_path || old.mappings != server.mappings);
        if changed && let Err(message) = on_save::validate_mappings(server) {
            bail!("{}: {message}", server.name);
        }
        // The lenient matcher skips an invalid pattern, which would upload a file the user
        // meant to exclude: refuse a changed list that has one.
        let excluded_changed = old.is_none_or(|old| old.excluded_local != server.excluded_local);
        if excluded_changed && let Err(message) = on_save::validate_excluded(server) {
            bail!("{}: {message}", server.name);
        }
    }
    Ok(())
}

struct GlobalServerStore(Entity<ServerStore>);

impl Global for GlobalServerStore {}

/// Runs `remove` up to three times, 100 ms apart, while it fails for a transient reason (a
/// cancelled download that is still creating files makes `remove_dir_all` fail with
/// "directory not empty"). Other errors, such as a permission error, are returned at once.
/// Known gap: a download started again within these ~200 ms (reconnect) can still recreate
/// files after the last attempt; the next Disconnect removes them.
fn remove_with_retry(mut remove: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    use std::io::ErrorKind::{DirectoryNotEmpty, ResourceBusy};
    let mut result = remove();
    for _ in 0..2 {
        match &result {
            Err(error) if matches!(error.kind(), DirectoryNotEmpty | ResourceBusy) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                result = remove();
            }
            _ => break,
        }
    }
    result
}

/// Removes every server folder of the cache whose name is a server id (a UUID) no saved server
/// has. Only real directories: a symlink or a file with such a name is left alone, and
/// `remove_dir_all` never follows links inside. Failures are logged.
fn sweep_cache(cache_dir: &std::path::Path, keep: &std::collections::HashSet<String>) {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("deploy: reading {}: {error}", cache_dir.display());
            }
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = uuid::Uuid::parse_str(&name).is_ok() && !keep.contains(&name);
        if !stale || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Err(error) = remove_with_retry(|| std::fs::remove_dir_all(entry.path())) {
            log::warn!("deploy: removing {}: {error}", entry.path().display());
        }
    }
}

impl ServerStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalServerStore>().0.clone()
    }

    fn load(path: PathBuf, cache_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let load = cx.spawn({
            let path = path.clone();
            let cache_dir = cache_dir.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move { Servers::load(&path) })
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(servers) => {
                            // A server deleted while the app was not running (or whose cache
                            // removal failed) must not leave its cached files behind.
                            let keep = servers.servers.iter().map(|s| s.id.clone()).collect();
                            cx.background_spawn(async move { sweep_cache(&cache_dir, &keep) })
                                .detach();
                            this.servers = servers;
                        }
                        Err(error) => this.load_error = Some(format!("{error:#}")),
                    }
                    this.loaded = true;
                    cx.notify();
                })
                .ok();
            }
        });
        Self {
            servers: Servers::default(),
            path: Some(path),
            cache_dir,
            loaded: false,
            writing: false,
            busy: false,
            load_error: None,
            session_passwords: HashMap::new(),
            keychain: Arc::new(secrets::SystemKeychain),
            keychain_reads: Default::default(),
            connect: Arc::new(|server| Box::pin(async move { connect(&server).await })),
            copies: HashMap::new(),
            upload_decisions: HashMap::new(),
            uploads: HashMap::new(),
            pending_clear: Default::default(),
            downloading: Default::default(),
            mapped: HashMap::new(),
            auto_upload_paused: false,
            skip_no_baseline_prompt: Default::default(),
            connection_limits: HashMap::new(),
            _load: load,
        }
    }

    /// An in-memory store that never touches the disk; tests pass a fake `connect`.
    pub fn in_memory(servers: Servers, cache_dir: PathBuf, connect: ConnectFn) -> Self {
        Self {
            servers,
            path: None,
            cache_dir,
            loaded: true,
            writing: false,
            busy: false,
            load_error: None,
            session_passwords: HashMap::new(),
            keychain: Arc::new(secrets::SystemKeychain),
            keychain_reads: Default::default(),
            connect,
            copies: HashMap::new(),
            upload_decisions: HashMap::new(),
            uploads: HashMap::new(),
            pending_clear: Default::default(),
            downloading: Default::default(),
            mapped: HashMap::new(),
            auto_upload_paused: false,
            skip_no_baseline_prompt: Default::default(),
            connection_limits: HashMap::new(),
            _load: Task::ready(()),
        }
    }

    pub fn servers(&self) -> &Servers {
        &self.servers
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Servers offered in a project: those not limited to another project.
    pub fn servers_for(&self, project: Option<&str>) -> Vec<ServerConfig> {
        self.servers
            .servers
            .iter()
            .filter(|server| server.is_visible_in(project))
            .cloned()
            .collect()
    }

    /// One of [`Self::servers_for`], cloning only that server.
    pub fn server_for(&self, project: Option<&str>, id: &str) -> Option<ServerConfig> {
        self.servers
            .servers
            .iter()
            .find(|server| server.id == id && server.is_visible_in(project))
            .cloned()
    }

    pub fn default_for(&self, project: &str) -> Option<&str> {
        self.servers
            .project_defaults
            .get(project)
            .map(String::as_str)
    }

    /// Writes servers.json (0600, atomically) off the main thread, and only when that worked
    /// replaces every server and applies the session password changes: a failed write changes
    /// nothing here.
    ///
    /// `previous` is the saved list the caller started from. A save whose starting point is no
    /// longer the saved list (another save finished meanwhile, e.g. a dialog closed with Esc
    /// while its save waited on the Keychain) is refused, never merged: it would overwrite the
    /// newer servers with its stale copy. Only one write runs at a time, so the list cannot
    /// change between this check and `apply` (nothing else replaces `servers`).
    ///
    /// `reconfirmed` maps the id of a server whose "upload on save" confirmation the user has
    /// just given again to the [`on_save::fingerprint`] of the settings they confirmed; the
    /// store keeps the flag only while the server being saved still has that fingerprint, and
    /// clears it for any other server that needs a new confirmation
    /// ([`on_save::needs_reconfirm`]).
    fn save(
        &mut self,
        previous: Servers,
        mut servers: Servers,
        session_changes: Vec<(ServerConfig, Option<Password>)>,
        reconfirmed: &HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = validate_changed(&previous, &servers) {
            return Task::ready(Err(error));
        }
        // The one-time "upload on save" confirmation was given for what `on_save::fingerprint`
        // covers: it stays on only when the user just confirmed these settings or it was on and
        // none of them changed. A flag switched on any other way is cleared (plan 37 M4b).
        for server in &mut servers.servers {
            let old = previous.servers.iter().find(|old| old.id == server.id);
            server.upload_on_save_confirmed =
                on_save::confirmation_holds(old, server, reconfirmed.get(&server.id));
        }
        if let Some(error) = &self.load_error {
            return Task::ready(Err(anyhow!(
                "the saved servers could not be read, so they are not overwritten: {error}"
            )));
        }
        if self.writing {
            return Task::ready(Err(anyhow!(
                "another save is still running; wait for it and try again"
            )));
        }
        if self.servers != previous {
            return Task::ready(Err(anyhow!(
                "the saved servers changed while this was saving, so they are not overwritten; reopen Deployment settings and try again"
            )));
        }
        let Some(path) = self.path.clone() else {
            self.apply(servers, session_changes, cx);
            return Task::ready(Ok(()));
        };
        self.writing = true;
        let to_write = servers.clone();
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_spawn(async move { to_write.save(&path) })
                .await;
            this.update(cx, |this, cx| {
                this.writing = false;
                written.map(|()| this.apply(servers, session_changes, cx))
            })?
        })
    }

    /// Marks a dialog save as running, from OK until its task ends, because a save that waits
    /// on the Keychain must not be overtaken (its late write would land after a newer save's).
    /// Refused while another one runs. Pair with [`Self::end_save`].
    fn begin_save(&mut self) -> Result<()> {
        if self.busy {
            bail!("another save is still running; wait for it and try again");
        }
        self.busy = true;
        Ok(())
    }

    fn end_save(&mut self) {
        self.busy = false;
    }

    fn apply(
        &mut self,
        servers: Servers,
        session_changes: Vec<(ServerConfig, Option<Password>)>,
        cx: &mut Context<Self>,
    ) {
        for (server, password) in session_changes {
            self.set_session_password(&server, password);
        }
        let kept: std::collections::HashSet<_> =
            servers.servers.iter().map(|server| &server.id).collect();
        self.session_passwords.retain(|id, _| kept.contains(id));
        // A decision lives only while its server still has the target it was made for, a skip
        // while it still has the same `on_save::fingerprint`: a host that changed and came back
        // must ask again.
        let target_of = |id: &String| {
            servers
                .servers
                .iter()
                .find(|server| &server.id == id)
                .map(secrets::upload_target)
        };
        self.upload_decisions
            .retain(|id, (target, _)| target_of(id).as_ref() == Some(target));
        self.skip_no_baseline_prompt.retain(|id, saved| {
            servers
                .servers
                .iter()
                .find(|server| &server.id == id)
                .map(on_save::fingerprint)
                .as_ref()
                == Some(saved)
        });
        self.connection_limits.retain(|id, _| kept.contains(id));
        let removed: Vec<String> = self
            .servers
            .servers
            .iter()
            .filter(|server| !kept.contains(&server.id))
            .map(|server| server.id.clone())
            .collect();
        for id in removed {
            self.clear_cache(&id, cx);
        }
        self.servers = servers;
        cx.notify();
    }

    /// Deletes the cached copies of a server's remote files (maybe `wp-config.php`) off the
    /// main thread. Cancelled downloads only stop at their next await, so one can still be
    /// writing here: the removal is retried (see [`remove_with_retry`]).
    /// The cache is shared by every window: Disconnect in one window also deletes the files
    /// another window has open.
    ///
    /// A copy with a live buffer (clean or dirty) or a running upload keeps its file: the
    /// session still ends, the server is remembered in `pending_clear` and finished once the
    /// copy is released. The files of the server's closed copies go at once. Returns how many
    /// open copies were kept, for "Kept N open remote files".
    pub(crate) fn clear_cache(&mut self, server_id: &str, cx: &mut Context<Self>) -> usize {
        if remote_path::validate_name(server_id).is_err() {
            return 0;
        }
        let open = self
            .copies
            .values()
            .filter(|copy| copy.server_id == server_id && copy.is_open())
            .count();
        if open > 0 {
            let closed: Vec<PathBuf> = self
                .copies
                .iter()
                .filter(|(_, copy)| copy.server_id == server_id && !copy.is_open())
                .map(|(path, _)| path.clone())
                .collect();
            for path in &closed {
                self.copies.remove(path);
            }
            self.pending_clear.insert(server_id.to_string());
            cx.background_spawn(async move {
                for path in closed {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => log::warn!("deploy: removing {}: {error}", path.display()),
                    }
                }
            })
            .detach();
            return open;
        }
        self.pending_clear.remove(server_id);
        self.copies.retain(|_, copy| copy.server_id != server_id);
        let directory = self.cache_dir.join(server_id);
        cx.background_spawn(async move {
            match remove_with_retry(|| std::fs::remove_dir_all(&directory)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("deploy: removing {}: {error}", directory.display()),
            }
        })
        .detach();
        0
    }

    /// Remembers (or forgets) the typed password of `server` for its current target only.
    fn set_session_password(&mut self, server: &ServerConfig, password: Option<Password>) {
        match password {
            Some(password) => {
                self.session_passwords.insert(
                    server.id.clone(),
                    (secrets::target_account(server), password),
                );
            }
            None => {
                self.session_passwords.remove(&server.id);
            }
        }
    }

    /// The session password, only when it was typed for `server`'s target as it is now.
    fn session_password_for(&self, server: &ServerConfig) -> Option<Password> {
        self.session_passwords
            .get(&server.id)
            .filter(|(account, _)| *account == secrets::target_account(server))
            .map(|(_, password)| password.clone())
    }

    /// The password to log in with: none for anonymous FTP and SFTP (key or agent auth), else
    /// the session copy or the Keychain entry.
    fn password_for(&self, server: &ServerConfig, cx: &App) -> Task<Result<Option<Password>>> {
        if !server.uses_password() {
            return Task::ready(Ok(None));
        }
        if let Some(password) = self.session_password_for(server) {
            return Task::ready(Ok(Some(password)));
        }
        let key = (server.id.clone(), secrets::target_account(server));
        let load = {
            let mut reads = self
                .keychain_reads
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match reads.get(&key) {
                Some(read) => read.clone(),
                None => {
                    let read = secrets::load_password(self.keychain.as_ref(), server, cx);
                    let reads_map = self.keychain_reads.clone();
                    let finished = key.clone();
                    let shared: KeychainRead = async move {
                        let result = read.await.map_err(|error| format!("{error:#}"));
                        reads_map
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&finished);
                        result
                    }
                    .boxed()
                    .shared();
                    reads.insert(key, shared.clone());
                    shared
                }
            }
        };
        let name = server.name.clone();
        cx.background_spawn(async move {
            match load.await.map_err(|error| anyhow!(error))? {
                secrets::Lookup::Found(password) => Ok(Some(Password(password))),
                secrets::Lookup::Missing => {
                    bail!("no password is stored for {name}; enter it in Deployment settings")
                }
                secrets::Lookup::OtherTarget => bail!(
                    "the password stored for {name} belongs to another host, port, type or username, or was saved by an older version; enter it again in Deployment settings (a one-time step)"
                ),
            }
        })
    }

    /// Resolves the password, then connects through [`spawn_operation`].
    fn open_session(&self, server: ServerConfig, cx: &App) -> Task<Result<Box<dyn Transport>>> {
        let password = self.password_for(&server, cx);
        let connect = self.connect.clone();
        cx.spawn(async move |cx| {
            let mut server = server;
            server.password = password.await.context("reading the password")?;
            cx.update(|cx| spawn_operation(cx, connect(server))).await
        })
    }
}

/// Runs a network operation off the main thread with [`OPERATION_TIMEOUT`]. Dropping the
/// returned task cancels it, which drops the connection or kills the `sftp` child.
pub fn spawn_operation<T: Send + 'static>(
    cx: &App,
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> Task<Result<T>> {
    spawn_operation_with_timeout(cx, OPERATION_TIMEOUT, operation)
}

/// [`spawn_operation`] with a caller-chosen deadline (uploads: [`upload_timeout`]).
pub fn spawn_operation_with_timeout<T: Send + 'static>(
    cx: &App,
    timeout: Duration,
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> Task<Result<T>> {
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        operation
            .with_timeout(timeout, &executor)
            .await
            .map_err(|_| anyhow!("the server did not answer within {timeout:?}"))?
    })
}

/// The deadline of an upload of `len` bytes: [`OPERATION_TIMEOUT`] plus one second per 64 KiB
/// (a 64 KiB/s floor), so a 20 MiB file gets about six minutes instead of being cut off after
/// 30 seconds with the remote file truncated.
pub fn upload_timeout(len: usize) -> Duration {
    OPERATION_TIMEOUT + Duration::from_secs((len / (64 * 1024)) as u64)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::Project;
    use workspace::{AppState, MultiWorkspace, Workspace};

    use super::*;

    /// A remote tree in memory; every call is appended to `log`.
    #[derive(Clone, Default)]
    pub struct FakeRemote {
        pub tree: Arc<HashMap<String, Vec<RemoteEntry>>>,
        pub log: Arc<Mutex<Vec<String>>>,
        pub fail_connect: bool,
        /// Folders whose listing fails like a dead connection, not a refusal.
        pub broken: Vec<String>,
        /// Folders that refuse the listing (like a 550) even though the tree has them.
        pub refusing: Arc<Mutex<Vec<String>>>,
        /// Folders that list as empty from now on, whatever the tree says.
        pub emptied: Arc<Mutex<Vec<String>>>,
        /// A listing of this folder waits (once) until its sender fires or is dropped.
        pub list_gates: Arc<Mutex<HashMap<String, futures::channel::oneshot::Receiver<()>>>>,
        /// Every upload as (path, bytes).
        pub puts: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
        pub stats: Arc<Mutex<HashMap<String, RemoteStat>>>,
        /// Makes every `stat` / `upload_file` fail with this text.
        pub stat_error: Arc<Mutex<Option<String>>>,
        pub put_error: Arc<Mutex<Option<String>>>,
        /// Makes the first `stat` after a successful put fail with this text, once.
        pub stat_error_after_put: Arc<Mutex<Option<String>>>,
        /// Makes `stat` / `upload_file` never answer (a dead connection).
        pub stat_hang: Arc<Mutex<bool>>,
        pub put_hang: Arc<Mutex<bool>>,
        /// The next put waits until this resolves (a slow upload).
        pub put_gate: Arc<Mutex<Option<futures::channel::oneshot::Receiver<()>>>>,
        /// A put does not change the server's stat (a server that rewrites the file).
        pub put_keeps_stat: Arc<Mutex<bool>>,
    }

    impl FakeRemote {
        pub fn new(tree: &[(&str, &[(&str, EntryKind)])]) -> Self {
            let tree = tree
                .iter()
                .map(|(path, entries)| {
                    let entries = entries
                        .iter()
                        .map(|(name, kind)| RemoteEntry {
                            name: name.to_string(),
                            kind: *kind,
                            size: 5,
                            modified: None,
                        })
                        .collect();
                    (path.to_string(), entries)
                })
                .collect();
            Self {
                tree: Arc::new(tree),
                ..Self::default()
            }
        }

        pub fn log(&self) -> Vec<String> {
            self.log.lock().map(|log| log.clone()).unwrap_or_default()
        }

        fn push(&self, line: String) {
            if let Ok(mut log) = self.log.lock() {
                log.push(line);
            }
        }

        pub fn connect_fn(&self) -> ConnectFn {
            let remote = self.clone();
            Arc::new(move |server: ServerConfig| {
                let remote = remote.clone();
                Box::pin(async move {
                    let password = server.password.map(|password| password.0);
                    remote.push(format!("connect {} password={password:?}", server.host));
                    if remote.fail_connect {
                        return Err(anyhow!("530 Login incorrect"));
                    }
                    Ok(Box::new(remote) as Box<dyn Transport>)
                })
            })
        }
    }

    #[async_trait]
    impl Transport for FakeRemote {
        async fn pwd(&mut self) -> Result<String> {
            Ok("/www".into())
        }
        async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
            self.push(format!("list {path}"));
            let gate = self
                .list_gates
                .lock()
                .ok()
                .and_then(|mut gates| gates.remove(path));
            if let Some(gate) = gate {
                gate.await.ok();
            }
            if self
                .refusing
                .lock()
                .is_ok_and(|refusing| refusing.iter().any(|refused| refused == path))
            {
                return Err(anyhow::Error::new(ServerRefused(format!("550 {path}"))));
            }
            if self
                .emptied
                .lock()
                .is_ok_and(|emptied| emptied.iter().any(|folder| folder == path))
            {
                return Ok(Vec::new());
            }
            self.tree.get(path).cloned().ok_or_else(|| {
                if self.broken.iter().any(|broken| broken == path) {
                    anyhow!("connection reset by peer")
                } else {
                    anyhow::Error::new(ServerRefused(format!("550 {path}: permission denied")))
                }
            })
        }
        async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
            self.push(format!("get {remote_path}"));
            std::fs::write(local_path, "<?php")?;
            Ok(())
        }
        async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>> {
            self.push(format!("stat {path}"));
            if self.stat_hang.lock().is_ok_and(|hang| *hang) {
                futures::future::pending::<()>().await;
            }
            if let Some(error) = self.stat_error.lock().ok().and_then(|error| error.clone()) {
                return Err(anyhow!(error));
            }
            let put_done = self.puts.lock().is_ok_and(|puts| !puts.is_empty());
            if put_done
                && let Some(error) = self
                    .stat_error_after_put
                    .lock()
                    .ok()
                    .and_then(|mut error| error.take())
            {
                return Err(anyhow!(error));
            }
            Ok(self
                .stats
                .lock()
                .ok()
                .and_then(|stats| stats.get(path).cloned()))
        }
        async fn upload_file(&mut self, remote_path: &str, bytes: &[u8]) -> Result<()> {
            self.push(format!("put {remote_path}"));
            if self.put_hang.lock().is_ok_and(|hang| *hang) {
                futures::future::pending::<()>().await;
            }
            let gate = self.put_gate.lock().ok().and_then(|mut gate| gate.take());
            if let Some(gate) = gate {
                gate.await.ok();
            }
            if let Some(error) = self.put_error.lock().ok().and_then(|error| error.clone()) {
                return Err(anyhow!(error));
            }
            if let Ok(mut puts) = self.puts.lock() {
                puts.push((remote_path.to_string(), bytes.to_vec()));
                // The upload changes the file on the server: a new stamp per put.
                let keeps = self.put_keeps_stat.lock().is_ok_and(|keeps| *keeps);
                if let (Ok(mut stats), false) = (self.stats.lock(), keeps) {
                    stats.insert(
                        remote_path.to_string(),
                        RemoteStat {
                            size: bytes.len() as u64,
                            stamp: Some(format!("put-{}", puts.len())),
                        },
                    );
                }
            }
            Ok(())
        }
        async fn disconnect(&mut self) -> Result<()> {
            self.push("disconnect".into());
            Ok(())
        }
    }

    pub fn server(name: &str, project: Option<&str>) -> ServerConfig {
        let mut server = ServerConfig::new();
        server.name = name.into();
        server.host = format!("{}.example.com", name.to_lowercase());
        server.username = "deploy".into();
        server.root_path = "/www".into();
        server.project = project.map(String::from);
        server
    }

    pub fn init(
        cx: &mut TestAppContext,
        servers: Servers,
        remote: &FakeRemote,
        cache_dir: PathBuf,
    ) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            DeploySettings::register(cx);
            let store = cx.new(|_| ServerStore::in_memory(servers, cache_dir, remote.connect_fn()));
            cx.set_global(GlobalServerStore(store));
            dialog::init(cx);
            panel::init(cx);
            app_state
        })
    }

    pub async fn workspace<'a>(
        app_state: &Arc<AppState>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        app_state
            .fs
            .as_fake()
            .insert_tree("/p", serde_json::json!({ "a.txt": "" }))
            .await;
        let project = Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();
        (workspace, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};

    #[test]
    fn a_changed_invalid_excluded_pattern_is_refused_but_a_saved_one_is_left_alone() {
        let saved = Servers {
            servers: vec![ServerConfig::new()],
            ..Servers::default()
        };
        let mut edited = saved.clone();
        edited.servers[0].excluded_local.push("[unclosed".into());
        let error = validate_changed(&saved, &edited).expect_err("a changed list is checked");
        assert!(format!("{error:#}").contains("[unclosed"), "{error:#}");
        // Already saved with it: an edit of something else is not blocked.
        let mut elsewhere = edited.clone();
        elsewhere.servers[0].host = "other.example.com".into();
        assert!(validate_changed(&edited, &elsewhere).is_ok());
        // A new server is checked too.
        assert!(validate_changed(&Servers::default(), &edited).is_err());
    }

    #[test]
    fn an_upload_deadline_grows_with_the_size_down_to_a_64_kib_per_second_floor() {
        assert_eq!(upload_timeout(0), OPERATION_TIMEOUT);
        assert_eq!(upload_timeout(64 * 1024 - 1), OPERATION_TIMEOUT);
        assert_eq!(
            upload_timeout(64 * 1024),
            OPERATION_TIMEOUT + Duration::from_secs(1)
        );
        // 20 MiB at 64 KiB/s is 320 s: the deadline covers it.
        assert!(upload_timeout(20 * 1024 * 1024) >= Duration::from_secs(320));
        assert_eq!(
            upload_timeout(64 * 1024 * 1024),
            OPERATION_TIMEOUT + Duration::from_secs(1024)
        );
    }

    #[test]
    fn removing_the_cache_is_retried_while_a_cancelled_download_still_writes() {
        let mut calls = 0;
        let result = remove_with_retry(|| {
            calls += 1;
            if calls == 1 {
                Err(std::io::ErrorKind::DirectoryNotEmpty.into())
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls, 2);

        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let mut calls = 0;
            let result = remove_with_retry(|| {
                calls += 1;
                Err(kind.into())
            });
            assert!(result.is_err());
            assert_eq!(calls, 1, "{kind:?} is not transient");
        }
    }
    use settings::{SettingsStore, ZenithDeploySettingsContent, ZenithSettingsContent};

    #[gpui::test]
    fn settings_default_to_enabled_on_the_right_and_follow_user_values(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            init(cx);
            let defaults = DeploySettings::get_global(cx);
            assert!(defaults.enabled);
            assert_eq!(defaults.dock, DockPosition::Right);

            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(ZenithSettingsContent {
                        deploy: Some(ZenithDeploySettingsContent {
                            enabled: Some(false),
                            dock: Some(DockPosition::Left),
                        }),
                        ..Default::default()
                    });
                });
            });
            let changed = DeploySettings::get_global(cx);
            assert!(!changed.enabled);
            assert_eq!(changed.dock, DockPosition::Left);
        });
    }

    #[gpui::test]
    async fn a_hung_operation_times_out_and_returns_an_error(cx: &mut TestAppContext) {
        let task = cx.update(|cx| {
            spawn_operation(cx, async {
                futures::future::pending::<()>().await;
                Ok(())
            })
        });
        cx.executor()
            .advance_clock(OPERATION_TIMEOUT + Duration::from_secs(1));
        let error = task.await.expect_err("must time out");
        assert!(error.to_string().contains("did not answer"), "{error}");
    }

    /// TLS must be verified: nothing in this crate may switch the verifier off.
    #[test]
    fn crate_never_uses_a_dangerous_tls_config() {
        let sources = [
            ("config.rs", include_str!("config.rs")),
            ("dialog.rs", include_str!("dialog.rs")),
            ("panel.rs", include_str!("panel.rs")),
            ("ftp.rs", include_str!("ftp.rs")),
            ("remote_edit.rs", include_str!("remote_edit.rs")),
            ("remote_path.rs", include_str!("remote_path.rs")),
            ("secrets.rs", include_str!("secrets.rs")),
            ("sftp.rs", include_str!("sftp.rs")),
            ("transport.rs", include_str!("transport.rs")),
            ("zenith_deploy.rs", include_str!("zenith_deploy.rs")),
        ];
        let banned = [
            "dangerous(",
            "danger::",
            "DangerousClientConfig",
            "NoCertificateVerification",
            "ServerCertVerifier",
        ];
        for (file, source) in sources {
            // This test names the banned words, so it skips its own source.
            if file == "zenith_deploy.rs" {
                continue;
            }
            for word in banned {
                assert!(!source.contains(word), "{file} contains {word}");
            }
        }
        // Code only: the doc comments also name the verifier.
        let ftp_code: String = include_str!("ftp.rs")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let ftps_branch = ftp_code
            .split("ServerType::Ftps =>")
            .nth(1)
            .and_then(|rest| rest.split("ServerType::Sftp").next())
            .expect("the FTPS branch of connect_with_logger");
        assert!(ftps_branch.contains("tls_connector()"), "{ftps_branch}");
        assert!(
            ftps_branch.contains("into_secure(connector"),
            "{ftps_branch}"
        );
        let connector_body = ftp_code
            .split("fn tls_connector()")
            .nth(1)
            .and_then(|rest| rest.split("\nasync fn open").next())
            .expect("tls_connector");
        assert!(
            connector_body.contains("http_client_tls::tls_config()"),
            "{connector_body}"
        );
    }

    /// Run with `ZENITH_DEPLOY_SELF_SIGNED_FTPS=host:port cargo test -p zenith_deploy -- --ignored`
    /// against an explicit-FTPS server with a self-signed certificate: connecting must fail.
    #[test]
    #[ignore = "needs a self-signed FTPS server; set ZENITH_DEPLOY_SELF_SIGNED_FTPS=host:port"]
    fn ftps_rejects_a_self_signed_certificate() {
        let target = std::env::var("ZENITH_DEPLOY_SELF_SIGNED_FTPS")
            .expect("set ZENITH_DEPLOY_SELF_SIGNED_FTPS=host:port");
        let (host, port) = target.rsplit_once(':').expect("host:port");
        let mut server = ServerConfig::new();
        server.host = host.into();
        server.port = port.parse().expect("numeric port");
        server.username = "u".into();
        server.password = Some(Password("p".into()));
        let result = smol::block_on(connect(&server));
        let error = match result {
            Ok(_) => panic!("a self-signed certificate was accepted"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains("TLS handshake failed"), "{error}");
        assert!(error.to_lowercase().contains("certificate"), "{error}");
    }

    #[test]
    fn the_cache_sweep_removes_only_stale_server_folders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (kept, stale) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        for name in [kept.as_str(), stale.as_str(), "notes"] {
            std::fs::create_dir_all(dir.path().join(name).join("www")).expect("mkdir");
            std::fs::write(dir.path().join(name).join("www/wp-config.php"), "<?php")
                .expect("write");
        }
        // A symlink named like a stale server must not lead the sweep out of the cache.
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("keep.txt"), "x").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path(),
            dir.path().join(uuid::Uuid::new_v4().to_string()),
        )
        .expect("symlink");
        sweep_cache(dir.path(), &[kept.clone()].into_iter().collect());
        assert!(dir.path().join(&kept).exists());
        assert!(
            !dir.path().join(&stale).exists(),
            "a deleted server's files go"
        );
        assert!(
            dir.path().join("notes").exists(),
            "not a server folder: untouched"
        );
        assert!(outside.path().join("keep.txt").exists());
        sweep_cache(&dir.path().join("missing"), &Default::default());
    }

    #[gpui::test]
    async fn loading_the_servers_sweeps_the_cache_of_deleted_ones(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, path) = (dir.path().join("cache"), dir.path().join("servers.json"));
        let saved = test_support::server("Prod", None);
        let servers = Servers {
            servers: vec![saved.clone()],
            ..Servers::default()
        };
        servers.save(&path).expect("save");
        let stale = uuid::Uuid::new_v4().to_string();
        for id in [saved.id.as_str(), stale.as_str()] {
            std::fs::create_dir_all(cache.join(id)).expect("mkdir");
            std::fs::write(cache.join(id).join("wp-config.php"), "<?php").expect("write");
        }
        let store = cx.new(|cx| ServerStore::load(path.clone(), cache.clone(), cx));
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| store.is_loaded()));
        assert!(cache.join(&saved.id).exists());
        assert!(!cache.join(&stale).exists());

        // A damaged servers.json is not proof that no server exists: nothing is swept.
        std::fs::write(&path, "{ not json").expect("write");
        let other = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(cache.join(&other)).expect("mkdir");
        let store = cx.new(|cx| ServerStore::load(path.clone(), cache.clone(), cx));
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| store.load_error().is_some()));
        assert!(cache.join(&other).exists());
    }

    #[gpui::test]
    async fn retries_join_the_keychain_read_in_flight_until_it_ends(cx: &mut TestAppContext) {
        let keychain = Arc::new(secrets::fake::FakeKeychain::default());
        *keychain.read_hang.lock().expect("lock") = true;
        let mut server = test_support::server("Prod", None);
        server.save_password = true;
        let store = cx.new(|_| {
            let mut store = ServerStore::in_memory(
                Servers::default(),
                PathBuf::from("/cache"),
                Arc::new(|_| Box::pin(async { Err(anyhow!("unused")) })),
            );
            store.keychain = keychain.clone();
            store
        });
        let ask =
            |cx: &mut TestAppContext| store.update(cx, |store, cx| store.password_for(&server, cx));
        let (first, second) = (ask(cx), ask(cx));
        cx.run_until_parked();
        assert_eq!(*keychain.reads.lock().expect("lock"), 1, "the retry joined");
        cx.executor().advance_clock(Duration::from_secs(121));
        for task in [first, second] {
            let error = task.await.expect_err("a hung Keychain");
            assert!(error.to_string().contains("did not answer"), "{error}");
        }
        let _third = ask(cx);
        cx.run_until_parked();
        assert_eq!(
            *keychain.reads.lock().expect("lock"),
            2,
            "the ended read is not reused"
        );
    }
}
