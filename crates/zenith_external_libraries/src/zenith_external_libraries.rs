use fuzzy::CharBag;
use git::status::GitSummary;
use gpui::{App, SharedString};
use icons::IconName;
use project::{GitEntry, Project, ProjectPath};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::sync::Arc;
use util::rel_path::RelPath;
use worktree::{Entry, EntryKind, ProjectEntryId, Snapshot as WorktreeSnapshot, WorktreeId};

/// Whether library folders are moved under the External Libraries node
/// (`zenith.external_libraries.enabled`, default true).
#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct ExternalLibrariesSettings {
    pub enabled: bool,
}

impl Settings for ExternalLibrariesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.external_libraries.as_ref())
                .and_then(|settings| settings.enabled)
                .unwrap_or(true),
        }
    }
}

/// Whether the feature is on. Defaults to on when the setting store is not available.
pub fn is_enabled(cx: &App) -> bool {
    ExternalLibrariesSettings::try_get(cx).is_none_or(|settings| settings.enabled)
}

/// Synthetic entry ids live far above any id the worktree scanner hands out.
const SECTION_ID_BASE: u64 = 1 << 62;
const SECTION_ID_SPAN: u64 = 1 << 32;

/// The id of the "External Libraries" row when `worktree_id` owns it. The panel shows one node,
/// owned by the first worktree with libraries, so its expansion lives in that worktree's
/// expanded ids and is never shared between windows.
pub fn section_id(worktree_id: WorktreeId) -> ProjectEntryId {
    let offset = (worktree_id.to_usize() as u64) % SECTION_ID_SPAN;
    ProjectEntryId::from_proto(SECTION_ID_BASE + offset)
}

/// The worktree an "External Libraries" row id belongs to, or `None` for a real entry id.
pub fn section_worktree(entry_id: ProjectEntryId) -> Option<WorktreeId> {
    let id = entry_id.to_proto();
    (SECTION_ID_BASE..SECTION_ID_BASE + SECTION_ID_SPAN)
        .contains(&id)
        .then(|| WorktreeId::from_proto(id - SECTION_ID_BASE))
}

/// Whether the External Libraries row of `worktree_id` is expanded, given the panel's
/// expanded-directory ids for that worktree (kept sorted).
pub fn is_section_expanded(worktree_id: WorktreeId, expanded_dir_ids: &[ProjectEntryId]) -> bool {
    expanded_dir_ids
        .binary_search(&section_id(worktree_id))
        .is_ok()
}

