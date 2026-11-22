//! Deployment: server definitions and the FTP/FTPS/SFTP transports behind the Remote Host
//! tool window and the Deployment dialog (plan 37). M1a has no UI.

pub mod config;
mod ftp;
pub mod remote_path;
pub mod secrets;
mod sftp;
pub mod transport;

use std::time::Duration;

use anyhow::{Result, anyhow};
use gpui::{App, AppContext as _, FutureExt as _, Task};
use settings::{DockPosition, RegisterSetting, Settings, SettingsContent};

pub use config::{Password, ServerConfig, ServerType, Servers};
pub use transport::{EntryKind, RemoteEntry, Transport, connect, download_entry};

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
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};
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
        let ftp = include_str!("ftp.rs");
        assert!(ftp.contains("http_client_tls::tls_config()"));
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
