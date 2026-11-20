use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use fuzzy::CharBag;
use git::status::GitSummary;
use gpui::SharedString;
use icons::IconName;
use project::GitEntry;
use util::rel_path::RelPath;
use worktree::{Entry, EntryKind, ProjectEntryId, Snapshot as WorktreeSnapshot, WorktreeId};

/// Special synthetic project entry ID for the "External Libraries" root node.
pub fn external_libraries_root_id() -> ProjectEntryId {
    ProjectEntryId::from_proto(u64::MAX - 200)
}

static EXTERNAL_LIBRARIES_EXPANDED: AtomicBool = AtomicBool::new(false);

/// Returns whether the External Libraries root node is currently expanded.
pub fn is_expanded() -> bool {
    EXTERNAL_LIBRARIES_EXPANDED.load(Ordering::Relaxed)
}

/// Sets whether the External Libraries root node is currently expanded.
pub fn set_expanded(expanded: bool) {
    EXTERNAL_LIBRARIES_EXPANDED.store(expanded, Ordering::Relaxed);
}

/// Toggles expansion if `entry_id` matches the External Libraries root node.
/// Returns `true` if handled.
pub fn toggle_expanded(entry_id: ProjectEntryId) -> bool {
    if entry_id == external_libraries_root_id() {
        let current = is_expanded();
        set_expanded(!current);
        true
    } else {
        false
    }
}

/// Information about a detected external library root directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedLibrary {
    pub worktree_id: WorktreeId,
    pub path: Arc<RelPath>,
    pub label: SharedString,
    pub icon: IconName,
    pub entry_id: ProjectEntryId,
}

#[inline]
fn rel(s: &str) -> &RelPath {
    RelPath::from_unix_str(s).expect("valid rel path")
}

/// Detects all external libraries in a worktree snapshot (Composer, npm, WordPress).
pub fn detect_libraries(snapshot: &WorktreeSnapshot) -> Vec<DetectedLibrary> {
    let mut libraries = Vec::new();
    let worktree_id = snapshot.id();

    // 1. PHP / Composer dependencies (vendor/)
    let has_composer = snapshot.entry_for_path(rel("composer.json")).is_some()
        || snapshot.entry_for_path(rel("composer.lock")).is_some();
    if has_composer {
        if let Some(vendor_entry) = snapshot.entry_for_path(rel("vendor")) {
            if vendor_entry.is_dir() {
                libraries.push(DetectedLibrary {
                    worktree_id,
                    path: rel("vendor").into(),
                    label: SharedString::from("Composer Packages"),
                    icon: IconName::Box,
                    entry_id: vendor_entry.id,
                });
            }
        }
    }

    // 2. JavaScript / Node dependencies (node_modules/)
    let has_package_json = snapshot.entry_for_path(rel("package.json")).is_some()
        || snapshot.entry_for_path(rel("package-lock.json")).is_some()
        || snapshot.entry_for_path(rel("yarn.lock")).is_some()
        || snapshot.entry_for_path(rel("pnpm-lock.yaml")).is_some()
        || snapshot.entry_for_path(rel("bun.lockb")).is_some();
    if has_package_json {
        if let Some(nm_entry) = snapshot.entry_for_path(rel("node_modules")) {
            if nm_entry.is_dir() {
                libraries.push(DetectedLibrary {
                    worktree_id,
                    path: rel("node_modules").into(),
                    label: SharedString::from("node_modules"),
                    icon: IconName::Box,
                    entry_id: nm_entry.id,
                });
            }
        }
    }

    // 3. WordPress Core (wp-includes/, wp-admin/)
    let has_wp = snapshot.entry_for_path(rel("wp-load.php")).is_some()
        || snapshot.entry_for_path(rel("wp-config.php")).is_some()
        || snapshot.entry_for_path(rel("wp-settings.php")).is_some();
    if has_wp {
        if let Some(wp_inc) = snapshot.entry_for_path(rel("wp-includes")) {
            if wp_inc.is_dir() {
                libraries.push(DetectedLibrary {
                    worktree_id,
                    path: rel("wp-includes").into(),
                    label: SharedString::from("WordPress Core (wp-includes)"),
                    icon: IconName::BookCopy,
                    entry_id: wp_inc.id,
                });
            }
        }
        if let Some(wp_admin) = snapshot.entry_for_path(rel("wp-admin")) {
            if wp_admin.is_dir() {
                libraries.push(DetectedLibrary {
                    worktree_id,
                    path: rel("wp-admin").into(),
                    label: SharedString::from("WordPress Core (wp-admin)"),
                    icon: IconName::BookCopy,
                    entry_id: wp_admin.id,
                });
            }
        }
    }

    libraries
}

/// Checks whether an entry path should be filtered from the main worktree project tree
/// because it is an external library.
pub fn is_library_path(path: &RelPath) -> bool {
    let unix = path.as_unix_str();
    unix == "vendor"
        || unix.starts_with("vendor/")
        || unix == "node_modules"
        || unix.starts_with("node_modules/")
        || unix == "wp-includes"
        || unix.starts_with("wp-includes/")
        || unix == "wp-admin"
        || unix.starts_with("wp-admin/")
}

