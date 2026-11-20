use crate::{LocalHistorySettings, Revision, Store, capture};
use gpui::{BorrowAppContext, TestAppContext};
use project::{FakeFs, Fs as _, Project};
use serde_json::json;
use settings::{
    Settings as _, SettingsContent, SettingsStore, ZenithLocalHistorySettingsContent,
    ZenithSettingsContent,
};
use std::path::{Path, PathBuf};

fn history_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// Starts recording into a database under `dir`; call before creating the project.
fn start(cx: &mut TestAppContext, dir: &Path) {
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

fn configure(cx: &mut TestAppContext, update: impl FnOnce(&mut ZenithLocalHistorySettingsContent)) {
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
fn revisions(cx: &mut TestAppContext, dir: &Path, rel_path: &str) -> Vec<Revision> {
    revisions_in(cx, dir, "/dir", rel_path)
}

fn revisions_in(cx: &mut TestAppContext, dir: &Path, root: &str, rel_path: &str) -> Vec<Revision> {
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

fn sources(revisions: &[Revision]) -> Vec<&str> {
    revisions
        .iter()
        .map(|revision| revision.source.as_str())
        .collect()
}

async fn open_edit_save(
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

async fn project_with(files: serde_json::Value, cx: &mut TestAppContext) -> gpui::Entity<Project> {
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
    assert!(
        defaults
            .zenith
            .and_then(|zenith| zenith.local_history)
            .is_some()
    );
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
