use crate::{LocalHistorySettings, Revision, Store, capture};
use gpui::{BorrowAppContext, TestAppContext};
use project::{FakeFs, Fs as _, Project};
use serde_json::json;
use settings::{
    Settings as _, SettingsContent, SettingsStore, ZenithLocalHistorySettingsContent,
    ZenithSettingsContent,
};
use std::path::{Path, PathBuf};

pub(crate) fn history_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// Starts recording into a database under `dir`; call before creating the project.
pub(crate) fn start(cx: &mut TestAppContext, dir: &Path) {
    let dir = dir.to_path_buf();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        capture::start(cx, move |create| {
            if create {
                Store::open_in(&dir).map(Some)
            } else {
                Store::open_existing_in(&dir)
            }
        });
    });
}

pub(crate) fn configure(
    cx: &mut TestAppContext,
    update: impl FnOnce(&mut ZenithLocalHistorySettingsContent),
) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                let mut history = ZenithLocalHistorySettingsContent::default();
                update(&mut history);
                content.zenith = Some(ZenithSettingsContent {
                    local_history: Some(history),
                    ..Default::default()
                });
            });
        });
    });
}

/// Revisions of `/dir/<rel_path>`, newest first, read through a second connection.
pub(crate) fn revisions(cx: &mut TestAppContext, dir: &Path, rel_path: &str) -> Vec<Revision> {
    revisions_in(cx, dir, "/dir", rel_path)
}

pub(crate) fn revisions_in(
    cx: &mut TestAppContext,
    dir: &Path,
    root: &str,
    rel_path: &str,
) -> Vec<Revision> {
    cx.run_until_parked();
    Store::open_in(dir)
        .and_then(|store| store.revisions(root, rel_path))
        .expect("read revisions")
}

/// A project whose only worktree is the single file `/dir/<name>`.
async fn single_file_project(name: &str, cx: &mut TestAppContext) -> gpui::Entity<Project> {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", json!({ name: "base" })).await;
    Project::test(fs, [Path::new("/dir").join(name).as_path()], cx).await
}

pub(crate) fn sources(revisions: &[Revision]) -> Vec<&str> {
    revisions
        .iter()
        .map(|revision| revision.source.as_str())
        .collect()
}

pub(crate) async fn open_edit_save(
    project: &gpui::Entity<Project>,
    path: &str,
    text: &str,
    cx: &mut TestAppContext,
) {
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .await
        .expect("open buffer");
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, text)], None, cx));
    project
        .update(cx, |project, cx| project.save_buffer(buffer, cx))
        .await
        .expect("save buffer");
}

pub(crate) async fn project_with(
    files: serde_json::Value,
    cx: &mut TestAppContext,
) -> gpui::Entity<Project> {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", files).await;
    Project::test(fs, [Path::new("/dir")], cx).await
}

#[gpui::test]
async fn test_three_saves_after_open_make_one_baseline_and_three_revisions(
    cx: &mut TestAppContext,
) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    for edit in ["1", "2", "3"] {
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, edit)], None, cx));
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .expect("save buffer");
    }
    let revisions = revisions(cx, dir.path(), "a.txt");
    assert_eq!(sources(&revisions), ["save", "save", "save", "open"]);
    assert!(revisions.iter().all(|revision| revision.ts > 1_000_000_000));
}

#[gpui::test]
async fn test_private_glob_files_are_not_recorded(cx: &mut TestAppContext) {
    // `.pem` is not in the store's own secret list, so only the gate's
    // `is_private` check keeps it out.
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "server.pem": "key", "ok.txt": "ok" }), cx).await;
    for name in ["server.pem", "ok.txt"] {
        open_edit_save(&project, &format!("/dir/{name}"), "x", cx).await;
    }
    assert!(revisions(cx, dir.path(), "server.pem").is_empty());
    assert_eq!(revisions(cx, dir.path(), "ok.txt").len(), 2);
}

