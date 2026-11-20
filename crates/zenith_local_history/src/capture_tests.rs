use super::{capture_external, read_bounded, read_text};
use crate::tests::{history_dir, open_and_close, project_with, revisions, settle, sources, start};
use fs::{FakeFs, Fs as _};
use gpui::TestAppContext;
use serde_json::json;
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::rel_path::RelPath;
use worktree::PathChange;

async fn fake_fs(files: serde_json::Value, cx: &TestAppContext) -> Arc<FakeFs> {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", files).await;
    fs
}

async fn read(fs: &FakeFs, path: &str, max_bytes: u64) -> Option<Vec<u8>> {
    read_text(fs, Path::new("/dir"), Path::new(path), max_bytes).await
}

#[gpui::test]
async fn test_read_text_returns_the_text_a_buffer_would_hold(cx: &mut TestAppContext) {
    let fs = fake_fs(
        json!({ "plain.txt": "a\nb", "mixed.txt": "\u{feff}a\r\nb\rc" }),
        cx,
    )
    .await;
    assert_eq!(
        read(&fs, "/dir/plain.txt", 100).await.as_deref(),
        Some(b"a\nb".as_slice())
    );
    // BOM dropped, CRLF and a lone CR both become LF.
    assert_eq!(
        read(&fs, "/dir/mixed.txt", 100).await.as_deref(),
        Some(b"a\nb\nc".as_slice())
    );
}

#[gpui::test]
async fn test_read_text_refuses_what_is_not_a_plain_file_in_the_folder(cx: &mut TestAppContext) {
    let fs = fake_fs(
        json!({ "real.txt": "data", "sub": {}, "bin.dat": "a\0b" }),
        cx,
    )
    .await;
    fs.insert_tree("/outside", json!({ "secret.txt": "SECRET" }))
        .await;
    fs.create_symlink(Path::new("/dir/link.txt"), PathBuf::from("/dir/real.txt"))
        .await
        .expect("symlink");
    fs.create_symlink(
        Path::new("/dir/out.txt"),
        PathBuf::from("/outside/secret.txt"),
    )
    .await
    .expect("symlink");
    fs.create_symlink(Path::new("/dir/escape"), PathBuf::from("/outside"))
        .await
        .expect("symlink");
    // The control: the same file read directly is fine.
    assert!(read(&fs, "/dir/real.txt", 100).await.is_some());
    for refused in [
        "/dir/link.txt",
        "/dir/out.txt",
        "/dir/sub",
        "/dir/bin.dat",
        "/dir/missing.txt",
    ] {
        assert_eq!(read(&fs, refused, 100).await, None, "{refused}");
    }
    // The file itself is no link, but its folder leads outside.
    assert_eq!(read(&fs, "/dir/escape/secret.txt", 100).await, None);
    // Too large, by the file's size.
    assert_eq!(read(&fs, "/dir/real.txt", 3).await, None);
}

#[test]
fn test_read_bounded_never_reads_past_the_limit() {
    // An endless source: unbounded `read_to_end` would never come back.
    assert_eq!(read_bounded(std::io::repeat(b'a'), 10), None);
    assert_eq!(read_bounded(Cursor::new(vec![b'a'; 11]), 10), None);
    assert_eq!(
        read_bounded(Cursor::new(vec![b'a'; 10]), 10),
        Some(vec![b'a'; 10])
    );
}

#[gpui::test]
async fn test_the_cap_counts_changed_files_not_every_entry(cx: &mut TestAppContext) {
    let dir = history_dir();
    start(cx, dir.path());
    let project = project_with(json!({ "a.txt": "base" }), cx).await;
    open_and_close(&project, "/dir/a.txt", cx).await;
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    // The change is reported by hand below, so keep the real report away.
    fs.as_fake().pause_events();
    fs.write(Path::new("/dir/a.txt"), b"changed")
        .await
        .expect("write");
    // More entries than the cap that are not changed files (no such entry,
    // or just `Loaded`), then the one file that is.
    let id = project::ProjectEntryId::from_proto(0);
    let mut changes: Vec<_> = (0..600)
        .map(|ix| {
            let path: Arc<RelPath> = RelPath::from_unix_str(&format!("none{ix}"))
                .expect("rel")
                .into();
            (path, id, PathChange::Updated)
        })
        .collect();
    let a: Arc<RelPath> = RelPath::from_unix_str("a.txt").expect("rel").into();
    changes.push((a, id, PathChange::Updated));
    project.update(cx, |project, cx| {
        let worktree_id = project
            .worktrees(cx)
            .next()
            .expect("worktree")
            .read(cx)
            .id();
        capture_external(project, worktree_id, &changes, cx);
    });
    settle(cx);
    assert_eq!(
        sources(&revisions(cx, dir.path(), "a.txt")),
        ["external", "open"]
    );
    fs.as_fake().unpause_events_and_flush();
}

#[cfg(unix)]
mod unix {
    use super::super::open_unix;
    use std::{io::Read as _, os::unix::fs::MetadataExt as _, path::Path};

    fn inode(path: &Path) -> u64 {
        std::fs::symlink_metadata(path).expect("stat").ino()
    }

    #[test]
    fn test_open_unix_opens_only_the_plain_file_that_was_checked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("a.txt");
        let other = dir.path().join("b.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&file, "hello").expect("write");
        std::fs::write(&other, "other").expect("write");
        std::os::unix::fs::symlink(&other, &link).expect("symlink");

        let mut text = String::new();
        open_unix(&file, inode(&file), 100)
            .expect("the control opens")
            .read_to_string(&mut text)
            .expect("read");
        assert_eq!(text, "hello");
        // A link at the name, even one the caller thought was the file (same inode as
        // its target): `O_NOFOLLOW`.
        assert!(open_unix(&link, inode(&other), 100).is_none());
        // Not the file the earlier check saw.
        assert!(open_unix(&file, inode(&other), 100).is_none());
        // Over the cap, by the handle's own size.
        assert!(open_unix(&file, inode(&file), 3).is_none());
        // A folder is not a plain file.
        assert!(open_unix(dir.path(), inode(dir.path()), u64::MAX).is_none());
    }
}
