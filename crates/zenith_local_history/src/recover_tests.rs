use super::{RecoverDelegate, RecoverModal, Refusal, restore, validate_rel, write_new};
use crate::{
    Revision, Store, capture,
    tests::{configure, history_dir},
};
use fs::{FakeFs, Fs as _};
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use util::rel_path::RelPath;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::zenith_local_history::RecoverDeletedFiles;

fn bind_keymap(json: &str, cx: &mut gpui::App) {
    let key_bindings = match KeymapFile::load(json, cx) {
        KeymapFileLoadResult::Success { key_bindings }
        | KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
        KeymapFileLoadResult::JsonParseFailure { error } => panic!("{error:#}"),
    };
    cx.bind_keys(key_bindings);
}

struct Fixture {
    workspace: Entity<Workspace>,
    project: Entity<Project>,
    fs: Arc<FakeFs>,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn store(&self) -> Store {
        Store::open_in(self.dir.path()).expect("store")
    }

    /// A revision of `rel_path` (text `text`, time `ts`) in the history database.
    fn seed(&self, rel_path: &str, text: &str, ts: i64) -> Revision {
        let store = self.store();
        store
            .record("/dir", rel_path, text.as_bytes(), None, "save", ts)
            .expect("record");
        store
            .revisions("/dir", rel_path)
            .expect("revisions")
            .remove(0)
    }

    fn exists(&self, path: &str) -> bool {
        self.fs.paths(false).contains(&PathBuf::from(path))
    }

    async fn text(&self, path: &str) -> String {
        self.fs.load(Path::new(path)).await.expect("load")
    }
}

/// A workspace over `/dir` holding `files`, recording into a temp database. With
/// `trust == false` the folder is untrusted.
async fn fixture(
    files: serde_json::Value,
    trust: bool,
    cx: &mut TestAppContext,
) -> (Fixture, &mut VisualTestContext) {
    let dir = history_dir();
    let history_dir = dir.path().to_path_buf();
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        menu::init();
        workspace::init(app_state.clone(), cx);
        crate::recover::init(cx);
        if !trust {
            project::trusted_worktrees::init(Default::default(), cx);
        }
        capture::start(cx, move |create| {
            if create {
                Store::open_in(&history_dir).map(Some)
            } else {
                Store::open_existing_in(&history_dir)
            }
        });
        bind_keymap(
            include_str!("../../../assets/keymaps/default-macos.json"),
            cx,
        );
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/dir", files).await;
    let project = Project::test(app_state.fs.clone(), [Path::new("/dir")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();
    (
        Fixture {
            workspace,
            project,
            fs,
            dir,
        },
        cx,
    )
}

fn open_modal(fixture: &Fixture, cx: &mut VisualTestContext) -> Entity<RecoverModal> {
    cx.dispatch_action(RecoverDeletedFiles);
    cx.run_until_parked();
    fixture
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<RecoverModal>(cx)
        })
        .expect("the recover modal is open")
}

fn with_delegate<R>(
    modal: &Entity<RecoverModal>,
    cx: &mut VisualTestContext,
    read: impl FnOnce(&RecoverDelegate) -> R,
) -> R {
    modal.read_with(cx, |modal, cx| read(&modal.picker.read(cx).delegate))
}

fn listed_paths(modal: &Entity<RecoverModal>, cx: &mut VisualTestContext) -> Vec<String> {
    with_delegate(modal, cx, |delegate| {
        delegate
            .matches
            .iter()
            .map(|found| {
                delegate.entries[found.candidate_id]
                    .revision
                    .rel_path
                    .clone()
            })
            .collect()
    })
}

fn status(modal: &Entity<RecoverModal>, cx: &mut VisualTestContext) -> String {
    with_delegate(modal, cx, |delegate| delegate.footer_text().to_string())
}

async fn try_restore(
    fixture: &Fixture,
    revision: &Revision,
    cx: &mut VisualTestContext,
) -> Result<(), Refusal> {
    let task = cx.update(|_, cx| {
        let project = fixture.project.clone();
        let worktree_id = project
            .read(cx)
            .worktrees(cx)
            .next()
            .expect("worktree")
            .read(cx)
            .id();
        restore(&project, worktree_id, revision, cx)
    });
    task.await
}