#[gpui::test]
async fn test_secret_named_files_are_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ ".env": "A=1", "wp-config.php": "<?php" }), cx).await;
    for name in [".env", "wp-config.php"] {
        open_edit_save(&project, &format!("/dir/{name}"), "x", cx).await;
        assert!(revisions(cx, dir.path(), name).is_empty(), "{name}");
    }
}

#[gpui::test]
async fn test_single_file_worktrees_use_the_real_file_name(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    // `server.pem` is only stopped by the gate, the others also by the store.
    for name in [".env", "wp-config.php", "server.pem", "ok.txt"] {
        let project = single_file_project(name, cx).await;
        open_edit_save(&project, &format!("/dir/{name}"), "x", cx).await;
        let root = format!("/dir/{name}");
        let count = revisions_in(cx, dir.path(), &root, "").len();
        assert_eq!(count, if name == "ok.txt" { 2 } else { 0 }, "{name}");
    }
}

#[gpui::test]
async fn test_remote_host_caches_under_the_data_dir_are_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let fs = FakeFs::new(cx.executor());
    let cache = paths::data_dir().join("deployment/cache/srv");
    fs.insert_tree(&cache, json!({ "index.php": "base" })).await;
    let file = cache.join("index.php");
    let project = Project::test(fs, [file.as_path()], cx).await;
    let path = file.to_string_lossy().into_owned();
    open_edit_save(&project, &path, "SECRET", cx).await;
    assert!(revisions_in(cx, dir.path(), &path, "").is_empty());
}

#[gpui::test]
async fn test_a_worktree_above_the_data_dir_still_skips_remote_caches_but_keeps_scratches(
    cx: &mut TestAppContext,
) {
    let dir = history_dir();
    start(cx, dir.path());
    let fs = FakeFs::new(cx.executor());
    let data = paths::data_dir();
    let root = data.parent().expect("data dir has a parent").to_path_buf();
    fs.insert_tree(
        &data,
        json!({
            "deployment": { "cache": { "srv": { "index.php": "base" } } },
            "scratches": { "a.txt": "base" },
        }),
    )
    .await;
    let project = Project::test(fs, [root.as_path()], cx).await;
    let root_str = root.to_string_lossy().into_owned();
    let name = data
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    let cache_file = data.join("deployment/cache/srv/index.php");
    open_edit_save(&project, &cache_file.to_string_lossy(), "SECRET", cx).await;
    let scratch = data.join("scratches/a.txt");
    open_edit_save(&project, &scratch.to_string_lossy(), "x", cx).await;
    let cache_rel = format!("{name}/deployment/cache/srv/index.php");
    let scratch_rel = format!("{name}/scratches/a.txt");
    assert!(revisions_in(cx, dir.path(), &root_str, &cache_rel).is_empty());
    assert!(!revisions_in(cx, dir.path(), &root_str, &scratch_rel).is_empty());
}

#[gpui::test]
async fn test_symlinks_are_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", json!({ "a.txt": "a" })).await;
    fs.insert_tree("/outside", json!({ "credentials": "SECRET" }))
        .await;
    fs.create_symlink(
        Path::new("/dir/link.txt"),
        PathBuf::from("/outside/credentials"),
    )
    .await
    .expect("symlink");
    // A link to a file inside the worktree is not external, only symlinked.
    fs.create_symlink(Path::new("/dir/inner.txt"), PathBuf::from("/dir/a.txt"))
        .await
        .expect("inner symlink");
    let project = Project::test(fs, [Path::new("/dir")], cx).await;
    cx.run_until_parked();
    // FakeFs cannot save through a symlink, so this pins the "open" baseline;
    // saves go through the same gate.
    for name in ["a.txt", "link.txt", "inner.txt"] {
        project
            .update(cx, |project, cx| {
                project.open_local_buffer(format!("/dir/{name}"), cx)
            })
            .await
            .expect("open buffer");
    }
    assert!(revisions(cx, dir.path(), "link.txt").is_empty());
    assert!(revisions(cx, dir.path(), "inner.txt").is_empty());
    assert_eq!(revisions(cx, dir.path(), "a.txt").len(), 1);
}

