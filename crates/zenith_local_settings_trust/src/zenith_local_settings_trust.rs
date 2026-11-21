//! Keeps project-local settings, tasks, debug configs, `.vscode` and `.editorconfig` files that
//! live inside library folders from ever being loaded. Upstream worktree trust is all-or-nothing,
//! so `vendor/.zed/settings.json` inside a trusted project would otherwise still apply.

use gpui::{App, Entity, Global};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::rel_path::RelPath;
use worktree::{PathChange, ProjectEntryId, UpdatedEntriesSet, Worktree};

/// Folder names that hold third-party code, at any depth.
pub const LIBRARY_FOLDERS: &[&str] = &["vendor", "node_modules", "wp-includes", "wp-admin"];

/// User-added library roots from `zenith.external_libraries.paths` (user settings only).
#[derive(Clone, Debug, Default, PartialEq, Eq, RegisterSetting)]
struct LibraryRootSettings {
    paths: Vec<PathBuf>,
}

impl Settings for LibraryRootSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let paths = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.external_libraries.as_ref())
            .and_then(|libraries| libraries.paths.as_ref());
        Self {
            paths: paths
                .map(|paths| paths.iter().map(PathBuf::from).collect())
                .unwrap_or_default(),
        }
    }
}

/// Roots added while the app runs, for example from a "mark as library" action.
#[derive(Default)]
struct SessionLibraryRoots(HashSet<Arc<Path>>);

impl Global for SessionLibraryRoots {}

/// Whether any component of `path` is a library folder. Manifest-agnostic on purpose: a
/// `vendor/` folder without a `composer.json` is still treated as a library.
pub fn is_library_path(path: &RelPath) -> bool {
    path.components()
        .any(|component| LIBRARY_FOLDERS.contains(&component))
}

fn library_roots(cx: &App) -> Vec<PathBuf> {
    let mut roots = LibraryRootSettings::try_get(cx)
        .map(|settings| settings.paths.clone())
        .unwrap_or_default();
    if let Some(session) = cx.try_global::<SessionLibraryRoots>() {
        roots.extend(session.0.iter().map(|root| root.to_path_buf()));
    }
    roots
}

/// Whether `abs` is a user-added library root or inside one.
pub fn is_library_root(abs: &Path, cx: &App) -> bool {
    library_roots(cx).iter().any(|root| abs.starts_with(root))
}

/// Marks `abs` as a library root until the app quits.
pub fn add_library_root(abs: Arc<Path>, cx: &mut App) {
    cx.default_global::<SessionLibraryRoots>().0.insert(abs);
}

/// Drops the changes whose settings files must not be loaded: everything in a worktree that is a
/// library root, entries under a library root nested in the worktree, and entries under a library
/// folder. Decided up front so the iterator does not borrow `cx`.
pub fn gate<'a>(
    wt: &Entity<Worktree>,
    changes: &'a UpdatedEntriesSet,
    cx: &App,
) -> impl Iterator<Item = &'a (Arc<RelPath>, ProjectEntryId, PathChange)> + use<'a> {
    let worktree_root = wt.read(cx).abs_path();
    let roots = library_roots(cx);
    let all_library = roots.iter().any(|root| worktree_root.starts_with(root));
    let nested: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| root.strip_prefix(&worktree_root).ok())
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    changes.iter().filter(move |(path, _, _)| {
        !all_library
            && !is_library_path(path)
            && !nested
                .iter()
                .any(|root| path.as_std_path().starts_with(root))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::atomic::AtomicUsize;
    use util::rel_path::rel_path;
    use worktree::WorktreeId;

    #[test]
    fn library_paths_match_at_any_depth() {
        for library in [
            "vendor/acme/.zed/settings.json",
            "vendor",
            "packages/app/node_modules/x/.zed/tasks.json",
            "wp-admin/.vscode/tasks.json",
            "a/wp-includes/.editorconfig",
        ] {
            assert!(is_library_path(rel_path(library)), "{library}");
        }
        for project in [
            ".zed/settings.json",
            "src/vendor_x/.zed/settings.json",
            "my-vendor/.editorconfig",
            "node_modules_backup/x",
            "",
        ] {
            assert!(!is_library_path(rel_path(project)), "{project}");
        }
    }

    fn init_settings(cx: &mut TestAppContext, user_paths: &[&str]) {
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store.update_user_settings(cx, |content| {
                content
                    .zenith
                    .get_or_insert_default()
                    .external_libraries
                    .get_or_insert_default()
                    .paths = Some(user_paths.iter().map(|path| path.to_string()).collect());
            });
            cx.set_global(store);
        });
    }

    #[gpui::test]
    fn roots_come_from_settings_and_session(cx: &mut TestAppContext) {
        init_settings(cx, &["/lib/acme"]);
        cx.update(|cx| {
            assert!(is_library_root(Path::new("/lib/acme"), cx));
            assert!(is_library_root(Path::new("/lib/acme/sub"), cx));
            assert!(!is_library_root(Path::new("/lib/acme-two"), cx));
            assert!(!is_library_root(Path::new("/work"), cx));
            add_library_root(Path::new("/work/extra").into(), cx);
            assert!(is_library_root(Path::new("/work/extra/x"), cx));
        });
    }

    async fn worktree_at(
        root: &str,
        fs: &Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> Entity<Worktree> {
        Worktree::local(
            Path::new(root),
            true,
            fs.clone(),
            Arc::new(AtomicUsize::new(0)),
            true,
            WorktreeId::from_usize(1),
            &mut cx.to_async(),
        )
        .await
        .expect("worktree")
    }

    fn change(path: &str, id: usize) -> (Arc<RelPath>, ProjectEntryId, PathChange) {
        (
            rel_path(path).into(),
            ProjectEntryId::from_usize(id),
            PathChange::Added,
        )
    }

    fn gated(wt: &Entity<Worktree>, cx: &mut TestAppContext) -> Vec<String> {
        let changes: UpdatedEntriesSet = Arc::from(vec![
            change(".zed/settings.json", 1),
            change("vendor/acme/.zed/settings.json", 2),
            change("node_modules/x/.zed/tasks.json", 3),
            change("src/vendor_x/.editorconfig", 4),
            change("tools/.zed/settings.json", 5),
        ]);
        cx.update(|cx| {
            gate(wt, &changes, cx)
                .map(|(path, _, _)| path.as_unix_str().to_string())
                .collect()
        })
    }

    #[gpui::test]
    async fn gate_drops_library_entries(cx: &mut TestAppContext) {
        init_settings(cx, &[]);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({})).await;
        let wt = worktree_at("/root", &fs, cx).await;
        assert_eq!(
            gated(&wt, cx),
            [
                ".zed/settings.json",
                "src/vendor_x/.editorconfig",
                "tools/.zed/settings.json"
            ]
        );
    }

    #[gpui::test]
    async fn gate_drops_library_root_worktree_and_nested_roots(cx: &mut TestAppContext) {
        init_settings(cx, &["/other/lib", "/root/tools"]);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({})).await;
        fs.insert_tree("/other/lib", json!({})).await;
        let wt = worktree_at("/root", &fs, cx).await;
        assert_eq!(
            gated(&wt, cx),
            [".zed/settings.json", "src/vendor_x/.editorconfig"]
        );

        let lib = worktree_at("/other/lib", &fs, cx).await;
        assert!(gated(&lib, cx).is_empty());
    }
}
