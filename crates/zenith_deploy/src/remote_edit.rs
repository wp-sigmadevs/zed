//! Edit a remote file (plan 37 M4a, chunk C2): the index of cached copies and the uploader.
//!
//! No UI here. The panel (C3) registers a download with [`ServerStore::register_download`],
//! attaches the editor's buffer, and on every save calls [`ServerStore::upload`] with the buffer
//! that saved; the answer to each prompt goes back through [`ServerStore::set_upload_decision`],
//! `consent_once` or `force`.
//!
//! Trust rules the code below keeps:
//! - The upload target (server id + remote path) comes only from an index entry created by our
//!   own download, never from the buffer path, project config or any file in a repo. The entry
//!   is found by the saving buffer's own file path and that buffer must be attached to it.
//! - "Upload without asking" lives only in memory for this session ([`ServerStore::upload_decisions`]);
//!   no setting and no file can turn it on.
//! - A project that is not local and unshared never uploads (a collab guest's save would use the
//!   host's credentials).
//! - The bytes are the saving buffer's text as saved, never a re-read of the cache file and never
//!   another buffer's text.
//! - A copy is bound to the server target (host, port, type, user) it was downloaded from.
//!
//! Known limits (M4a; each needs more than this crate):
//! - A save by an agent (ACP `write_text_file`) of an attached cache path fires `Saved` like the
//!   user's own save and uploads without a prompt when the server's decision is Allow. Telling
//!   an agent write from an explicit save needs a save intent (M4b) or a hook outside this crate.
//! - There is no quit guard while an in-place upload runs: quitting mid-write leaves the remote
//!   file truncated.
//! - Allow plus autosave uploads half-typed files on every autosave (M4b).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, TaskExt as _, WeakEntity,
};
use language::{Buffer, LineEnding, Rope};

use crate::{
    OPERATION_TIMEOUT, ServerStore,
    config::{ServerConfig, ServerType},
    on_save::{self, Excluded},
    remote_path::{validate_name, validate_remote_path},
    secrets, spawn_operation, spawn_operation_with_timeout,
    transport::{MAX_DOWNLOAD_BYTES, RemoteStat, Transport, upload_entry},
    upload_timeout,
};

const DENIED: &str = "Uploading to this server is off for this session.";
const MAPPING_CHANGED: &str = "The mapping changed; save again.";
const TARGET_CHANGED: &str =
    "The server's host, port, type or user changed since this download; download it again.";

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

/// A snapshot of a buffer's saved text. Taking it on the main thread costs a rope clone; the
/// bytes (line endings, BOM) are built on a background thread by [`SavedText::into_bytes`].
pub struct SavedText {
    rope: Rope,
    line_ending: LineEnding,
    bom: bool,
}