#[gpui::test]
async fn test_save_records_the_saved_text_not_later_edits(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "saved")], None, cx));
    let save = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
    // Typed while the save is in flight.
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
    save.await.expect("save buffer");
    let latest = revisions(cx, dir.path(), "a.txt");
    let saved = latest.first().expect("a save revision");
    let store = Store::open_in(dir.path()).expect("store");
    let text = store
        .content(&saved.blob_hash)
        .expect("content")
        .expect("blob");
    assert_eq!(String::from_utf8(text).expect("utf8"), "saved");
}

#[gpui::test]
async fn test_git_ignored_files_are_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(
        json!({ ".gitignore": "ignored.txt\n", "ignored.txt": "a" }),
        cx,
    )
    .await;
    cx.run_until_parked();
    open_edit_save(&project, "/dir/ignored.txt", "x", cx).await;
    assert!(revisions(cx, dir.path(), "ignored.txt").is_empty());
}

#[gpui::test]
async fn test_file_over_max_size_is_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.max_file_size_kb = Some(1));
    let project = project_with(json!({ "a.txt": "small" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", &"x".repeat(2048), cx).await;
    // Only the 5-byte baseline; the 2 KiB save is over the cap.
    assert_eq!(sources(&revisions(cx, dir.path(), "a.txt")), ["open"]);
}

#[gpui::test]
async fn test_binary_content_is_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.bin": "text" }), cx).await;
    open_edit_save(&project, "/dir/a.bin", "\0\0", cx).await;
    assert_eq!(sources(&revisions(cx, dir.path(), "a.bin")), ["open"]);
}

#[gpui::test]
async fn test_disabled_records_nothing(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.enabled = Some(false));
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", "x", cx).await;
    assert!(revisions(cx, dir.path(), "a.txt").is_empty());
}

#[gpui::test]
async fn test_gate_rejects_a_file_that_is_not_local(cx: &mut TestAppContext) {
    // Projects here are always local (`Project::test`), so this pins the
    // gate's own `file.is_local` check, not `project.is_local()`.
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    cx.update(|cx| {
        let worktree = project.read(cx).worktrees(cx).next().expect("worktree");
        let file = |is_local| worktree::File {
            worktree: worktree.clone(),
            path: util::rel_path::rel_path("a.txt").into(),
            disk_state: language::DiskState::New,
            entry_id: None,
            is_local,
            is_private: false,
        };
        assert!(capture::should_record(&file(true), 1, cx));
        assert!(!capture::should_record(&file(false), 1, cx));
    });
}

#[gpui::test]
async fn test_huge_max_file_size_does_not_overflow(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.max_file_size_kb = Some(u64::MAX));
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", "x", cx).await;
    assert_eq!(
        sources(&revisions(cx, dir.path(), "a.txt")),
        ["save", "open"]
    );
}

fn seed(dir: &Path, rel_path: &str, ts: i64) {
    let store = Store::open_in(dir).expect("open store");
    store
        .record("/dir", rel_path, b"old", None, "save", ts)
        .expect("seed");
}

fn unix_now() -> i64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(elapsed.as_secs()).expect("seconds")
}

#[gpui::test]
async fn test_old_revisions_are_pruned_at_start(cx: &mut TestAppContext) {
    let dir = history_dir();
    seed(dir.path(), "old.txt", 1);
    start(cx, dir.path());
    assert!(revisions(cx, dir.path(), "old.txt").is_empty());
}

#[gpui::test]
async fn test_existing_database_is_pruned_even_when_disabled(cx: &mut TestAppContext) {
    let dir = history_dir();
    seed(dir.path(), "old.txt", 1);
    start(cx, dir.path());
    configure(cx, |history| history.enabled = Some(false));
    assert!(revisions(cx, dir.path(), "old.txt").is_empty());
}

#[gpui::test]
async fn test_disabled_never_creates_a_database(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.enabled = Some(false));
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", "x", cx).await;
    cx.run_until_parked();
    assert!(!dir.path().join("history.db").exists());
}