/// Expands (`Some(true)`), collapses (`Some(false)`) or toggles (`None`) the section row of
/// `worktree_id` in the panel's sorted expanded ids. Returns whether the ids changed.
pub fn set_section_expanded(
    worktree_id: WorktreeId,
    expanded_dir_ids: &mut Vec<ProjectEntryId>,
    expand: Option<bool>,
) -> bool {
    let id = section_id(worktree_id);
    match (expanded_dir_ids.binary_search(&id), expand) {
        (Err(ix), Some(true) | None) => {
            expanded_dir_ids.insert(ix, id);
            true
        }
        (Ok(ix), Some(false) | None) => {
            expanded_dir_ids.remove(ix);
            true
        }
        _ => false,
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

/// Library folder, the manifests of which one must exist at the worktree root, its label.
const LIBRARY_TABLE: &[(&str, &[&str], &str, IconName)] = &[
    (
        "vendor",
        &["composer.json", "composer.lock"],
        "Composer Packages",
        IconName::Box,
    ),
    (
        "node_modules",
        &[
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lockb",
        ],
        "node_modules",
        IconName::Box,
    ),
    (
        "wp-includes",
        &["wp-load.php", "wp-config.php", "wp-settings.php"],
        "WordPress Core (wp-includes)",
        IconName::BookCopy,
    ),
    (
        "wp-admin",
        &["wp-load.php", "wp-config.php", "wp-settings.php"],
        "WordPress Core (wp-admin)",
        IconName::BookCopy,
    ),
];

/// Finds library roots. `lookup` returns `(id, is_dir)` of the worktree-root-relative path.
/// A folder only counts when its ecosystem's manifest is present, so a Go module's `vendor/`
/// stays an ordinary folder.
pub fn detect(
    worktree_id: WorktreeId,
    lookup: impl Fn(&str) -> Option<(ProjectEntryId, bool)>,
) -> Vec<DetectedLibrary> {
    LIBRARY_TABLE
        .iter()
        .filter(|(_, manifests, _, _)| manifests.iter().any(|manifest| lookup(manifest).is_some()))
        .filter_map(|&(folder, _, label, icon)| {
            let (entry_id, is_dir) = lookup(folder)?;
            let path = RelPath::from_unix_str(folder).ok()?;
            is_dir.then(|| DetectedLibrary {
                worktree_id,
                path: path.into(),
                label: SharedString::from(label),
                icon,
                entry_id,
            })
        })
        .collect()
}

/// Detects all external libraries in a worktree snapshot (Composer, npm, WordPress).
pub fn detect_libraries(snapshot: &WorktreeSnapshot) -> Vec<DetectedLibrary> {
    detect(snapshot.id(), |path| {
        let entry = snapshot.entry_for_path(RelPath::from_unix_str(path).ok()?)?;
        Some((entry.id, entry.is_dir()))
    })
}

/// Whether `path` is a detected library root or inside one.
pub fn is_under_library(libraries: &[DetectedLibrary], path: &RelPath) -> bool {
    libraries
        .iter()
        .any(|library| path.starts_with(&library.path))
}

/// Could `path` be inside a library folder? Cheap pre-check before looking up manifests.
pub fn may_be_library_path(path: &RelPath) -> bool {
    path.components()
        .next()
        .is_some_and(|first| LIBRARY_TABLE.iter().any(|(folder, ..)| *folder == first))
}

/// How an entry shows up in the panel when the section is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// The "External Libraries" node.
    Section,
    /// A library root (`vendor`, ...), a child of the node.
    Library { label: SharedString, icon: IconName },
    /// Something inside a library root.
    Inside,
}

/// Per-row check, no detection pass: the section id, then the first path component against the
/// library table, and only then that one table row's manifests (at most five lookups).
pub fn row_kind(
    snap: &WorktreeSnapshot,
    entry_id: ProjectEntryId,
    path: &RelPath,
) -> Option<RowKind> {
    row_kind_with(snap.id(), entry_id, path, |path| {
        let entry = snap.entry_for_path(RelPath::from_unix_str(path).ok()?)?;
        Some((entry.id, entry.is_dir()))
    })
}

/// [`row_kind`] over a root-relative lookup returning `(id, is_dir)`.
fn row_kind_with(
    worktree_id: WorktreeId,
    entry_id: ProjectEntryId,
    path: &RelPath,
    lookup: impl Fn(&str) -> Option<(ProjectEntryId, bool)>,
) -> Option<RowKind> {
    if entry_id == section_id(worktree_id) {
        return Some(RowKind::Section);
    }
    let first = path.components().next()?;
    let &(folder, manifests, label, icon) =
        LIBRARY_TABLE.iter().find(|(folder, ..)| *folder == first)?;
    if !manifests.iter().any(|manifest| lookup(manifest).is_some()) {
        return None;
    }
    if !lookup(folder).is_some_and(|(_, is_dir)| is_dir) {
        return None;
    }
    Some(if path.as_unix_str() == folder {
        RowKind::Library {
            label: SharedString::from(label),
            icon,
        }
    } else {
        RowKind::Inside
    })
}

/// The row's presentation after the library section adjusted it. The chevron is not part of it:
/// the panel derives it from the expanded ids, which include the section's synthetic id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowDecoration {
    pub filename: String,
    pub icon: Option<SharedString>,
    pub depth: usize,
}

/// Renames, re-icons and indents a row: library rows sit one level under the node. A library
/// root is labelled with `project` (its project's name, when several projects have libraries)
/// or else read-only. Library rows are listed without their worktree root row, so the panel
/// computes them one level too shallow.
pub fn decorate_row(kind: RowKind, project: Option<&str>, row: RowDecoration) -> RowDecoration {
    match kind {
        RowKind::Section => RowDecoration {
            filename: "External Libraries".to_string(),
            icon: Some(SharedString::from(IconName::BookCopy.path())),
            depth: 0,
        },
        RowKind::Library { label, icon } => RowDecoration {
            filename: format!("{label} ({})", project.unwrap_or("read-only")),
            icon: Some(SharedString::from(icon.path())),
            depth: row.depth + 1,
        },
        RowKind::Inside => RowDecoration {
            depth: row.depth + 1,
            ..row
        },
    }
}

