use anyhow::Result;
use gpui::{App, AppContext as _, Task};

use crate::ServerConfig;

fn keychain_key(server_id: &str) -> String {
    format!("zenith-deploy:{server_id}")
}

/// The Keychain account of a stored secret. It names the exact target the secret was typed
/// for, so an entry that outlives a host, port, type or username change (a failed or late
/// delete) is never handed to the new target.
fn target_account(server: &ServerConfig) -> String {
    format!(
        "{}@{}:{}/{:?}",
        server.username, server.host, server.port, server.server_type
    )
}

/// The stored secret, only when it was saved for `account`.
fn secret_for_target(stored: Option<(String, Vec<u8>)>, account: &str) -> Option<String> {
    stored
        .filter(|(stored_account, _)| stored_account == account)
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
}

/// Never `zed_credentials_provider::global`: on the Dev channel it stores plaintext JSON.
pub fn save_password(server: &ServerConfig, password: &str, cx: &App) -> Task<Result<()>> {
    cx.write_credentials(
        &keychain_key(&server.id),
        &target_account(server),
        password.as_bytes(),
    )
}

/// `None` when nothing is stored for this server's current target.
pub fn load_password(server: &ServerConfig, cx: &App) -> Task<Result<Option<String>>> {
    let read = cx.read_credentials(&keychain_key(&server.id));
    let account = target_account(server);
    cx.background_spawn(async move { Ok(secret_for_target(read.await?, &account)) })
}

/// Deleting nothing is a success (an SFTP server or one without a stored password): the
/// Keychain reports a missing item as an error, so look first.
pub fn delete_password(server_id: &str, cx: &App) -> Task<Result<()>> {
    let key = keychain_key(server_id);
    let read = cx.read_credentials(&key);
    cx.spawn(async move |cx| {
        if read.await?.is_none() {
            return Ok(());
        }
        cx.update(|cx| cx.delete_credentials(&key)).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_key_is_namespaced_per_server() {
        assert_eq!(keychain_key("abc"), "zenith-deploy:abc");
    }

    #[test]
    fn a_stored_secret_is_only_returned_for_the_target_it_was_saved_for() {
        let mut saved_for = ServerConfig::new();
        saved_for.host = "a.example.com".into();
        saved_for.username = "deploy".into();
        let stored = || Some((target_account(&saved_for), b"not-a-real-value-1".to_vec()));

        assert_eq!(
            secret_for_target(stored(), &target_account(&saved_for)).as_deref(),
            Some("not-a-real-value-1")
        );
        let changes: [fn(&mut ServerConfig); 4] = [
            |server| server.host = "b.example.com".into(),
            |server| server.port += 1,
            |server| server.server_type = crate::ServerType::Sftp,
            |server| server.username = "someone-else".into(),
        ];
        for change in changes {
            let mut other = saved_for.clone();
            change(&mut other);
            assert_eq!(secret_for_target(stored(), &target_account(&other)), None);
        }
        assert_eq!(secret_for_target(None, &target_account(&saved_for)), None);
    }
}