#[gpui::test]
async fn test_zero_retention_days_keeps_recent_revisions(cx: &mut TestAppContext) {
    let dir = history_dir();
    seed(dir.path(), "recent.txt", unix_now() - 3600);
    start(cx, dir.path());
    configure(cx, |history| history.retention_days = Some(0));
    assert_eq!(revisions(cx, dir.path(), "recent.txt").len(), 1);
}

#[test]
fn test_default_settings_file_matches_the_built_in_defaults() {
    let defaults: SettingsContent =
        settings::parse_json_with_comments(&settings::default_settings()).expect("default.json");
    assert_eq!(
        LocalHistorySettings::from_settings(&defaults),
        LocalHistorySettings::from_settings(&SettingsContent::default())
    );
    let history = defaults
        .zenith
        .and_then(|zenith| zenith.local_history)
        .expect("default.json has a local_history block");
    assert_eq!(history.snapshot_external_changes, Some(true));
}

#[gpui::test]
async fn test_zero_db_size_limit_is_clamped_and_never_wipes_history(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.max_db_size_mb = Some(0));
    assert_eq!(
        cx.update(|cx| LocalHistorySettings::get_global(cx).max_db_size_mb),
        1
    );
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", "x", cx).await;
    assert_eq!(revisions(cx, dir.path(), "a.txt").len(), 2);

    cx.update(capture::send_prune);
    assert_eq!(revisions(cx, dir.path(), "a.txt").len(), 2);
}

// FakeFs cannot hold a non-UTF-8 path, so the key function is pinned directly;
// capture and the history view both get the key only through it.
#[cfg(unix)]
#[test]
fn test_non_utf8_worktree_root_has_no_key() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};
    let root = Path::new(OsStr::from_bytes(b"/d\xffir"));
    assert_eq!(capture::root_key(root), None);
    assert_eq!(
        capture::root_key(Path::new("/dir")),
        Some("/dir".to_string())
    );
}

// APFS refuses non-UTF-8 names, so `open_in` fails earlier on macOS; the
// conversion itself is pinned directly.
#[cfg(unix)]
#[test]
fn test_non_utf8_database_path_is_rejected() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};
    let bad = Path::new(OsStr::from_bytes(b"/h\xffist/history.db"));
    assert!(crate::store::utf8_path(bad).is_err());
    assert_eq!(
        crate::store::utf8_path(Path::new("/h/history.db")).expect("utf-8"),
        "/h/history.db"
    );
}

#[test]
fn test_one_undecodable_blob_does_not_hide_the_history() {
    let dir = history_dir();
    let store = Store::open_in(dir.path()).expect("store");
    for (text, ts) in [("one", 1), ("three", 2)] {
        store
            .record("/dir", "a.txt", text.as_bytes(), None, "save", ts)
            .expect("record");
    }
    let oldest = store.revisions("/dir", "a.txt").expect("revisions")[1]
        .blob_hash
        .clone();
    let file = dir.path().join("history.db");
    let connection = sqlez::connection::Connection::open_file(file.to_str().expect("utf-8"));
    connection
        .exec(&format!(
            "UPDATE blobs SET content = X'DEADBEEF' WHERE hash = '{oldest}'"
        ))
        .expect("prepare")()
    .expect("corrupt the blob");

    let entries = capture::list_entries(&store, "/dir", "a.txt").expect("list still works");
    let sizes: Vec<u64> = entries.iter().map(|entry| entry.size).collect();
    assert_eq!(sizes, [5, 0]);
}

#[gpui::test]
async fn test_max_file_size_is_capped_at_what_can_be_read_back(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| history.max_file_size_kb = Some(u64::MAX));
    assert_eq!(
        cx.update(|cx| LocalHistorySettings::get_global(cx).max_file_size_kb),
        crate::store::MAX_CONTENT_BYTES / 1024
    );
}

// External changes: something other than the editor rewrites a file on disk.

pub(crate) async fn write_on_disk(
    project: &gpui::Entity<Project>,
    path: &str,
    text: &str,
    cx: &mut TestAppContext,
) {
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    fs.write(Path::new(path), text.as_bytes())
        .await
        .expect("write file");
    settle(cx);
}

