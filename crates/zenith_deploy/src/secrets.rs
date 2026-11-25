use std::sync::Arc;

use anyhow::Result;
use gpui::{App, AppContext as _, FutureExt as _, Task};
use std::time::Duration;

/// A Keychain read that nothing answers (a closed prompt, a stuck daemon) must not hold an
/// upload or a connect forever; generous because the system may ask the user to unlock.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(120);

use crate::{ServerConfig, ServerType};

/// The platform credential store, behind a trait so tests can make it fail.
pub trait Keychain: Send + Sync + 'static {
    fn read(&self, key: &str, cx: &App) -> Task<Result<Option<(String, Vec<u8>)>>>;
    fn write(&self, key: &str, account: &str, secret: &[u8], cx: &App) -> Task<Result<()>>;
    /// A missing entry is an error here, as on the real Keychain; use [`delete_password`].
    fn delete(&self, key: &str, cx: &App) -> Task<Result<()>>;
}

/// Never `zed_credentials_provider::global`: on the Dev channel it stores plaintext JSON.
pub struct SystemKeychain;

impl Keychain for SystemKeychain {
    fn read(&self, key: &str, cx: &App) -> Task<Result<Option<(String, Vec<u8>)>>> {
        cx.read_credentials(key)
    }

    fn write(&self, key: &str, account: &str, secret: &[u8], cx: &App) -> Task<Result<()>> {
        cx.write_credentials(key, account, secret)
    }

    fn delete(&self, key: &str, cx: &App) -> Task<Result<()>> {
        cx.delete_credentials(key)
    }
}

fn keychain_key(server_id: &str) -> String {
    format!("zenith-deploy:{server_id}")
}

/// The Keychain account of a stored secret. It names the exact target the secret was typed
/// for, so an entry that outlives a host, port, type or username change (a failed or late
/// delete) is never handed to the new target. Length prefixes keep it unambiguous when a
/// username or host contains `@`, `:` or `/`.
pub(crate) fn target_account(server: &ServerConfig) -> String {
    let server_type = match server.server_type {
        ServerType::Ftp => "ftp",
        ServerType::Ftps => "ftps",
        ServerType::Sftp => "sftp",
    };
    format!(
        "{}:{}|{}:{}|{}|{server_type}",
        server.username.len(),
        server.username,
        server.host.len(),
        server.host,
        server.port,
    )
}

/// What the Keychain holds for a server's current target.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(String),
    /// An entry exists but was saved for another target (or by an older version).
    OtherTarget,
    Missing,
}

fn lookup(stored: Option<(String, Vec<u8>)>, account: &str) -> Lookup {
    match stored {
        None => Lookup::Missing,
        Some((stored_account, bytes)) if stored_account == account => {
            Lookup::Found(String::from_utf8_lossy(&bytes).into_owned())
        }
        Some(_) => Lookup::OtherTarget,
    }
}

pub fn save_password(
    keychain: &dyn Keychain,
    server: &ServerConfig,
    password: &str,
    cx: &App,
) -> Task<Result<()>> {
    keychain.write(
        &keychain_key(&server.id),
        &target_account(server),
        password.as_bytes(),
        cx,
    )
}

pub fn load_password(
    keychain: &dyn Keychain,
    server: &ServerConfig,
    cx: &App,
) -> Task<Result<Lookup>> {
    let read = keychain.read(&keychain_key(&server.id), cx);
    let account = target_account(server);
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        let stored = read
            .with_timeout(KEYCHAIN_TIMEOUT, &executor)
            .await
            .map_err(|_| {
                anyhow::anyhow!("the Keychain did not answer within {KEYCHAIN_TIMEOUT:?}")
            })??;
        Ok(lookup(stored, &account))
    })
}

/// Deleting nothing is a success (an SFTP server or one without a stored password): the
/// Keychain reports a missing item as an error, so look first. Resolves to whether an entry
/// was actually removed.
pub fn delete_password(
    keychain: &Arc<dyn Keychain>,
    server_id: &str,
    cx: &App,
) -> Task<Result<bool>> {
    let key = keychain_key(server_id);
    let read = keychain.read(&key, cx);
    let keychain = keychain.clone();
    cx.spawn(async move |cx| {
        if read.await?.is_none() {
            return Ok(false);
        }
        cx.update(|cx| keychain.delete(&key, cx)).await?;
        Ok(true)
    })
}

#[cfg(test)]
pub(crate) mod fake {
    use std::{collections::HashMap, sync::Mutex};

    use anyhow::anyhow;

    use super::*;

    /// An in-memory Keychain whose writes and deletes can be made to fail.
    #[derive(Default)]
    pub struct FakeKeychain {
        pub entries: Mutex<HashMap<String, (String, Vec<u8>)>>,
        pub fail_delete: Mutex<bool>,
        pub fail_write: Mutex<bool>,
        /// Reads never answer (a Keychain that hangs).
        pub read_hang: Mutex<bool>,
        /// Keys whose delete fails while other deletes work.
        pub failing_delete_keys: Mutex<Vec<String>>,
        /// The next write finishes only when this resolves (a Keychain prompt waiting).
        pub write_gate: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    }

