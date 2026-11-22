//! Snapshots buffers into the [`Store`]: an "open" baseline when a buffer is
//! added and a "save" revision on every save, for local projects only.
//!
//! The main thread only decides (cheap metadata checks) and clones the rope;
//! one background task owns the store, so revisions land in event order and no
//! database call touches the main thread.
//!
//! Symlinked and external files (outside the worktree) are intentionally not recorded.

use crate::{LocalHistorySettings, Revision, Store, store::is_secret_file};
use anyhow::Result;
use fs::Fs;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use language::{Buffer, BufferEvent, LineEnding};
use project::{Project, ProjectPath, buffer_store::BufferStoreEvent};
use rope::Rope;
use settings::{Settings, SettingsLocation};
use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use util::{ResultExt as _, rel_path::RelPath};
use worktree::{PathChange, WorktreeSettings};

const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const BYTES_PER_KB: u64 = 1024;
const BYTES_PER_MB: u64 = 1024 * 1024;
/// Changed files looked at per worktree update, after the cheap filter (a file
/// whose change kind counts): a checkout that rewrites thousands of files must not
/// stall the main thread. The rest of such a burst is skipped.
const MAX_EXTERNAL_PER_UPDATE: usize = 512;
/// How long a changed file with unsaved edits waits before it counts as an
/// external change.
const DIRTY_SETTLE: Duration = Duration::from_secs(2);
/// Most files "Recover Deleted Files" reads from the database, newest first. The
/// list is filtered afterwards (the files that still exist are dropped), so this is a
/// ceiling on distinct files with history in one folder, not on what is shown.
// ponytail: older files than the newest 20 000 are not offered; page the query if
// a retention window ever holds more.
const MAX_DELETED_LISTED: i64 = 20_000;

enum Job {
    Record {
        worktree_root: String,
        rel_path: String,
        rope: Rope,
        /// Set for a checkpoint: kept even when the text equals the latest revision.
        label: Option<String>,
        source: &'static str,
        ts: i64,
    },
    /// A file changed on disk behind the editor's back (checkout, formatter,
    /// another tool) and no clean buffer shows it. Only files that already
    /// have history are recorded, which bounds the cost of a large checkout.
    External {
        worktree_root: String,
        rel_path: String,
        fs: Arc<dyn Fs>,
        max_bytes: u64,
        ts: i64,
    },
    Prune {
        ts: i64,
        days: u32,
        max_bytes: u64,
    },
    /// Read requests are answered in queue order, so they see every record
    /// sent before them. A dropped reply means there is no database yet.
    List {
        worktree_root: String,
        rel_path: String,
        reply: oneshot::Sender<Result<Vec<Entry>>>,
    },
    Content {
        hash: String,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    },
    /// Text of the newest revision recorded under a label.
    Labeled {
        worktree_root: String,
        rel_path: String,
        label: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    /// The newest revision of each file of one worktree.
    Latest {
        worktree_root: String,
        reply: oneshot::Sender<Result<Vec<Revision>>>,
    },
}

/// One revision of a file and the size of its content in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub revision: Revision,
    pub size: u64,
}

/// Held for the life of the app: dropping it cancels both tasks.
struct LocalHistory {
    jobs: mpsc::UnboundedSender<Job>,
    _owner: Task<()>,
    _retention: Task<()>,
}

impl Global for LocalHistory {}

/// Starts recording. `open(create)` runs on a background thread. Only a
/// record job passes `create == true`, so a disabled feature never creates a
/// database, yet an existing one is still pruned.
pub(crate) fn start(cx: &mut App, open: impl Fn(bool) -> Result<Option<Store>> + Send + 'static) {
    LocalHistorySettings::register(cx);
    let (jobs, receiver) = mpsc::unbounded();
    let owner = cx.background_spawn(own_store(receiver, open));
    let retention = cx.spawn(async move |cx| {
        loop {
            cx.update(send_prune);
            cx.background_executor().timer(PRUNE_EVERY).await;
        }
    });
    cx.set_global(LocalHistory {
        jobs,
        _owner: owner,
        _retention: retention,
    });
    cx.observe_new(|project: &mut Project, _, cx: &mut Context<Project>| {
        if !project.is_local() {
            return;
        }
        let buffer_store = project.buffer_store().clone();
        cx.subscribe(&buffer_store, |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                capture(buffer, "open", cx);
                cx.subscribe(buffer, |project, buffer, event, cx| match event {
                    BufferEvent::Saved => capture(&buffer, "save", cx),
                    // The buffer took a change made on disk by something else.
                    BufferEvent::Reloaded => capture_reloaded(project, &buffer, cx),
                    _ => {}
                })
                .detach();
            }
        })
        .detach();
        let this = cx.entity();
        cx.subscribe(&this, |project, _, event, cx| {
            if let project::Event::WorktreeUpdatedEntries(worktree_id, changes) = event {
                capture_external(project, *worktree_id, changes, cx);
            }
        })
        .detach();
    })
    .detach();
}