/// The largest text taken from a buffer; the same limit as a download.
fn check_len(len: usize) -> Result<(), String> {
    if len as u64 > MAX_DOWNLOAD_BYTES {
        return Err(format!(
            "This file is larger than {} MiB; it is not uploaded.",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

impl SavedText {
    /// The text of `buffer` as last saved. Refuses non-UTF-8 files and, before anything is
    /// copied, a buffer over the size limit.
    fn capture(buffer: &Buffer) -> Result<Self, String> {
        let encoding = buffer.encoding();
        if encoding.name() != "UTF-8" {
            return Err(format!(
                "This file is saved as {}; only UTF-8 files are uploaded.",
                encoding.name()
            ));
        }
        check_len(buffer.len())?;
        let rope = buffer.rope_for_version(buffer.saved_version());
        check_len(rope.len())?;
        Ok(Self {
            rope,
            line_ending: buffer.line_ending(),
            bom: buffer.has_bom(),
        })
    }

    fn estimated_len(&self) -> usize {
        self.rope.len()
    }

    /// The file's bytes: the file's line endings and a UTF-8 BOM if it had one.
    fn into_bytes(self) -> Vec<u8> {
        let mut text = self.rope.to_string();
        if matches!(self.line_ending, LineEnding::Windows) {
            text = text.replace('\n', "\r\n");
        }
        let mut bytes = Vec::with_capacity(text.len() + 3);
        if self.bom {
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        }
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }
}

/// One cached copy of a remote file that this app downloaded.
pub struct RemoteCopy {
    pub server_id: String,
    pub remote_path: String,
    /// The remote stat taken before the download: what the server looked like when this copy
    /// was made, or after our last verified upload.
    pub stat: Option<RemoteStat>,
    pub buffers: Vec<WeakEntity<Buffer>>,
    pub state: CopyState,
    /// The newest save that arrived during an upload (older ones are replaced) and whether any
    /// of them was forced: one rerun follows, however many saves came.
    pub again: Option<(SavedText, bool)>,
    /// The server target (`secrets::upload_target`) this copy was downloaded from.
    target: String,
    /// A write to this file started in this session (it succeeded or failed). A missing
    /// baseline after that is a failed verification, not a first save: never "no baseline".
    uploaded_once: bool,
}

impl RemoteCopy {
    /// A live buffer (clean or dirty) or a running upload.
    pub fn is_open(&self) -> bool {
        self.state == CopyState::Uploading
            || self.buffers.iter().any(|buffer| buffer.upgrade().is_some())
    }
}

/// A claim on one cache path, released on drop ([`ServerStore::reserve_download`]).
pub struct DownloadReservation {
    claims: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for DownloadReservation {
    fn drop(&mut self) {
        if let Ok(mut claims) = self.claims.lock() {
            claims.remove(&self.path);
        }
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
    /// The user has not said yes for this server: ask (Upload / Always / Not now), then call
    /// [`ServerStore::set_upload_decision`] (Always) or upload again with `consent_once`.
    NeedsConfirmation,
    /// Not uploaded, with a message for the user.
    Refused(String),
    /// The buffer is not an attached buffer of a registered copy (not our download, another
    /// file, or a pre-existing cache file).
    NotRemote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    Uploaded,
    /// The remote file differs from the copy: prompt Overwrite / Compare / Cancel; Overwrite is
    /// `upload(.., force: true, ..)`.
    Conflict,
    /// The remote file is missing or unverifiable (FTP SIZE answers 550 for all of those) or
    /// there was no baseline: ask before writing; yes is `force: true`.
    AskFirst,
    /// Not sent: the remote could not be checked or the file cannot be uploaded.
    Blocked(String),
    /// A file under the built-in secret list, never uploaded automatically; the text says so.
    Excluded(String),
    /// The write started and failed. The text says the server file may be incomplete.
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum ServerStoreEvent {
    UploadFinished {
        cache_path: PathBuf,
        outcome: UploadOutcome,
    },
}

impl EventEmitter<ServerStoreEvent> for ServerStore {}

/// Where the download of `remote_path` from `server_id` lands:
/// `cache_dir/<server id>/<16 hex digits of a hash of the whole remote path>/<file name>`.
/// The hash directory keeps two remote paths that a case- or normalisation-insensitive file
/// system (APFS) would fold together apart, and the file name stays for the tab title and
/// language detection. The panel builds its path with this function and
/// [`ServerStore::register_download`] accepts no other.
pub fn cache_path_for(cache_dir: &Path, server_id: &str, remote_path: &str) -> Result<PathBuf> {
    validate_name(server_id)?;
    validate_remote_path(remote_path)?;
    let name = validate_name(remote_path.rsplit('/').next().unwrap_or_default())?;
    Ok(cache_dir
        .join(server_id)
        .join(format!("{:016x}", fnv1a(remote_path)))
        .join(name))
}

/// FNV-1a, 64 bit: stable across runs and Rust versions (unlike `DefaultHasher`).
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

enum Gate {
    Go,
    Ask,
    Stop(String),
}

/// Whether a save may upload at all, before anything touches the network. `consent_once` (the
/// prompt's one-time "Upload") passes an Ask for this call only; it never passes a Deny and is
/// never stored.
fn gate(is_local_unshared: bool, decision: UploadDecision, consent_once: bool) -> Gate {
    if !is_local_unshared {
        return Gate::Stop("Remote files are only uploaded from a local, unshared project.".into());
    }
    match decision {
        UploadDecision::Allow => Gate::Go,
        UploadDecision::Ask if consent_once => Gate::Go,
        UploadDecision::Ask => Gate::Ask,
        UploadDecision::Deny => Gate::Stop(DENIED.into()),
    }
}

/// Why a copy may not upload to its server now: the server is gone, or its target is not the
/// one the copy was downloaded from.
fn target_problem(copy_target: &str, current: Option<&str>) -> Option<&'static str> {
    match current {
        None => Some("This server was removed."),
        Some(current) if current != copy_target => Some(TARGET_CHANGED),
        Some(_) => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StatCheck {
    Proceed,
    Conflict,
    AskFirst,
}

/// `recorded` is the baseline, `current` the server now. Only a match with BOTH stamps present
/// proceeds: a `None` stamp never proves equality, so a same-size file without stamps asks.
/// No baseline or a missing file is never "go ahead"; a size change is a conflict either way.
fn check_stat(recorded: Option<&RemoteStat>, current: Option<&RemoteStat>) -> StatCheck {
    match (recorded, current) {
        (Some(recorded), Some(current)) if recorded.size != current.size => StatCheck::Conflict,
        (Some(recorded), Some(current)) => match (&recorded.stamp, &current.stamp) {
            (Some(recorded), Some(current)) if recorded == current => StatCheck::Proceed,
            (Some(_), Some(_)) => StatCheck::Conflict,
            _ => StatCheck::AskFirst,
        },
        _ => StatCheck::AskFirst,
    }
}

/// Names the target in messages: path and server name only, never file contents.
struct Target<'a> {
    server_name: &'a str,
    remote_path: &'a str,
}

/// The stop (if any) before the write: a stat error always blocks; `force` skips only the
/// Conflict and AskFirst stops.
fn check_remote(
    target: &Target<'_>,
    recorded: Option<&RemoteStat>,
    current: Result<Option<RemoteStat>>,
    force: bool,
) -> Result<(), UploadOutcome> {
    let Target {
        server_name,
        remote_path,
    } = target;
    let current = current.map_err(|error| {
        UploadOutcome::Blocked(format!(
            "Could not check {remote_path} on {server_name}, so nothing was uploaded: {error:#}"
        ))
    })?;
    if force {
        return Ok(());
    }
    match check_stat(recorded, current.as_ref()) {
        StatCheck::Proceed => Ok(()),
        StatCheck::Conflict => Err(UploadOutcome::Conflict),
        StatCheck::AskFirst => Err(UploadOutcome::AskFirst),
    }
}

/// The new baseline after a write of `len` bytes: the re-stat only when its size is the size
/// we wrote. Anything else (another writer, a server that rewrote the file, no answer) leaves
/// none, so the next save asks first.
fn verified_baseline(stat: Option<RemoteStat>, len: u64) -> Option<RemoteStat> {
    stat.filter(|stat| stat.size == len)
}

/// Stat and re-stat get the plain deadline; only the write scales with the size.
struct Deadlines {
    check: Duration,
    put: Duration,
    verify: Duration,
}

impl Deadlines {
    fn for_len(len: usize) -> Self {
        Self {
            check: OPERATION_TIMEOUT,
            put: upload_timeout(len),
            verify: OPERATION_TIMEOUT,
        }
    }
}

fn failed_message(server_name: &str, remote_path: &str, error: &dyn std::fmt::Display) -> String {
    format!(
        "Uploading {remote_path} to {server_name} failed; the server file may be incomplete: {error:#}"
    )
}

type Moved<T> = Result<(Box<dyn Transport>, T)>;

async fn step_stat(
    mut transport: Box<dyn Transport>,
    path: String,
) -> Moved<Result<Option<RemoteStat>>> {
    let result = transport.stat(&path).await;
    Ok((transport, result))
}

enum Put {
    Done(u64),
    TooLarge,
    Failed(anyhow::Error),
}

/// Builds the bytes on the background thread, then writes them through [`upload_entry`].
async fn step_put(mut transport: Box<dyn Transport>, path: String, text: SavedText) -> Moved<Put> {
    let bytes = text.into_bytes();
    let len = bytes.len() as u64;
    if len > MAX_DOWNLOAD_BYTES {
        return Ok((transport, Put::TooLarge));
    }
    let put = match upload_entry(transport.as_mut(), &path, &bytes).await {
        Ok(()) => Put::Done(len),
        Err(error) => Put::Failed(error),
    };
    Ok((transport, put))
}

/// What one check-then-write did. `transport` is `None` when a timeout dropped the session.
struct Cycle {
    transport: Option<Box<dyn Transport>>,
    outcome: UploadOutcome,
    new_stat: Option<Option<RemoteStat>>,
}

impl Cycle {
    fn stopped(transport: Option<Box<dyn Transport>>, outcome: UploadOutcome) -> Self {
        Self {
            transport,
            outcome,
            new_stat: None,
        }
    }
}

/// One stat, one write, one re-stat, each under its own deadline ([`Deadlines`]).
async fn cycle(
    cx: &mut AsyncApp,
    transport: Box<dyn Transport>,
    target: &Target<'_>,
    recorded: Option<RemoteStat>,
    text: SavedText,
    force: bool,
) -> Cycle {
    let Target {
        server_name,
        remote_path,
    } = target;
    let deadlines = Deadlines::for_len(text.estimated_len());
    let job = cx.update(|cx| {
        spawn_operation_with_timeout(
            cx,
            deadlines.check,
            step_stat(transport, remote_path.to_string()),
        )
    });
    let (transport, current) = match job.await {
        Ok(done) => done,
        Err(error) => {
            return Cycle::stopped(
                None,
                UploadOutcome::Blocked(format!(
                    "Could not check {remote_path} on {server_name}, so nothing was uploaded: {error:#}"
                )),
            );
        }
    };
    if let Err(outcome) = check_remote(target, recorded.as_ref(), current, force) {
        return Cycle::stopped(Some(transport), outcome);
    }
    let job = cx.update(|cx| {
        spawn_operation_with_timeout(
            cx,
            deadlines.put,
            step_put(transport, remote_path.to_string(), text),
        )
    });
    let (transport, put) = match job.await {
        Ok(done) => done,
        // The deadline or a dead connection, possibly mid-write; the session is dropped.
        Err(error) => {
            return Cycle::stopped(
                None,
                UploadOutcome::Failed(failed_message(server_name, remote_path, &error)),
            );
        }
    };
    let len = match put {
        Put::Done(len) => len,
        Put::TooLarge => {
            return Cycle::stopped(
                Some(transport),
                UploadOutcome::Blocked(format!(
                    "{remote_path} is larger than {} MiB; it is not uploaded to {server_name}.",
                    MAX_DOWNLOAD_BYTES / 1024 / 1024
                )),
            );
        }
        Put::Failed(error) => {
            return Cycle::stopped(
                Some(transport),
                UploadOutcome::Failed(failed_message(server_name, remote_path, &error)),
            );
        }
    };
    let job = cx.update(|cx| {
        spawn_operation_with_timeout(
            cx,
            deadlines.verify,
            step_stat(transport, remote_path.to_string()),
        )
    });
    match job.await {
        Ok((transport, Ok(stat))) => Cycle {
            transport: Some(transport),
            outcome: UploadOutcome::Uploaded,
            new_stat: Some(verified_baseline(stat, len)),
        },
        // The re-stat failed (421, an I/O error): the session may be out of step, so it is
        // closed rather than reused for a queued save.
        Ok((transport, Err(_))) => {
            disconnect(cx, transport);
            Cycle {
                transport: None,
                outcome: UploadOutcome::Uploaded,
                new_stat: Some(None),
            }
        }
        // Written, but not verified: no baseline, so the next save asks first.
        Err(_) => Cycle {
            transport: None,
            outcome: UploadOutcome::Uploaded,
            new_stat: Some(None),
        },
    }
}

/// The absolute path of the local file a buffer shows.
pub(crate) fn buffer_path(buffer: &Buffer, cx: &App) -> Option<PathBuf> {
    Some(buffer.file()?.as_local()?.abs_path(cx))
}

impl ServerStore {
    /// The target of a saved server, as it is now.
    fn current_target(&self, server_id: &str) -> Option<String> {
        self.servers
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .map(secrets::upload_target)
    }

    /// The [`on_save::fingerprint`] of a saved server, as it is now.
    pub(crate) fn current_fingerprint(&self, server_id: &str) -> Option<String> {
        self.servers
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .map(on_save::fingerprint)
    }

    /// The decision for `server`: [`UploadDecision::Ask`] unless the user chose one this session
    /// for the server's current target (a changed host, port, type, username or anonymous login resets it).
    pub fn upload_decision(&self, server_id: &str) -> UploadDecision {
        self.upload_decisions
            .get(server_id)
            .filter(|(account, _)| Some(account) == self.current_target(server_id).as_ref())
            .map_or(UploadDecision::Ask, |(_, decision)| *decision)
    }

    /// [`Self::set_upload_decision`] only while the server's target is still `expected_target`
    /// (`secrets::upload_target` taken when the prompt was built): an answer given for a host
    /// the user saw is never recorded for a host they did not. False when it does not match.
    pub(crate) fn set_upload_decision_for_target(
        &mut self,
        server_id: &str,
        expected_target: &str,
        decision: UploadDecision,
    ) -> bool {
        self.current_target(server_id).as_deref() == Some(expected_target)
            && self.set_upload_decision(server_id, decision)
    }

    /// The remote stat recorded for the copy at `cache_path`, only while it is still the same
    /// file on the same server target (`secrets::upload_target`: host, port, type, user and
    /// anonymous login): a stat from another host says nothing about this one. `None` makes the next upload ask first.
    pub(crate) fn baseline_for(
        &self,
        cache_path: &Path,
        server_id: &str,
        remote_path: &str,
    ) -> Option<RemoteStat> {
        let copy = self.copies.get(cache_path)?;
        let current = self.current_target(server_id)?;
        (copy.server_id == server_id
            && copy.remote_path == remote_path
            && target_problem(&copy.target, Some(&current)).is_none())
        .then(|| copy.stat.clone())
        .flatten()
    }

    /// Claims `cache_path` for a download that is about to start: `None` while the copy is open
    /// or another download into it (any window) is running. The claim ends when the returned
    /// guard is dropped, so a cancelled task cannot leave it behind. Without it two windows
    /// connecting at once would both pass the "not open" check and the second download would
    /// replace the file under the first one's buffer.
    pub fn reserve_download(&self, cache_path: &Path) -> Option<DownloadReservation> {
        if self.copies.get(cache_path).is_some_and(RemoteCopy::is_open) {
            return None;
        }
        let inserted = self
            .downloading
            .lock()
            .ok()?
            .insert(cache_path.to_path_buf());
        inserted.then(|| DownloadReservation {
            claims: self.downloading.clone(),
            path: cache_path.to_path_buf(),
        })
    }

    /// Records the user's answer for this session. False when the server does not exist.
    pub fn set_upload_decision(&mut self, server_id: &str, decision: UploadDecision) -> bool {
        let Some(account) = self.current_target(server_id) else {
            return false;
        };
        self.upload_decisions
            .insert(server_id.to_string(), (account, decision));
        true
    }

    /// Records a download made by the panel. `cache_path` must be exactly where
    /// [`cache_path_for`] puts `remote_path`, so a path outside the cache or one that does not
    /// match the remote file is refused. `stat` is the remote stat taken before the download.
    /// The copy is bound to the server's target as it is now. Refused while the copy is open (a
    /// live buffer or running upload): a download over it would replace the baseline the open
    /// buffer's edits were made against.
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
        let target = self
            .current_target(server_id)
            .ok_or_else(|| anyhow!("unknown server"))?;
        if self
            .copies
            .get(&cache_path)
            .is_some_and(RemoteCopy::is_open)
        {
            bail!("{remote_path} is already open for editing");
        }
        // The user is using this server again: an earlier Disconnect no longer applies.
        self.pending_clear.remove(server_id);
        self.copies.insert(
            cache_path,
            RemoteCopy {
                server_id: server_id.to_string(),
                remote_path: remote_path.to_string(),
                stat,
                buffers: Vec::new(),
                state: CopyState::Idle,
                again: None,
                target,
                uploaded_once: false,
            },
        );
        Ok(())
    }

    /// Tells the index which buffer shows `cache_path`. False when it is not registered or the
    /// buffer's own file is not `cache_path`.
    pub fn attach_buffer(&mut self, cache_path: &Path, buffer: &Entity<Buffer>, cx: &App) -> bool {
        if buffer_path(buffer.read(cx), cx).as_deref() != Some(cache_path) {
            return false;
        }
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

    /// Call when a remote copy's buffer is released (its tab closed): finishes a Disconnect or
    /// server deletion that had to leave the open copies' files in place.
    pub fn buffer_released(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<String> = self.pending_clear.iter().cloned().collect();
        for server_id in pending {
            self.clear_cache(&server_id, cx);
        }
    }

    /// Uploads the saved text of `buffer`, which just saved, to the server and remote path of
    /// the copy its own file path indexes. The buffer must be attached to that copy: text of
    /// another buffer (another window, a collab guest's) is never sent in its place.
    /// `is_local_unshared` is `project.is_local() && !project.is_shared()` of the project holding
    /// the buffer. `consent_once` (the prompt's one-time "Upload") passes this call's Ask gate
    /// and is not stored. `force` (Overwrite, "upload anyway") skips the Conflict/AskFirst stops
    /// but is not consent; a forced save queued behind a running upload keeps its force.
    /// The result arrives as [`ServerStoreEvent::UploadFinished`]; the store owns the work, so
    /// closing the panel or window does not cancel it.
    pub fn upload(
        &mut self,
        buffer: &Entity<Buffer>,
        is_local_unshared: bool,
        force: bool,
        consent_once: bool,
        cx: &mut Context<Self>,
    ) -> UploadStart {
        let Some(cache_path) = buffer_path(buffer.read(cx), cx) else {
            return UploadStart::NotRemote;
        };
        let Some(copy) = self.copies.get(&cache_path) else {
            return UploadStart::NotRemote;
        };
        if !copy
            .buffers
            .iter()
            .any(|weak| weak.entity_id() == buffer.entity_id())
        {
            return UploadStart::NotRemote;
        }
        let Some(server) = self
            .servers
            .servers
            .iter()
            .find(|server| server.id == copy.server_id)
            .cloned()
        else {
            return UploadStart::Refused("This server was removed.".into());
        };
        if let Some(message) = target_problem(&copy.target, Some(&secrets::upload_target(&server)))
        {
            return UploadStart::Refused(message.into());
        }
        match gate(
            is_local_unshared,
            self.upload_decision(&server.id),
            consent_once,
        ) {
            Gate::Go => {}
            Gate::Ask => return UploadStart::NeedsConfirmation,
            Gate::Stop(message) => return UploadStart::Refused(message),
        }
        let saved = match SavedText::capture(buffer.read(cx)) {
            Ok(saved) => saved,
            Err(message) => return UploadStart::Refused(message),
        };
        let Some(copy) = self.copies.get_mut(&cache_path) else {
            return UploadStart::NotRemote;
        };
        if copy.state == CopyState::Uploading {
            let force = force || copy.again.as_ref().is_some_and(|(_, forced)| *forced);
            copy.again = Some((saved, force));
            return UploadStart::Queued;
        }
        copy.state = CopyState::Uploading;
        let task = cx.spawn({
            let cache_path = cache_path.clone();
            async move |this, cx| {
                run_upload(this, cx, cache_path, server, saved, force, None).await;
            }
        });
        self.uploads.insert(cache_path, task);
        UploadStart::Started
    }

    /// The copy of a downloaded cache file or of a mapped project file. The two maps never
    /// share a path: a file under the cache is never mapped.
    fn copy_ref(&self, path: &Path) -> Option<&RemoteCopy> {
        self.copies.get(path).or_else(|| self.mapped.get(path))
    }

    fn copy_mut(&mut self, path: &Path) -> Option<&mut RemoteCopy> {
        if self.copies.contains_key(path) {
            self.copies.get_mut(path)
        } else {
            self.mapped.get_mut(path)
        }
    }

    /// The kill switch of auto-upload: while paused no mapped file uploads, queued saves
    /// included. Not saved anywhere; every launch starts unpaused.
    pub fn set_auto_upload_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        self.auto_upload_paused = paused;
        cx.notify();
    }

    pub fn auto_upload_paused(&self) -> bool {
        self.auto_upload_paused
    }

    /// "Always upload the first save without asking" for this session, one server and its
    /// current [`on_save::fingerprint`]: a change of anything it covers forgets it, and so does
    /// one that comes back. Only the first save of a file that was never uploaded in this
    /// session: a remote file that changed since our last upload (or whose last upload could
    /// not be verified) still asks. False when the server does not exist.
    ///
    /// Like [`Self::set_upload_decision_for_target`], only while the server's fingerprint is
    /// still `expected` (the one the prompt was built for); false when it does not match.
    pub fn set_skip_no_baseline_prompt(
        &mut self,
        server_id: &str,
        expected: &str,
        skip: bool,
    ) -> bool {
        let Some(current) = self.current_fingerprint(server_id) else {
            return false;
        };
        if current != expected {
            return false;
        }
        if skip {
            self.skip_no_baseline_prompt
                .insert(server_id.to_string(), current);
        } else {
            self.skip_no_baseline_prompt.remove(server_id);
        }
        true
    }

    fn skips_no_baseline_prompt(&self, server_id: &str) -> bool {
        self.skip_no_baseline_prompt
            .get(server_id)
            .is_some_and(|saved| Some(saved) == self.current_fingerprint(server_id).as_ref())
    }

    /// Whether a mapped file still goes where it was queued for: `project_root` still has this
    /// server as its default and the saved mapping, root path and exclusions still send the file
    /// to the same remote path. A save that waited for a slot or behind a running upload
    /// is not sent on the strength of a configuration that has since been edited.
    fn mapping_problem(
        &self,
        local: &Path,
        copy: &RemoteCopy,
        project_root: &str,
    ) -> Option<&'static str> {
        if self.default_for(project_root) != Some(copy.server_id.as_str()) {
            return Some(MAPPING_CHANGED);
        }
        let Some(server) = self.server_for(Some(project_root), &copy.server_id) else {
            return Some(MAPPING_CHANGED);
        };
        match on_save::resolve(&server, local, &self.cache_dir) {
            // An exclusion added while the save waited (the user's own, or the secret list).
            Ok(Some(resolved))
                if resolved.remote == copy.remote_path
                    && on_save::excluded(&resolved.rel, &on_save::user_matcher(&server))
                        .is_none() =>
            {
                None
            }
            _ => Some(MAPPING_CHANGED),
        }
    }

    /// Why auto-upload to `server_id` may not run now. Both switches of the server's own saved
    /// config must be on (the second is the user's one-time confirmation), and neither the
    /// pause nor a session Deny may be set.
    fn auto_upload_problem(&self, server_id: &str) -> Option<&'static str> {
        if self.auto_upload_paused {
            return Some("Auto-upload is paused.");
        }
        let Some(server) = self.servers.servers.iter().find(|s| s.id == server_id) else {
            return Some("This server was removed.");
        };
        if !(server.upload_on_save && server.upload_on_save_confirmed) {
            return Some("Auto-upload is off for this server.");
        }
        (self.upload_decision(server_id) == UploadDecision::Deny).then_some(DENIED)
    }

    /// The session limit of a server: `advanced.connections` (at least one) at once.
    fn connection_limit(&mut self, server: &ServerConfig) -> std::sync::Arc<smol::lock::Semaphore> {
        let size = (server.advanced.connections as usize).max(1);
        match self.connection_limits.get(&server.id) {
            Some((current, limit)) if *current == size => limit.clone(),
            _ => {
                let limit = std::sync::Arc::new(smol::lock::Semaphore::new(size));
                self.connection_limits
                    .insert(server.id.clone(), (size, limit.clone()));
                limit
            }
        }
    }

    /// Uploads the saved text of `buffer`, which just saved on an explicit save (the caller
    /// checks [`on_save::is_explicit`]), when its file maps to a path on the project's default
    /// server (plan 37 M4b). The target is only ever computed from the user's own saved server
    /// ([`ServerConfig::mappings`]): `project_root` picks the default server, never any file of
    /// the project. Nothing happens, and the answer is [`UploadStart::NotRemote`], unless the
    /// project is local and unshared, the server is visible in it and has both
    /// `upload_on_save` and `upload_on_save_confirmed`, and neither the pause nor a session
    /// Deny is set. A file under a built-in secret exclusion also answers `NotRemote` and
    /// emits [`UploadOutcome::Excluded`] ("never uploaded automatically"); one under the
    /// user's own exclusions answers `NotRemote` silently. An unsafe path or an unreadable
    /// buffer is [`UploadStart::Refused`]. `force` is "upload anyway" after an
    /// [`UploadOutcome::AskFirst`] or [`UploadOutcome::Conflict`]. The result arrives as
    /// [`ServerStoreEvent::UploadFinished`] with the project file's path.
    pub fn upload_mapped(
        &mut self,
        buffer: &Entity<Buffer>,
        project_root: Option<&str>,
        is_local_unshared: bool,
        force: bool,
        cx: &mut Context<Self>,
    ) -> UploadStart {
        if !is_local_unshared {
            return UploadStart::NotRemote;
        }
        let Some(project_root) = project_root else {
            return UploadStart::NotRemote;
        };
        let Some(server) = self
            .default_for(project_root)
            .and_then(|id| self.server_for(Some(project_root), id))
        else {
            return UploadStart::NotRemote;
        };
        if self.auto_upload_problem(&server.id).is_some() {
            return UploadStart::NotRemote;
        }
        let Some(local) = buffer_path(buffer.read(cx), cx) else {
            return UploadStart::NotRemote;
        };
        // A buffer of another folder of the same window is not covered by this project's opt-in.
        if local.starts_with(&self.cache_dir) || !local.starts_with(project_root) {
            return UploadStart::NotRemote;
        }
        let resolved = match on_save::resolve(&server, &local, &self.cache_dir) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return UploadStart::NotRemote,
            Err(message) => return UploadStart::Refused(message),
        };
        match on_save::excluded(&resolved.rel, &on_save::user_matcher(&server)) {
            Some(Excluded::Fixed) => {
                cx.emit(ServerStoreEvent::UploadFinished {
                    cache_path: local,
                    outcome: UploadOutcome::Excluded(format!(
                        "{} is never uploaded automatically: it can hold passwords or keys.",
                        resolved.rel.display()
                    )),
                });
                return UploadStart::NotRemote;
            }
            Some(Excluded::User) => return UploadStart::NotRemote,
            None => {}
        }
        let saved = match SavedText::capture(buffer.read(cx)) {
            Ok(saved) => saved,
            Err(message) => return UploadStart::Refused(message),
        };
        let target = secrets::upload_target(&server);
        let same_target = |copy: &RemoteCopy| {
            copy.server_id == server.id
                && copy.remote_path == resolved.remote
                && copy.target == target
        };
        match self.mapped.get_mut(&local) {
            Some(copy) if copy.state == CopyState::Uploading => {
                if !same_target(copy) {
                    return UploadStart::Refused(
                        "An upload of this file is still running; save again when it ends.".into(),
                    );
                }
                let force = force || copy.again.as_ref().is_some_and(|(_, forced)| *forced);
                copy.again = Some((saved, force));
                return UploadStart::Queued;
            }
            Some(copy) if same_target(copy) => copy.state = CopyState::Uploading,
            // New file, or its server, target or mapped path changed: the old baseline says
            // nothing about this remote file.
            _ => {
                self.mapped.insert(
                    local.clone(),
                    RemoteCopy {
                        server_id: server.id.clone(),
                        remote_path: resolved.remote.clone(),
                        stat: None,
                        buffers: Vec::new(),
                        state: CopyState::Uploading,
                        again: None,
                        target,
                        uploaded_once: false,
                    },
                );
            }
        }
        let run = MappedRun {
            root: resolved.folder,
            project_root: project_root.to_string(),
            limit: self.connection_limit(&server),
        };
        let task = cx.spawn({
            let local = local.clone();
            async move |this, cx| {
                run_upload(this, cx, local, server, saved, force, Some(run)).await;
            }
        });
        self.uploads.insert(local, task);
        UploadStart::Started
    }

    /// Ends one cycle in a single update so a save cannot slip between "no rerun" and "idle":
    /// returns the queued rerun, if there is one to run. A queued save reruns after an upload,
    /// or, when any save in it was forced, after a Conflict/AskFirst stop (that stop is then not
    /// reported: the user already said overwrite).
    fn settle(
        &mut self,
        cache_path: &Path,
        outcome: &UploadOutcome,
        new_stat: Option<Option<RemoteStat>>,
        cx: &mut Context<Self>,
    ) -> Option<(SavedText, bool)> {
        let mut next = None;
        if let Some(copy) = self.copy_mut(cache_path) {
            if let Some(stat) = new_stat {
                copy.stat = stat;
            }
            if matches!(outcome, UploadOutcome::Uploaded | UploadOutcome::Failed(_)) {
                copy.uploaded_once = true;
            }
            next = copy.again.take().filter(|(_, forced)| match outcome {
                UploadOutcome::Uploaded => true,
                UploadOutcome::Conflict | UploadOutcome::AskFirst => *forced,
                _ => false,
            });
            if next.is_none() {
                copy.state = CopyState::Idle;
            }
        }
        if next.is_none() || *outcome == UploadOutcome::Uploaded {
            cx.emit(ServerStoreEvent::UploadFinished {
                cache_path: cache_path.to_path_buf(),
                outcome: outcome.clone(),
            });
        }
        if next.is_none() {
            self.buffer_released(cx);
        }
        next
    }
}

/// What a mapped file's upload loop needs besides a copy: the mapping folder for the real-path
/// check and the server's session limit.
struct MappedRun {
    root: PathBuf,
    project_root: String,
    limit: std::sync::Arc<smol::lock::Semaphore>,
}

fn disconnect(cx: &mut AsyncApp, mut transport: Box<dyn Transport>) {
    cx.update(|cx| {
        spawn_operation(cx, async move { transport.disconnect().await }).detach_and_log_err(cx)
    });
}

/// The upload loop of one copy: one session, one cycle per saved text, stopping at the first
/// outcome that is not a success or when no newer save is queued. Before each cycle the copy's
/// target is checked against the server as it is now.
async fn run_upload(
    this: WeakEntity<ServerStore>,
    cx: &mut AsyncApp,
    cache_path: PathBuf,
    server: ServerConfig,
    mut text: SavedText,
    mut force: bool,
    mapped: Option<MappedRun>,
) {
    // Save All must not open one session per file: wait here for one of the server's slots.
    let _slot = match &mapped {
        Some(run) => Some(run.limit.acquire_arc().await),
        None => None,
    };
    let mut session: Option<Box<dyn Transport>> = None;
    loop {
        let copy = this.read_with(cx, |this, _| {
            this.copy_ref(&cache_path).map(|copy| {
                (
                    copy.remote_path.clone(),
                    copy.stat.clone(),
                    // The gate again: a Deny said while this save waited in the queue applies.
                    target_problem(
                        &copy.target,
                        this.current_target(&copy.server_id).as_deref(),
                    )
                    .or_else(|| {
                        (this.upload_decision(&copy.server_id) == UploadDecision::Deny)
                            .then_some(DENIED)
                    })
                    // A pause or a switch turned off while this save waited applies too.
                    .or_else(|| {
                        mapped
                            .is_some()
                            .then(|| this.auto_upload_problem(&copy.server_id))
                            .flatten()
                    })
                    // The mapping, root path or default server was edited while this waited.
                    .or_else(|| {
                        mapped.as_ref().and_then(|run| {
                            this.mapping_problem(&cache_path, copy, &run.project_root)
                        })
                    }),
                    // The user's "always" for the first save of a file never uploaded in this
                    // session. With a baseline there is nothing to skip, so a Conflict is never
                    // silenced; after an upload whose verification failed there is no baseline
                    // either, but that is not a first save and asks.
                    mapped.is_some()
                        && copy.stat.is_none()
                        && !copy.uploaded_once
                        && this.skips_no_baseline_prompt(&copy.server_id),
                )
            })
        });
        let Ok(Some((remote_path, recorded, problem, skip_no_baseline))) = copy else {
            return;
        };
        if let Some(message) = problem {
            this.update(cx, |this, cx| {
                this.settle(
                    &cache_path,
                    &UploadOutcome::Blocked(message.into()),
                    None,
                    cx,
                )
            })
            .ok();
            if let Some(transport) = session {
                disconnect(cx, transport);
            }
            return;
        }
        // A mapped file must still be where its path says (no symbolic link in the way).
        if let Some(run) = &mapped {
            let check = cx.background_spawn(on_save::check_real_path(
                run.root.clone(),
                cache_path.clone(),
                Some(PathBuf::from(&run.project_root)),
            ));
            if let Err(error) = check.await {
                this.update(cx, |this, cx| {
                    this.settle(
                        &cache_path,
                        &UploadOutcome::Blocked(format!("{error:#}")),
                        None,
                        cx,
                    )
                })
                .ok();
                if let Some(transport) = session {
                    disconnect(cx, transport);
                }
                return;
            }
        }
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
        let target = Target {
            server_name: &server.name,
            remote_path: &remote_path,
        };
        let done = cycle(
            cx,
            transport,
            &target,
            recorded,
            text,
            force || skip_no_baseline,
        )
        .await;
        let next = this
            .update(cx, |this, cx| {
                this.settle(&cache_path, &done.outcome, done.new_stat, cx)
            })
            .ok()
            .flatten();
        match next {
            Some((next_text, next_force)) => {
                text = next_text;
                force = next_force;
                session = done.transport;
            }
            None => {
                if let Some(transport) = done.transport {
                    disconnect(cx, transport);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

    use gpui::{Subscription, TestAppContext};
    use project::Project;

    use super::*;
    use crate::{
        Servers,
        test_support::{FakeRemote, init, server},
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
        app_state: Arc<workspace::AppState>,
        buffer: Entity<Buffer>,
        cache_path: PathBuf,
        cache_dir: PathBuf,
        server_id: String,
        dir: tempfile::TempDir,
        events: Rc<RefCell<Vec<UploadOutcome>>>,
        _subscription: Subscription,
    }

    /// Opens `path` (a file of the test file system) in a project of its own, like a window.
    async fn open(
        app_state: &Arc<workspace::AppState>,
        path: &Path,
        cx: &mut TestAppContext,
    ) -> Entity<Buffer> {
        let project =
            Project::test(app_state.fs.clone(), [path.parent().expect("parent")], cx).await;
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(path, cx))
            .await
            .expect("open buffer");
        std::mem::forget(project);
        buffer
    }

    async fn fixture(cx: &mut TestAppContext) -> Fixture {
        fixture_with(cx, FakeRemote::default()).await
    }

    /// One registered copy of `/www/a.php` (remote stamp "v1") with a clean saved buffer "hello"
    /// attached and uploads allowed.
    async fn fixture_with(cx: &mut TestAppContext, remote: FakeRemote) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().join("cache");
        let mut prod = server("Prod", None);
        prod.anonymous = true;
        let server_id = prod.id.clone();
        remote
            .stats
            .lock()
            .expect("stats")
            .insert(REMOTE.into(), stat("v1"));
        let servers = Servers {
            servers: vec![prod],
            ..Servers::default()
        };
        let app_state = init(cx, servers, &remote, cache_dir.clone());
        let store = cx.update(|cx| ServerStore::global(cx));
        let cache_path = cache_path_for(&cache_dir, &server_id, REMOTE).expect("cache path");
        std::fs::create_dir_all(cache_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache_path, "cache bytes").expect("write");
        // The editor runs on the fake file system: mirror the copy there with other text.
        app_state
            .fs
            .as_fake()
            .insert_tree(
                cache_path.parent().expect("parent"),
                serde_json::json!({ "a.php": "hello", "other.php": "elsewhere" }),
            )
            .await;
        let buffer = open(&app_state, &cache_path, cx).await;
        let (path, id) = (cache_path.clone(), server_id.clone());
        store.update(cx, |store, cx| {
            store
                .register_download(path.clone(), &id, REMOTE, Some(stat("v1")))
                .expect("register");
            assert!(store.attach_buffer(&path, &buffer, cx));
            assert!(store.set_upload_decision(&id, UploadDecision::Allow));
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
            app_state,
            buffer,
            cache_path,
            cache_dir,
            server_id,
            dir,
            events,
            _subscription: subscription,
        }
    }

    impl Fixture {
        /// Replaces the text of `buffer` and marks it saved, like a save.
        fn save_in(buffer: &Entity<Buffer>, text: &str, cx: &mut TestAppContext) {
            buffer.update(cx, |buffer, cx| {
                let len = buffer.len();
                buffer.edit([(0..len, text)], None, cx);
                buffer.did_save(buffer.version(), None, cx);
            });
        }

        fn save(&self, text: &str, cx: &mut TestAppContext) {
            Self::save_in(&self.buffer, text, cx);
        }

        fn upload_with(
            &self,
            buffer: &Entity<Buffer>,
            (unshared, force, once): (bool, bool, bool),
            cx: &mut TestAppContext,
        ) -> UploadStart {
            self.store.update(cx, |store, cx| {
                store.upload(buffer, unshared, force, once, cx)
            })
        }

        fn upload(&self, unshared: bool, force: bool, cx: &mut TestAppContext) -> UploadStart {
            self.upload_with(&self.buffer, (unshared, force, false), cx)
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
            self.store
                .update(cx, |store, cx| store.clear_cache(&id, cx))
        }

        /// Frees the buffer (the tab closed), as the last strong reference goes.
        fn close_buffer(&mut self, cx: &mut TestAppContext) {
            drop(std::mem::replace(
                &mut self.buffer,
                cx.new(|cx| Buffer::local("", cx)),
            ));
        }

        fn cache_file_exists(&self) -> bool {
            self.cache_path.exists()
        }

        fn change_host(&self, cx: &mut TestAppContext) {
            self.store.update(cx, |store, cx| {
                let mut servers = store.servers().clone();
                servers.servers[0].host = "other.example.com".into();
                store.apply(servers, Vec::new(), cx);
            });
        }

        fn copy_stat(&self, cx: &mut TestAppContext) -> Option<RemoteStat> {
            self.store.read_with(cx, |store, _| {
                store
                    .copy_for_buffer_path(&self.cache_path)
                    .and_then(|copy| copy.stat.clone())
            })
        }
    }

    #[test]
    fn the_gate_needs_a_local_unshared_project_and_a_yes_for_the_server() {
        for decision in [
            UploadDecision::Ask,
            UploadDecision::Allow,
            UploadDecision::Deny,
        ] {
            for once in [false, true] {
                assert!(
                    matches!(gate(false, decision, once), Gate::Stop(_)),
                    "a shared or remote project never uploads ({decision:?}, once={once})"
                );
            }
        }
        assert!(matches!(gate(true, UploadDecision::Ask, false), Gate::Ask));
        assert!(matches!(
            gate(true, UploadDecision::Deny, false),
            Gate::Stop(_)
        ));
        assert!(matches!(gate(true, UploadDecision::Allow, false), Gate::Go));
        assert!(matches!(gate(true, UploadDecision::Ask, true), Gate::Go));
        assert!(
            matches!(gate(true, UploadDecision::Deny, true), Gate::Stop(_)),
            "a one-time yes does not undo a no"
        );
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
        // A None stamp never proves equality, even with the same size.
        let s5n = RemoteStat {
            size: 5,
            stamp: None,
        };
        let s6n = RemoteStat {
            size: 6,
            stamp: None,
        };
        assert_eq!(check_stat(Some(&s5n), Some(&s5n)), StatCheck::AskFirst);
        assert_eq!(check_stat(Some(&v1), Some(&s5n)), StatCheck::AskFirst);
        assert_eq!(check_stat(Some(&s5n), Some(&v1)), StatCheck::AskFirst);
        assert_eq!(check_stat(Some(&s5n), Some(&s6n)), StatCheck::Conflict);
    }

    #[gpui::test]
    async fn a_same_size_file_without_stamps_asks_instead_of_overwriting(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let no_stamp = RemoteStat {
            size: 5,
            stamp: None,
        };
        f.remote
            .stats
            .lock()
            .expect("stats")
            .insert(REMOTE.into(), no_stamp.clone());
        f.store.update(cx, |store, _| {
            store.copies.get_mut(&f.cache_path).expect("copy").stat = Some(no_stamp);
        });
        f.save("hello", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(*f.events.borrow(), [UploadOutcome::AskFirst]);
        assert!(f.puts().is_empty(), "an unprovable match must not write");
    }

    #[test]
    fn a_new_baseline_needs_the_size_we_wrote() {
        assert_eq!(verified_baseline(Some(stat("v")), 5), Some(stat("v")));
        assert_eq!(verified_baseline(Some(stat("v")), 6), None);
        assert_eq!(verified_baseline(None, 5), None);
    }

    #[test]
    fn only_the_write_deadline_grows_with_the_size() {
        let small = Deadlines::for_len(10);
        let big = Deadlines::for_len(20 * 1024 * 1024);
        for deadlines in [&small, &big] {
            assert_eq!(deadlines.check, OPERATION_TIMEOUT);
            assert_eq!(deadlines.verify, OPERATION_TIMEOUT);
        }
        assert_eq!(small.put, OPERATION_TIMEOUT);
        assert!(big.put >= Duration::from_secs(320));
    }

    #[test]
    fn a_target_that_moved_or_a_removed_server_blocks() {
        assert_eq!(target_problem("t", Some("t")), None);
        assert_eq!(target_problem("t", Some("u")), Some(TARGET_CHANGED));
        assert!(target_problem("t", None).is_some());
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
        let good = cache_path_for(root, "id1", "/www/a.php").expect("path");
        assert_eq!(good.file_name().and_then(|n| n.to_str()), Some("a.php"));
        assert_eq!(
            good.parent().and_then(Path::parent),
            Some(Path::new("/cache/id1"))
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

    #[test]
    fn remote_paths_that_a_case_or_normalisation_insensitive_disk_folds_stay_apart() {
        let root = Path::new("/cache");
        // "é" as one code point and as "e" plus a combining accent: APFS folds these together.
        let nfc = "/www/caf\u{e9}/a.php";
        let nfd = "/www/cafe\u{301}/a.php";
        let paths = [
            "/www/a.php",
            "/www/A.php",
            "/WWW/a.php",
            "/www/B/a.php",
            "/www/b/a.php",
            nfc,
            nfd,
        ];
        let built: Vec<PathBuf> = paths
            .iter()
            .map(|path| cache_path_for(root, "id1", path).expect("path"))
            .collect();
        for (i, left) in built.iter().enumerate() {
            for right in &built[i + 1..] {
                assert_ne!(left, right);
                let fold =
                    |path: &PathBuf| path.parent().map(|p| p.to_string_lossy().to_lowercase());
                assert_ne!(
                    fold(left),
                    fold(right),
                    "the folder differs, not only the file name"
                );
            }
        }
    }

    #[gpui::test]
    async fn registering_refuses_a_path_that_is_not_the_cache_location(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let register = |cache_path: PathBuf, id: &str, remote: &str, cx: &mut TestAppContext| {
            f.store.update(cx, |store, _| {
                store.register_download(cache_path, id, remote, None)
            })
        };
        let other = cache_path_for(&f.cache_dir, &f.server_id, "/www/b.php").expect("path");
        assert!(register(other, &f.server_id, "/www/b.php", cx).is_ok());
        for (path, remote) in [
            // Outside the cache root.
            (f.dir.path().join("elsewhere/b.php"), "/www/b.php"),
            // Inside the cache but not where that remote file is cached.
            (f.cache_path.clone(), "/www/c.php"),
            // A traversing remote path.
            (f.cache_dir.join("../etc/passwd"), "/../etc/passwd"),
        ] {
            assert!(
                register(path.clone(), &f.server_id, remote, cx).is_err(),
                "{path:?}"
            );
        }
        // A server that is not saved cannot own a copy.
        let ghost = cache_path_for(&f.cache_dir, "ghost", "/www/b.php").expect("path");
        assert!(register(ghost, "ghost", "/www/b.php", cx).is_err());
        // An open copy (live buffer) is not replaced by a new download.
        assert!(register(f.cache_path.clone(), &f.server_id, REMOTE, cx).is_err());
    }

    #[gpui::test]
    async fn a_conflict_stops_the_put_and_force_overwrites(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
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
    async fn a_missing_remote_file_asks_first_and_never_goes_ahead(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
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
    async fn a_stat_error_blocks_the_upload_even_when_forced(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
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
            assert!(
                message.contains("550 is a directory") && message.contains(REMOTE),
                "{message}"
            );
        }
        assert!(f.puts().is_empty());
    }

    #[gpui::test]
    async fn saves_during_an_upload_coalesce_into_one_rerun_with_the_newest_bytes(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
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
    async fn a_failed_verify_restat_closes_the_session_instead_of_reusing_it(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        *f.remote.stat_error_after_put.lock().expect("error") = Some("421 closing".into());
        f.save("one", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        f.save("two", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Queued);
        cx.run_until_parked();
        let log: Vec<_> = f
            .remote
            .log()
            .into_iter()
            .filter(|line| !line.starts_with("connect"))
            .collect();
        let connects = f
            .remote
            .log()
            .iter()
            .filter(|line| line.starts_with("connect"))
            .count();
        assert_eq!(connects, 2, "the queued save opens a fresh session");
        assert_eq!(
            log,
            [
                format!("stat {REMOTE}"),
                format!("put {REMOTE}"),
                format!("stat {REMOTE}"),
                "disconnect".to_string(),
                format!("stat {REMOTE}"),
                "disconnect".to_string(),
            ]
        );
        assert_eq!(
            *f.events.borrow(),
            [UploadOutcome::Uploaded, UploadOutcome::AskFirst],
            "no baseline after the failed verify, so the queued save asks"
        );
        assert_eq!(f.puts().len(), 1);
        assert_eq!(f.copy_stat(cx), None);
    }

    #[gpui::test]
    async fn a_forced_save_keeps_its_force_when_it_is_queued(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.set_remote_stamp("v2");
        f.save("first", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        f.save("second", cx);
        assert_eq!(f.upload(true, true, cx), UploadStart::Queued, "Overwrite");
        f.save("third", cx);
        assert_eq!(
            f.upload(true, false, cx),
            UploadStart::Queued,
            "a later plain save does not take the force back"
        );
        cx.run_until_parked();
        assert_eq!(f.puts(), [(REMOTE.to_string(), b"third".to_vec())]);
        assert_eq!(
            *f.events.borrow(),
            [UploadOutcome::Uploaded],
            "the Conflict the user already answered is not reported"
        );
    }

    #[gpui::test]
    async fn the_stat_after_an_upload_is_the_new_baseline(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(
            f.copy_stat(cx),
            Some(RemoteStat {
                size: 5,
                stamp: Some("put-1".into())
            })
        );
        // The baseline moved, so the next save is not a conflict with our own write.
        f.save("again", cx);
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.events.borrow().last(), Some(&UploadOutcome::Uploaded));
        assert_eq!(f.puts().len(), 2);
    }

    #[gpui::test]
    async fn a_restat_with_another_size_leaves_no_baseline_so_the_next_save_asks(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        *f.remote.put_keeps_stat.lock().expect("flag") = true;
        f.save("a longer text", cx);
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.events.borrow().last(), Some(&UploadOutcome::Uploaded));
        assert_eq!(
            f.copy_stat(cx),
            None,
            "the server still reports 5 bytes, we wrote 13"
        );
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.events.borrow().last(), Some(&UploadOutcome::AskFirst));
        assert_eq!(f.puts().len(), 1);
    }

    #[gpui::test]
    async fn a_shared_or_remote_project_never_uploads_even_when_allowed(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        assert!(matches!(
            f.upload(false, false, cx),
            UploadStart::Refused(_)
        ));
        assert!(matches!(f.upload(false, true, cx), UploadStart::Refused(_)));
        cx.run_until_parked();
        assert!(
            f.remote.log().is_empty(),
            "no connection: {:?}",
            f.remote.log()
        );
        assert!(f.events.borrow().is_empty());
    }

    #[gpui::test]
    async fn ask_and_deny_never_upload_and_allow_is_per_server_and_session(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Ask);
        });
        assert_eq!(f.upload(true, false, cx), UploadStart::NeedsConfirmation);
        assert_eq!(
            f.upload(true, true, cx),
            UploadStart::NeedsConfirmation,
            "force is not consent"
        );
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Deny);
        });
        assert!(matches!(f.upload(true, false, cx), UploadStart::Refused(_)));
        cx.run_until_parked();
        assert!(
            f.remote.log().is_empty(),
            "no connection: {:?}",
            f.remote.log()
        );
        f.store.update(cx, |store, _| {
            assert!(!store.set_upload_decision("nope", UploadDecision::Allow));
            assert_eq!(store.upload_decision("nope"), UploadDecision::Ask);
        });
    }

    #[gpui::test]
    async fn a_one_time_yes_uploads_once_and_is_not_remembered(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Ask);
        });
        assert_eq!(
            f.upload_with(&f.buffer, (true, false, true), cx),
            UploadStart::Started
        );
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 1);
        assert_eq!(
            f.store
                .read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Ask
        );
        assert_eq!(f.upload(true, false, cx), UploadStart::NeedsConfirmation);
        // A one-time yes does not cross the project gate or a Deny.
        assert!(matches!(
            f.upload_with(&f.buffer, (false, false, true), cx),
            UploadStart::Refused(_)
        ));
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Deny);
        });
        assert!(matches!(
            f.upload_with(&f.buffer, (true, false, true), cx),
            UploadStart::Refused(_)
        ));
    }

    #[gpui::test]
    async fn allow_is_forgotten_when_the_servers_target_changes(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        assert_eq!(
            f.store
                .read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Allow
        );
        f.change_host(cx);
        assert_eq!(
            f.store
                .read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Ask
        );
    }

    #[gpui::test]
    async fn switching_to_or_from_anonymous_forgets_allow_and_refuses_a_copy(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        f.store.update(cx, |store, cx| {
            let mut servers = store.servers().clone();
            servers.servers[0].anonymous = !servers.servers[0].anonymous;
            store.apply(servers, Vec::new(), cx);
        });
        assert_eq!(
            f.store
                .read_with(cx, |store, _| store.upload_decision(&f.server_id)),
            UploadDecision::Ask
        );
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Allow);
        });
        let UploadStart::Refused(message) = f.upload(true, true, cx) else {
            panic!("a copy downloaded under the other login must refuse");
        };
        assert_eq!(message, TARGET_CHANGED);
        cx.run_until_parked();
        assert!(f.remote.log().is_empty());
    }

    #[gpui::test]
    async fn a_copy_never_uploads_to_a_server_whose_target_changed_since_the_download(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        f.change_host(cx);
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Allow);
        });
        let UploadStart::Refused(message) = f.upload(true, true, cx) else {
            panic!("a changed target must refuse");
        };
        assert_eq!(message, TARGET_CHANGED);
        cx.run_until_parked();
        assert!(f.remote.log().is_empty());
    }

    #[gpui::test]
    async fn a_queued_rerun_rechecks_the_target(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        f.save("one", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        cx.run_until_parked();
        f.save("two", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Queued);
        // The first put is in flight; the server is edited, then the put finishes.
        f.change_host(cx);
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts(), [(REMOTE.to_string(), b"one".to_vec())]);
        let events = f.events.borrow();
        assert_eq!(events.first(), Some(&UploadOutcome::Uploaded));
        assert_eq!(
            events.last(),
            Some(&UploadOutcome::Blocked(TARGET_CHANGED.into()))
        );
    }

    #[gpui::test]
    async fn the_bytes_come_from_the_saved_buffer_not_the_cache_file(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
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
    async fn crlf_and_a_bom_are_kept_in_the_uploaded_bytes(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.buffer.update(cx, |buffer, cx| {
            buffer.set_line_ending(LineEnding::Windows, cx);
            buffer.set_has_bom(true);
        });
        f.save("a\nb\n", cx);
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [(REMOTE.to_string(), b"\xEF\xBB\xBFa\r\nb\r\n".to_vec())]
        );
    }

    #[gpui::test]
    async fn a_non_utf8_file_is_refused_before_any_connection(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.buffer
            .update(cx, |buffer, _| buffer.set_encoding(encoding_rs::SHIFT_JIS));
        let UploadStart::Refused(message) = f.upload(true, false, cx) else {
            panic!("a Shift-JIS file must be refused");
        };
        assert!(message.contains("UTF-8"), "{message}");
        cx.run_until_parked();
        assert!(f.remote.log().is_empty());
    }

    #[gpui::test]
    async fn a_buffer_over_the_size_limit_is_refused_before_it_is_copied(cx: &mut TestAppContext) {
        let over = "a".repeat(MAX_DOWNLOAD_BYTES as usize + 1);
        let buffer = cx.new(|cx| Buffer::local(over, cx));
        let result = buffer.read_with(cx, |buffer, _| SavedText::capture(buffer).map(|_| ()));
        assert!(result.expect_err("too big").contains("larger than"));
        assert!(check_len(MAX_DOWNLOAD_BYTES as usize).is_ok());
        assert!(check_len(MAX_DOWNLOAD_BYTES as usize + 1).is_err());
    }

    #[gpui::test]
    async fn saving_one_buffer_never_sends_another_buffers_text(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        // Window B shows the same file; both buffers are attached.
        let second = open(&f.app_state, &f.cache_path, cx).await;
        f.store.update(cx, |store, cx| {
            assert!(store.attach_buffer(&f.cache_path, &second, cx));
        });
        f.save("text of window A", cx);
        Fixture::save_in(&second, "text of window B", cx);
        assert_eq!(
            f.upload_with(&second, (true, false, false), cx),
            UploadStart::Started
        );
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [(REMOTE.to_string(), b"text of window B".to_vec())]
        );
    }

    #[gpui::test]
    async fn a_buffer_that_is_not_the_copys_file_never_uploads(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let other_path = f.cache_path.with_file_name("other.php");
        let other = open(&f.app_state, &other_path, cx).await;
        let detached = open(&f.app_state, &f.cache_path, cx).await;
        let no_file = cx.new(|cx| Buffer::local("no file", cx));
        f.store.update(cx, |store, cx| {
            assert!(
                !store.attach_buffer(&f.cache_path, &other, cx),
                "another file's buffer cannot be attached to this copy"
            );
            assert!(!store.attach_buffer(&f.cache_path, &no_file, cx));
        });
        for buffer in [&other, &detached, &no_file] {
            assert_eq!(
                f.upload_with(buffer, (true, true, true), cx),
                UploadStart::NotRemote
            );
        }
        cx.run_until_parked();
        assert!(f.remote.log().is_empty());
    }

    #[gpui::test]
    async fn an_upload_target_cannot_be_redirected_by_a_buffer_path(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let sneaky = f.cache_path.parent().expect("parent").join("../x/../a.php");
        f.store.read_with(cx, |store, _| {
            assert!(store.copy_for_buffer_path(&sneaky).is_none());
            assert!(
                store
                    .copy_for_buffer_path(&f.dir.path().join("project/a.php"))
                    .is_none()
            );
        });
        // The real entry goes to its own recorded path.
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert_eq!(f.puts()[0].0, REMOTE);
    }

    #[gpui::test]
    async fn files_under_the_cache_that_are_not_ours_are_orphans(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.store.read_with(cx, |store, _| {
            assert!(!store.is_orphan(&f.cache_path));
            assert!(store.is_orphan(&f.cache_dir.join(&f.server_id).join("old.php")));
            assert!(!store.is_orphan(&f.dir.path().join("project/a.php")));
        });
    }

    #[gpui::test]
    async fn a_failed_upload_names_the_file_and_server_never_the_contents(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.save("TOP-SECRET-CONTENTS", cx);
        *f.remote.put_error.lock().expect("error") = Some("connection reset".into());
        f.upload(true, false, cx);
        cx.run_until_parked();
        let events = f.events.borrow();
        let [UploadOutcome::Failed(message)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(
            message.contains(REMOTE) && message.contains("Prod"),
            "{message}"
        );
        assert!(
            message.contains("the server file may be incomplete"),
            "{message}"
        );
        assert!(!message.contains("TOP-SECRET"), "{message}");
    }

    #[gpui::test]
    async fn a_failed_connection_ends_the_upload_and_the_next_save_starts_a_new_one(
        cx: &mut TestAppContext,
    ) {
        let mut remote = FakeRemote::default();
        remote.fail_connect = true;
        let f = fixture_with(cx, remote).await;
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        cx.run_until_parked();
        let first = f.events.borrow().clone();
        let [UploadOutcome::Blocked(message)] = first.as_slice() else {
            panic!("{first:?}");
        };
        assert!(message.contains("Could not connect to Prod"), "{message}");
        assert!(f.puts().is_empty());
        f.store.read_with(cx, |store, _| {
            let copy = store.copy_for_buffer_path(&f.cache_path).expect("copy");
            assert_eq!(copy.state, CopyState::Idle);
            assert!(copy.again.is_none());
        });
        assert_eq!(f.upload(true, false, cx), UploadStart::Started, "not stuck");
    }

    #[gpui::test]
    async fn a_stat_that_never_answers_blocks_after_the_plain_deadline(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        *f.remote.stat_hang.lock().expect("flag") = true;
        f.upload(true, false, cx);
        cx.run_until_parked();
        assert!(f.events.borrow().is_empty(), "still waiting");
        cx.executor()
            .advance_clock(OPERATION_TIMEOUT - Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            f.events.borrow().is_empty(),
            "the deadline is the plain one, not shorter"
        );
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        let events = f.events.borrow();
        let [UploadOutcome::Blocked(message)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.contains("did not answer"), "{message}");
        assert!(f.puts().is_empty());
        drop(events);
        assert_eq!(
            f.upload(true, false, cx),
            UploadStart::Started,
            "idle again"
        );
    }

    #[gpui::test]
    async fn a_put_that_never_answers_fails_as_possibly_incomplete(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        // 640 KiB: the write deadline (40 s) is longer than the plain one (30 s).
        f.save(&"a".repeat(640 * 1024), cx);
        let deadline = Deadlines::for_len(640 * 1024).put;
        assert!(deadline > OPERATION_TIMEOUT);
        *f.remote.put_hang.lock().expect("flag") = true;
        f.upload(true, false, cx);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(OPERATION_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            f.events.borrow().is_empty(),
            "the write keeps its own, longer deadline"
        );
        cx.executor().advance_clock(deadline - OPERATION_TIMEOUT);
        cx.run_until_parked();
        let events = f.events.borrow();
        let [UploadOutcome::Failed(message)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(message.contains("may be incomplete"), "{message}");
    }

    #[gpui::test]
    async fn a_deny_said_while_a_save_is_queued_stops_that_save(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        f.save("one", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        cx.run_until_parked();
        f.save("two", cx);
        assert_eq!(f.upload(true, false, cx), UploadStart::Queued);
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Deny);
        });
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts(), [(REMOTE.to_string(), b"one".to_vec())]);
        assert_eq!(
            f.events.borrow().last(),
            Some(&UploadOutcome::Blocked(DENIED.into()))
        );
    }

    #[gpui::test]
    async fn disconnect_keeps_the_cache_while_a_buffer_is_live_then_deletes_it(
        cx: &mut TestAppContext,
    ) {
        let mut f = fixture(cx).await;
        assert_eq!(f.clear(cx), 1, "clean live buffer");
        cx.run_until_parked();
        assert!(f.cache_file_exists());

        f.close_buffer(cx);
        f.store.update(cx, |store, cx| store.buffer_released(cx));
        cx.run_until_parked();
        assert!(
            !f.cache_file_exists(),
            "released: the pending Disconnect finishes"
        );
        f.store.read_with(cx, |store, _| {
            assert!(
                store.copy_for_buffer_path(&f.cache_path).is_none(),
                "index pruned"
            );
        });
    }

    #[gpui::test]
    async fn disconnect_keeps_the_cache_while_a_buffer_is_dirty(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.buffer
            .update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
        assert!(f.buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        assert_eq!(f.clear(cx), 1);
        cx.run_until_parked();
        assert!(f.cache_file_exists());
    }

    #[gpui::test]
    async fn disconnect_keeps_the_cache_while_an_upload_runs_and_cleans_up_after_it(
        cx: &mut TestAppContext,
    ) {
        let mut f = fixture(cx).await;
        assert_eq!(f.upload(true, false, cx), UploadStart::Started);
        // The tab is closed; only the upload holds the copy open.
        f.close_buffer(cx);
        assert_eq!(f.clear(cx), 1);
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 1, "the upload finished despite Disconnect");
        assert!(
            !f.cache_file_exists(),
            "the upload's end finishes the pending Disconnect without any other call"
        );
    }

    #[gpui::test]
    async fn disconnect_removes_closed_copies_at_once_and_keeps_only_the_open_ones(
        cx: &mut TestAppContext,
    ) {
        let f = fixture(cx).await;
        let closed = cache_path_for(&f.cache_dir, &f.server_id, "/www/b.php").expect("path");
        std::fs::create_dir_all(closed.parent().expect("parent")).expect("mkdir");
        std::fs::write(&closed, "b").expect("write");
        f.store.update(cx, |store, _| {
            store
                .register_download(closed.clone(), &f.server_id, "/www/b.php", None)
                .expect("register");
        });
        assert_eq!(f.clear(cx), 1, "only the open copy counts as kept");
        cx.run_until_parked();
        assert!(
            !closed.exists(),
            "a closed copy of the server is not left behind"
        );
        assert!(f.cache_file_exists());
        f.store.read_with(cx, |store, _| {
            assert!(store.copy_for_buffer_path(&closed).is_none());
        });
    }

    #[gpui::test]
    async fn downloading_again_cancels_a_pending_disconnect(cx: &mut TestAppContext) {
        let mut f = fixture(cx).await;
        assert_eq!(f.clear(cx), 1);
        let fresh = cache_path_for(&f.cache_dir, &f.server_id, "/www/c.php").expect("path");
        std::fs::create_dir_all(fresh.parent().expect("parent")).expect("mkdir");
        std::fs::write(&fresh, "c").expect("write");
        f.store.update(cx, |store, _| {
            store
                .register_download(fresh.clone(), &f.server_id, "/www/c.php", None)
                .expect("register");
        });
        f.close_buffer(cx);
        f.store.update(cx, |store, cx| store.buffer_released(cx));
        cx.run_until_parked();
        assert!(fresh.exists(), "the user came back to this server");
        assert!(f.cache_file_exists());
    }

    #[gpui::test]
    async fn deleting_a_server_keeps_the_cache_of_an_open_copy(cx: &mut TestAppContext) {
        let f = fixture(cx).await;
        f.store.update(cx, |store, cx| {
            store.apply(Servers::default(), Vec::new(), cx)
        });
        cx.run_until_parked();
        assert!(f.cache_file_exists(), "live buffer: nothing deleted");
    }

    #[gpui::test]
    async fn deleting_a_server_without_open_copies_removes_its_cache(cx: &mut TestAppContext) {
        let mut f = fixture(cx).await;
        f.close_buffer(cx);
        f.store.update(cx, |store, cx| {
            store.apply(Servers::default(), Vec::new(), cx)
        });
        cx.run_until_parked();
        assert!(!f.cache_file_exists());
    }

    // ---- Auto-upload of mapped project files (plan 37 M4b) ----

    const MAPPED_REMOTE: &str = "/var/www/t/inc/a.php";
    const PROJECT_FILES: [(&str, &str); 4] = [
        ("inc/a.php", "orig"),
        ("inc/b.php", "orig"),
        ("inc/c.php", "orig"),
        ("wp-config.php", "secret"),
    ];

    /// A project folder `proj` mapped to `/var/www/t` of the default server, which has both
    /// upload switches on. The files exist on the real disk (the real-path check) and in the
    /// fake file system (the editor).
    struct Mapped {
        store: Entity<ServerStore>,
        remote: FakeRemote,
        app_state: Arc<workspace::AppState>,
        dir: tempfile::TempDir,
        project: PathBuf,
        server_id: String,
        events: Rc<RefCell<Vec<(PathBuf, UploadOutcome)>>>,
        _subscription: Subscription,
    }

    async fn mapped_fixture(
        cx: &mut TestAppContext,
        tune: impl FnOnce(&mut ServerConfig),
    ) -> Mapped {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("proj");
        let project_text = project.to_string_lossy().into_owned();
        let mut prod = server("Prod", None);
        prod.anonymous = true;
        prod.root_path = "/var/www".into();
        prod.mappings = vec![crate::config::Mapping {
            local: project_text.clone(),
            remote: "t".into(),
        }];
        prod.upload_on_save = true;
        prod.upload_on_save_confirmed = true;
        tune(&mut prod);
        let server_id = prod.id.clone();
        let mut servers = Servers::default();
        servers
            .project_defaults
            .insert(project_text, server_id.clone());
        servers.servers = vec![prod];
        let remote = FakeRemote::default();
        let app_state = init(cx, servers, &remote, dir.path().join("cache"));
        for (rel, text) in PROJECT_FILES {
            let path = project.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, text).expect("write");
        }
        app_state
            .fs
            .as_fake()
            .insert_tree(
                &project,
                serde_json::json!({
                    "inc": { "a.php": "orig", "b.php": "orig", "c.php": "orig" },
                    "wp-config.php": "secret",
                    "node_modules": { "x.js": "x" },
                    "link.php": "link",
                }),
            )
            .await;
        let store = cx.update(|cx| ServerStore::global(cx));
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let subscription = cx.update(|cx| {
            cx.subscribe(&store, move |_, event: &ServerStoreEvent, _| {
                let ServerStoreEvent::UploadFinished {
                    cache_path,
                    outcome,
                } = event;
                sink.borrow_mut()
                    .push((cache_path.clone(), outcome.clone()));
            })
        });
        Mapped {
            store,
            remote,
            app_state,
            dir,
            project,
            server_id,
            events,
            _subscription: subscription,
        }
    }

    impl Mapped {
        async fn open(&self, rel: &str, cx: &mut TestAppContext) -> Entity<Buffer> {
            open(&self.app_state, &self.project.join(rel), cx).await
        }

        fn upload_in(
            &self,
            buffer: &Entity<Buffer>,
            project_root: Option<&str>,
            force: bool,
            cx: &mut TestAppContext,
        ) -> UploadStart {
            self.store.update(cx, |store, cx| {
                store.upload_mapped(buffer, project_root, true, force, cx)
            })
        }

        fn upload(
            &self,
            buffer: &Entity<Buffer>,
            force: bool,
            cx: &mut TestAppContext,
        ) -> UploadStart {
            self.upload_in(buffer, self.project.to_str(), force, cx)
        }

        fn edit_servers(&self, cx: &mut TestAppContext, edit: impl FnOnce(&mut Servers)) {
            self.store.update(cx, |store, cx| {
                let mut servers = store.servers().clone();
                edit(&mut servers);
                store.apply(servers, Vec::new(), cx);
            });
        }

        fn puts(&self) -> Vec<(String, Vec<u8>)> {
            self.remote.puts.lock().expect("puts").clone()
        }

        fn outcomes(&self) -> Vec<UploadOutcome> {
            self.events
                .borrow()
                .iter()
                .map(|(_, outcome)| outcome.clone())
                .collect()
        }

        fn connects(&self) -> usize {
            self.remote
                .log()
                .iter()
                .filter(|line| line.starts_with("connect"))
                .count()
        }

        fn real_text(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.project.join(rel)).ok()
        }
    }

    #[gpui::test]
    async fn auto_upload_needs_both_switches_the_default_server_and_no_pause(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        Fixture::save_in(&buffer, "<?php echo 1;", cx);
        let try_upload = |f: &Mapped, cx: &mut TestAppContext| f.upload(&buffer, true, cx);

        // Only upload_on_save, only the confirmation, neither.
        for (on, confirmed) in [(true, false), (false, true), (false, false)] {
            f.edit_servers(cx, |servers| {
                servers.servers[0].upload_on_save = on;
                servers.servers[0].upload_on_save_confirmed = confirmed;
            });
            assert_eq!(
                try_upload(&f, cx),
                UploadStart::NotRemote,
                "{on} {confirmed}"
            );
        }
        f.edit_servers(cx, |servers| {
            servers.servers[0].upload_on_save = true;
            servers.servers[0].upload_on_save_confirmed = true;
        });
        // Not the project's default server.
        f.edit_servers(cx, |servers| servers.project_defaults.clear());
        assert_eq!(try_upload(&f, cx), UploadStart::NotRemote);
        let (project, id) = (
            f.project.to_string_lossy().into_owned(),
            f.server_id.clone(),
        );
        f.edit_servers(cx, |servers| {
            servers.project_defaults.insert(project, id);
        });
        // A project root that has no default, no project root, a shared project.
        assert_eq!(
            f.upload_in(&buffer, Some("/elsewhere"), true, cx),
            UploadStart::NotRemote
        );
        assert_eq!(f.upload_in(&buffer, None, true, cx), UploadStart::NotRemote);
        let shared = f.store.update(cx, |store, cx| {
            store.upload_mapped(&buffer, f.project.to_str(), false, true, cx)
        });
        assert_eq!(shared, UploadStart::NotRemote);
        // A session Deny and the pause.
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Deny);
        });
        assert_eq!(try_upload(&f, cx), UploadStart::NotRemote);
        f.store.update(cx, |store, _| {
            store.set_upload_decision(&f.server_id, UploadDecision::Ask);
        });
        f.store
            .update(cx, |store, cx| store.set_auto_upload_paused(true, cx));
        assert_eq!(try_upload(&f, cx), UploadStart::NotRemote);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0, "nothing was even connected to");

        // Everything on: the same save uploads.
        f.store
            .update(cx, |store, cx| store.set_auto_upload_paused(false, cx));
        assert_eq!(try_upload(&f, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [(MAPPED_REMOTE.to_string(), b"<?php echo 1;".to_vec())]
        );
    }

    #[gpui::test]
    async fn a_pause_set_while_a_save_is_queued_stops_that_save(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        Fixture::save_in(&buffer, "one", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        cx.run_until_parked();
        Fixture::save_in(&buffer, "two", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Queued);
        f.store
            .update(cx, |store, cx| store.set_auto_upload_paused(true, cx));
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts(), [(MAPPED_REMOTE.to_string(), b"one".to_vec())]);
    }

    #[gpui::test]
    async fn the_first_save_asks_then_a_verified_baseline_means_no_prompt_until_the_server_changes(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        Fixture::save_in(&buffer, "<?php echo 1;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes(), [UploadOutcome::AskFirst]);
        assert!(
            f.puts().is_empty(),
            "no baseline: nothing is written unasked"
        );

        // "Upload anyway".
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [(MAPPED_REMOTE.to_string(), b"<?php echo 1;".to_vec())]
        );
        assert_eq!(
            f.events.borrow().last().map(|(path, _)| path.clone()),
            Some(f.project.join("inc/a.php"))
        );

        // The next save has a baseline: no prompt.
        Fixture::save_in(&buffer, "<?php echo 2;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 2);
        assert_eq!(f.outcomes().last(), Some(&UploadOutcome::Uploaded));

        // Someone else changed the server's file.
        f.remote.stats.lock().expect("stats").insert(
            MAPPED_REMOTE.into(),
            RemoteStat {
                size: 99,
                stamp: Some("other".into()),
            },
        );
        Fixture::save_in(&buffer, "<?php echo 3;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes().last(), Some(&UploadOutcome::Conflict));
        assert_eq!(f.puts().len(), 2, "a conflict writes nothing");
    }

    #[gpui::test]
    async fn the_skip_set_silences_the_no_baseline_question_but_never_a_conflict(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        let id = f.server_id.clone();
        f.store.update(cx, |store, _| {
            store.set_skip_no_baseline_prompt(
                &id,
                &store.current_fingerprint(&id).expect("fingerprint"),
                true,
            )
        });
        Fixture::save_in(&buffer, "<?php echo 1;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes(), [UploadOutcome::Uploaded], "no AskFirst");
        assert_eq!(f.puts().len(), 1);

        f.remote.stats.lock().expect("stats").insert(
            MAPPED_REMOTE.into(),
            RemoteStat {
                size: 99,
                stamp: Some("other".into()),
            },
        );
        Fixture::save_in(&buffer, "<?php echo 2;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes().last(), Some(&UploadOutcome::Conflict));
        assert_eq!(f.puts().len(), 1, "the skip set never skips a Conflict");
    }

    #[gpui::test]
    async fn disconnect_leaves_the_mapped_project_files_alone(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let first = f.open("inc/a.php", cx).await;
        Fixture::save_in(&first, "<?php echo 1;", cx);
        assert_eq!(f.upload(&first, true, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 1);
        // A second upload is still running, so Disconnect takes the "kept open" path that
        // deletes the files of the closed copies.
        let second = f.open("inc/b.php", cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        Fixture::save_in(&second, "<?php echo 2;", cx);
        assert_eq!(f.upload(&second, true, cx), UploadStart::Started);
        cx.run_until_parked();
        let id = f.server_id.clone();
        f.store.update(cx, |store, cx| store.clear_cache(&id, cx));
        cx.run_until_parked();
        release.send(()).expect("release");
        cx.run_until_parked();
        f.store.update(cx, |store, cx| store.clear_cache(&id, cx));
        cx.run_until_parked();
        assert_eq!(f.real_text("inc/a.php").as_deref(), Some("orig"));
        assert_eq!(f.real_text("inc/b.php").as_deref(), Some("orig"));
        assert!(f.dir.path().join("proj").is_dir());
        assert_eq!(f.puts().len(), 2);
    }

    #[gpui::test]
    async fn the_connection_limit_bounds_the_sessions_of_a_save_all(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |server| server.advanced.connections = 1).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        let mut buffers = Vec::new();
        for rel in ["inc/a.php", "inc/b.php", "inc/c.php"] {
            let buffer = f.open(rel, cx).await;
            Fixture::save_in(&buffer, "new", cx);
            assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
            buffers.push(buffer);
        }
        cx.run_until_parked();
        assert_eq!(f.connects(), 1, "three saves, one slot, one session");
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 3, "the others went one after the other");
        assert_eq!(f.connects(), 3);
        assert_eq!(f.outcomes(), vec![UploadOutcome::Uploaded; 3]);
    }

    #[gpui::test]
    async fn secrets_under_a_mapping_never_upload_and_say_so_while_user_exclusions_are_silent(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let secret = f.open("wp-config.php", cx).await;
        Fixture::save_in(&secret, "<?php define('X', 1);", cx);
        assert_eq!(f.upload(&secret, true, cx), UploadStart::NotRemote);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0);
        let events = f.events.borrow().clone();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].1
                == UploadOutcome::Excluded(
                    "wp-config.php is never uploaded automatically: it can hold passwords or keys."
                        .into()
                ),
            "{events:?}"
        );

        // The default `excluded_local` list: silent.
        let ignored = f.open("node_modules/x.js", cx).await;
        Fixture::save_in(&ignored, "x2", cx);
        assert_eq!(f.upload(&ignored, true, cx), UploadStart::NotRemote);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.events.borrow().len(), 1, "a user exclusion says nothing");
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn a_symlink_in_the_mapped_tree_is_blocked_before_any_connection(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        std::fs::write(f.dir.path().join("outside.txt"), "secret").expect("write");
        std::os::unix::fs::symlink("../outside.txt", f.project.join("link.php")).expect("link");
        let buffer = f.open("link.php", cx).await;
        Fixture::save_in(&buffer, "linked", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0);
        let outcomes = f.outcomes();
        assert!(
            matches!(outcomes.as_slice(), [UploadOutcome::Blocked(message)] if message.contains("symbolic link")),
            "{outcomes:?}"
        );
    }

    #[gpui::test]
    async fn an_unsafe_mapping_or_a_file_that_is_not_mapped_never_uploads(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        Fixture::save_in(&buffer, "x", cx);
        f.edit_servers(cx, |servers| {
            servers.servers[0].mappings[0].remote = "../up".into()
        });
        assert!(matches!(
            f.upload(&buffer, true, cx),
            UploadStart::Refused(_)
        ));
        f.edit_servers(cx, |servers| servers.servers[0].mappings.clear());
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::NotRemote);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0);
    }

    /// Starts an upload, queues a second save behind it, edits the saved servers, then lets the
    /// first upload finish: the queued save must not follow the old configuration.
    async fn a_queued_save_after_an_edit(cx: &mut TestAppContext, edit: impl FnOnce(&mut Servers)) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        Fixture::save_in(&buffer, "one", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        cx.run_until_parked();
        Fixture::save_in(&buffer, "two", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Queued);
        f.edit_servers(cx, edit);
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(
            f.puts(),
            [(MAPPED_REMOTE.to_string(), b"one".to_vec())],
            "nothing more goes to the old path"
        );
        assert_eq!(
            f.outcomes(),
            [
                UploadOutcome::Uploaded,
                UploadOutcome::Blocked("The mapping changed; save again.".into())
            ]
        );
    }

    #[gpui::test]
    async fn a_queued_save_stops_when_the_mapping_is_deleted(cx: &mut TestAppContext) {
        a_queued_save_after_an_edit(cx, |servers| servers.servers[0].mappings.clear()).await;
    }

    #[gpui::test]
    async fn a_queued_save_stops_when_the_mapped_remote_folder_changes(cx: &mut TestAppContext) {
        a_queued_save_after_an_edit(cx, |servers| {
            servers.servers[0].mappings[0].remote = "t-staging".into()
        })
        .await;
    }

    #[gpui::test]
    async fn a_queued_save_stops_when_the_root_path_changes(cx: &mut TestAppContext) {
        a_queued_save_after_an_edit(cx, |servers| {
            servers.servers[0].root_path = "/var/www2".into()
        })
        .await;
    }

    #[gpui::test]
    async fn a_queued_save_stops_when_the_project_default_moves_to_another_server(
        cx: &mut TestAppContext,
    ) {
        a_queued_save_after_an_edit(cx, |servers| {
            let mut other = servers.servers[0].clone();
            other.id = "other-server".into();
            servers.servers.push(other);
            for default in servers.project_defaults.values_mut() {
                *default = "other-server".into();
            }
        })
        .await;
    }

    #[gpui::test]
    async fn a_save_waiting_for_a_slot_rechecks_the_mapping_too(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |server| server.advanced.connections = 1).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        let mut buffers = Vec::new();
        for rel in ["inc/a.php", "inc/b.php"] {
            let buffer = f.open(rel, cx).await;
            Fixture::save_in(&buffer, "new", cx);
            assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
            buffers.push(buffer);
        }
        cx.run_until_parked();
        f.edit_servers(cx, |servers| servers.servers[0].mappings.clear());
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts().len(), 1, "only the upload that already ran");
        assert_eq!(f.outcomes().len(), 2);
        assert!(matches!(&f.outcomes()[1], UploadOutcome::Blocked(_)));
    }

    #[gpui::test]
    async fn a_user_exclusion_added_while_a_save_waits_stops_it(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |server| server.advanced.connections = 1).await;
        let (release, gate) = futures::channel::oneshot::channel();
        *f.remote.put_gate.lock().expect("gate") = Some(gate);
        for rel in ["inc/a.php", "inc/b.php"] {
            let buffer = f.open(rel, cx).await;
            Fixture::save_in(&buffer, "new", cx);
            assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        }
        cx.run_until_parked();
        f.edit_servers(cx, |servers| {
            servers.servers[0].excluded_local.push("secret".into());
            servers.servers[0].excluded_local.push("b.php".into());
        });
        release.send(()).expect("release");
        cx.run_until_parked();
        assert_eq!(f.puts(), [(MAPPED_REMOTE.to_string(), b"new".to_vec())]);
        assert_eq!(
            f.outcomes(),
            [
                UploadOutcome::Uploaded,
                UploadOutcome::Blocked(MAPPING_CHANGED.into())
            ]
        );
    }

    #[gpui::test]
    async fn a_skip_or_decision_does_not_come_back_when_the_host_returns(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let id = f.server_id.clone();
        let (host, target, fingerprint) = f.store.read_with(cx, |store, _| {
            (
                store.servers().servers[0].host.clone(),
                store.current_target(&id).expect("target"),
                store.current_fingerprint(&id).expect("fingerprint"),
            )
        });
        f.store.update(cx, |store, _| {
            assert!(store.set_skip_no_baseline_prompt(&id, &fingerprint, true));
            assert!(store.set_upload_decision_for_target(&id, &target, UploadDecision::Deny));
        });
        f.edit_servers(cx, |servers| {
            servers.servers[0].host = "other.example.com".into()
        });
        f.edit_servers(cx, |servers| servers.servers[0].host = host.clone());
        f.store.read_with(cx, |store, _| {
            assert!(!store.skips_no_baseline_prompt(&id));
            assert_eq!(store.upload_decision(&id), UploadDecision::Ask);
        });
    }

    #[gpui::test]
    async fn a_skip_for_a_target_the_user_did_not_see_is_not_recorded(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let id = f.server_id.clone();
        f.store.update(cx, |store, _| {
            assert!(!store.set_skip_no_baseline_prompt(&id, "1:x|1:y|21|ftp", true));
            assert!(!store.skips_no_baseline_prompt(&id));
        });
    }

    #[gpui::test]
    async fn a_reconfirmed_server_keeps_its_confirmation_across_a_root_change(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |server| server.root_path = "/".into()).await;
        let id = f.server_id.clone();
        for (reconfirmed, expected) in [(true, true), (false, false)] {
            f.edit_servers(cx, |servers| {
                servers.servers[0].root_path = "/".into();
                servers.servers[0].upload_on_save_confirmed = true;
            });
            let task = f.store.update(cx, |store, cx| {
                let previous = store.servers().clone();
                let mut servers = previous.clone();
                servers.servers[0].root_path = "/public_html".into();
                let ids: HashMap<String, String> = reconfirmed
                    .then(|| (id.clone(), on_save::fingerprint(&servers.servers[0])))
                    .into_iter()
                    .collect();
                store.save(previous, servers, Vec::new(), &ids, cx)
            });
            task.await.expect("saved");
            let server = f
                .store
                .read_with(cx, |store, _| store.servers().servers[0].clone());
            assert_eq!(server.upload_on_save_confirmed, expected, "{reconfirmed}");
        }
    }

    #[gpui::test]
    async fn the_skip_set_never_applies_after_an_upload_whose_verification_failed(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        let id = f.server_id.clone();
        f.store.update(cx, |store, _| {
            store.set_skip_no_baseline_prompt(
                &id,
                &store.current_fingerprint(&id).expect("fingerprint"),
                true,
            )
        });
        // The server keeps answering with a size we did not write: the re-stat cannot verify.
        *f.remote.put_keeps_stat.lock().expect("flag") = true;
        f.remote.stats.lock().expect("stats").insert(
            MAPPED_REMOTE.into(),
            RemoteStat {
                size: 99,
                stamp: Some("x".into()),
            },
        );
        Fixture::save_in(&buffer, "<?php echo 1;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes(), [UploadOutcome::Uploaded]);
        assert_eq!(f.puts().len(), 1);

        // Someone changes the file; the next save must ask, not overwrite unchecked.
        f.remote.stats.lock().expect("stats").insert(
            MAPPED_REMOTE.into(),
            RemoteStat {
                size: 77,
                stamp: Some("y".into()),
            },
        );
        Fixture::save_in(&buffer, "<?php echo 2;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes().last(), Some(&UploadOutcome::AskFirst));
        assert_eq!(f.puts().len(), 1);
    }

    #[gpui::test]
    async fn the_skip_set_is_forgotten_when_the_servers_host_changes(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let buffer = f.open("inc/a.php", cx).await;
        let id = f.server_id.clone();
        f.store.update(cx, |store, _| {
            store.set_skip_no_baseline_prompt(
                &id,
                &store.current_fingerprint(&id).expect("fingerprint"),
                true,
            )
        });
        f.edit_servers(cx, |servers| {
            servers.servers[0].host = "other.example.com".into()
        });
        Fixture::save_in(&buffer, "<?php echo 1;", cx);
        assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started);
        cx.run_until_parked();
        assert_eq!(f.outcomes(), [UploadOutcome::AskFirst]);
        assert!(f.puts().is_empty(), "the new host was not sent the file");
    }

    #[gpui::test]
    async fn the_skip_set_is_forgotten_when_the_root_path_changes(cx: &mut TestAppContext) {
        type Edit = fn(&mut ServerConfig);
        let rows: [(&str, Edit); 2] = [
            ("root", |s| s.root_path = "/var/other".into()),
            ("mapping remote", |s| {
                s.mappings[0].remote = "elsewhere".into()
            }),
        ];
        for (what, edit) in rows {
            let f = mapped_fixture(cx, |_| {}).await;
            let buffer = f.open("inc/a.php", cx).await;
            let id = f.server_id.clone();
            f.store.update(cx, |store, _| {
                store.set_skip_no_baseline_prompt(
                    &id,
                    &store.current_fingerprint(&id).expect("fingerprint"),
                    true,
                )
            });
            f.edit_servers(cx, |servers| edit(&mut servers.servers[0]));
            Fixture::save_in(&buffer, "<?php echo 1;", cx);
            assert_eq!(f.upload(&buffer, false, cx), UploadStart::Started, "{what}");
            cx.run_until_parked();
            assert_eq!(f.outcomes(), [UploadOutcome::AskFirst], "{what}");
            assert!(
                f.puts().is_empty(),
                "{what}: the new place was not sent the file"
            );
        }
    }

    #[gpui::test]
    async fn a_skip_does_not_come_back_when_the_root_returns(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |_| {}).await;
        let id = f.server_id.clone();
        let root = f
            .store
            .read_with(cx, |store, _| store.servers().servers[0].root_path.clone());
        f.store.update(cx, |store, _| {
            let fingerprint = store.current_fingerprint(&id).expect("fingerprint");
            assert!(store.set_skip_no_baseline_prompt(&id, &fingerprint, true));
        });
        f.edit_servers(cx, |servers| {
            servers.servers[0].root_path = "/var/other".into()
        });
        f.edit_servers(cx, |servers| servers.servers[0].root_path = root.clone());
        f.store
            .read_with(cx, |store, _| assert!(!store.skips_no_baseline_prompt(&id)));
    }

    #[gpui::test]
    async fn a_skip_does_not_survive_a_switch_to_or_from_anonymous_sign_in(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let id = f.server_id.clone();
        f.store.update(cx, |store, _| {
            let fingerprint = store.current_fingerprint(&id).expect("fingerprint");
            assert!(store.set_skip_no_baseline_prompt(&id, &fingerprint, true));
        });
        f.edit_servers(cx, |servers| {
            servers.servers[0].anonymous = !servers.servers[0].anonymous
        });
        f.store
            .read_with(cx, |store, _| assert!(!store.skips_no_baseline_prompt(&id)));
    }

    #[gpui::test]
    async fn a_skip_for_a_prompt_built_before_a_root_change_is_not_recorded(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let id = f.server_id.clone();
        let seen = f.store.read_with(cx, |store, _| {
            store.current_fingerprint(&id).expect("fingerprint")
        });
        f.edit_servers(cx, |servers| {
            servers.servers[0].root_path = "/var/other".into()
        });
        f.store.update(cx, |store, _| {
            assert!(!store.set_skip_no_baseline_prompt(&id, &seen, true));
            assert!(!store.skips_no_baseline_prompt(&id));
        });
    }

    #[gpui::test]
    async fn a_reconfirmation_for_other_settings_does_not_keep_the_flag(cx: &mut TestAppContext) {
        let f = mapped_fixture(cx, |server| server.root_path = "/".into()).await;
        let id = f.server_id.clone();
        let task = f.store.update(cx, |store, cx| {
            let previous = store.servers().clone();
            let mut confirmed = previous.clone();
            confirmed.servers[0].root_path = "/public_html".into();
            let reconfirmed =
                HashMap::from([(id.clone(), on_save::fingerprint(&confirmed.servers[0]))]);
            // The user confirmed "/public_html", but the list being saved says "/elsewhere".
            let mut servers = previous.clone();
            servers.servers[0].root_path = "/elsewhere".into();
            store.save(previous, servers, Vec::new(), &reconfirmed, cx)
        });
        task.await.expect("saved");
        let server = f
            .store
            .read_with(cx, |store, _| store.servers().servers[0].clone());
        assert_eq!(server.root_path, "/elsewhere");
        assert!(!server.upload_on_save_confirmed);
    }

    #[gpui::test]
    async fn saving_an_invalid_mapping_is_refused_and_an_old_one_does_not_block_other_edits(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let save = |cx: &mut TestAppContext, edit: fn(&mut Servers)| {
            f.store.update(cx, |store, cx| {
                let previous = store.servers().clone();
                let mut servers = previous.clone();
                edit(&mut servers);
                store.save(previous, servers, Vec::new(), &Default::default(), cx)
            })
        };
        // A changed mapping with `..` is refused, naming the server and the mapping.
        let error = save(cx, |servers| {
            servers.servers[0].mappings[0].remote = "../x".into()
        })
        .await
        .expect_err("refused");
        assert!(error.to_string().contains("Prod"), "{error}");
        assert!(error.to_string().contains("../x"), "{error}");
        // A new server with one is refused too.
        let error = save(cx, |servers| {
            let mut added = servers.servers[0].clone();
            added.id = "new".into();
            added.name = "New".into();
            added.mappings[0].local = "relative".into();
            servers.servers.push(added);
        })
        .await
        .expect_err("refused");
        assert!(error.to_string().contains("New"), "{error}");
        f.store
            .read_with(cx, |store, _| assert_eq!(store.servers().servers.len(), 1));

        // An invalid mapping that is already saved does not block an edit of something else,
        // but touching the mappings or the root asks for a valid one.
        f.edit_servers(cx, |servers| {
            servers.servers[0].mappings[0].remote = "..".into()
        });
        save(cx, |servers| servers.servers[0].name = "Renamed".into())
            .await
            .expect("an unrelated edit is not blocked");
        save(cx, |servers| servers.servers[0].root_path = "/other".into())
            .await
            .expect_err("a root edit with a bad mapping is refused");
        save(cx, |servers| {
            servers.servers[0].mappings[0].remote = "ok".into()
        })
        .await
        .expect("a repaired mapping saves");
    }

    #[gpui::test]
    async fn saving_a_server_that_points_elsewhere_clears_its_upload_confirmation(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        type Edit = fn(&mut ServerConfig);
        let rows: [(&str, Edit, bool); 7] = [
            ("host", |s| s.host = "other.example.com".into(), false),
            ("port", |s| s.port = 2222, false),
            ("user", |s| s.username = "someone".into(), false),
            ("type", |s| s.server_type = crate::ServerType::Sftp, false),
            ("root", |s| s.root_path = "/var/www2".into(), false),
            (
                "mappings",
                |s| s.mappings[0].remote = "t-staging".into(),
                false,
            ),
            ("name", |s| s.name = "Renamed".into(), true),
        ];
        let original = f
            .store
            .read_with(cx, |store, _| store.servers().servers[0].clone());
        for (what, edit, stays) in rows {
            // Every row starts from the confirmed server as it was.
            f.edit_servers(cx, |servers| servers.servers[0] = original.clone());
            let task = f.store.update(cx, |store, cx| {
                let previous = store.servers().clone();
                let mut servers = previous.clone();
                edit(&mut servers.servers[0]);
                store.save(previous, servers, Vec::new(), &Default::default(), cx)
            });
            task.await.expect("saved");
            let server = f
                .store
                .read_with(cx, |store, _| store.servers().servers[0].clone());
            assert_eq!(server.upload_on_save_confirmed, stays, "{what}");
            assert!(server.upload_on_save, "{what}: the switch itself stays");
        }
    }

    #[gpui::test]
    async fn a_file_of_another_folder_of_the_window_is_not_covered_by_this_projects_opt_in(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let other = f.dir.path().join("other");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::write(other.join("x.php"), "x").expect("write");
        f.app_state
            .fs
            .as_fake()
            .insert_tree(&other, serde_json::json!({ "x.php": "x" }))
            .await;
        let other_text = other.to_string_lossy().into_owned();
        f.edit_servers(cx, |servers| {
            servers.servers[0].mappings.push(crate::config::Mapping {
                local: other_text,
                remote: "o".into(),
            });
        });
        let buffer = open(&f.app_state, &other.join("x.php"), cx).await;
        Fixture::save_in(&buffer, "changed", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::NotRemote);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0);
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn a_link_between_the_project_root_and_the_mapped_folder_blocks_the_upload(
        cx: &mut TestAppContext,
    ) {
        let f = mapped_fixture(cx, |_| {}).await;
        let elsewhere = f.dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("inner")).expect("mkdir");
        std::fs::write(elsewhere.join("inner/x.php"), "x").expect("write");
        std::os::unix::fs::symlink(&elsewhere, f.project.join("real")).expect("link");
        let mapped = f.project.join("real/inner");
        f.app_state
            .fs
            .as_fake()
            .insert_tree(&mapped, serde_json::json!({ "x.php": "x" }))
            .await;
        let mapped_text = mapped.to_string_lossy().into_owned();
        f.edit_servers(cx, |servers| {
            servers.servers[0].mappings.push(crate::config::Mapping {
                local: mapped_text,
                remote: "m".into(),
            });
        });
        let buffer = open(&f.app_state, &mapped.join("x.php"), cx).await;
        Fixture::save_in(&buffer, "changed", cx);
        assert_eq!(f.upload(&buffer, true, cx), UploadStart::Started);
        cx.run_until_parked();
        assert!(f.puts().is_empty());
        assert_eq!(f.connects(), 0);
        let outcomes = f.outcomes();
        assert!(
            matches!(outcomes.as_slice(), [UploadOutcome::Blocked(message)] if message.contains("symbolic link")),
            "{outcomes:?}"
        );
    }
}