fn with_path(revision: &Revision, rel_path: &str) -> Revision {
    Revision {
        rel_path: rel_path.to_string(),
        ..revision.clone()
    }
}

#[gpui::test]
async fn test_recover_lists_deleted_files_and_restores_the_selected_one(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(
        json!({ "keep.txt": "kept", "sub": { "gone.txt": "now" } }),
        true,
        cx,
    )
    .await;
    fixture.seed("keep.txt", "kept", 100);
    fixture.seed("sub/gone.txt", "the old text", 300);
    fixture.seed("renamed-away.txt", "other", 200);
    fixture
        .fs
        .remove_file(Path::new("/dir/sub/gone.txt"), Default::default())
        .await
        .expect("delete");
    cx.run_until_parked();

    let modal = open_modal(&fixture, cx);
    // Newest first; `keep.txt` still exists, so it is not offered.
    assert_eq!(
        listed_paths(&modal, cx),
        ["sub/gone.txt", "renamed-away.txt"]
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(fixture.text("/dir/sub/gone.txt").await, "the old text");
    assert_eq!(status(&modal, cx), "Restored sub/gone.txt.");
    assert_eq!(listed_paths(&modal, cx), ["renamed-away.txt"]);
    // Nothing else was touched.
    assert!(!fixture.exists("/dir/renamed-away.txt"));
    assert_eq!(fixture.text("/dir/keep.txt").await, "kept");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        fixture.workspace.read_with(cx, |workspace, cx| workspace
            .active_modal::<RecoverModal>(cx)
            .is_none()),
        "Escape closes the modal"
    );
}

#[gpui::test]
async fn test_recover_filters_the_list_as_you_type(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({}), true, cx).await;
    fixture.seed("src/alpha.php", "a", 2);
    fixture.seed("src/beta.php", "b", 1);
    let modal = open_modal(&fixture, cx);
    assert_eq!(listed_paths(&modal, cx), ["src/alpha.php", "src/beta.php"]);
    cx.simulate_input("bet");
    cx.run_until_parked();
    assert_eq!(listed_paths(&modal, cx), ["src/beta.php"]);
}

#[gpui::test]
async fn test_recover_says_so_when_there_is_nothing_to_recover(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "a.txt": "a" }), true, cx).await;
    let modal = open_modal(&fixture, cx);
    assert!(listed_paths(&modal, cx).is_empty());
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(status(&modal, cx), "There is no file to restore.");
}

#[gpui::test]
async fn test_restore_creates_missing_parent_folders_inside_the_folder(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "a.txt": "a" }), true, cx).await;
    let revision = fixture.seed("deep/er/c.txt", "content", 1);
    assert_eq!(try_restore(&fixture, &revision, cx).await, Ok(()));
    assert_eq!(fixture.text("/dir/deep/er/c.txt").await, "content");
}

#[gpui::test]
async fn test_restore_never_overwrites_a_file_that_came_back(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({}), true, cx).await;
    fixture.seed("a.txt", "history", 1);
    let modal = open_modal(&fixture, cx);
    assert_eq!(listed_paths(&modal, cx), ["a.txt"]);
    // Recreated after the list was made.
    fixture
        .fs
        .write(Path::new("/dir/a.txt"), b"new work")
        .await
        .expect("write");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(fixture.text("/dir/a.txt").await, "new work");
    assert_eq!(
        status(&modal, cx),
        format!("a.txt was not restored. {}", Refusal::Exists)
    );
}

