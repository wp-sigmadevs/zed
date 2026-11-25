//! Edit a remote file (plan 37 M4a, chunk C2): the index of cached copies and the uploader.
//!
//! No UI here. The panel (C3) registers a download with [`ServerStore::register_download`],
//! attaches the editor's buffer, and on every save calls [`ServerStore::upload`]; the answer to
//! each prompt goes back through [`ServerStore::set_upload_decision`] or `force`.
//!
//! Trust rules the code below keeps:
//! - The upload target (server id + remote path) comes only from an index entry created by our
//!   own download, never from the buffer path, project config or any file in a repo.
//! - "Upload without asking" lives only in memory for this session ([`ServerStore::upload_decisions`]);
//!   no setting and no file can turn it on.
//! - A project that is not local and unshared never uploads (a collab guest's save would use the
//!   host's credentials).
//! - The bytes are the buffer's text as saved, never a re-read of the cache file.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use gpui::{AsyncApp, Context, Entity, EventEmitter, TaskExt as _, WeakEntity};
use language::Buffer;

use crate::{
    ServerStore,
    config::{ServerConfig, ServerType},
    remote_path::{validate_name, validate_remote_path},
    secrets, spawn_operation, spawn_operation_with_timeout,
    transport::{MAX_DOWNLOAD_BYTES, RemoteStat, Transport, upload_entry},
    upload_timeout,
};

/// What the user said about uploading on save for one server, this session only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UploadDecision {
    /// Nothing said yet: the save asks first ([`UploadStart::NeedsConfirmation`]).
    #[default]
    Ask,
    /// Upload without asking again until the app quits or the server's target changes.
    Allow,
    /// Never upload this session.
    Deny,
}

/// Facts the confirmation text must state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadWarnings {
    /// Passwords and file contents cross the network unencrypted (plain FTP).
    pub clear_text: bool,
    /// The server may write through a symlink to another file (FTP cannot tell a link from a file).
    pub follows_symlinks: bool,
}