/// Creates a synthetic GitEntry for the "External Libraries" root header row.
pub fn create_external_libraries_root_entry(worktree_id: WorktreeId) -> GitEntry {
    GitEntry {
        entry: Entry {
            id: section_id(worktree_id),
            kind: EntryKind::Dir,
            path: RelPath::from_unix_str("External Libraries")
                .map_or_else(|_| RelPath::empty_arc(), Into::into),
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

/// Moves a project worktree's library folders out of its visible `entries` and returns the rows
/// that go under the "External Libraries" node: each library root and, while it is expanded, its
/// contents. `None` when the worktree has no detected libraries.
pub fn take_libraries(
    snapshot: &WorktreeSnapshot,
    expanded_dir_ids: &[ProjectEntryId],
    entries: &mut Vec<GitEntry>,
) -> Option<Vec<GitEntry>> {
    let libraries = detect_libraries(snapshot);
    if libraries.is_empty() {
        return None;
    }
    entries.retain(|entry| !is_under_library(&libraries, &entry.path));
    let mut rows = Vec::new();
    for library in &libraries {
        if let Some(entry) = snapshot.entry_for_id(library.entry_id) {
            rows.push(GitEntry {
                entry: entry.clone(),
                git_summary: GitSummary::default(),
            });
            if expanded_dir_ids.binary_search(&library.entry_id).is_ok() {
                append_children_recursive(snapshot, &library.path, expanded_dir_ids, &mut rows);
            }
        }
    }
    Some(rows)
}

/// Whether `abs` is a folder the user added with "Add Library Path…" (or inside one). Its
/// worktree is shown read-only under the "External Libraries" node, never as a project root.
pub fn is_library_worktree(abs: &std::path::Path, cx: &App) -> bool {
    zenith_local_settings_trust::is_library_root(abs, cx)
}

/// Appends `parent_path`'s children to `target`, descending into the expanded folders.
pub fn append_children_recursive(
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

mod add_path;
mod banner;
pub use add_path::{AddLibraryPath, ConfigurePhpIncludePaths};
pub use banner::{LibraryFileBanner, init};

/// Whether `path` is a file the soft "external library" banner applies to: inside a detected
/// library folder (manifest present, feature enabled) or in a worktree under a user library root.
pub fn is_library_file(project: &Project, path: &ProjectPath, cx: &App) -> bool {
    let Some(worktree) = project.worktree_for_id(path.worktree_id, cx) else {
        return false;
    };
    let worktree = worktree.read(cx);
    if zenith_local_settings_trust::is_library_root(&worktree.abs_path(), cx) {
        return true;
    }
    // Id 0 is never the synthetic section id, so only the path decides.
    is_enabled(cx)
        && may_be_library_path(&path.path)
        && row_kind(
            &worktree.snapshot(),
            ProjectEntryId::from_proto(0),
            &path.path,
        ) == Some(RowKind::Inside)
}

#[cfg(test)]
mod local_settings_trust_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rel(s: &str) -> &RelPath {
        RelPath::from_unix_str(s).unwrap()
    }

    fn wt(id: usize) -> WorktreeId {
        WorktreeId::from_usize(id)
    }

    fn eid(id: u64) -> ProjectEntryId {
        ProjectEntryId::from_proto(id)
    }

    /// A fake worktree root: name -> is_dir.
    fn detect_in(files: &[(&str, bool)]) -> Vec<DetectedLibrary> {
        let map: HashMap<&str, (ProjectEntryId, bool)> = files
            .iter()
            .enumerate()
            .map(|(ix, (name, is_dir))| (*name, (eid(ix as u64 + 1), *is_dir)))
            .collect();
        detect(wt(1), |path| map.get(path).copied())
    }

    #[test]
    fn test_section_ids_are_per_worktree() {
        assert_ne!(section_id(wt(1)), section_id(wt(2)));
        assert_eq!(section_worktree(section_id(wt(7))), Some(wt(7)));
        assert_eq!(section_worktree(eid(5)), None);
        assert_eq!(section_worktree(eid(u64::MAX - 200)), None);
    }

    #[test]
    fn test_expansion_is_per_worktree() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        assert!(set_section_expanded(wt(1), &mut first, None));
        assert!(is_section_expanded(wt(1), &first));
        assert!(!is_section_expanded(wt(2), &second));
        assert!(!is_section_expanded(wt(2), &first));
        assert!(!set_section_expanded(wt(2), &mut second, Some(false)));
        assert!(set_section_expanded(wt(1), &mut first, None));
        assert!(!is_section_expanded(wt(1), &first));
    }

    #[test]
    fn test_set_section_expanded_keeps_ids_sorted() {
        let mut ids = vec![eid(3), eid(9)];
        assert!(set_section_expanded(wt(1), &mut ids, Some(true)));
        assert!(!set_section_expanded(wt(1), &mut ids, Some(true)));
        assert_eq!(ids, vec![eid(3), eid(9), section_id(wt(1))]);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(set_section_expanded(wt(1), &mut ids, Some(false)));
        assert_eq!(ids, vec![eid(3), eid(9)]);
    }

    #[test]
    fn test_detects_only_with_manifest() {
        assert_eq!(
            detect_in(&[("composer.json", false), ("vendor", true)]).len(),
            1
        );
        assert_eq!(
            detect_in(&[("package.json", false), ("node_modules", true)]).len(),
            1
        );
        assert_eq!(
            detect_in(&[
                ("wp-load.php", false),
                ("wp-includes", true),
                ("wp-admin", true)
            ])
            .len(),
            2
        );
        // A Go module vendor/ has no Composer manifest: it is not a library.
        assert!(detect_in(&[("go.mod", false), ("vendor", true)]).is_empty());
        // node_modules without package.json, vendor next to package.json only.
        assert!(detect_in(&[("node_modules", true)]).is_empty());
        assert!(detect_in(&[("package.json", false), ("vendor", true)]).is_empty());
        // A file named vendor is not a library folder.
        assert!(detect_in(&[("composer.json", false), ("vendor", false)]).is_empty());
    }

    #[test]
    fn test_hides_only_under_detected_roots() {
        let libraries = detect_in(&[("composer.json", false), ("vendor", true)]);
        assert!(is_under_library(&libraries, rel("vendor")));
        assert!(is_under_library(
            &libraries,
            rel("vendor/laravel/framework")
        ));
        // node_modules is not detected here, so it stays visible.
        assert!(!is_under_library(&libraries, rel("node_modules/axios")));
        assert!(!is_under_library(&libraries, rel("src/vendor_utils.rs")));
        assert!(!is_under_library(&libraries, rel("vendors/x")));
        assert!(!is_under_library(&[], rel("vendor")));
    }

    fn kind_in(files: &[(&str, bool)], entry_id: u64, path: &str) -> Option<RowKind> {
        let map: HashMap<&str, (ProjectEntryId, bool)> = files
            .iter()
            .enumerate()
            .map(|(ix, (name, is_dir))| (*name, (eid(ix as u64 + 1), *is_dir)))
            .collect();
        row_kind_with(wt(1), eid(entry_id), rel(path), |path| {
            map.get(path).copied()
        })
    }

    #[test]
    fn test_row_kind_needs_manifest() {
        let bare = [("vendor", true)];
        assert_eq!(kind_in(&bare, 9, "vendor"), None);
        assert_eq!(kind_in(&bare, 9, "vendor/a/b"), None);

        let with = [("composer.json", false), ("vendor", true)];
        assert_eq!(
            kind_in(&with, 9, "vendor"),
            Some(RowKind::Library {
                label: "Composer Packages".into(),
                icon: IconName::Box
            })
        );
        assert_eq!(kind_in(&with, 9, "vendor/a/b"), Some(RowKind::Inside));

        // Another ecosystem's manifest does not count; a file is no library folder.
        assert_eq!(
            kind_in(&[("package.json", false), ("vendor", true)], 9, "vendor/a"),
            None
        );
        assert_eq!(
            kind_in(
                &[("composer.json", false), ("vendor", false)],
                9,
                "vendor/a"
            ),
            None
        );
        // Not a library folder name, or not at the root.
        assert_eq!(kind_in(&with, 9, "src/main.php"), None);
        assert_eq!(kind_in(&with, 9, "src/vendor"), None);
        assert_eq!(kind_in(&with, 9, "vendors/x"), None);
        // The section row needs no manifest at all.
        assert_eq!(
            kind_in(&[], section_id(wt(1)).to_proto(), "External Libraries"),
            Some(RowKind::Section)
        );
    }

    #[test]
    fn test_decorate_row() {
        let row = RowDecoration {
            filename: "vendor".into(),
            icon: None,
            depth: 0,
        };
        let library = RowKind::Library {
            label: "Composer Packages".into(),
            icon: IconName::Box,
        };
        // Listed without its root row, a library root moves down to sit directly under the node.
        let root = decorate_row(library.clone(), None, row.clone());
        assert_eq!(root.filename, "Composer Packages (read-only)");
        assert_eq!(root.icon, Some(SharedString::from(IconName::Box.path())));
        assert_eq!(root.depth, 1);
        let named = decorate_row(library, Some("plinth"), row.clone());
        assert_eq!(named.filename, "Composer Packages (plinth)");

        let inside = decorate_row(
            RowKind::Inside,
            None,
            RowDecoration {
                depth: 1,
                ..row.clone()
            },
        );
        assert_eq!((inside.filename.as_str(), inside.depth), ("vendor", 2));

        let section = decorate_row(RowKind::Section, Some("plinth"), row);
        assert_eq!(section.filename, "External Libraries");
        assert_eq!(section.depth, 0);
    }

    #[test]
    fn test_may_be_library_path() {
        assert!(may_be_library_path(rel("vendor/x")));
        assert!(may_be_library_path(rel("wp-admin")));
        assert!(!may_be_library_path(rel("src/vendor")));
        assert!(!may_be_library_path(rel("app")));
    }

    #[test]
    fn test_create_external_libraries_root_entry() {
        let root = create_external_libraries_root_entry(wt(3));
        assert_eq!(root.id, section_id(wt(3)));
        assert!(root.is_dir());
        assert!(root.is_external);
        assert_eq!(root.path.as_unix_str(), "External Libraries");
    }

    #[test]
    fn test_settings_default_and_override() {
        let default = ExternalLibrariesSettings::from_settings(&SettingsContent::default());
        assert!(default.enabled);

        let mut content = SettingsContent::default();
        content.zenith = Some(settings::settings_content::ZenithSettingsContent {
            external_libraries: Some(
                settings::settings_content::ZenithExternalLibrariesSettingsContent {
                    enabled: Some(false),
                    ..Default::default()
                },
            ),
            ..Default::default()
        });
        assert!(!ExternalLibrariesSettings::from_settings(&content).enabled);
    }

    #[gpui::test]
    async fn test_apply_with_worktree(cx: &mut gpui::TestAppContext) {
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
                "vendor": { "autoload.php": "", "laravel": { "framework": { "readme.md": "" } } },
                "package.json": "{}",
                "node_modules": { "axios": { "index.js": "" } },
                "src": { "main.php": "" }
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
        cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
            .await;
        let snapshot = tree.read_with(cx, |tree, _| tree.snapshot());

        let libraries = detect_libraries(&snapshot);
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].label, "Composer Packages");
        assert_eq!(libraries[1].label, "node_modules");

        let entry = |path: &str| snapshot.entry_for_path(rel(path)).unwrap().id;
        assert_eq!(
            row_kind(&snapshot, entry("vendor/laravel"), rel("vendor/laravel")),
            Some(RowKind::Inside)
        );
        assert_eq!(
            row_kind(&snapshot, entry("src/main.php"), rel("src/main.php")),
            None
        );

        let all = |snapshot: &WorktreeSnapshot| -> Vec<GitEntry> {
            snapshot
                .entries(true, 0)
                .map(|entry| GitEntry {
                    entry: entry.clone(),
                    git_summary: GitSummary::default(),
                })
                .collect()
        };
        let paths = |entries: &[GitEntry]| -> Vec<String> {
            entries
                .iter()
                .map(|entry| entry.path.as_unix_str().to_string())
                .collect()
        };

        // Library folders and their children leave the tree; the library roots are returned.
        let mut visible = all(&snapshot);
        let libraries = take_libraries(&snapshot, &[], &mut visible).unwrap();
        let remaining = paths(&visible);
        assert!(
            !remaining
                .iter()
                .any(|path| path.starts_with("vendor") || path.starts_with("node_modules"))
        );
        assert!(remaining.contains(&"src/main.php".to_string()));
        assert_eq!(paths(&libraries), vec!["vendor", "node_modules"]);

        // An expanded library root brings its children along.
        let vendor = entry("vendor");
        let mut visible = all(&snapshot);
        let libraries = take_libraries(&snapshot, &[vendor], &mut visible).unwrap();
        assert_eq!(
            paths(&libraries),
            vec![
                "vendor",
                "vendor/autoload.php",
                "vendor/laravel",
                "node_modules"
            ]
        );
    }

    #[gpui::test]
    async fn test_go_vendor_stays_visible(cx: &mut gpui::TestAppContext) {
        use fs::FakeFs;
        use serde_json::json;
        use std::path::Path;

        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/go",
            json!({ "go.mod": "", "vendor": { "modules.txt": "" }, "main.go": "" }),
        )
        .await;
        let tree = worktree::Worktree::local(
            Path::new("/go"),
            true,
            fs,
            Default::default(),
            true,
            WorktreeId::from_proto(2),
            &mut cx.to_async(),
        )
        .await
        .unwrap();
        cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
            .await;
        let snapshot = tree.read_with(cx, |tree, _| tree.snapshot());
        let mut visible: Vec<GitEntry> = snapshot
            .entries(true, 0)
            .map(|entry| GitEntry {
                entry: entry.clone(),
                git_summary: GitSummary::default(),
            })
            .collect();
        let before = visible.len();
        assert!(take_libraries(&snapshot, &[], &mut visible).is_none());
        assert_eq!(visible.len(), before);
    }
}
