//! Snapshots buffers into the [`Store`]: an "open" baseline when a buffer is
//! added and a "save" revision on every save, for local projects only.
//!
//! The main thread only decides (cheap metadata checks) and clones the rope;
//! one background task owns the store, so revisions land in event order and no
//! database call touches the main thread.
//!
//! Symlinked and external files (outside the worktree) are intentionally not recorded.

use crate::{LocalHistorySettings, Revision, Store};
use anyhow::Result;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, Context, Global, Task};
use language::{Buffer, BufferEvent};
use project::{Project, buffer_store::BufferStoreEvent};
use rope::Rope;
use settings::{Settings, SettingsLocation};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use util::{ResultExt as _, rel_path::RelPath};
use worktree::WorktreeSettings;

const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const BYTES_PER_KB: u64 = 1024;
const BYTES_PER_MB: u64 = 1024 * 1024;

enum Job {
    Record {
        worktree_root: String,
        rel_path: String,
        rope: Rope,
        source: &'static str,
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
                cx.subscribe(buffer, |_, buffer, event, cx| {
                    if matches!(event, BufferEvent::Saved) {
                        capture(&buffer, "save", cx);
                    }
                })
                .detach();
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
    let mut store = None;
    let mut create_failed = false;
    while let Some(job) = receiver.next().await {
        let create = matches!(job, Job::Record { .. });
        if store.is_none() && !(create && create_failed) {
            store = open(create).log_err().flatten();
            create_failed = create && store.is_none();
        }
        let Some(store) = &store else { continue };
        match job {
            Job::Record {
                worktree_root,
                rel_path,
                rope,
                source,
                ts,
            } => {
                let bytes = rope.to_string().into_bytes();
                if bytes.contains(&0) {
                    continue; // binary
                }
                store
                    .record(&worktree_root, &rel_path, &bytes, None, source, ts)
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
        }
    }
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

fn capture(buffer: &gpui::Entity<Buffer>, source: &'static str, cx: &mut App) {
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
    if !should_record(file, rope.len(), cx) {
        return;
    }
    let Some(worktree_root) = root_key(&file.worktree.read(cx).abs_path()) else {
        return;
    };
    history
        .jobs
        .unbounded_send(Job::Record {
            worktree_root,
            rel_path: file.path.as_unix_str().to_owned(),
            rope,
            source,
            ts: now(),
        })
        .log_err();
}