/// Lets queued work finish, including the wait for a file with unsaved edits.
pub(crate) fn settle(cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(3));
    cx.run_until_parked();
}

/// Opens a file, which records its "open" baseline, then closes it again.
pub(crate) async fn open_and_close(
    project: &gpui::Entity<Project>,
    path: &str,
    cx: &mut TestAppContext,
) {
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .await
        .expect("open buffer");
    cx.run_until_parked();
    drop(buffer);
    cx.run_until_parked();
}

#[gpui::test]
async fn test_external_change_to_a_clean_open_buffer_is_a_revision(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    write_on_disk(&project, "/dir/a.txt", "from git", cx).await;
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "from git",
        "the buffer reloaded"
    );
    assert_eq!(
        sources(&revisions(cx, dir.path(), "a.txt")),
        ["external", "open"]
    );
}

#[gpui::test]
async fn test_external_change_to_a_closed_file_with_history_is_a_revision(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_and_close(&project, "/dir/a.txt", cx).await;
    write_on_disk(&project, "/dir/a.txt", "from a formatter", cx).await;
    let revisions = revisions(cx, dir.path(), "a.txt");
    assert_eq!(sources(&revisions), ["external", "open"]);
    let store = Store::open_in(dir.path()).expect("store");
    let text = store.content(&revisions[0].blob_hash).expect("content");
    assert_eq!(text.as_deref(), Some(b"from a formatter".as_slice()));
}

#[gpui::test]
async fn test_external_change_under_unsaved_edits_records_the_disk_text(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
    write_on_disk(&project, "/dir/a.txt", "from git", cx).await;
    let revisions = revisions(cx, dir.path(), "a.txt");
    assert_eq!(sources(&revisions), ["external", "open"]);
    let store = Store::open_in(dir.path()).expect("store");
    let text = store.content(&revisions[0].blob_hash).expect("content");
    assert_eq!(text.as_deref(), Some(b"from git".as_slice()));
}

#[gpui::test]
async fn test_external_change_to_a_file_without_history_is_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base", "seen.txt": "x" }), cx).await;
    // One tracked file makes sure the database exists.
    open_and_close(&project, "/dir/seen.txt", cx).await;
    write_on_disk(&project, "/dir/a.txt", "changed", cx).await;
    assert!(revisions(cx, dir.path(), "a.txt").is_empty());
}

#[gpui::test]
async fn test_external_rewrite_with_the_same_text_is_not_a_revision(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "one\ntwo" }), cx).await;
    open_and_close(&project, "/dir/a.txt", cx).await;
    // A touch, then CRLF or lone CR line endings and a BOM: none changes the text.
    for same in ["one\ntwo", "one\r\ntwo", "one\rtwo", "\u{feff}one\ntwo"] {
        write_on_disk(&project, "/dir/a.txt", same, cx).await;
    }
    assert_eq!(sources(&revisions(cx, dir.path(), "a.txt")), ["open"]);
}

#[gpui::test]
async fn test_saving_in_the_editor_is_not_an_external_change(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "one\r\ntwo" }), cx).await;
    open_edit_save(&project, "/dir/a.txt", "x", cx).await;
    settle(cx);
    assert_eq!(
        sources(&revisions(cx, dir.path(), "a.txt")),
        ["save", "open"]
    );
}

#[gpui::test]
async fn test_external_change_goes_through_the_same_gate(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(
        json!({ "off.txt": "a", "big.txt": "a", "private.txt": "a", "ignored.txt": "a" }),
        cx,
    )
    .await;
    for name in ["off", "big", "private", "ignored"] {
        open_and_close(&project, &format!("/dir/{name}.txt"), cx).await;
    }
    // Each file is tracked; now each is something the gate refuses.
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.worktree.private_files =
                    Some(vec!["**/private.txt".to_string()].into());
            });
        });
    });
    write_on_disk(&project, "/dir/.gitignore", "ignored.txt\n", cx).await;
    configure(cx, |history| history.max_file_size_kb = Some(1));
    write_on_disk(&project, "/dir/big.txt", &"x".repeat(2048), cx).await;
    write_on_disk(&project, "/dir/private.txt", "b", cx).await;
    write_on_disk(&project, "/dir/ignored.txt", "b", cx).await;
    configure(cx, |history| history.enabled = Some(false));
    write_on_disk(&project, "/dir/off.txt", "b", cx).await;
    for name in ["off", "big", "private", "ignored"] {
        let found = revisions(cx, dir.path(), &format!("{name}.txt"));
        assert_eq!(sources(&found), ["open"], "{name}");
    }
}