#[gpui::test]
async fn test_restore_refuses_unsafe_recorded_paths(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "a.txt": "a" }), true, cx).await;
    let base = fixture.seed("ok.txt", "payload", 1);
    let before = fixture.fs.paths(true);
    for bad in [
        "",
        "../outside.txt",
        "a/../../outside.txt",
        "a/../b.txt",
        "/etc/outside.txt",
        "./b.txt",
        "a//b.txt",
        "dir/",
        "a\\b.txt",
        "nul\0.txt",
        ".git/hooks/pre-commit",
        "sub/.GIT/config",
    ] {
        assert_eq!(
            try_restore(&fixture, &with_path(&base, bad), cx).await,
            Err(Refusal::BadPath),
            "{bad:?}"
        );
    }
    assert_eq!(fixture.fs.paths(true), before, "nothing was created");
    assert!(!fixture.exists("/outside.txt"));
}

#[test]
fn test_validate_rel_accepts_plain_relative_paths_only() {
    for good in ["a.txt", "a/b/c.php", "dir/.hidden", "a b/c d.txt"] {
        assert!(validate_rel(good).is_ok(), "{good:?}");
    }
    for bad in ["", "..", ".", "/a", "a/..", "a/./b", "a/", ".git", "x\\y"] {
        assert_eq!(validate_rel(bad), Err(Refusal::BadPath), "{bad:?}");
    }
}

#[gpui::test]
async fn test_restore_refuses_secret_named_files(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({}), true, cx).await;
    let base = fixture.seed("ok.txt", "SECRET=1", 1);
    // The store never records these, so a row for one means the database was edited.
    for secret in [
        ".env",
        ".env.local",
        "sub/.ENV",
        "wp-config.php",
        "x/WP-Config.php",
    ] {
        assert_eq!(
            try_restore(&fixture, &with_path(&base, secret), cx).await,
            Err(Refusal::Protected),
            "{secret}"
        );
        assert!(!fixture.exists(&format!("/dir/{secret}")));
    }
}

#[gpui::test]
async fn test_restore_refuses_a_path_that_is_private_or_excluded_now(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "a.txt": "a" }), true, cx).await;
    let private = fixture.seed("keys/server.pem", "k", 1);
    let excluded = fixture.seed("build/out.js", "o", 2);
    let fine = fixture.seed("fine.txt", "f", 3);
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.worktree.private_files = Some(vec!["**/*.pem".to_string()].into());
                content.project.worktree.file_scan_exclusions =
                    Some(vec!["**/build".to_string()].into());
            });
        });
    });
    cx.run_until_parked();
    assert_eq!(
        try_restore(&fixture, &private, cx).await,
        Err(Refusal::Protected)
    );
    assert_eq!(
        try_restore(&fixture, &excluded, cx).await,
        Err(Refusal::Protected)
    );
    assert!(!fixture.exists("/dir/keys/server.pem"));
    assert!(!fixture.exists("/dir/build/out.js"));
    // And the list does not offer them.
    let modal = open_modal(&fixture, cx);
    assert_eq!(listed_paths(&modal, cx), ["fine.txt"]);
    // The control: an ordinary path still restores.
    assert_eq!(try_restore(&fixture, &fine, cx).await, Ok(()));
}

#[gpui::test]
async fn test_restore_refuses_a_path_that_is_gitignored_now(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(
        json!({ ".gitignore": "out/\n", "out": { "keep.txt": "k" } }),
        true,
        cx,
    )
    .await;
    let revision = fixture.seed("out/gone.txt", "g", 1);
    cx.run_until_parked();
    assert_eq!(
        try_restore(&fixture, &revision, cx).await,
        Err(Refusal::Protected)
    );
    assert!(!fixture.exists("/dir/out/gone.txt"));
    let modal = open_modal(&fixture, cx);
    assert!(listed_paths(&modal, cx).is_empty());
}

#[gpui::test]
async fn test_restore_refuses_to_write_through_a_symlink(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "real": { "x.txt": "x" } }), true, cx).await;
    fixture
        .fs
        .insert_tree("/outside", json!({ "keep.txt": "k" }))
        .await;
    fixture
        .fs
        .create_symlink(Path::new("/dir/escape"), PathBuf::from("/outside"))
        .await
        .expect("symlink");
    fixture
        .fs
        .create_symlink(Path::new("/dir/inner"), PathBuf::from("/dir/real"))
        .await
        .expect("symlink");
    cx.run_until_parked();
    let base = fixture.seed("ok.txt", "payload", 1);
    for rel in ["escape/new.txt", "escape/deep/new.txt", "inner/new.txt"] {
        assert_eq!(
            try_restore(&fixture, &with_path(&base, rel), cx).await,
            Err(Refusal::Symlink),
            "{rel}"
        );
    }
    assert!(!fixture.exists("/outside/new.txt"));
    assert!(!fixture.exists("/outside/deep"));
    assert!(!fixture.exists("/dir/real/new.txt"));
}