    impl FakeKeychain {
        pub fn put(&self, server: &ServerConfig, secret: &str) {
            self.entries.lock().expect("lock").insert(
                keychain_key(&server.id),
                (target_account(server), secret.as_bytes().to_vec()),
            );
        }

        pub fn fail_delete_of(&self, server: &ServerConfig) {
            self.failing_delete_keys
                .lock()
                .expect("lock")
                .push(keychain_key(&server.id));
        }

        pub fn account_of(&self, server: &ServerConfig) -> Option<String> {
            self.entries
                .lock()
                .expect("lock")
                .get(&keychain_key(&server.id))
                .map(|(account, _)| account.clone())
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self, key: &str, cx: &App) -> Task<Result<Option<(String, Vec<u8>)>>> {
            if *self.read_hang.lock().expect("lock") {
                return cx.background_spawn(futures::future::pending());
            }
            Task::ready(Ok(self.entries.lock().expect("lock").get(key).cloned()))
        }

        fn write(&self, key: &str, account: &str, secret: &[u8], cx: &App) -> Task<Result<()>> {
            if *self.fail_write.lock().expect("lock") {
                return Task::ready(Err(anyhow!("the Keychain is locked")));
            }
            self.entries
                .lock()
                .expect("lock")
                .insert(key.to_string(), (account.to_string(), secret.to_vec()));
            match self.write_gate.lock().expect("lock").take() {
                Some(gate) => cx.background_spawn(async move {
                    gate.await.ok();
                    Ok(())
                }),
                None => Task::ready(Ok(())),
            }
        }

        fn delete(&self, key: &str, _: &App) -> Task<Result<()>> {
            let failing_key = self
                .failing_delete_keys
                .lock()
                .expect("lock")
                .iter()
                .any(|failing| failing == key);
            if *self.fail_delete.lock().expect("lock") || failing_key {
                return Task::ready(Err(anyhow!("the user denied the Keychain prompt")));
            }
            let removed = self.entries.lock().expect("lock").remove(key);
            Task::ready(removed.map(drop).ok_or_else(|| anyhow!("item not found")))
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::{fake::FakeKeychain, *};

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
            lookup(stored(), &target_account(&saved_for)),
            Lookup::Found("not-a-real-value-1".into())
        );
        let changes: [fn(&mut ServerConfig); 4] = [
            |server| server.host = "b.example.com".into(),
            |server| server.port += 1,
            |server| server.server_type = ServerType::Sftp,
            |server| server.username = "someone-else".into(),
        ];
        for change in changes {
            let mut other = saved_for.clone();
            change(&mut other);
            assert_eq!(
                lookup(stored(), &target_account(&other)),
                Lookup::OtherTarget
            );
        }
        assert_eq!(lookup(None, &target_account(&saved_for)), Lookup::Missing);
    }

    #[test]
    fn the_account_is_unambiguous_when_the_username_contains_separators() {
        let mut first = ServerConfig::new();
        first.username = "a@b".into();
        first.host = "c".into();
        let mut second = first.clone();
        second.username = "a".into();
        second.host = "b@c".into();
        assert_ne!(target_account(&first), target_account(&second));
        first.username = "a|0:b".into();
        first.host = "c".into();
        second.username = "a".into();
        second.host = "b|0:c".into();
        assert_ne!(target_account(&first), target_account(&second));
    }

    #[gpui::test]
    async fn deleting_a_missing_entry_succeeds_and_a_failing_delete_is_reported(
        cx: &mut TestAppContext,
    ) {
        let fake = Arc::new(FakeKeychain::default());
        let keychain: Arc<dyn Keychain> = fake.clone();
        let server = ServerConfig::new();

        cx.update(|cx| delete_password(&keychain, &server.id, cx))
            .await
            .expect("nothing stored is not an error");

        fake.put(&server, "not-a-real-value-1");
        *fake.fail_delete.lock().expect("lock") = true;
        let result = cx
            .update(|cx| delete_password(&keychain, &server.id, cx))
            .await;
        assert!(result.is_err());
        assert!(fake.account_of(&server).is_some());

        *fake.fail_delete.lock().expect("lock") = false;
        cx.update(|cx| delete_password(&keychain, &server.id, cx))
            .await
            .expect("delete");
        assert!(fake.account_of(&server).is_none());
    }

    #[gpui::test]
    async fn a_keychain_read_that_never_answers_times_out(cx: &mut TestAppContext) {
        let keychain = FakeKeychain::default();
        *keychain.read_hang.lock().expect("lock") = true;
        let server = ServerConfig::new();
        let task = cx.update(|cx| load_password(&keychain, &server, cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(KEYCHAIN_TIMEOUT + Duration::from_secs(1));
        match task.await {
            Err(error) => assert!(error.to_string().contains("did not answer"), "{error}"),
            Ok(_) => panic!("a hung Keychain must not look like an answer"),
        }
    }
}