async fn own_store(
    mut receiver: mpsc::UnboundedReceiver<Job>,
    open: impl Fn(bool) -> Result<Option<Store>>,
) {
    let mut opened = None;
    let mut create_failed = false;
    while let Some(job) = receiver.next().await {
        let create = matches!(job, Job::Record { .. });
        if opened.is_none() && !(create && create_failed) {
            opened = open(create).log_err().flatten();
            create_failed = create && opened.is_none();
        }
        let Some(store) = &opened else { continue };
        match job {
            Job::Record {
                worktree_root,
                rel_path,
                rope,
                label,
                source,
                ts,
            } => {
                let bytes = rope.to_string().into_bytes();
                if bytes.contains(&0) {
                    continue; // binary
                }
                store
                    .record(
                        &worktree_root,
                        &rel_path,
                        &bytes,
                        label.as_deref(),
                        source,
                        ts,
                    )
                    .log_err();
            }
            Job::External {
                worktree_root,
                rel_path,
                fs,
                max_bytes,
                ts,
            } => {
                if !store
                    .has_history(&worktree_root, &rel_path)
                    .unwrap_or(false)
                {
                    continue;
                }
                // ponytail: the read blocks this queue; fine for files under the size
                // cap, give reads their own task if a slow disk makes history lists lag.
                let root = PathBuf::from(&worktree_root);
                let path = root.join(&rel_path);
                let Some(bytes) = read_text(fs.as_ref(), &root, &path, max_bytes).await else {
                    continue;
                };
                // Not the borrow from before the await: `Store` is not `Sync`.
                let Some(store) = &opened else { continue };
                store
                    .record(&worktree_root, &rel_path, &bytes, None, "external", ts)
                    .log_err();
            }
            Job::Prune {
                ts,
                days,
                max_bytes,
            } => {
                store.prune(ts, days, max_bytes).log_err();
            }
            Job::List {
                worktree_root,
                rel_path,
                reply,
            } => {
                reply
                    .send(list_entries(store, &worktree_root, &rel_path))
                    .ok();
            }
            Job::Content { hash, reply } => {
                reply.send(store.content(&hash)).ok();
            }
            Job::Labeled {
                worktree_root,
                rel_path,
                label,
                reply,
            } => {
                reply
                    .send(labeled_text_in(store, &worktree_root, &rel_path, &label))
                    .ok();
            }
            Job::Latest {
                worktree_root,
                reply,
            } => {
                reply
                    .send(store.latest_per_path(&worktree_root, MAX_DELETED_LISTED))
                    .ok();
            }
        }
    }
}