#[gpui::test]
async fn test_binary_external_change_is_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_and_close(&project, "/dir/a.txt", cx).await;
    write_on_disk(&project, "/dir/a.txt", "a\0b", cx).await;
    assert_eq!(sources(&revisions(cx, dir.path(), "a.txt")), ["open"]);
}

#[gpui::test]
async fn test_initial_scan_is_not_an_external_change(cx: &mut TestAppContext) {
    // A file whose history is older than the text now on disk: the first scan
    // reports it as `Loaded`, which is not a change made while the editor ran.
    let dir = history_dir();
    seed(dir.path(), "a.txt", unix_now());
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "different now" }), cx).await;
    settle(cx);
    drop(project);
    assert_eq!(sources(&revisions(cx, dir.path(), "a.txt")), ["save"]);
}

/// The file on disk is written without letting the clock run, so a file with
/// unsaved edits is still waiting.
async fn write_without_waiting(
    project: &gpui::Entity<Project>,
    path: &str,
    text: &str,
    cx: &mut TestAppContext,
) {
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    fs.write(Path::new(path), text.as_bytes())
        .await
        .expect("write file");
    cx.run_until_parked();
}

#[gpui::test]
async fn test_external_change_is_checked_again_after_the_wait(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
    write_without_waiting(&project, "/dir/a.txt", "from git", cx).await;
    // While the change waits, the file becomes private.
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.worktree.private_files = Some(vec!["**/a.txt".to_string()].into());
            });
        });
    });
    settle(cx);
    assert_eq!(sources(&revisions(cx, dir.path(), "a.txt")), ["open"]);
    drop(buffer);
}

#[gpui::test]
async fn test_external_change_that_became_a_symlink_is_not_recorded(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base", "b.txt": "closed" }), cx).await;
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    fs.create_dir(Path::new("/outside")).await.expect("dir");
    fs.write(Path::new("/outside/secret"), b"SECRET")
        .await
        .expect("write");
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    open_and_close(&project, "/dir/b.txt", cx).await;
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
    write_without_waiting(&project, "/dir/a.txt", "from git", cx).await;
    // Both files are swapped for links to a file outside the folder.
    for name in ["a.txt", "b.txt"] {
        let path = PathBuf::from("/dir").join(name);
        fs.remove_file(&path, Default::default())
            .await
            .expect("remove");
        fs.create_symlink(&path, PathBuf::from("/outside/secret"))
            .await
            .expect("symlink");
    }
    settle(cx);
    for name in ["a.txt", "b.txt"] {
        assert_eq!(
            sources(&revisions(cx, dir.path(), name)),
            ["open"],
            "{name}"
        );
    }
    drop(buffer);
}

#[gpui::test]
async fn test_reload_under_typing_records_the_disk_text_not_the_buffer(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer");
    let before = buffer.read_with(cx, |buffer, _| buffer.version());
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "typed ")], None, cx));
    write_without_waiting(&project, "/dir/a.txt", "from git", cx).await;
    // A reload that finishes after the user typed: the buffer holds both.
    buffer.update(cx, |buffer, cx| {
        let line_ending = buffer.line_ending();
        buffer.did_reload(before, line_ending, None, cx);
    });
    cx.run_until_parked();
    let revisions = revisions(cx, dir.path(), "a.txt");
    let store = Store::open_in(dir.path()).expect("store");
    let texts: Vec<_> = revisions
        .iter()
        .map(|revision| store.content(&revision.blob_hash).expect("content"))
        .collect();
    assert_eq!(sources(&revisions), ["external", "open"]);
    assert_eq!(texts[0].as_deref(), Some(b"from git".as_slice()));
}

