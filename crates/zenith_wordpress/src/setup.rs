//! Puts the WordPress Hooks panel into a workspace and keeps its index current.
//!
//! The panel exists only for a local project with a trusted, visible folder that looks like
//! WordPress. Reading a project's PHP is harmless, but the trust gate keeps this in line with
//! the other tools: nothing of an untrusted folder is read until the user trusts it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, PoisonError, RwLock, RwLockWriteGuard},
};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity,
};
use project::{
    Project, UpdatedEntriesSet, WorktreeId,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use util::rel_path::RelPath;
use workspace::Workspace;
use worktree::{Snapshot, Worktree};
use zenith_php_tools::{ProjectKind, detect_project, read_regular, worktree_trusted};

use crate::{
    MAX_INDEXED_FILE_BYTES, ToggleWpHooksPanel, WordPressIndex, WpHooksPanel, scan_php_source,
    wp_hooks_panel::read_index,
};

/// Most PHP files indexed per worktree; the rest are left out.
/// ponytail: a core install plus plugins is a few thousand files. Raise it, or index in the
/// background by directory, if a monorepo needs more.
const MAX_INDEXED_FILES: usize = 20_000;

/// Files the worker reads per round. The panel fills round by round, and a removed folder or a
/// closed workspace stops the work between rounds.
const FILES_PER_BATCH: usize = 500;

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct WordPressSettings {
    pub enabled: bool,
}

impl Settings for WordPressSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.wordpress.as_ref())
                .and_then(|wordpress| wordpress.enabled)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    WordPressSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleWpHooksPanel, window, cx| {
            workspace.toggle_panel_focus::<WpHooksPanel>(window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let this = cx.entity().downgrade();
        let window = window.window_handle();
        let project = workspace.project().clone();
        let wiring = cx.new(|cx| Wiring::new(this, window, project, cx));
        // The workspace owns the wiring; it goes when the workspace does.
        cx.on_release(move |_, _| drop(wiring)).detach();
    })
    .detach();
}

/// Per-workspace state: which worktrees are WordPress, the shared index and the panel.
struct Wiring {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    project: Entity<Project>,
    index: Arc<RwLock<WordPressIndex>>,
    panel: Option<Entity<WpHooksPanel>>,
    /// One detection-and-scan task per worktree that was trusted when seen.
    scans: HashMap<WorktreeId, Task<()>>,
    /// Worktrees found to be WordPress, with their root.
    wordpress: HashMap<WorktreeId, Arc<Path>>,
    /// Files waiting for the worker: `true` = (re)index, `false` = forget.
    pending: HashMap<PathBuf, bool>,
    worker: Option<Task<()>>,
    /// Roots removed while the worker was busy: its running batch may have put their files back,
    /// so they are forgotten again once it is done.
    stale_roots: Vec<Arc<Path>>,
    _subscriptions: Vec<Subscription>,
}