/// The text of a file as a buffer would hold it: UTF-8 without BOM, `\n` line
/// endings (a lone `\r` too), so a rewrite that only changes line endings or a BOM is
/// no new revision. `None` for anything that is not a plain file inside the
/// folder: a symbolic link, a folder, a pipe or device, a path that resolves
/// outside `root`; and for a file that is too large, binary or not UTF-8. The
/// read is bounded by `max_bytes` whatever the file turns out to be.
///
/// The main thread checked the entry before queueing this, and the checks here run
/// again on the disk. That narrows the window for a file swapped in between; it does
/// not close it: the checks are by path, and the read opens by path afterwards. On
/// unix the open itself refuses a link and the handle is compared with the path
/// (`open_unix`), which leaves only a swap between those last two steps.
async fn read_text(fs: &dyn Fs, root: &Path, path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let metadata = fs.metadata(path).await.ok().flatten()?;
    if metadata.is_symlink || metadata.is_dir || metadata.is_fifo || metadata.len > max_bytes {
        return None;
    }
    // Follows links, so only a regular file passes: no device, socket or pipe.
    if !fs.is_file(path).await {
        return None;
    }
    // Backstop for a link in a parent folder.
    let canonical_root = fs.canonicalize(root).await.ok()?;
    if !fs
        .canonicalize(path)
        .await
        .ok()?
        .starts_with(canonical_root)
    {
        return None;
    }
    // ponytail: a blocking read on this background thread, like the store calls.
    let bytes = match open_plain_file(fs, path, metadata.inode, max_bytes).await {
        Some(reader) => read_bounded(reader, max_bytes)?,
        None => return None,
    };
    if bytes.contains(&0) {
        return None;
    }
    let mut text = String::from_utf8(bytes).ok()?;
    if text.starts_with('\u{feff}') {
        text.remove(0);
    }
    LineEnding::normalize(&mut text);
    Some(text.into_bytes())
}

async fn open_plain_file(
    fs: &dyn Fs,
    path: &Path,
    inode: u64,
    max_bytes: u64,
) -> Option<Box<dyn std::io::Read + Send>> {
    #[cfg(unix)]
    if !fs.is_fake() {
        return Some(Box::new(open_unix(path, inode, max_bytes)?));
    }
    let _ = (inode, max_bytes);
    Some(Box::new(fs.open_sync(path).await.ok()?))
}

/// Opens `path` the way a file that may be swapped under us must be: no link
/// followed at the last component (`O_NOFOLLOW`) and no wait on a pipe (`O_NONBLOCK`).
/// The open handle must be a regular file within the size cap, the file the earlier
/// check saw (`inode`), and still the file the path names now.
#[cfg(unix)]
fn open_unix(path: &Path, inode: u64, max_bytes: u64) -> Option<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let held = file.metadata().ok()?;
    let named = std::fs::symlink_metadata(path).ok()?;
    let same_file = (held.dev(), held.ino()) == (named.dev(), named.ino());
    (held.is_file() && held.len() <= max_bytes && held.ino() == inode && same_file).then_some(file)
}

/// Everything `reader` holds, or `None` when it holds more than `max_bytes`
/// (or cannot be read). Never reads past `max_bytes + 1`.
fn read_bounded(reader: impl std::io::Read, max_bytes: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

/// Revisions newest first, each with its content size.
// ponytail: decompresses every distinct blob of the file once per list; store a size
// column if histories with hundreds of multi-megabyte revisions make this slow.
pub(crate) fn list_entries(
    store: &Store,
    worktree_root: &str,
    rel_path: &str,
) -> Result<Vec<Entry>> {
    let mut sizes = HashMap::new();
    Ok(store
        .revisions(worktree_root, rel_path)?
        .into_iter()
        .map(|revision| {
            // One undecodable or oversized blob must not hide the whole history.
            let size = *sizes.entry(revision.blob_hash.clone()).or_insert_with(|| {
                store
                    .content(&revision.blob_hash)
                    .log_err()
                    .flatten()
                    .map_or(0, |bytes| bytes.len() as u64)
            });
            Entry { revision, size }
        })
        .collect())
}

fn labeled_text_in(
    store: &Store,
    worktree_root: &str,
    rel_path: &str,
    label: &str,
) -> Result<Option<String>> {
    let Some(revision) = store.labeled(worktree_root, rel_path, label)? else {
        return Ok(None);
    };
    let Some(bytes) = store.content(&revision.blob_hash)? else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8(bytes)?))
}

/// Revisions of one file, newest first. Empty when no database exists yet.
pub(crate) fn list(cx: &App, worktree_root: String, rel_path: String) -> Task<Result<Vec<Entry>>> {
    let (reply, receiver) = oneshot::channel();
    let sent = send(
        cx,
        Job::List {
            worktree_root,
            rel_path,
            reply,
        },
    );
    cx.background_spawn(async move {
        if !sent {
            return Ok(Vec::new());
        }
        receiver.await.unwrap_or_else(|_| Ok(Vec::new()))
    })
}