/// Checks whether an entry in a worktree is part of external libraries.
pub fn is_external_library_entry(entry_id: ProjectEntryId, path: &RelPath) -> bool {
    entry_id == external_libraries_root_id() || is_library_path(path)
}

/// Customizes filename, icon, and chevron for external library entries.
pub fn customize_entry(
    entry_id: ProjectEntryId,
    path: &RelPath,
    filename: &mut String,
    icon: &mut Option<SharedString>,
    chevron: &mut Option<SharedString>,
) {
    if entry_id == external_libraries_root_id() {
        *filename = "External Libraries".to_string();
        *icon = Some(SharedString::from(IconName::BookCopy.path()));
        *chevron = Some(SharedString::from(if is_expanded() {
            IconName::ChevronDown.path()
        } else {
            IconName::ChevronRight.path()
        }));
    } else {
        match path.as_unix_str() {
            "vendor" => {
                *filename = "Composer Packages".to_string();
                *icon = Some(SharedString::from(IconName::Box.path()));
            }
            "node_modules" => {
                *filename = "node_modules".to_string();
                *icon = Some(SharedString::from(IconName::Box.path()));
            }
            "wp-includes" => {
                *filename = "WordPress Core (wp-includes)".to_string();
                *icon = Some(SharedString::from(IconName::BookCopy.path()));
            }
            "wp-admin" => {
                *filename = "WordPress Core (wp-admin)".to_string();
                *icon = Some(SharedString::from(IconName::BookCopy.path()));
            }
            _ => {}
        }
    }
}

/// Creates a synthetic GitEntry for the "External Libraries" root header row.
pub fn create_external_libraries_root_entry() -> GitEntry {
    GitEntry {
        entry: Entry {
            id: external_libraries_root_id(),
            kind: EntryKind::Dir,
            path: RelPath::from_unix_str("External Libraries").unwrap().into(),
            inode: 0,
            mtime: None,
            canonical_path: None,
            is_ignored: false,
            is_hidden: false,
            is_always_included: true,
            is_external: true,
            is_private: false,
            size: 0,
            char_bag: CharBag::default(),
            is_fifo: false,
        },
        git_summary: GitSummary::default(),
    }
}

/// Appends the External Libraries section to visible worktree entries.
/// Returns the number of entries added.
pub fn append_external_libraries_to_worktree(
    snapshot: &WorktreeSnapshot,
    expanded_dir_ids: &[ProjectEntryId],
    target: &mut Vec<GitEntry>,
) -> usize {
    let libraries = detect_libraries(snapshot);
    if libraries.is_empty() {
        return 0;
    }

    let start_len = target.len();

    // Add root node
    target.push(create_external_libraries_root_entry());

    // If root node is expanded, add library subtrees
    if is_expanded() {
        for lib in libraries {
            if let Some(entry) = snapshot.entry_for_id(lib.entry_id) {
                target.push(GitEntry {
                    entry: entry.clone(),
                    git_summary: GitSummary::default(),
                });

                // If this library directory is expanded, recursively add visible children
                if expanded_dir_ids.binary_search(&lib.entry_id).is_ok() {
                    append_children_recursive(snapshot, &lib.path, expanded_dir_ids, target);
                }
            }
        }
    }

    target.len() - start_len
}