impl Wiring {
    fn new(
        workspace: WeakEntity<Workspace>,
        window: AnyWindowHandle,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![
            cx.subscribe(&project, Self::on_project_event),
            cx.observe_global::<SettingsStore>(|this, cx| this.refresh(cx)),
        ];
        if let Some(store) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(
                &store,
                |this, _, event: &TrustedWorktreesEvent, cx| {
                    if matches!(event, TrustedWorktreesEvent::Trusted(..)) {
                        this.refresh(cx);
                    }
                },
            ));
        }
        let mut this = Self {
            workspace,
            window,
            project,
            index: Arc::default(),
            panel: None,
            scans: HashMap::new(),
            wordpress: HashMap::new(),
            pending: HashMap::new(),
            worker: None,
            stale_roots: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// Starts a scan for every trusted local folder not seen yet.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !WordPressSettings::get_global(cx).enabled || !self.project.read(cx).is_local() {
            return;
        }
        let worktrees: Vec<Entity<Worktree>> =
            self.project.read(cx).visible_worktrees(cx).collect();
        for worktree in worktrees {
            let (id, is_local_folder) = worktree.read_with(cx, |worktree, _| {
                (
                    worktree.id(),
                    worktree.is_local() && !worktree.is_single_file(),
                )
            });
            if !is_local_folder
                || self.scans.contains_key(&id)
                || !worktree_trusted(&self.project, id, cx)
            {
                continue;
            }
            let root = worktree.read(cx).abs_path();
            let task = self.scan(worktree, root, cx);
            self.scans.insert(id, task);
        }
    }

    /// Detects WordPress off the main thread; if it is one, adds the panel and queues its files.
    fn scan(
        &self,
        worktree: Entity<Worktree>,
        root: Arc<Path>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let detect_root = root.clone();
            let kind = cx
                .background_spawn(async move { detect_project(&detect_root) })
                .await;
            if kind != ProjectKind::WordPress {
                return;
            }
            // The snapshot is taken in the same step that starts following the worktree's
            // changes, so no update falls in between.
            let Ok(snapshot) = this.update(cx, |this, cx| {
                let (id, snapshot) =
                    worktree.read_with(cx, |worktree, _| (worktree.id(), worktree.snapshot()));
                this.wordpress.insert(id, root);
                this.ensure_panel(cx);
                snapshot
            }) else {
                return;
            };
            let files = cx
                .background_spawn(async move { php_files(&snapshot) })
                .await;
            // Changes that arrived while listing are newer than the listing.
            this.update(cx, |this, cx| this.queue(files, false, cx))
                .ok();
        })
    }

    fn ensure_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            return;
        }
        let workspace = self.workspace.clone();
        let index = self.index.clone();
        let panel = self.window.update(cx, |_, window, cx| {
            let panel = cx.new(|cx| WpHooksPanel::new(workspace.clone(), index, cx));
            workspace
                .update(cx, |workspace, cx| {
                    workspace.add_panel(panel.clone(), window, cx)
                })
                .ok()?;
            Some(panel)
        });
        self.panel = panel.ok().flatten();
    }

    fn on_project_event(
        &mut self,
        _: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            project::Event::WorktreeAdded(_) => self.refresh(cx),
            project::Event::WorktreeRemoved(id) => {
                self.scans.remove(id);
                if let Some(root) = self.wordpress.remove(id) {
                    self.pending.retain(|path, _| !path.starts_with(&root));
                    write_index(&self.index).remove_dir(&root);
                    if self.worker.is_some() {
                        self.stale_roots.push(root);
                    }
                    self.notify_panel(cx);
                }
            }
            project::Event::WorktreeUpdatedEntries(id, changes) => {
                self.on_entries_changed(*id, changes, cx)
            }
            _ => {}
        }
    }

    fn on_entries_changed(
        &mut self,
        id: WorktreeId,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        // Once a folder is indexed its changes are followed whatever the setting says (the setting
        // only decides whether a folder is picked up) or the index would go stale while it is off.
        if !self.wordpress.contains_key(&id) {
            return;
        }
        let Some(worktree) = self.project.read(cx).worktree_for_id(id, cx) else {
            return;
        };
        let snapshot = worktree.read(cx).snapshot();
        let changed: Vec<(PathBuf, bool)> = changes
            .iter()
            .filter(|(path, ..)| is_indexable(path))
            .map(|(path, ..)| {
                let live = snapshot
                    .entry_for_path(path)
                    .is_some_and(|entry| entry.is_file() && !entry.is_ignored);
                (snapshot.absolutize(path), live)
            })
            .collect();
        self.queue(changed, true, cx);
    }

    /// Hands files to the one worker. `newer` says the entries win over ones already waiting.
    /// The worker is single so two reads of one file cannot finish out of order.
    fn queue(
        &mut self,
        files: impl IntoIterator<Item = (PathBuf, bool)>,
        newer: bool,
        cx: &mut Context<Self>,
    ) {
        for (path, live) in files {
            if newer {
                self.pending.insert(path, live);
            } else {
                self.pending.entry(path).or_insert(live);
            }
        }
        if self.pending.is_empty() || self.worker.is_some() {
            return;
        }
        let index = self.index.clone();
        self.worker = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(job) = this.update(cx, |this, cx| this.next_batch(cx)) else {
                    return;
                };
                let Some((batch, roots)) = job else {
                    return;
                };
                let index = index.clone();
                cx.background_spawn(async move {
                    apply_changes(&index, batch, &roots, MAX_INDEXED_FILES)
                })
                .await;
            }
        }));
    }

    /// The worker's next round: up to [`FILES_PER_BATCH`] waiting files that lie under a folder
    /// still in the project, with those folders. `None` ends the worker.
    fn next_batch(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(HashMap<PathBuf, bool>, Vec<Arc<Path>>)> {
        // Forget before taking more, so a re-added folder's files are queued after it.
        let stale = std::mem::take(&mut self.stale_roots);
        if !stale.is_empty() {
            let mut index = write_index(&self.index);
            for root in &stale {
                index.remove_dir(root);
            }
        }
        // The previous round's results are in the index now.
        self.notify_panel(cx);
        let roots: Vec<Arc<Path>> = self.wordpress.values().cloned().collect();
        let paths: Vec<PathBuf> = self.pending.keys().take(FILES_PER_BATCH).cloned().collect();
        let mut batch = HashMap::new();
        for path in paths {
            let live = self.pending.remove(&path).unwrap_or(false);
            if roots.iter().any(|root| path.starts_with(root)) {
                batch.insert(path, live);
            }
        }
        if batch.is_empty() && self.pending.is_empty() {
            // Detached on purpose: this task is the worker, and it ends right here.
            if let Some(worker) = self.worker.take() {
                worker.detach();
            }
            return None;
        }
        Some((batch, roots))
    }

    fn notify_panel(&self, cx: &mut Context<Self>) {
        if let Some(panel) = &self.panel {
            panel.update(cx, |panel, cx| panel.index_changed(cx));
        }
    }
}