/// Decompressed content of one blob, `None` when it is gone (pruned).
pub(crate) fn content(cx: &App, hash: String) -> Task<Result<Option<Vec<u8>>>> {
    let (reply, receiver) = oneshot::channel();
    let sent = send(cx, Job::Content { hash, reply });
    cx.background_spawn(async move {
        if !sent {
            return Ok(None);
        }
        receiver.await.unwrap_or(Ok(None))
    })
}

/// The newest revision of every file ever recorded under `worktree_root`.
pub(crate) fn latest(cx: &App, worktree_root: String) -> Task<Result<Vec<Revision>>> {
    let (reply, receiver) = oneshot::channel();
    let sent = send(
        cx,
        Job::Latest {
            worktree_root,
            reply,
        },
    );
    cx.background_spawn(async move {
        if !sent {
            return Ok(Vec::new());
        }
        receiver.await.unwrap_or_else(|_| Ok(Vec::new()))
    })
}

fn send(cx: &App, job: Job) -> bool {
    cx.try_global::<LocalHistory>()
        .is_some_and(|history| history.jobs.unbounded_send(job).log_err().is_some())
}

pub(crate) fn send_prune(cx: &mut App) {
    let settings = *LocalHistorySettings::get_global(cx);
    if let Some(history) = cx.try_global::<LocalHistory>() {
        history
            .jobs
            .unbounded_send(Job::Prune {
                ts: now(),
                days: settings.retention_days,
                max_bytes: settings.max_db_size_mb.saturating_mul(BYTES_PER_MB),
            })
            .log_err();
    }
}

/// Wall clock, never the file's mtime.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_secs().try_into().unwrap_or(i64::MAX)
        })
}