#[gpui::test]
async fn test_write_new_never_replaces_anything_at_the_target(cx: &mut TestAppContext) {
    let (fixture, _cx) = fixture(json!({ "file.txt": "mine", "folder": {} }), true, cx).await;
    fixture
        .fs
        .create_symlink(Path::new("/dir/link.txt"), PathBuf::from("/dir/file.txt"))
        .await
        .expect("symlink");
    for (rel, expected) in [
        ("file.txt", Refusal::Exists),
        ("folder", Refusal::Exists),
        ("link.txt", Refusal::Symlink),
    ] {
        let rel: Arc<RelPath> = RelPath::from_unix_str(rel).expect("rel").into();
        let result = write_new(
            fixture.fs.as_ref(),
            PathBuf::from("/dir"),
            rel,
            b"history".to_vec(),
        )
        .await;
        assert_eq!(result, Err(expected));
    }
    assert_eq!(fixture.text("/dir/file.txt").await, "mine");
    // A file in the way of a folder stops the walk before anything is created.
    let nested: Arc<RelPath> = RelPath::from_unix_str("file.txt/inner.txt")
        .expect("rel")
        .into();
    let result = write_new(
        fixture.fs.as_ref(),
        PathBuf::from("/dir"),
        nested,
        b"history".to_vec(),
    )
    .await;
    assert!(matches!(result, Err(Refusal::Io(_))), "{result:?}");
    assert_eq!(fixture.text("/dir/file.txt").await, "mine");
}

#[gpui::test]
async fn test_restore_refuses_content_over_the_size_limit_or_already_pruned(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(json!({}), true, cx).await;
    let big = fixture.seed("big.txt", &"x".repeat(2048), 1);
    let pruned = Revision {
        blob_hash: "0".repeat(64),
        ..fixture.seed("pruned.txt", "p", 2)
    };
    configure(cx, |history| history.max_file_size_kb = Some(1));
    assert_eq!(
        try_restore(&fixture, &big, cx).await,
        Err(Refusal::TooLarge)
    );
    assert_eq!(
        try_restore(&fixture, &pruned, cx).await,
        Err(Refusal::Pruned)
    );
    assert!(!fixture.exists("/dir/big.txt"));
    assert!(!fixture.exists("/dir/pruned.txt"));
}

#[gpui::test]
async fn test_restore_refuses_an_untrusted_folder(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({ "a.txt": "a" }), false, cx).await;
    let revision = fixture.seed("gone.txt", "g", 1);
    assert_eq!(
        try_restore(&fixture, &revision, cx).await,
        Err(Refusal::Untrusted)
    );
    assert!(!fixture.exists("/dir/gone.txt"));
    // The modal lists nothing and says why; Enter keeps saying it.
    let modal = open_modal(&fixture, cx);
    assert!(listed_paths(&modal, cx).is_empty());
    assert_eq!(status(&modal, cx), Refusal::Untrusted.to_string());
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(status(&modal, cx), Refusal::Untrusted.to_string());
    assert!(!fixture.exists("/dir/gone.txt"));
}

#[gpui::test]
async fn test_restore_refuses_a_revision_of_another_folder(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(json!({}), true, cx).await;
    let revision = Revision {
        worktree_root: "/elsewhere".to_string(),
        ..fixture.seed("a.txt", "a", 1)
    };
    assert_eq!(
        try_restore(&fixture, &revision, cx).await,
        Err(Refusal::BadPath)
    );
    assert!(!fixture.exists("/dir/a.txt"));
}