#[gpui::test]
async fn test_external_changes_can_be_switched_off(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    configure(cx, |history| {
        history.snapshot_external_changes = Some(false)
    });
    let project = project_with(
        json!({ "open.txt": "a", "closed.txt": "a", "own.txt": "a" }),
        cx,
    )
    .await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/open.txt", cx)
        })
        .await
        .expect("open buffer");
    open_and_close(&project, "/dir/closed.txt", cx).await;
    write_on_disk(&project, "/dir/open.txt", "b", cx).await;
    write_on_disk(&project, "/dir/closed.txt", "b", cx).await;
    // Saving in the editor is not affected by the switch.
    open_edit_save(&project, "/dir/own.txt", "x", cx).await;
    for name in ["open.txt", "closed.txt"] {
        assert_eq!(
            sources(&revisions(cx, dir.path(), name)),
            ["open"],
            "{name}"
        );
    }
    assert_eq!(
        sources(&revisions(cx, dir.path(), "own.txt")),
        ["save", "open"]
    );
    drop(buffer);
}

async fn open_buffer(
    project: &gpui::Entity<Project>,
    path: &str,
    cx: &mut TestAppContext,
) -> gpui::Entity<language::Buffer> {
    project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .await
        .expect("open buffer")
}

#[gpui::test]
async fn test_checkpoints_are_kept_and_read_back_by_label(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    let buffer = open_buffer(&project, "/dir/a.txt", cx).await;
    // Same text as the "open" baseline, twice: neither is dropped as a duplicate.
    for label in ["agent:s:1", "agent:s:2"] {
        assert!(cx.update(|cx| crate::checkpoint(&buffer, "base".into(), label, cx)));
    }
    assert!(cx.update(|cx| crate::checkpoint(&buffer, "edited".into(), "agent:s:3", cx)));
    let revisions = revisions(cx, dir.path(), "a.txt");
    assert_eq!(sources(&revisions), ["agent", "agent", "agent", "open"]);
    assert_eq!(revisions[1].label.as_deref(), Some("agent:s:2"));

    for (label, expected) in [
        ("agent:s:1", Some("base")),
        ("agent:s:3", Some("edited")),
        ("agent:s:9", None),
    ] {
        let text = cx
            .update(|cx| crate::labeled_text(&buffer, label, cx))
            .await
            .expect("labeled text");
        assert_eq!(text.as_deref(), expected, "{label}");
    }
}

#[gpui::test]
async fn test_checkpoint_refuses_secret_private_and_disabled(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(
        json!({ ".env": "A=1", "wp-config.php": "<?php", "server.pem": "key", "ok.txt": "ok" }),
        cx,
    )
    .await;
    for name in [".env", "wp-config.php", "server.pem"] {
        let buffer = open_buffer(&project, &format!("/dir/{name}"), cx).await;
        assert!(
            !cx.update(|cx| crate::checkpoint(&buffer, "x".into(), "agent:s:1", cx)),
            "{name}"
        );
        assert!(revisions(cx, dir.path(), name).is_empty(), "{name}");
    }
    let buffer = open_buffer(&project, "/dir/ok.txt", cx).await;
    configure(cx, |history| history.enabled = Some(false));
    assert!(!cx.update(|cx| crate::checkpoint(&buffer, "x".into(), "agent:s:1", cx)));
    configure(cx, |history| history.max_file_size_kb = Some(1));
    let too_big = "x".repeat(2048);
    assert!(!cx.update(|cx| crate::checkpoint(&buffer, too_big.into(), "agent:s:1", cx)));
    configure(cx, |_| {});
    assert!(!cx.update(|cx| crate::checkpoint(&buffer, "a\0b".into(), "agent:s:1", cx)));
    assert!(cx.update(|cx| crate::checkpoint(&buffer, "x".into(), "agent:s:1", cx)));
}