/// The worktree root as stored in the database. `None` for a path that is not
/// valid UTF-8: a lossy conversion would map different folders to one key.
pub(crate) fn root_key(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

/// The one gate every capture path goes through. `len` is the content size in
/// bytes, known before anything is read or hashed.
pub(crate) fn should_record(file: &worktree::File, len: usize, cx: &App) -> bool {
    let max_bytes = LocalHistorySettings::get_global(cx)
        .max_file_size_kb
        .saturating_mul(BYTES_PER_KB);
    is_trackable(file, cx) && u64::try_from(len).is_ok_and(|len| len <= max_bytes)
}

/// Everything the gate checks except size.
pub(crate) fn is_trackable(file: &worktree::File, cx: &App) -> bool {
    LocalHistorySettings::get_global(cx).enabled && is_viewable(file, cx)
}

/// What decides whether old revisions may be shown: the gate minus `enabled`
/// and size. A file that has since become private, ignored or a symlink does
/// not expose its old revisions, but turning recording off hides nothing.
pub(crate) fn is_viewable(file: &worktree::File, cx: &App) -> bool {
    let worktree = file.worktree.read(cx);
    // A single-file worktree (`/dir/.env`) has an empty relative path; the
    // private and exclusion globs must see the real file name instead.
    let abs_path = worktree.abs_path();
    let path = if file.path.is_empty() {
        abs_path
            .file_name()
            .and_then(|name| RelPath::from_unix_str(name).ok())
    } else {
        Some(&*file.path)
    };
    let Some(path) = path else { return false };
    let exclusions = WorktreeSettings::get(
        Some(SettingsLocation {
            worktree_id: worktree.id(),
            path: &file.path,
        }),
        cx,
    );
    // A symlink may point anywhere (`~/.aws/credentials`); never follow one.
    let entry = file.entry_id.and_then(|id| worktree.entry_for_id(id));
    root_key(&abs_path).is_some()
        && file.is_local
        && !file.is_private
        && !exclusions.is_path_private(path)
        && !exclusions.is_path_excluded(path)
        && !entry.is_some_and(|entry| {
            entry.is_ignored || entry.is_external || entry.canonical_path.is_some()
        })
}

/// A buffer that still differs from the disk: unsaved edits or a conflict.
fn is_unsettled(buffer: &Buffer) -> bool {
    buffer.is_dirty() || buffer.has_conflict()
}

/// The job that records `path` as an external change, if every check still
/// holds now: the setting, the entry (a file), no clean open buffer (its reload
/// records it) and the same gate as every other capture. The flag says whether an
/// open buffer is involved.
fn external_job(
    project: &Project,
    worktree_id: worktree::WorktreeId,
    path: &Arc<RelPath>,
    cx: &App,
) -> Option<(Job, bool)> {
    let settings = *LocalHistorySettings::get_global(cx);
    if !settings.snapshot_external_changes {
        return None;
    }
    let worktree = project.worktree_for_id(worktree_id, cx)?;
    let worktree_root = root_key(&worktree.read(cx).abs_path())?;
    let entry = worktree.read(cx).entry_for_path(path)?;
    if !entry.is_file() || path.is_empty() {
        return None;
    }
    let buffer = project.buffer_store().read(cx).get_by_path(&ProjectPath {
        worktree_id,
        path: path.clone(),
    });
    if buffer
        .as_ref()
        .is_some_and(|buffer| !is_unsettled(buffer.read(cx)))
    {
        return None;
    }
    let file = worktree::File::for_entry(entry.clone(), worktree.clone());
    if !should_record(&file, usize::try_from(entry.size).ok()?, cx) {
        return None;
    }
    let job = Job::External {
        worktree_root,
        rel_path: path.as_unix_str().to_owned(),
        fs: project.fs().clone(),
        max_bytes: settings.max_file_size_kb.saturating_mul(BYTES_PER_KB),
        ts: now(),
    };
    Some((job, buffer.is_some()))
}

/// A buffer took a change from disk. Its text is the disk text unless the user typed
/// since: then the file is read instead.
fn capture_reloaded(project: &Project, buffer: &gpui::Entity<Buffer>, cx: &mut App) {
    if !LocalHistorySettings::get_global(cx).snapshot_external_changes {
        return;
    }
    if !is_unsettled(buffer.read(cx)) {
        return capture(buffer, "external", cx);
    }
    let Some(file) = worktree::File::from_dyn(buffer.read(cx).file()) else {
        return;
    };
    let (worktree_id, path) = (file.worktree_id(cx), file.path.clone());
    if let Some((job, _)) = external_job(project, worktree_id, &path, cx) {
        send_all(cx, [job]);
    }
}

/// Files changed on disk: queue the ones no clean open buffer already covers
/// (its reload records them), through the same gate as every other capture.
///
/// A file whose buffer has unsaved edits waits [`DIRTY_SETTLE`] first: the editor's
/// own save reaches the worktree before it marks the buffer clean, and that must
/// stay a "save", not an "external" change. If the buffer is still dirty then,
/// the disk text is a real conflict and is recorded before a later save overwrites
/// it. Every check runs again after the wait, right before the job is queued.
fn capture_external(
    project: &Project,
    worktree_id: worktree::WorktreeId,
    changes: &[(Arc<RelPath>, project::ProjectEntryId, PathChange)],
    cx: &mut Context<Project>,
) {
    if !cx.has_global::<LocalHistory>() || !project.is_local() {
        return;
    }
    let Some(worktree) = project.worktree_for_id(worktree_id, cx) else {
        return;
    };
    let mut paths: Vec<_> = changes
        .iter()
        // Never `Loaded`: that is the initial scan, not a change.
        .filter(|(path, _, change)| {
            matches!(
                change,
                PathChange::Added | PathChange::Updated | PathChange::AddedOrUpdated
            ) && worktree
                .read(cx)
                .entry_for_path(path)
                .is_some_and(|entry| entry.is_file())
        })
        .map(|(path, _, _)| path.clone())
        .collect();
    if paths.len() > MAX_EXTERNAL_PER_UPDATE {
        log::info!(
            "local history: {} external changes in one update, recording the first {MAX_EXTERNAL_PER_UPDATE}",
            paths.len()
        );
        paths.truncate(MAX_EXTERNAL_PER_UPDATE);
    }
    let mut now_jobs = Vec::new();
    let mut later = Vec::new();
    for path in paths {
        match external_job(project, worktree_id, &path, cx) {
            Some((job, false)) => now_jobs.push(job),
            Some((_, true)) => later.push(path),
            None => {}
        }
    }
    send_all(cx, now_jobs);
    if later.is_empty() {
        return;
    }
    cx.spawn(async move |project, cx| {
        cx.background_executor().timer(DIRTY_SETTLE).await;
        project
            .update(cx, |project, cx| {
                let jobs = later
                    .iter()
                    .filter_map(|path| external_job(project, worktree_id, path, cx))
                    .map(|(job, _)| job);
                send_all(cx, jobs.collect::<Vec<_>>());
            })
            .ok();
    })
    .detach();
}

fn send_all(cx: &App, jobs: impl IntoIterator<Item = Job>) {
    if let Some(history) = cx.try_global::<LocalHistory>() {
        for job in jobs {
            history.jobs.unbounded_send(job).log_err();
        }
    }
}

pub(crate) fn capture(buffer: &gpui::Entity<Buffer>, source: &'static str, cx: &mut App) {
    let Some(history) = cx.try_global::<LocalHistory>() else {
        return;
    };
    let buffer = buffer.read(cx);
    let Some(file) = worktree::File::from_dyn(buffer.file()) else {
        return;
    };
    // A baseline needs a file on disk; a save always has one.
    if source == "open" && !file.disk_state.exists() {
        return;
    }
    // A save records the text that was written, not edits typed while it ran.
    let rope = if source == "save" {
        buffer.rope_for_version(buffer.saved_version())
    } else {
        buffer.as_rope().clone()
    };
    send_record(history, file, rope, None, source, cx);
}

/// Queues `rope` as a revision of `file` if the gate allows it.
fn send_record(
    history: &LocalHistory,
    file: &worktree::File,
    rope: Rope,
    label: Option<String>,
    source: &'static str,
    cx: &App,
) -> bool {
    if !should_record(file, rope.len(), cx) {
        return false;
    }
    let Some(worktree_root) = root_key(&file.worktree.read(cx).abs_path()) else {
        return false;
    };
    history
        .jobs
        .unbounded_send(Job::Record {
            worktree_root,
            rel_path: file.path.as_unix_str().to_owned(),
            rope,
            label,
            source,
            ts: now(),
        })
        .log_err()
        .is_some()
}

/// Records `text` as an "agent" revision of the buffer's file under `label`,
/// even when it equals the latest revision. `false` when nothing is queued:
/// local history is off or not started, or the file is not a local plain
/// file, is private, ignored, excluded, a likely secret, over the size cap or
/// binary (contains NUL).
pub fn checkpoint(buffer: &Entity<Buffer>, text: Rope, label: &str, cx: &App) -> bool {
    let Some(history) = cx.try_global::<LocalHistory>() else {
        return false;
    };
    let Some(file) = worktree::File::from_dyn(buffer.read(cx).file()) else {
        return false;
    };
    // The store refuses these too, but the caller must know nothing was kept.
    let abs_path = file
        .worktree
        .read(cx)
        .abs_path()
        .join(file.path.as_std_path());
    if is_secret_file(&abs_path) {
        return false;
    }
    // The store drops binary text silently (see `Job::Record`).
    if text.chunks().any(|chunk| chunk.contains('\0')) {
        return false;
    }
    send_record(history, file, text, Some(label.to_owned()), "agent", cx)
}

/// Text recorded by [`checkpoint`] under `label` for the buffer's file, newest
/// first. `None` when there is none, no database yet, or the file's history
/// may not be shown.
pub fn labeled_text(
    buffer: &Entity<Buffer>,
    label: &str,
    cx: &App,
) -> Task<Result<Option<String>>> {
    let job = worktree::File::from_dyn(buffer.read(cx).file())
        .filter(|file| is_viewable(file, cx))
        .and_then(|file| {
            Some((
                root_key(&file.worktree.read(cx).abs_path())?,
                file.path.as_unix_str().to_owned(),
            ))
        });
    let Some((worktree_root, rel_path)) = job else {
        return Task::ready(Ok(None));
    };
    let (reply, receiver) = oneshot::channel();
    let sent = send(
        cx,
        Job::Labeled {
            worktree_root,
            rel_path,
            label: label.to_owned(),
            reply,
        },
    );
    cx.background_spawn(async move {
        if !sent {
            return Ok(None);
        }
        receiver.await.unwrap_or(Ok(None))
    })
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