fn append_children_recursive(
    snapshot: &WorktreeSnapshot,
    parent_path: &RelPath,
    expanded_dir_ids: &[ProjectEntryId],
    target: &mut Vec<GitEntry>,
) {
    for child in snapshot.child_entries(parent_path) {
        target.push(GitEntry {
            entry: child.clone(),
            git_summary: GitSummary::default(),
        });

        if child.is_dir() && expanded_dir_ids.binary_search(&child.id).is_ok() {
            append_children_recursive(snapshot, &child.path, expanded_dir_ids, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toggle_expanded() {
        set_expanded(false);
        assert!(!is_expanded());

        assert!(toggle_expanded(external_libraries_root_id()));
        assert!(is_expanded());

        assert!(toggle_expanded(external_libraries_root_id()));
        assert!(!is_expanded());

        // Arbitrary entry ID should return false
        assert!(!toggle_expanded(ProjectEntryId::from_proto(123)));
        assert!(!is_expanded());
    }

    #[test]
    fn test_is_library_path() {
        assert!(is_library_path(rel("vendor")));
        assert!(is_library_path(rel("vendor/autoload.php")));
        assert!(is_library_path(rel("vendor/laravel/framework")));
        assert!(is_library_path(rel("node_modules")));
        assert!(is_library_path(rel("node_modules/axios")));
        assert!(is_library_path(rel("wp-includes")));
        assert!(is_library_path(rel("wp-includes/plugin.php")));
        assert!(is_library_path(rel("wp-admin")));
        assert!(is_library_path(rel("wp-admin/index.php")));

        assert!(!is_library_path(rel("src/vendor_utils.rs")));
        assert!(!is_library_path(rel("app/Http/Controllers")));
        assert!(!is_library_path(rel("composer.json")));
        assert!(!is_library_path(rel("package.json")));
    }

    #[test]
    fn test_create_external_libraries_root_entry() {
        let root = create_external_libraries_root_entry();
        assert_eq!(root.id, external_libraries_root_id());
        assert!(root.is_dir());
        assert!(root.is_external);
        assert_eq!(root.path.as_unix_str(), "External Libraries");
    }

    #[test]
    fn test_customize_entry() {
        let mut filename = String::new();
        let mut icon = None;
        let mut chevron = None;

        set_expanded(false);
        customize_entry(
            external_libraries_root_id(),
            rel("External Libraries"),
            &mut filename,
            &mut icon,
            &mut chevron,
        );
        assert_eq!(filename, "External Libraries");
        assert_eq!(icon, Some(SharedString::from(IconName::BookCopy.path())));
        assert_eq!(chevron, Some(SharedString::from(IconName::ChevronRight.path())));

        set_expanded(true);
        customize_entry(
            external_libraries_root_id(),
            rel("External Libraries"),
            &mut filename,
            &mut icon,
            &mut chevron,
        );
        assert_eq!(chevron, Some(SharedString::from(IconName::ChevronDown.path())));

        // Composer vendor customization
        let other_id = ProjectEntryId::from_proto(42);
        customize_entry(
            other_id,
            rel("vendor"),
            &mut filename,
            &mut icon,
            &mut chevron,
        );
        assert_eq!(filename, "Composer Packages");
        assert_eq!(icon, Some(SharedString::from(IconName::Box.path())));

        // npm node_modules customization
        customize_entry(
            other_id,
            rel("node_modules"),
            &mut filename,
            &mut icon,
            &mut chevron,
        );
        assert_eq!(filename, "node_modules");
        assert_eq!(icon, Some(SharedString::from(IconName::Box.path())));

        // WordPress Core customization
        customize_entry(
            other_id,
            rel("wp-includes"),
            &mut filename,
            &mut icon,
            &mut chevron,
        );
        assert_eq!(filename, "WordPress Core (wp-includes)");
        assert_eq!(icon, Some(SharedString::from(IconName::BookCopy.path())));
    }

    #[test]
    fn test_is_external_library_entry() {
        assert!(is_external_library_entry(
            external_libraries_root_id(),
            rel("External Libraries")
        ));
        assert!(is_external_library_entry(
            ProjectEntryId::from_proto(1),
            rel("vendor")
        ));
        assert!(is_external_library_entry(
            ProjectEntryId::from_proto(2),
            rel("vendor/autoload.php")
        ));
        assert!(is_external_library_entry(
            ProjectEntryId::from_proto(3),
            rel("node_modules/vue")
        ));
        assert!(!is_external_library_entry(
            ProjectEntryId::from_proto(4),
            rel("src/main.rs")
        ));
    }

    #[gpui::test]
    async fn test_library_detection_with_worktree(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/project",
            json!({
                "composer.json": "{}",
                "vendor": {
                    "autoload.php": "",
                    "laravel": {
                        "framework": {
                            "readme.md": ""
                        }
                    }
                },
                "package.json": "{}",
                "node_modules": {
                    "axios": {
                        "index.js": ""
                    }
                },
                "src": {
                    "main.php": ""
                }
            }),
        )
        .await;

        let tree = worktree::Worktree::local(
            Path::new("/project"),
            true,
            fs,
            Default::default(),
            true,
            WorktreeId::from_proto(1),
            &mut cx.to_async(),
        )
        .await
        .unwrap();

        cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete()).await;

        let snapshot = tree.read_with(cx, |tree, _| tree.snapshot());
        let libraries = detect_libraries(&snapshot);

        // Verify both Composer and npm were detected
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].label, "Composer Packages");
        assert_eq!(libraries[0].path.as_unix_str(), "vendor");
        assert_eq!(libraries[1].label, "node_modules");
        assert_eq!(libraries[1].path.as_unix_str(), "node_modules");

        // Test appending to visible entries when collapsed
        set_expanded(false);
        let mut visible = Vec::new();
        let added = append_external_libraries_to_worktree(&snapshot, &[], &mut visible);
        assert_eq!(added, 1);
        assert_eq!(visible[0].id, external_libraries_root_id());

        // Test appending when expanded
        set_expanded(true);
        let mut visible = Vec::new();
        let added = append_external_libraries_to_worktree(&snapshot, &[], &mut visible);
        // Root + vendor + node_modules = 3 entries
        assert_eq!(added, 3);
        assert_eq!(visible[0].id, external_libraries_root_id());
        assert_eq!(visible[1].entry.path.as_unix_str(), "vendor");
        assert_eq!(visible[2].entry.path.as_unix_str(), "node_modules");
    }
}