pub fn upload_warnings(server_type: ServerType) -> UploadWarnings {
    UploadWarnings {
        clear_text: server_type == ServerType::Ftp,
        follows_symlinks: matches!(server_type, ServerType::Ftp | ServerType::Ftps),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyState {
    Idle,
    Uploading,
}

/// One cached copy of a remote file that this app downloaded.
pub struct RemoteCopy {
    pub server_id: String,
    pub remote_path: String,
    /// The remote stat taken before the download: what the server looked like when this copy
    /// was made, or after our last upload.
    pub stat: Option<RemoteStat>,
    pub buffers: Vec<WeakEntity<Buffer>>,
    pub state: CopyState,
    /// Bytes of the newest save that arrived during an upload (older ones are replaced): one
    /// rerun follows, however many saves came.
    pub again: Option<Vec<u8>>,
}

impl RemoteCopy {
    /// A live buffer (clean or dirty) or a running upload.
    pub fn is_open(&self) -> bool {
        self.state == CopyState::Uploading
            || self.buffers.iter().any(|buffer| buffer.upgrade().is_some())
    }
}

/// The synchronous answer of [`ServerStore::upload`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadStart {
    /// An upload runs; its result arrives as [`ServerStoreEvent::UploadFinished`].
    Started,
    /// An upload of this copy was running: these bytes replace any older queued ones and go out
    /// in exactly one rerun.
    Queued,
    /// The user has not said yes for this server: ask (Upload / Always / Not now), call
    /// [`ServerStore::set_upload_decision`], then upload again.
    NeedsConfirmation,
    /// Not uploaded, with a message for the user.
    Refused(String),
    /// The path is not in the index (not our download, or a pre-existing cache file).
    NotRemote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    Uploaded,
    /// The remote file differs from the copy: prompt Overwrite / Compare / Cancel; Overwrite is
    /// `upload(.., force: true)`.
    Conflict,
    /// The remote file is missing or unverifiable (FTP SIZE answers 550 for all of those) or
    /// there was no baseline: ask before writing; yes is `force: true`.
    AskFirst,
    /// Not sent: the remote could not be checked or the file cannot be uploaded.
    Blocked(String),
    /// The write started and failed. The text says the server file may be incomplete.
    Failed(String),
}

pub enum ServerStoreEvent {
    UploadFinished {
        cache_path: PathBuf,
        outcome: UploadOutcome,
    },
}

impl EventEmitter<ServerStoreEvent> for ServerStore {}

/// Where the download of `remote_path` from `server_id` lands. The panel builds its path the same
/// way; [`ServerStore::register_download`] accepts no other.
pub fn cache_path_for(cache_dir: &Path, server_id: &str, remote_path: &str) -> Result<PathBuf> {
    validate_name(server_id)?;
    validate_remote_path(remote_path)?;
    validate_name(remote_path.rsplit('/').next().unwrap_or_default())?;
    Ok(cache_dir
        .join(server_id)
        .join(remote_path.trim_start_matches('/')))
}

enum Gate {
    Go,
    Ask,
    Stop(String),
}

/// Whether a save may upload at all, before anything touches the network.
fn gate(is_local_unshared: bool, decision: UploadDecision) -> Gate {
    if !is_local_unshared {
        return Gate::Stop("Remote files are only uploaded from a local, unshared project.".into());
    }
    match decision {
        UploadDecision::Allow => Gate::Go,
        UploadDecision::Ask => Gate::Ask,
        UploadDecision::Deny => {
            Gate::Stop("Uploading to this server is off for this session.".into())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StatCheck {
    Proceed,
    Conflict,
    AskFirst,
}

/// `recorded` is the baseline, `current` the server now. Only an exact match proceeds; no
/// baseline or a missing file is never "go ahead".
fn check_stat(recorded: Option<&RemoteStat>, current: Option<&RemoteStat>) -> StatCheck {
    match (recorded, current) {
        (Some(recorded), Some(current)) if recorded == current => StatCheck::Proceed,
        (Some(_), Some(_)) => StatCheck::Conflict,
        _ => StatCheck::AskFirst,
    }
}

/// Names the target in messages: path and server name only, never file contents.
struct Target<'a> {
    server_name: &'a str,
    remote_path: &'a str,
}

enum Cycle {
    Uploaded(Option<RemoteStat>),
    Stopped(UploadOutcome),
}

/// One check-then-write over a connected session. `force` skips only the Conflict and AskFirst
/// stops; a stat error still blocks.
async fn run_cycle(
    transport: &mut dyn Transport,
    target: &Target<'_>,
    recorded: Option<&RemoteStat>,
    bytes: &[u8],
    force: bool,
) -> Cycle {
    let Target {
        server_name,
        remote_path,
    } = target;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Cycle::Stopped(UploadOutcome::Blocked(format!(
            "{remote_path} is larger than {} MiB; it is not uploaded to {server_name}.",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        )));
    }
    let current = match transport.stat(remote_path).await {
        Ok(current) => current,
        Err(error) => {
            return Cycle::Stopped(UploadOutcome::Blocked(format!(
                "Could not check {remote_path} on {server_name}, so nothing was uploaded: {error:#}"
            )));
        }
    };
    if !force {
        match check_stat(recorded, current.as_ref()) {
            StatCheck::Proceed => {}
            StatCheck::Conflict => return Cycle::Stopped(UploadOutcome::Conflict),
            StatCheck::AskFirst => return Cycle::Stopped(UploadOutcome::AskFirst),
        }
    }
    if let Err(error) = upload_entry(transport, remote_path, bytes).await {
        return Cycle::Stopped(UploadOutcome::Failed(failed_message(
            server_name,
            remote_path,
            &error,
        )));
    }
    // A failed re-stat leaves no baseline: the next save asks first instead of guessing.
    Cycle::Uploaded(transport.stat(remote_path).await.ok().flatten())
}

fn failed_message(server_name: &str, remote_path: &str, error: &dyn std::fmt::Display) -> String {
    format!(
        "Uploading {remote_path} to {server_name} failed; the server file may be incomplete: {error:#}"
    )
}

/// The buffer's text as last saved, in the file's line endings and (UTF-8 only) encoding.
fn saved_bytes(buffer: &Buffer) -> Result<Vec<u8>, String> {
    let encoding = buffer.encoding();
    if encoding.name() != "UTF-8" {
        return Err(format!(
            "This file is saved as {}; only UTF-8 files are uploaded.",
            encoding.name()
        ));
    }
    let text = buffer
        .rope_for_version(buffer.saved_version())
        .to_string()
        .replace('\n', buffer.line_ending().as_str());
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if buffer.has_bom() {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

impl ServerStore {
    /// The decision for `server`: [`UploadDecision::Ask`] unless the user chose one this session
    /// for the server's current target (a changed host, port, type or username resets it).
    pub fn upload_decision(&self, server_id: &str) -> UploadDecision {
        let Some(server) = self.servers.servers.iter().find(|s| s.id == server_id) else {
            return UploadDecision::Ask;
        };
        self.upload_decisions
            .get(server_id)
            .filter(|(account, _)| *account == secrets::target_account(server))
            .map_or(UploadDecision::Ask, |(_, decision)| *decision)
    }

    /// Records the user's answer for this session. False when the server does not exist.
    pub fn set_upload_decision(&mut self, server_id: &str, decision: UploadDecision) -> bool {
        let Some(server) = self.servers.servers.iter().find(|s| s.id == server_id) else {
            return false;
        };
        let account = secrets::target_account(server);
        self.upload_decisions
            .insert(server_id.to_string(), (account, decision));
        true
    }

    /// Records a download made by the panel. `cache_path` must be exactly where
    /// [`cache_path_for`] puts `remote_path`, so a path outside the cache or one that does not
    /// match the remote file is refused. `stat` is the remote stat taken before the download.
    /// Refused while the copy is open (a live buffer or running upload): a download over it
    /// would replace the baseline the open buffer's edits were made against.
    pub fn register_download(
        &mut self,
        cache_path: PathBuf,
        server_id: &str,
        remote_path: &str,
        stat: Option<RemoteStat>,
    ) -> Result<()> {
        if cache_path != cache_path_for(&self.cache_dir, server_id, remote_path)? {
            bail!(
                "{} is not the cache location of {remote_path}",
                cache_path.display()
            );
        }
        if self
            .copies
            .get(&cache_path)
            .is_some_and(RemoteCopy::is_open)
        {
            bail!("{remote_path} is already open for editing");
        }
        self.copies.insert(
            cache_path,
            RemoteCopy {
                server_id: server_id.to_string(),
                remote_path: remote_path.to_string(),
                stat,
                buffers: Vec::new(),
                state: CopyState::Idle,
                again: None,
            },
        );
        Ok(())
    }

    /// Tells the index which buffer shows `cache_path`. False when it is not registered.
    pub fn attach_buffer(&mut self, cache_path: &Path, buffer: &Entity<Buffer>) -> bool {
        let Some(copy) = self.copies.get_mut(cache_path) else {
            return false;
        };
        copy.buffers.retain(|weak| weak.upgrade().is_some());
        if !copy
            .buffers
            .iter()
            .any(|weak| weak.entity_id() == buffer.entity_id())
        {
            copy.buffers.push(buffer.downgrade());
        }
        true
    }

    /// The copy a buffer's absolute path belongs to: an exact match only, no normalising, so a
    /// path that merely resembles a cache file is not one.
    pub fn copy_for_buffer_path(&self, path: &Path) -> Option<&RemoteCopy> {
        self.copies.get(path)
    }

    /// Under the cache but not in the index (left from an earlier session, or not ours): C3 opens
    /// such a file read-only.
    pub fn is_orphan(&self, path: &Path) -> bool {
        path.starts_with(&self.cache_dir) && !self.copies.contains_key(path)
    }

    /// Uploads the saved text of the buffer attached to `cache_path` to the copy's own server and
    /// remote path. Call it after the buffer saved. `is_local_unshared` is
    /// `project.is_local() && !project.is_shared()` of the project holding the buffer.
    /// `force` (the user chose Overwrite or "upload anyway") skips the Conflict/AskFirst stops
    /// of the first run only. The result arrives as [`ServerStoreEvent::UploadFinished`]; the
    /// store owns the work, so closing the panel or window does not cancel it.
    pub fn upload(
        &mut self,
        cache_path: &Path,
        is_local_unshared: bool,
        force: bool,
        cx: &mut Context<Self>,
    ) -> UploadStart {
        let Some(copy) = self.copies.get(cache_path) else {
            return UploadStart::NotRemote;
        };
        let Some(server) = self
            .servers
            .servers
            .iter()
            .find(|server| server.id == copy.server_id)
            .cloned()
        else {
            return UploadStart::Refused("This server was removed.".into());
        };
        match gate(is_local_unshared, self.upload_decision(&server.id)) {
            Gate::Go => {}
            Gate::Ask => return UploadStart::NeedsConfirmation,
            Gate::Stop(message) => return UploadStart::Refused(message),
        }
        let Some(buffer) = copy.buffers.iter().find_map(WeakEntity::upgrade) else {
            return UploadStart::Refused("The file is not open.".into());
        };
        let bytes = match saved_bytes(buffer.read(cx)) {
            Ok(bytes) => bytes,
            Err(message) => return UploadStart::Refused(message),
        };
        let Some(copy) = self.copies.get_mut(cache_path) else {
            return UploadStart::NotRemote;
        };
        if copy.state == CopyState::Uploading {
            copy.again = Some(bytes);
            return UploadStart::Queued;
        }
        copy.state = CopyState::Uploading;
        let cache = cache_path.to_path_buf();
        let task = cx.spawn(async move |this, cx| {
            run_upload(this, cx, cache, server, bytes, force).await;
        });
        self.uploads.insert(cache_path.to_path_buf(), task);
        UploadStart::Started
    }

    /// Ends one cycle in a single update so a save cannot slip between "no rerun" and "idle":
    /// returns the bytes of the queued rerun, if the copy was uploaded and another save came.
    fn settle(
        &mut self,
        cache_path: &Path,
        outcome: &UploadOutcome,
        new_stat: Option<Option<RemoteStat>>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<u8>> {
        let mut next = None;
        if let Some(copy) = self.copies.get_mut(cache_path) {
            if let Some(stat) = new_stat {
                copy.stat = stat;
            }
            next = copy
                .again
                .take()
                .filter(|_| *outcome == UploadOutcome::Uploaded);
            if next.is_none() {
                copy.state = CopyState::Idle;
            }
        }
        cx.emit(ServerStoreEvent::UploadFinished {
            cache_path: cache_path.to_path_buf(),
            outcome: outcome.clone(),
        });
        next
    }
}

fn disconnect(cx: &mut AsyncApp, mut transport: Box<dyn Transport>) {
    cx.update(|cx| {
        spawn_operation(cx, async move { transport.disconnect().await }).detach_and_log_err(cx)
    });
}

/// The upload loop of one copy: one session, one cycle per saved text, stopping at the first
/// outcome that is not a success or when no newer save is queued.
async fn run_upload(
    this: WeakEntity<ServerStore>,
    cx: &mut AsyncApp,
    cache_path: PathBuf,
    server: ServerConfig,
    mut bytes: Vec<u8>,
    mut force: bool,
) {
    let mut session: Option<Box<dyn Transport>> = None;
    loop {
        let copy = this.read_with(cx, |this, _| {
            this.copies
                .get(&cache_path)
                .map(|copy| (copy.remote_path.clone(), copy.stat.clone()))
        });
        let Ok(Some((remote_path, recorded))) = copy else {
            return;
        };
        let transport = match session.take() {
            Some(transport) => transport,
            None => {
                let open = this.update(cx, |this, cx| this.open_session(server.clone(), cx));
                let opened = match open {
                    Ok(open) => open.await,
                    Err(_) => return,
                };
                match opened {
                    Ok(transport) => transport,
                    Err(error) => {
                        let message = format!(
                            "Could not connect to {}, so nothing was uploaded: {error:#}",
                            server.name
                        );
                        this.update(cx, |this, cx| {
                            this.settle(&cache_path, &UploadOutcome::Blocked(message), None, cx)
                        })
                        .ok();
                        return;
                    }
                }
            }
        };
        let len = bytes.len();
        let job = cx.update(|cx| {
            let server_name = server.name.clone();
            let remote_path = remote_path.clone();
            let bytes = std::mem::take(&mut bytes);
            spawn_operation_with_timeout(cx, upload_timeout(len), async move {
                let mut transport = transport;
                let target = Target {
                    server_name: &server_name,
                    remote_path: &remote_path,
                };
                let cycle = run_cycle(
                    transport.as_mut(),
                    &target,
                    recorded.as_ref(),
                    &bytes,
                    force,
                )
                .await;
                Ok((transport, cycle))
            })
        });
        let (transport, cycle) = match job.await {
            Ok(done) => done,
            Err(error) => {
                // The deadline or a dead connection, possibly mid-write; the session is dropped.
                let message = failed_message(&server.name, &remote_path, &error);
                this.update(cx, |this, cx| {
                    this.settle(&cache_path, &UploadOutcome::Failed(message), None, cx)
                })
                .ok();
                return;
            }
        };
        let (outcome, new_stat) = match cycle {
            Cycle::Uploaded(stat) => (UploadOutcome::Uploaded, Some(stat)),
            Cycle::Stopped(outcome) => (outcome, None),
        };
        let next = this
            .update(cx, |this, cx| {
                this.settle(&cache_path, &outcome, new_stat, cx)
            })
            .ok()
            .flatten();
        match next {
            Some(next) => {
                bytes = next;
                force = false;
                session = Some(transport);
            }
            None => {
                disconnect(cx, transport);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{AppContext as _, Subscription, TestAppContext};

    use super::*;
    use crate::{
        Servers,
        test_support::{FakeRemote, server},
    };

    const REMOTE: &str = "/www/a.php";

    fn stat(stamp: &str) -> RemoteStat {
        RemoteStat {
            size: 5,
            stamp: Some(stamp.into()),
        }
    }

    struct Fixture {
        store: Entity<ServerStore>,
        remote: FakeRemote,
        buffer: Entity<Buffer>,
        cache_path: PathBuf,
        server_id: String,
        dir: tempfile::TempDir,
        events: Rc<RefCell<Vec<UploadOutcome>>>,
        _subscription: Subscription,
    }

    /// One registered copy of `/www/a.php` (remote stamp "v1") with a clean saved buffer "hello"
    /// and uploads allowed.
    fn fixture(cx: &mut TestAppContext) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().join("cache");
        let mut prod = server("Prod", None);
        prod.anonymous = true;
        let server_id = prod.id.clone();
        let remote = FakeRemote::default();
        remote
            .stats
            .lock()
            .expect("stats")
            .insert(REMOTE.into(), stat("v1"));
        let servers = Servers {
            servers: vec![prod],
            ..Servers::default()
        };
        let store = cx.new(|_| ServerStore::in_memory(servers, cache_dir.clone(), remote.connect_fn()));
        let cache_path = cache_path_for(&cache_dir, &server_id, REMOTE).expect("cache path");
        std::fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache_path, "cache bytes").expect("write");
        let buffer = cx.new(|cx| Buffer::local("hello", cx));
        buffer.update(cx, |buffer, cx| buffer.did_save(buffer.version(), None, cx));
        store.update(cx, |store, _| {
            store
                .register_download(cache_path.clone(), &server_id, REMOTE, Some(stat("v1")))
                .expect("register");
            assert!(store.attach_buffer(&cache_path, &buffer));
            assert!(store.set_upload_decision(&server_id, UploadDecision::Allow));
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let subscription = cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &ServerStoreEvent, _| {
                let ServerStoreEvent::UploadFinished { outcome, .. } = event;
                sink.borrow_mut().push(outcome.clone());
            })
        });
        Fixture {
            store,
            remote,
            buffer,
            cache_path,
            server_id,
            dir,
            events,
            _subscription: subscription,
        }
    }

    impl Fixture {
        /// Replaces the text and marks it saved, like a save.
        fn save(&self, text: &str, cx: &mut TestAppContext) {
            self.buffer.update(cx, |buffer, cx| {
                let len = buffer.len();
                buffer.edit([(0..len, text)], None, cx);
                buffer.did_save(buffer.version(), None, cx);
            });
        }

        fn upload(&self, unshared: bool, force: bool, cx: &mut TestAppContext) -> UploadStart {
            self.store
                .update(cx, |store, cx| store.upload(&self.cache_path, unshared, force, cx))
        }

        fn puts(&self) -> Vec<(String, Vec<u8>)> {
            self.remote.puts.lock().expect("puts").clone()
        }

        fn set_remote_stamp(&self, stamp: &str) {
            self.remote
                .stats
                .lock()
                .expect("stats")
                .insert(REMOTE.into(), stat(stamp));
        }

        fn clear(&self, cx: &mut TestAppContext) -> usize {
            let id = self.server_id.clone();
            self.store.update(cx, |store, cx| store.clear_cache(&id, cx))
        }
    }

    #[test]
    fn the_gate_needs_a_local_unshared_project_and_a_yes_for_the_server() {
        for decision in [
            UploadDecision::Ask,
            UploadDecision::Allow,
            UploadDecision::Deny,
        ] {
            assert!(
                matches!(gate(false, decision), Gate::Stop(_)),
                "a shared or remote project never uploads ({decision:?})"
            );
        }
        assert!(matches!(gate(true, UploadDecision::Ask), Gate::Ask));
        assert!(matches!(gate(true, UploadDecision::Deny), Gate::Stop(_)));
        assert!(matches!(gate(true, UploadDecision::Allow), Gate::Go));
        assert_eq!(UploadDecision::default(), UploadDecision::Ask);
    }

    #[test]
    fn only_an_exact_stat_match_proceeds() {
        let (v1, v2) = (stat("v1"), stat("v2"));
        assert_eq!(check_stat(Some(&v1), Some(&v1)), StatCheck::Proceed);
        assert_eq!(check_stat(Some(&v1), Some(&v2)), StatCheck::Conflict);
        assert_eq!(check_stat(Some(&v1), None), StatCheck::AskFirst);
        assert_eq!(check_stat(None, Some(&v1)), StatCheck::AskFirst);
        assert_eq!(check_stat(None, None), StatCheck::AskFirst);
    }

    #[test]
    fn ftp_warns_about_clear_text_and_symlinks_ftps_only_about_symlinks() {
        assert_eq!(
            upload_warnings(ServerType::Ftp),
            UploadWarnings {
                clear_text: true,
                follows_symlinks: true
            }
        );
        assert_eq!(
            upload_warnings(ServerType::Ftps),
            UploadWarnings {
                clear_text: false,
                follows_symlinks: true
            }
        );
        assert_eq!(
            upload_warnings(ServerType::Sftp),
            UploadWarnings {
                clear_text: false,
                follows_symlinks: false
            }
        );
    }

    #[test]
    fn a_cache_path_is_built_from_validated_parts_only() {
        let root = Path::new("/cache");
        assert_eq!(
            cache_path_for(root, "id1", "/www/a.php").ok(),
            Some(PathBuf::from("/cache/id1/www/a.php"))
        );
        for (id, path) in [
            ("../x", "/www/a.php"),
            ("a/b", "/www/a.php"),
            ("id1", "/www/../a.php"),
            ("id1", "www/a.php"),
            ("id1", "/www/"),
            ("id1", "/www/a\nb"),
        ] {
            assert!(cache_path_for(root, id, path).is_err(), "{id} {path:?}");
        }
    }

    #[gpui::test]
    fn registering_refuses_a_path_that_is_not_the_cache_location(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let cache_dir = f.dir.path().join("cache");
        let register = |cache_path: PathBuf, id: &str, remote: &str, cx: &mut TestAppContext| {
            f.store
                .update(cx, |store, _| store.register_download(cache_path, id, remote, None))
        };
        let other = cache_path_for(&cache_dir, &f.server_id, "/www/b.php").expect("path");
        assert!(register(other.clone(), &f.server_id, "/www/b.php", cx).is_ok());
        for (path, remote) in [
            // Outside the cache root.
            (f.dir.path().join("elsewhere/b.php"), "/www/b.php"),
            // Inside the cache but not where that remote file is cached.
            (f.cache_path.clone(), "/www/c.php"),
            // A traversing remote path.
            (cache_dir.join("../etc/passwd"), "/../etc/passwd"),
        ] {
            assert!(register(path.clone(), &f.server_id, remote, cx).is_err(), "{path:?}");
        }
        // An open copy (live buffer) is not replaced by a new download.
        assert!(register(f.cache_path.clone(), &f.server_id, REMOTE, cx).is_err());
    }

    #[gpui::test]
    fn a_conflict_stops_the_put_and_force_overwrites(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.set_remote_stamp("v2");
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(*f.events.borrow(), [UploadOutcome::Conflict]);
        assert!(f.puts().is_empty(), "a conflict must not write");

        assert_eq!(f.upload(true, true, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.events.borrow().last(), Some(&UploadOutcome::Uploaded));
        assert_eq!(f.puts(), [(REMOTE.to_string(), b"hello".to_vec())]);
    }

    #[gpui::test]
    fn a_missing_remote_file_asks_first_and_never_goes_ahead(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.remote.stats.lock().expect("stats").clear();
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(*f.events.borrow(), [UploadOutcome::AskFirst]);
        assert!(f.puts().is_empty());

        f.upload(true, true, cx);
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 1, "yes to the question uploads");
    }

    #[gpui::test]
    fn a_stat_error_blocks_the_upload_even_when_forced(cx: &mut TestAppContext) {
        let f = fixture(cx);
        *f.remote.stat_error.lock().expect("error") = Some("550 is a directory".into());
        for force in [false, true] {
            f.upload(true, force, cx);
            cx.run_until_parked();
        }
        let events = f.events.borrow();
        assert_eq!(events.len(), 2);
        for event in events.iter() {
            let UploadOutcome::Blocked(message) = event else {
                panic!("{event:?}");
            };
            assert!(message.contains("550 is a directory") && message.contains(REMOTE), "{message}");
        }
        assert!(f.puts().is_empty());
    }

    #[gpui::test]
    fn saves_during_an_upload_coalesce_into_one_rerun_with_the_newest_bytes(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.save("one", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        f.save("two", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Queued);
        f.save("three", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Queued);
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [
                (REMOTE.to_string(), b"one".to_vec()),
                (REMOTE.to_string(), b"three".to_vec())
            ],
            "exactly two puts, the second with the newest bytes"
        );
        assert_eq!(
            *f.events.borrow(),
            [UploadOutcome::Uploaded, UploadOutcome::Uploaded]
        );
        // Idle again: the next save starts a fresh upload.
        f.save("four", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
    }

    #[gpui::test]
    fn the_stat_after_an_upload_is_the_new_baseline(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.upload(true, false, cx);
        cx.run_until_parked();
        let recorded = f
            .store
            .read_with(cx, |store, _| store.copy_for_buffer_path(&f.cache_path).map(|c| c.stat.clone()));
        assert_eq!(recorded, Some(Some(stat_after_put(1, 5))));
        // The baseline moved, so the next save is not a conflict with our own write.
        f.save("again", cx);
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.events.borrow().last(), Some(&UploadOutcome::Uploaded));
        assert_eq!(f.puts().len(), 2);
    }

    fn stat_after_put(n: usize, size: u64) -> RemoteStat {
        RemoteStat {
            size,
            stamp: Some(format!("put-{n}")),
        }
    }

    #[gpui::test]
    fn a_shared_or_remote_project_never_uploads_even_when_allowed(cx: &mut TestAppContext) {
        let f = fixture(cx);
        assert!(matches!(f.upload(false, false, cx), UploadStart::Refused(_)));
        assert!(matches!(f.upload(false, true, cx), UploadStart::Refused(_)));
        cx.run_until_parked();
        assert!(f.remote.log().is_empty(), "no connection: {:?}", f.remote.log());
        assert!(f.events.borrow().is_empty());
    }

    #[gpui::test]
    fn ask_and_deny_never_upload_and_allow_is_per_server_and_session(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Ask);
        });
        assert_eq!(f.upload(true, false, cx), UploadStart::NeedsConfirmation);
        assert_eq!(f.upload(true, true, cx), UploadStart::NeedsConfirmation, "force is not consent");
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Deny);
        });
        assert!(matches!(f.upload(true, false, cx), UploadStart::Refused(_)));
        cx.run_until_parked();
        assert!(f.remote.log().is_empty(), "no connection: {:?}", f.remote.log());
        // Unknown servers cannot be allowed, and a new store starts at Ask.
        f.store.update(cx, |store, _| {
            assert!(!store.set_upload_decision("nope", UploadDecision::Allow));
            assert_eq!(store.upload_decision("nope"), UploadDecision::Ask);
        });
    }

    #[gpui::test]
    fn allow_is_forgotten_when_the_servers_target_changes(cx: &mut TestAppContext) {
        let f = fixture(cx);
        assert_eq!(
            f.store.read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Allow
        );
        f.store.update(cx, |store, cx| {
            let mut servers = store.servers().clone();
            servers.servers[0].host = "other.example.com".into();
            store.apply(servers, Vec::new(), cx);
        });
        assert_eq!(
            f.store.read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Ask
        );
    }

    #[gpui::test]
    fn the_bytes_come_from_the_saved_buffer_not_the_cache_file(cx: &mut TestAppContext) {
        let f = fixture(cx);
        assert_eq!(std::fs::read(&f.cache_path).expect("read"), b"cache bytes");
        f.save("saved text", cx);
        // Typed after the save, not yet saved: stays out of the upload.
        f.buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(len..len, " typing")], None, cx);
        });
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.puts(), [(REMOTE.to_string(), b"saved text".to_vec())]);
    }

    #[gpui::test]
    fn an_upload_target_cannot_be_redirected_by_a_buffer_path(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let cache_dir = f.dir.path().join("cache");
        let other = cache_dir.join(&f.server_id).join("www/other.php");
        let sneaky = cache_dir.join(&f.server_id).join("www/../www/a.php");
        for path in [other, sneaky, f.dir.path().join("project/a.php")] {
            f.store.update(cx, |store, cx| {
                assert_eq!(store.upload(&path, true, true, cx), UploadStart::NotRemote, "{path:?}");
                assert!(store.copy_for_buffer_path(&path).is_none());
            });
        }
        cx.run_until_parked();
        assert!(f.remote.log().is_empty());
        // The real entry still goes to its own recorded path.
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.puts()[0].0, REMOTE);
    }

    #[gpui::test]
    fn files_under_the_cache_that_are_not_ours_are_orphans(cx: &mut TestAppContext) {
        let f = fixture(cx);
        let cache_dir = f.dir.path().join("cache");
        f.store.read_with(cx, |store, _| {
            assert!(!store.is_orphan(&f.cache_path));
            assert!(store.is_orphan(&cache_dir.join(&f.server_id).join("www/old.php")));
            assert!(!store.is_orphan(&f.dir.path().join("project/a.php")));
        });
    }

    #[gpui::test]
    fn a_failed_upload_names_the_file_and_server_never_the_contents(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.save("TOP-SECRET-CONTENTS", cx);
        *f.remote.put_error.lock().expect("error") = Some("connection reset".into());
        f.upload(true, false, cx);
        cx.run_until_parked();
        let events = f.events.borrow();
        let [UploadOutcome::Failed(message)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.contains(REMOTE) && message.contains("Prod"), "{message}");
        assert!(message.contains("the server file may be incomplete"), "{message}");
        assert!(!message.contains("TOP-SECRET"), "{message}");
    }

    fn cache_file_exists(f: &Fixture) -> bool {
        f.cache_path.exists()
    }

    #[gpui::test]
    fn disconnect_keeps_the_cache_while_a_buffer_is_live_then_deletes_it(cx: &mut TestAppContext) {
        let mut f = fixture(cx);
        assert_eq!(f.clear(cx), 1, "clean live buffer");
        cx.run_until_parked();
        assert!(cache_file_exists(&f));

        let buffer = std::mem::replace(&mut f.buffer, cx.new(|cx| Buffer::local("", cx)));
        drop(buffer);
        assert_eq!(f.clear(cx), 0);
        cx.run_until_parked();
        assert!(!cache_file_exists(&f));
        f.store.read_with(cx, |store, _| {
            assert!(store.copy_for_buffer_path(&f.cache_path).is_none(), "index pruned");
        });
    }

    #[gpui::test]
    fn disconnect_keeps_the_cache_while_a_buffer_is_dirty(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        assert!(f.buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert_eq!(f.clear(cx), 1);
        cx.run_until_parked();
        assert!(cache_file_exists(&f));
    }

    #[gpui::test]
    fn disconnect_keeps_the_cache_while_an_upload_runs(cx: &mut TestAppContext) {
        let mut f = fixture(cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        // The tab is closed, only the upload holds the copy open.
        drop(std::mem::replace(&mut f.buffer, cx.new(|cx| Buffer::local("", cx))));
        assert_eq!(f.clear(cx), 1);
        cx.run_until_parked();
        assert!(cache_file_exists(&f));
        assert_eq!(f.puts().len(), 1, "the upload finished despite Disconnect");
        assert_eq!(f.clear(cx), 0);
        cx.run_until_parked();
        assert!(!cache_file_exists(&f));
    }

    #[gpui::test]
    fn deleting_a_server_keeps_the_cache_of_an_open_copy(cx: &mut TestAppContext) {
        let f = fixture(cx);
        f.store
            .update(cx, |store, cx| store.apply(Servers::default(), Vec::new(), cx));
        cx.run_until_parked();
        assert!(cache_file_exists(&f), "live buffer: nothing deleted");
    }

    #[gpui::test]
    fn deleting_a_server_without_open_copies_removes_its_cache(cx: &mut TestAppContext) {
        let mut f = fixture(cx);
        drop(std::mem::replace(&mut f.buffer, cx.new(|cx| Buffer::local("", cx))));
        f.store
            .update(cx, |store, cx| store.apply(Servers::default(), Vec::new(), cx));
        cx.run_until_parked();
        assert!(!cache_file_exists(&f));
    }
}