/// Whether the index takes this path: a `.php` file outside `vendor/` and `node_modules/`.
fn is_indexable(path: &RelPath) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
        && !path
            .components()
            .any(|part| part == "vendor" || part == "node_modules")
}

/// The first `cap` indexable paths.
fn indexable_paths<'a>(paths: impl Iterator<Item = &'a RelPath>, cap: usize) -> Vec<&'a RelPath> {
    paths.filter(|path| is_indexable(path)).take(cap).collect()
}

/// Absolute paths of a worktree's PHP files, ignored entries left out.
fn php_files(snapshot: &Snapshot) -> Vec<(PathBuf, bool)> {
    indexable_paths(
        snapshot.files(false, 0).map(|entry| entry.path.as_ref()),
        MAX_INDEXED_FILES,
    )
    .into_iter()
    .map(|path| (snapshot.absolutize(path), true))
    .collect()
}

/// Reads and indexes (`true`) or forgets (`false`) each file. The read is bounded and refuses
/// pipes and devices: a project can plant `x.php -> /dev/zero`. A file that cannot be read or
/// is over the size cap leaves the index. A file outside `roots` is ignored, and a new file is
/// left out once its root holds `max_files` indexed files. The parse runs without the lock; the
/// lock is held only to insert or remove.
fn apply_changes(
    index: &RwLock<WordPressIndex>,
    batch: HashMap<PathBuf, bool>,
    roots: &[Arc<Path>],
    max_files: usize,
) {
    // Room left per root, from what is really indexed (the initial listing, branch switches and
    // later events all end up here).
    let mut room: Vec<usize> = {
        let index = read_index(index);
        roots
            .iter()
            .map(|root| max_files.saturating_sub(index.file_count_under(root)))
            .collect()
    };
    for (path, live) in batch {
        let Some(slot) = roots.iter().position(|root| path.starts_with(root)) else {
            continue;
        };
        let known = read_index(index).contains_file(&path);
        if live && !known && room[slot] == 0 {
            continue;
        }
        let scanned = live
            .then(|| read_regular(&path, MAX_INDEXED_FILE_BYTES as u64 + 1))
            .flatten()
            .filter(|bytes| bytes.len() <= MAX_INDEXED_FILE_BYTES)
            .map(|bytes| scan_php_source(&path, &String::from_utf8_lossy(&bytes)));
        let mut index = write_index(index);
        let indexed = match scanned {
            Some((definitions, listeners)) => index.insert_scanned(path, definitions, listeners),
            None => {
                index.remove_file(&path);
                false
            }
        };
        match (known, indexed) {
            (false, true) => room[slot] -= 1,
            (true, false) => room[slot] += 1,
            _ => {}
        }
    }
}

