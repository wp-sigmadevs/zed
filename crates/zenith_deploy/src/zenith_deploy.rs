//! Deployment: server definitions and the FTP/FTPS/SFTP transports behind the Remote Host
//! tool window and the Deployment dialog (plan 37).

pub mod config;
mod dialog;
mod ftp;
mod panel;
pub mod remote_path;
pub mod secrets;
mod sftp;
pub mod transport;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Context, Entity, FutureExt as _, Global, Task};
use settings::{DockPosition, RegisterSetting, Settings, SettingsContent};

pub use config::{Password, ServerConfig, ServerType, Servers};
pub use dialog::{DeploymentDialog, OpenDeployment};
pub use panel::{RemoteHostPanel, ToggleFocus};
pub use transport::{EntryKind, RemoteEntry, ServerRefused, Transport, connect, download_entry};

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
    /// A damaged servers.json is never overwritten: saving stays off until the user fixes it.
    load_error: Option<String>,
    /// Passwords of servers with "Save password" off: memory only, gone on quit.
    session_passwords: HashMap<String, Password>,
    connect: ConnectFn,
    _load: Task<()>,
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

impl ServerStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalServerStore>().0.clone()
    }

    fn load(path: PathBuf, cache_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let load = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let result = cx
                    .background_spawn(async move { Servers::load(&path) })
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(servers) => this.servers = servers,
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
            load_error: None,
            session_passwords: HashMap::new(),
            connect: Arc::new(|server| Box::pin(async move { connect(&server).await })),
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
            load_error: None,
            session_passwords: HashMap::new(),
            connect,
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

    /// Replaces every server and writes servers.json (0600, atomically) off the main thread.
    fn save(&mut self, servers: Servers, cx: &mut Context<Self>) -> Task<Result<()>> {
        if let Some(error) = &self.load_error {
            return Task::ready(Err(anyhow!(
                "the saved servers could not be read, so they are not overwritten: {error}"
            )));
        }
        let kept: std::collections::HashSet<_> =
            servers.servers.iter().map(|server| &server.id).collect();
        self.session_passwords.retain(|id, _| kept.contains(id));
        for server in &self.servers.servers {
            if !kept.contains(&server.id) {
                self.clear_cache(&server.id, cx);
            }
        }
        self.servers = servers.clone();
        cx.notify();
        let Some(path) = self.path.clone() else {
            return Task::ready(Ok(()));
        };
        cx.background_spawn(async move { servers.save(&path) })
    }

    /// Deletes the cached copies of a server's remote files (maybe `wp-config.php`) off the
    /// main thread. Cancelled downloads only stop at their next await, so one can still be
    /// writing here: the removal is retried (see [`remove_with_retry`]).
    /// The cache is shared by every window: Disconnect in one window also deletes the files
    /// another window has open.
    pub(crate) fn clear_cache(&self, server_id: &str, cx: &App) {
        if remote_path::validate_name(server_id).is_err() {
            return;
        }
        let directory = self.cache_dir.join(server_id);
        cx.background_spawn(async move {
            match remove_with_retry(|| std::fs::remove_dir_all(&directory)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("deploy: removing {}: {error}", directory.display()),
            }
        })
        .detach();
    }

    fn set_session_password(&mut self, server_id: &str, password: Option<Password>) {
        match password {
            Some(password) => {
                self.session_passwords
                    .insert(server_id.to_string(), password);
            }
            None => {
                self.session_passwords.remove(server_id);
            }
        }
    }

    /// The password to log in with: none for anonymous FTP and SFTP (key or agent auth), else
    /// the session copy or the Keychain entry.
    fn password_for(&self, server: &ServerConfig, cx: &App) -> Task<Result<Option<Password>>> {
        if !server.uses_password() {
            return Task::ready(Ok(None));
        }
        if let Some(password) = self.session_passwords.get(&server.id) {
            return Task::ready(Ok(Some(password.clone())));
        }
        let load = secrets::load_password(server, cx);
        let name = server.name.clone();
        cx.background_spawn(async move {
            match load.await? {
                Some(password) => Ok(Some(Password(password))),
                None => bail!("no password is stored for {name}; enter it in Deployment settings"),
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
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        operation
            .with_timeout(OPERATION_TIMEOUT, &executor)
            .await
            .map_err(|_| anyhow!("the server did not answer within {OPERATION_TIMEOUT:?}"))?
    })
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
            if self
                .refusing
                .lock()
                .is_ok_and(|refusing| refusing.iter().any(|refused| refused == path))
            {
                return Err(anyhow::Error::new(ServerRefused(format!("550 {path}"))));
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
}