fn write_index(index: &RwLock<WordPressIndex>) -> RwLockWriteGuard<'_, WordPressIndex> {
    index.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wp_hooks_panel::read_index;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use serde_json::json;

    fn rel(path: &str) -> std::borrow::Cow<'_, RelPath> {
        RelPath::new_test(path)
    }

    #[test]
    fn test_settings_default_to_enabled() {
        assert!(WordPressSettings::from_settings(&SettingsContent::default()).enabled);
        let mut content = SettingsContent::default();
        content.zenith = Some(settings::settings_content::ZenithSettingsContent {
            wordpress: Some(settings::settings_content::ZenithWordPressSettingsContent {
                enabled: Some(false),
            }),
            ..Default::default()
        });
        assert!(!WordPressSettings::from_settings(&content).enabled);
    }

    #[test]
    fn test_only_php_outside_vendor_and_node_modules_is_indexable() {
        for yes in ["a.php", "wp-content/plugins/p/p.php", "A.PHP", "vendor.php"] {
            assert!(is_indexable(&rel(yes)), "{yes}");
        }
        for no in [
            "a.js",
            "a.php.bak",
            "php",
            "vendor/a.php",
            "wp-content/plugins/p/vendor/autoload.php",
            "theme/node_modules/x/y.php",
            "composer.json",
        ] {
            assert!(!is_indexable(&rel(no)), "{no}");
        }
    }

    #[test]
    fn test_indexable_paths_stop_at_the_cap() {
        let names: Vec<String> = (0..10).map(|n| format!("f{n}.php")).collect();
        let paths: Vec<_> = names.iter().map(|name| rel(name)).collect();
        let kept = indexable_paths(paths.iter().map(|path| path.as_ref()), 3);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[2].as_unix_str(), "f2.php");
        // Paths that are filtered out do not use up the cap.
        let mixed = [rel("vendor/x.php"), rel("a.txt"), rel("b.php")];
        let kept = indexable_paths(mixed.iter().map(|path| path.as_ref()), 1);
        assert_eq!(kept[0].as_unix_str(), "b.php");
    }

    /// `apply_changes` for a single root that covers every path, with the real cap.
    fn apply(index: &RwLock<WordPressIndex>, batch: HashMap<PathBuf, bool>) {
        apply_changes(
            index,
            batch,
            &[Arc::from(Path::new("/"))],
            MAX_INDEXED_FILES,
        );
    }

    #[test]
    fn test_apply_changes_reads_bounded_and_forgets_what_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook.php");
        let huge = dir.path().join("huge.php");
        std::fs::write(&hook, "<?php\ndo_action('my_hook');\n").unwrap();
        std::fs::write(
            &huge,
            format!(
                "<?php\ndo_action('huge_hook');\n{}",
                " ".repeat(MAX_INDEXED_FILE_BYTES)
            ),
        )
        .unwrap();
        let index = RwLock::new(WordPressIndex::new());
        let names = |index: &RwLock<WordPressIndex>| {
            let mut names: Vec<String> = read_index(index)
                .completions("")
                .into_iter()
                .map(|completion| completion.name)
                .filter(|name| name.ends_with("_hook"))
                .collect();
            names.sort();
            names
        };
        let missing = dir.path().join("gone.php");
        apply(
            &index,
            HashMap::from([(hook.clone(), true), (huge, true), (missing, true)]),
        );
        assert_eq!(names(&index), ["my_hook"], "the oversized file is left out");
        apply(&index, HashMap::from([(hook.clone(), false)]));
        assert!(names(&index).is_empty());
        // A file that disappears between the event and the read leaves the index.
        apply(&index, HashMap::from([(hook.clone(), true)]));
        std::fs::remove_file(&hook).unwrap();
        apply(&index, HashMap::from([(hook, true)]));
        assert!(names(&index).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_changes_does_not_read_a_device() {
        let index = RwLock::new(WordPressIndex::new());
        // Would never finish, were it read to the end.
        apply(&index, HashMap::from([(PathBuf::from("/dev/zero"), true)]));
        assert!(read_index(&index).find_listeners("x").is_empty());
    }

    struct Env {
        app_state: Arc<workspace::AppState>,
        dir: tempfile::TempDir,
    }

    /// A WordPress plugin folder on the real disk (detection and reads use it) mirrored in the
    /// fake one (the project's worktree lives there).
    async fn setup(cx: &mut TestAppContext, settings: serde_json::Value, trust_store: bool) -> Env {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            if trust_store {
                project::trusted_worktrees::init(Default::default(), cx);
            }
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(&json!({ "zenith": settings }).to_string(), cx)
                    .unwrap();
            });
            app_state
        });
        let dir = tempfile::tempdir().unwrap();
        let tree = json!({
            "wp-config.php": "<?php\n",
            "plugin.php": "<?php\ndo_action('plugin_loaded_hook');\nadd_action('init', 'boot');\n",
            "vendor": { "lib.php": "<?php\ndo_action('vendor_hook');\n" },
        });
        for (name, content) in [
            ("wp-config.php", "<?php\n"),
            (
                "plugin.php",
                "<?php\ndo_action('plugin_loaded_hook');\nadd_action('init', 'boot');\n",
            ),
            ("vendor/lib.php", "<?php\ndo_action('vendor_hook');\n"),
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        app_state
            .fs
            .as_fake()
            .insert_tree(dir.path().to_str().unwrap(), tree)
            .await;
        Env { app_state, dir }
    }

    async fn open_workspace<'a>(
        env: &Env,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<Workspace>,
        &'a mut gpui::VisualTestContext,
    ) {
        let project =
            Project::test_with_worktree_trust(env.app_state.fs.clone(), [env.dir.path()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        (project, workspace, cx)
    }

    fn panel_of(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<Entity<WpHooksPanel>> {
        workspace.read_with(cx, |workspace, cx| workspace.panel::<WpHooksPanel>(cx))
    }

    fn hook_names(panel: &Entity<WpHooksPanel>, cx: &mut gpui::VisualTestContext) -> Vec<String> {
        let index = panel.read_with(cx, |panel, _| panel.index.clone());
        let mut names: Vec<String> = read_index(&index)
            .completions("")
            .into_iter()
            .map(|completion| completion.name)
            .filter(|name| name.ends_with("_hook") || name.ends_with("_loaded_hook"))
            .collect();
        names.sort();
        names
    }

    fn trust(project: &Entity<Project>, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, cx| {
            let id = project.read(cx).worktrees(cx).next().unwrap().read(cx).id();
            let store = TrustedWorktrees::try_get_global(cx).expect("trust store");
            let worktree_store = project.read(cx).worktree_store();
            store.update(cx, |store, cx| {
                store.trust(
                    &worktree_store,
                    [project::trusted_worktrees::PathTrust::Worktree(id)]
                        .into_iter()
                        .collect(),
                    cx,
                )
            });
        });
    }

    #[gpui::test]
    async fn test_panel_is_added_for_a_trusted_wordpress_worktree(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        // The vendor/ copy stays out.
        assert_eq!(hook_names(&panel, cx), ["plugin_loaded_hook"]);

        // The command palette action focuses the panel through the keyboard path.
        cx.dispatch_action(ToggleWpHooksPanel);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn test_no_panel_when_the_setting_is_off(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "wordpress": { "enabled": false } }), false).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none());

        // Turning it on adds the panel without a restart.
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        &json!({ "zenith": { "wordpress": { "enabled": true } } }).to_string(),
                        cx,
                    )
                    .unwrap();
            });
        });
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_some());
    }

    #[gpui::test]
    async fn test_untrusted_worktree_is_not_indexed_until_trusted(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none(), "restricted worktree");

        trust(&project, cx);
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added once trusted");
        cx.run_until_parked();
        assert_eq!(hook_names(&panel, cx), ["plugin_loaded_hook"]);
    }

    #[gpui::test]
    async fn test_a_changed_file_is_reindexed(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();

        let path = env.dir.path().join("new.php");
        let source = "<?php\ndo_action('fresh_hook');\n";
        std::fs::write(&path, source).unwrap();
        env.app_state
            .fs
            .as_fake()
            .insert_file(path.to_str().unwrap(), source.as_bytes().to_vec())
            .await;
        cx.run_until_parked();
        assert_eq!(hook_names(&panel, cx), ["fresh_hook", "plugin_loaded_hook"]);
    }

    /// A second wiring for the test to drive by hand. The setting is off, so it does nothing by
    /// itself (nor does the workspace's own); it still follows the project's events.
    fn manual_wiring(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<Wiring> {
        let this = workspace.downgrade();
        let project = project.clone();
        cx.update(|window, cx| {
            let handle = window.window_handle();
            cx.new(|cx| Wiring::new(this, handle, project, cx))
        })
    }

    fn queue_plugin(
        wiring: &Entity<Wiring>,
        project: &Entity<Project>,
        env: &Env,
        cx: &mut gpui::VisualTestContext,
    ) -> WorktreeId {
        let (id, root) = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            (worktree.id(), worktree.abs_path())
        });
        let plugin = env.dir.path().join("plugin.php");
        wiring.update(cx, |wiring, cx| {
            wiring.wordpress.insert(id, root);
            wiring.queue([(plugin, true)], false, cx);
        });
        id
    }

    fn indexed(wiring: &Entity<Wiring>, cx: &mut gpui::VisualTestContext) -> bool {
        let index = wiring.read_with(cx, |wiring, _| wiring.index.clone());
        let found = read_index(&index).contains_file(
            &wiring
                .read_with(cx, |wiring, _| wiring.wordpress.values().next().cloned())
                .map(|root| root.join("plugin.php"))
                .unwrap_or_default(),
        );
        found
    }

    #[gpui::test]
    async fn test_removing_a_worktree_drops_the_files_still_queued(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "wordpress": { "enabled": false } }), false).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_plugin(&wiring, &project, &env, cx);
        assert_eq!(wiring.read_with(cx, |wiring, _| wiring.pending.len()), 1);

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.pending.is_empty());
            assert!(wiring.worker.is_none(), "the worker has nothing left to do");
            assert!(
                read_index(&wiring.index)
                    .hook_summaries()
                    .iter()
                    .all(|hook| { hook.name != "plugin_loaded_hook" })
            );
        });
    }

    #[gpui::test]
    async fn test_removing_a_worktree_during_a_running_batch_forgets_what_it_adds(
        cx: &mut TestAppContext,
    ) {
        let env = setup(cx, json!({ "wordpress": { "enabled": false } }), false).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_plugin(&wiring, &project, &env, cx);

        // Step the executor until the worker has taken the batch but not yet applied it.
        let mut taken = false;
        for _ in 0..200 {
            cx.executor().tick();
            if wiring.read_with(cx, |wiring, _| wiring.pending.is_empty()) {
                taken = true;
                break;
            }
        }
        assert!(taken, "the worker took the batch");
        assert!(!indexed(&wiring, cx), "the batch is still running");

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.stale_roots.is_empty());
            assert!(
                read_index(&wiring.index)
                    .hook_summaries()
                    .iter()
                    .all(|hook| { hook.name != "plugin_loaded_hook" })
            );
            assert_eq!(
                read_index(&wiring.index).file_count_under(Path::new("/")),
                0
            );
        });
    }

    #[gpui::test]
    async fn test_a_deleted_file_leaves_the_index_while_the_setting_is_off(
        cx: &mut TestAppContext,
    ) {
        let env = setup(cx, json!({}), false).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        assert_eq!(hook_names(&panel, cx), ["plugin_loaded_hook"]);

        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        &json!({ "zenith": { "wordpress": { "enabled": false } } }).to_string(),
                        cx,
                    )
                    .unwrap();
            });
        });
        cx.run_until_parked();
        std::fs::remove_file(env.dir.path().join("plugin.php")).unwrap();
        let deleted = project.update(cx, |project, cx| {
            let worktree_id = project.worktrees(cx).next().unwrap().read(cx).id();
            project.delete_file(
                project::ProjectPath {
                    worktree_id,
                    path: util::rel_path::rel_path("plugin.php").into(),
                },
                cx,
            )
        });
        deleted.expect("the file is in the project").await.unwrap();
        cx.run_until_parked();

        assert!(hook_names(&panel, cx).is_empty());
    }

    #[test]
    fn test_new_files_stop_at_the_cap_whatever_path_brings_them() {
        let dir = tempfile::tempdir().unwrap();
        let root: Arc<Path> = Arc::from(dir.path());
        let roots = [root.clone()];
        let write = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("<?php\ndo_action('{name}_hook');\n")).unwrap();
            path
        };
        let paths: Vec<PathBuf> = ["a.php", "b.php", "c.php", "d.php"].map(write).into();
        let index = RwLock::new(WordPressIndex::new());
        let files = |index: &RwLock<WordPressIndex>| read_index(index).file_count_under(&root);

        // The first batch (the initial listing) fills the root up to the cap.
        let first = paths[..3].iter().map(|path| (path.clone(), true)).collect();
        apply_changes(&index, first, &roots, 2);
        assert_eq!(files(&index), 2);

        // A later event for a new file finds no room ...
        apply_changes(&index, HashMap::from([(paths[3].clone(), true)]), &roots, 2);
        assert!(!read_index(&index).contains_file(&paths[3]));
        assert_eq!(files(&index), 2);

        // ... an indexed file may still change, and deleting one makes room again.
        let indexed_path = paths
            .iter()
            .find(|path| read_index(&index).contains_file(path))
            .unwrap()
            .clone();
        apply_changes(
            &index,
            HashMap::from([(indexed_path.clone(), true)]),
            &roots,
            2,
        );
        assert_eq!(files(&index), 2);
        apply_changes(&index, HashMap::from([(indexed_path, false)]), &roots, 2);
        apply_changes(&index, HashMap::from([(paths[3].clone(), true)]), &roots, 2);
        assert!(read_index(&index).contains_file(&paths[3]));
        assert_eq!(files(&index), 2);
    }

    #[test]
    fn test_apply_changes_stops_at_the_total_hook_cap() {
        let dir = tempfile::tempdir().unwrap();
        let roots = [Arc::<Path>::from(dir.path())];
        let two_hooks = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, "<?php\ndo_action('x');\ndo_action('y');\n").unwrap();
            path
        };
        let (a, b, c) = (two_hooks("a.php"), two_hooks("b.php"), two_hooks("c.php"));
        let index = RwLock::new(WordPressIndex::with_hook_cap(4));
        let batch = [&a, &b, &c].map(|path| (path.clone(), true)).into();
        apply_changes(&index, batch, &roots, MAX_INDEXED_FILES);
        let held = [&a, &b, &c]
            .iter()
            .filter(|path| read_index(&index).contains_file(path))
            .count();
        assert_eq!(held, 2, "two files of two hooks fill a cap of four");
    }
}
