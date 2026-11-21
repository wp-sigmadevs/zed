//! Puts the Laravel panel into a workspace and keeps its index current.
//!
//! The panel exists only for a local project with a trusted, visible folder that looks like
//! Laravel. Nothing of an untrusted folder is read until the user trusts it. Only what the panel
//! shows is indexed: the route files under `routes/` (read) and the view paths under
//! `resources/views/` (paths only). `.env` and `config/` are left alone: nothing consumes them,
//! and `.env` holds secrets.

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
use util::{paths::PathStyle, rel_path::RelPath};
use workspace::Workspace;
use worktree::{Snapshot, Worktree};
use zenith_php_tools::{read_regular, worktree_trusted};

use crate::{LaravelIndex, LaravelPanel, ToggleLaravelPanel, laravel_panel::read_index};

/// Most view files indexed per worktree; the rest are left out.
/// ponytail: a large app has a few hundred views. Raise it if a monorepo needs more.
const MAX_VIEW_FILES: usize = 20_000;

/// Most route files indexed per worktree; the rest are left out.
/// ponytail: an app has a handful of route files. Raise it if a modular monolith needs more.
const MAX_ROUTE_FILES: usize = 2_000;

/// A route file larger than this is not read (and is left out of the index).
/// ponytail: route files are a few KB. Raise it if a generated one is cut off.
const MAX_ROUTE_FILE_BYTES: usize = 1024 * 1024;

/// Files the worker handles per round. The panel fills round by round, and a removed folder or a
/// closed workspace stops the work between rounds.
const FILES_PER_BATCH: usize = 500;

/// The most files of each kind indexed per worktree.
#[derive(Clone, Copy)]
struct Caps {
    views: usize,
    route_files: usize,
}

const CAPS: Caps = Caps {
    views: MAX_VIEW_FILES,
    route_files: MAX_ROUTE_FILES,
};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LaravelSettings {
    pub enabled: bool,
}

impl Settings for LaravelSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.laravel.as_ref())
                .and_then(|laravel| laravel.enabled)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    LaravelSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleLaravelPanel, window, cx| {
            workspace.toggle_panel_focus::<LaravelPanel>(window, cx);
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

/// Per-workspace state: which worktrees are Laravel, the shared index and the panel.
struct Wiring {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    project: Entity<Project>,
    index: Arc<RwLock<LaravelIndex>>,
    panel: Option<Entity<LaravelPanel>>,
    /// One detection-and-scan task per worktree that was trusted when seen.
    scans: HashMap<WorktreeId, Task<()>>,
    /// Worktrees found to be Laravel, with their root.
    laravel: HashMap<WorktreeId, Arc<Path>>,
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
            laravel: HashMap::new(),
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
        if !LaravelSettings::get_global(cx).enabled || !self.project.read(cx).is_local() {
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

    /// Detects Laravel off the main thread; if it is one, adds the panel and queues its files.
    fn scan(
        &self,
        worktree: Entity<Worktree>,
        root: Arc<Path>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let detect_root = root.clone();
            let is_laravel = cx
                .background_spawn(async move { LaravelIndex::is_laravel_project(&detect_root) })
                .await;
            if !is_laravel {
                return;
            }
            // The snapshot is taken in the same step that starts following the worktree's
            // changes, so no update falls in between.
            let Ok(snapshot) = this.update(cx, |this, cx| {
                let (id, snapshot) =
                    worktree.read_with(cx, |worktree, _| (worktree.id(), worktree.snapshot()));
                this.laravel.insert(id, root);
                this.ensure_panel(cx);
                snapshot
            }) else {
                return;
            };
            let files = cx
                .background_spawn(async move { indexable_files(&snapshot, CAPS) })
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
            let panel = cx.new(|cx| LaravelPanel::new(workspace.clone(), index, cx));
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
                if let Some(root) = self.laravel.remove(id) {
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
        if !self.laravel.contains_key(&id) {
            return;
        }
        let Some(worktree) = self.project.read(cx).worktree_for_id(id, cx) else {
            return;
        };
        let snapshot = worktree.read(cx).snapshot();
        let changed: Vec<(PathBuf, bool)> = changes
            .iter()
            .filter(|(path, ..)| classify(path.as_unix_str()).is_some())
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
                cx.background_spawn(async move { apply_changes(&index, batch, &roots, CAPS) })
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
        let roots: Vec<Arc<Path>> = self.laravel.values().cloned().collect();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Route,
    View,
}

/// What the index takes from a path under a worktree root (unix separators): a `.php` file below
/// `routes/` (a route file) or below `resources/views/` (a view), outside `vendor/` and
/// `node_modules/`. Everything else, `.env` and `config/` included, is left alone.
fn classify(rel: &str) -> Option<Kind> {
    if !rel.ends_with(".php")
        || rel
            .split('/')
            .any(|part| part == "vendor" || part == "node_modules")
    {
        return None;
    }
    if rel.starts_with("routes/") {
        Some(Kind::Route)
    } else if rel.starts_with("resources/views/") {
        Some(Kind::View)
    } else {
        None
    }
}

/// The first `caps` paths of each kind that the index takes.
fn indexable_paths<'a>(paths: impl Iterator<Item = &'a RelPath>, caps: Caps) -> Vec<&'a RelPath> {
    let (mut views, mut routes) = (0, 0);
    let mut kept = Vec::new();
    for path in paths {
        let (count, cap) = match classify(path.as_unix_str()) {
            Some(Kind::View) => (&mut views, caps.views),
            Some(Kind::Route) => (&mut routes, caps.route_files),
            None => continue,
        };
        if *count < cap {
            *count += 1;
            kept.push(path);
        }
    }
    kept
}

/// Absolute paths of a worktree's route files and views, ignored entries left out.
fn indexable_files(snapshot: &Snapshot, caps: Caps) -> Vec<(PathBuf, bool)> {
    indexable_paths(
        snapshot.files(false, 0).map(|entry| entry.path.as_ref()),
        caps,
    )
    .into_iter()
    .map(|path| (snapshot.absolutize(path), true))
    .collect()
}

/// The batch as a list, removals first, so the room they free is there for the additions of the
/// same batch.
fn removals_first(batch: HashMap<PathBuf, bool>) -> Vec<(PathBuf, bool)> {
    let mut batch: Vec<_> = batch.into_iter().collect();
    batch.sort_by_key(|(_, live)| *live);
    batch
}

/// Indexes (`true`) or forgets (`false`) each file. A route file is read, bounded, refusing pipes
/// and devices (a project can plant `web.php -> /dev/zero`); one that cannot be read or is over
/// the size cap leaves the index. A view is a path only, nothing is read. A file outside `roots`
/// or outside `routes/` and `resources/views/` is ignored, and a new file is left out once its
/// root holds the cap of its kind, counted against what is really indexed. The parse runs without
/// the lock; the lock is held only to insert or remove.
fn apply_changes(
    index: &RwLock<LaravelIndex>,
    batch: HashMap<PathBuf, bool>,
    roots: &[Arc<Path>],
    caps: Caps,
) {
    // Room left per root and kind, from what is really indexed (the initial listing, branch
    // switches and later events all end up here).
    let mut room: Vec<(usize, usize)> = {
        let index = read_index(index);
        roots
            .iter()
            .map(|root| {
                (
                    caps.views.saturating_sub(index.view_count_under(root)),
                    caps.route_files
                        .saturating_sub(index.route_file_count_under(root)),
                )
            })
            .collect()
    };
    for (path, live) in removals_first(batch) {
        let Some(slot) = roots.iter().position(|root| path.starts_with(root)) else {
            continue;
        };
        let root = &roots[slot];
        let kind = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| RelPath::new(rel, PathStyle::local()).ok())
            .and_then(|rel| classify(rel.as_unix_str()));
        let Some(kind) = kind else {
            continue;
        };
        let known = read_index(index).contains_file(&path);
        let left = match kind {
            Kind::View => &mut room[slot].0,
            Kind::Route => &mut room[slot].1,
        };
        if live && !known && *left == 0 {
            continue;
        }
        let route_source = (live && kind == Kind::Route)
            .then(|| read_regular(&path, MAX_ROUTE_FILE_BYTES as u64 + 1))
            .flatten()
            .filter(|bytes| bytes.len() <= MAX_ROUTE_FILE_BYTES)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let mut index = write_index(index);
        match (live, kind, route_source) {
            (true, Kind::View, _) => index.index_view_path(&root.join("resources/views"), &path),
            (true, Kind::Route, Some(source)) => index.index_route_file(path.clone(), &source),
            _ => index.remove_file(&path),
        }
        match (known, index.contains_file(&path)) {
            (false, true) => *left -= 1,
            (true, false) => *left += 1,
            _ => {}
        }
    }
}

fn write_index(index: &RwLock<LaravelIndex>) -> RwLockWriteGuard<'_, LaravelIndex> {
    index.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use serde_json::{Value, json};

    fn rel(path: &str) -> std::borrow::Cow<'_, RelPath> {
        RelPath::new_test(path)
    }

    #[test]
    fn test_settings_default_to_enabled() {
        assert!(LaravelSettings::from_settings(&SettingsContent::default()).enabled);
        let mut content = SettingsContent::default();
        content.zenith = Some(settings::settings_content::ZenithSettingsContent {
            laravel: Some(settings::settings_content::ZenithLaravelSettingsContent {
                enabled: Some(false),
            }),
            ..Default::default()
        });
        assert!(!LaravelSettings::from_settings(&content).enabled);
    }

    #[test]
    fn test_only_routes_and_views_are_indexable() {
        for (yes, kind) in [
            ("routes/web.php", Kind::Route),
            ("routes/api/v1.php", Kind::Route),
            ("resources/views/welcome.blade.php", Kind::View),
            ("resources/views/a/b.php", Kind::View),
        ] {
            assert_eq!(classify(yes), Some(kind), "{yes}");
        }
        for no in [
            ".env",
            ".env.example",
            "config/app.php",
            "composer.json",
            "app/Http/routes.php",
            "app/routes/web.php",
            "routes/readme.md",
            "routes/web.php.bak",
            "routes",
            "resources/views/app.js",
            "resources/js/views/a.php",
            "vendor/pkg/routes/web.php",
            "routes/vendor/pkg.php",
            "resources/views/vendor/mail/a.blade.php",
            "resources/views/node_modules/a.php",
        ] {
            assert_eq!(classify(no), None, "{no}");
        }
    }

    #[test]
    fn test_listing_stops_at_each_cap_and_skips_what_is_not_indexed() {
        let names: Vec<String> = (0..5)
            .map(|n| format!("routes/r{n}.php"))
            .chain((0..5).map(|n| format!("resources/views/v{n}.blade.php")))
            .chain(["config/app.php".to_string(), "vendor/x.php".to_string()])
            .collect();
        let paths: Vec<_> = names.iter().map(|name| rel(name)).collect();
        let caps = Caps {
            views: 3,
            route_files: 2,
        };
        let kept = indexable_paths(paths.iter().map(|path| path.as_ref()), caps);
        let count = |kind| {
            kept.iter()
                .filter(|path| classify(path.as_unix_str()) == Some(kind))
                .count()
        };
        assert_eq!((count(Kind::Route), count(Kind::View)), (2, 3));
        assert_eq!(kept.len(), 5, "config/ and vendor/ use up no cap");
        // A full kind does not crowd out the other.
        assert!(kept.iter().any(|p| p.as_unix_str() == "routes/r1.php"));
        assert!(
            kept.iter()
                .any(|p| p.as_unix_str() == "resources/views/v2.blade.php")
        );
    }

    fn apply(index: &RwLock<LaravelIndex>, root: &Path, batch: HashMap<PathBuf, bool>, caps: Caps) {
        apply_changes(index, batch, &[Arc::from(root)], caps);
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn route_names(index: &RwLock<LaravelIndex>) -> Vec<String> {
        read_index(index)
            .routes_sorted()
            .into_iter()
            .map(|route| route.name)
            .collect()
    }

    fn view_names(index: &RwLock<LaravelIndex>) -> Vec<String> {
        read_index(index)
            .views_sorted()
            .into_iter()
            .map(|view| view.dot_name)
            .collect()
    }

    fn route_source(name: &str) -> String {
        format!("<?php\nRoute::get('/{name}', 'C@m')->name('{name}');\n")
    }

    #[test]
    fn test_apply_changes_reads_routes_bounded_and_forgets_what_vanished() {
        let dir = tempfile::tempdir().unwrap();
        let web = write(dir.path(), "routes/web.php", &route_source("web.home"));
        let huge = write(
            dir.path(),
            "routes/huge.php",
            &format!(
                "{}{}",
                route_source("huge.route"),
                " ".repeat(MAX_ROUTE_FILE_BYTES)
            ),
        );
        let missing = dir.path().join("routes/gone.php");
        let index = RwLock::new(LaravelIndex::new());
        let go = |batch| apply(&index, dir.path(), batch, CAPS);
        go(HashMap::from([
            (web.clone(), true),
            (huge, true),
            (missing, true),
        ]));
        assert_eq!(
            route_names(&index),
            ["web.home"],
            "the oversized file is left out"
        );
        go(HashMap::from([(web.clone(), false)]));
        assert!(route_names(&index).is_empty());
        // A file that disappears between the event and the read leaves the index.
        go(HashMap::from([(web.clone(), true)]));
        assert_eq!(route_names(&index), ["web.home"]);
        std::fs::remove_file(&web).unwrap();
        go(HashMap::from([(web.clone(), true)]));
        assert!(route_names(&index).is_empty());
        assert!(!read_index(&index).contains_file(&web));
    }

    #[test]
    fn test_apply_changes_forgets_a_deleted_view() {
        let dir = tempfile::tempdir().unwrap();
        let welcome = dir.path().join("resources/views/welcome.blade.php");
        let card = dir.path().join("resources/views/components/card.blade.php");
        let index = RwLock::new(LaravelIndex::new());
        apply(
            &index,
            dir.path(),
            HashMap::from([(welcome, true), (card.clone(), true)]),
            CAPS,
        );
        assert_eq!(view_names(&index), ["components.card", "welcome"]);
        apply(&index, dir.path(), HashMap::from([(card, false)]), CAPS);
        assert_eq!(view_names(&index), ["welcome"]);
        assert!(read_index(&index).find_component("x-card").is_none());
        assert_eq!(read_index(&index).view_count_under(dir.path()), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_changes_does_not_read_a_device() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routes/web.php");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Would never finish, were it read to the end.
        std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
        let index = RwLock::new(LaravelIndex::new());
        apply(
            &index,
            dir.path(),
            HashMap::from([(path.clone(), true)]),
            CAPS,
        );
        assert!(!read_index(&index).contains_file(&path));
    }

    #[test]
    fn test_apply_changes_leaves_everything_else_alone() {
        let dir = tempfile::tempdir().unwrap();
        let others: Vec<PathBuf> = [
            (".env", "APP_KEY=secret\n"),
            ("config/app.php", "<?php\nreturn ['name' => 'x'];\n"),
            (
                "vendor/pkg/routes/web.php",
                "<?php\nRoute::get('/v')->name('v');\n",
            ),
            (
                "app/routes/web.php",
                "<?php\nRoute::get('/a')->name('a');\n",
            ),
        ]
        .map(|(name, content)| write(dir.path(), name, content))
        .into();
        let index = RwLock::new(LaravelIndex::new());
        apply(
            &index,
            dir.path(),
            others.iter().map(|path| (path.clone(), true)).collect(),
            CAPS,
        );
        let index = read_index(&index);
        assert!(others.iter().all(|path| !index.contains_file(path)));
        assert!(index.find_env("APP_KEY").is_none());
        assert!(index.config_completions("").is_empty());
        assert!(index.routes_sorted().is_empty());
    }

    #[test]
    fn test_new_files_stop_at_the_cap_whatever_path_brings_them() {
        let dir = tempfile::tempdir().unwrap();
        let caps = Caps {
            views: 2,
            route_files: 2,
        };
        let route = |n: &str| write(dir.path(), &format!("routes/{n}.php"), &route_source(n));
        let view = |n: &str| dir.path().join(format!("resources/views/{n}.blade.php"));
        let routes: Vec<PathBuf> = ["a", "b", "c", "d"].map(route).into();
        let views: Vec<PathBuf> = ["a", "b", "c", "d"].map(view).into();
        let index = RwLock::new(LaravelIndex::new());
        let counts = |index: &RwLock<LaravelIndex>| {
            let index = read_index(index);
            (
                index.route_file_count_under(dir.path()),
                index.view_count_under(dir.path()),
            )
        };

        // The first batch (the initial listing) fills each kind up to its cap.
        let first = routes[..3]
            .iter()
            .chain(&views[..3])
            .map(|path| (path.clone(), true))
            .collect();
        apply(&index, dir.path(), first, caps);
        assert_eq!(counts(&index), (2, 2));

        // A later event for a new file finds no room ...
        let later = HashMap::from([(routes[3].clone(), true), (views[3].clone(), true)]);
        apply(&index, dir.path(), later, caps);
        assert!(!read_index(&index).contains_file(&routes[3]));
        assert!(!read_index(&index).contains_file(&views[3]));
        assert_eq!(counts(&index), (2, 2));

        // ... an indexed file may still change, and deleting one makes room again.
        let known_route = routes
            .iter()
            .find(|p| read_index(&index).contains_file(p))
            .unwrap();
        let known_view = views
            .iter()
            .find(|p| read_index(&index).contains_file(p))
            .unwrap();
        let again = HashMap::from([(known_route.clone(), true), (known_view.clone(), true)]);
        apply(&index, dir.path(), again, caps);
        assert_eq!(counts(&index), (2, 2));
        let gone = HashMap::from([(known_route.clone(), false), (known_view.clone(), false)]);
        apply(&index, dir.path(), gone, caps);
        let fresh = HashMap::from([(routes[3].clone(), true), (views[3].clone(), true)]);
        apply(&index, dir.path(), fresh, caps);
        assert!(read_index(&index).contains_file(&routes[3]));
        assert!(read_index(&index).contains_file(&views[3]));
        assert_eq!(counts(&index), (2, 2));

        // A batch that deletes one file and adds another nets out: the delete is counted first.
        let (route_e, view_e) = (route("e"), view("e"));
        let held = |files: &[PathBuf]| {
            files
                .iter()
                .find(|p| read_index(&index).contains_file(p))
                .unwrap()
                .clone()
        };
        let swap = HashMap::from([
            (held(&routes), false),
            (route_e.clone(), true),
            (held(&views), false),
            (view_e.clone(), true),
        ]);
        apply(&index, dir.path(), swap, caps);
        assert!(read_index(&index).contains_file(&route_e));
        assert!(read_index(&index).contains_file(&view_e));
        assert_eq!(counts(&index), (2, 2));
    }

    #[test]
    fn test_removals_come_before_additions() {
        // A HashMap lists in no fixed order; 64 mixed entries put an addition first unless sorted.
        let batch: HashMap<PathBuf, bool> = (0..64)
            .map(|n| (PathBuf::from(format!("/p/routes/{n}.php")), n % 2 == 0))
            .collect();
        let ordered = removals_first(batch);
        assert_eq!(ordered.len(), 64);
        let first_addition = ordered.iter().position(|(_, live)| *live).unwrap();
        assert_eq!(first_addition, 32);
        assert!(ordered[first_addition..].iter().all(|(_, live)| *live));
    }

    fn laravel_tree() -> Value {
        json!({
            "artisan": "#!/usr/bin/env php\n",
            ".env": "APP_KEY=secret\n",
            "config": { "app.php": "<?php\nreturn ['name' => 'x'];\n" },
            "app": { "routes": { "x.php": "<?php\nRoute::get('/x')->name('app.x');\n" } },
            "routes": {
                "web.php": "<?php\nRoute::get('/', 'H@i')->name('home');\n",
                "api.php": "<?php\nRoute::post('/login', 'A@l')->name('api.login');\n",
            },
            "resources": { "views": {
                "welcome.blade.php": "<html>\n",
                "auth": { "login.blade.php": "<form>\n" },
                "vendor": { "mail": { "header.blade.php": "<p>\n" } },
            } },
            "vendor": { "pkg": { "routes": { "web.php": "<?php\nRoute::get('/v')->name('vendor.route');\n" } } },
        })
    }

    fn write_tree(dir: &Path, tree: &Value) {
        for (name, value) in tree.as_object().unwrap() {
            match value {
                Value::String(content) => {
                    write(dir, name, content);
                }
                _ => {
                    std::fs::create_dir_all(dir.join(name)).unwrap();
                    write_tree(&dir.join(name), value);
                }
            }
        }
    }

    struct Env {
        app_state: Arc<workspace::AppState>,
        dir: tempfile::TempDir,
    }

    /// A Laravel app on the real disk (detection and reads use it) mirrored in the fake one
    /// (the project's worktree lives there). Without `laravel` the `artisan` marker is missing.
    async fn setup(
        cx: &mut TestAppContext,
        settings: serde_json::Value,
        trust_store: bool,
        laravel: bool,
    ) -> Env {
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
        let mut tree = laravel_tree();
        if !laravel {
            tree.as_object_mut().unwrap().remove("artisan");
        }
        write_tree(dir.path(), &tree);
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
    ) -> Option<Entity<LaravelPanel>> {
        workspace.read_with(cx, |workspace, cx| workspace.panel::<LaravelPanel>(cx))
    }

    fn index_of(
        panel: &Entity<LaravelPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> Arc<RwLock<LaravelIndex>> {
        panel.read_with(cx, |panel, _| panel.index.clone())
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

    fn set_enabled(enabled: bool, cx: &mut gpui::VisualTestContext) {
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        &json!({ "zenith": { "laravel": { "enabled": enabled } } }).to_string(),
                        cx,
                    )
                    .unwrap();
            });
        });
    }

    #[gpui::test]
    async fn test_panel_is_added_for_a_trusted_laravel_worktree(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        // Only named routes of routes/ and views of resources/views/; the vendor/ copies stay out.
        assert_eq!(route_names(&index), ["api.login", "home"]);
        assert_eq!(view_names(&index), ["auth.login", "welcome"]);
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 2));

        // The command palette action focuses the panel through the keyboard path.
        cx.dispatch_action(ToggleLaravelPanel);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn test_env_and_config_are_not_indexed(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        let index = read_index(&index);
        let root = env.dir.path();
        for name in [
            ".env",
            "config/app.php",
            "app/routes/x.php",
            "vendor/pkg/routes/web.php",
        ] {
            assert!(!index.contains_file(&root.join(name)), "{name}");
        }
        assert!(index.find_env("APP_KEY").is_none());
        assert!(index.config_completions("").is_empty());
        assert!(index.find_route("vendor.route").is_none() && index.find_route("app.x").is_none());
        assert!(index.find_view("vendor.mail.header").is_none());
    }

    #[gpui::test]
    async fn test_no_panel_for_a_project_that_is_not_laravel(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, false).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none());
    }

    #[gpui::test]
    async fn test_no_panel_when_the_setting_is_off(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "laravel": { "enabled": false } }), false, true).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none());

        // Turning it on adds the panel without a restart.
        set_enabled(true, cx);
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_some());
    }

    #[gpui::test]
    async fn test_untrusted_worktree_is_not_indexed_until_trusted(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), true, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none(), "restricted worktree");

        trust(&project, cx);
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added once trusted");
        cx.run_until_parked();
        assert_eq!(route_names(&index_of(&panel, cx)), ["api.login", "home"]);
    }

    /// Adds `name` with `content` to both the real disk and the fake one.
    async fn add_file(env: &Env, name: &str, content: &str) {
        let path = write(env.dir.path(), name, content);
        env.app_state
            .fs
            .create_dir(path.parent().unwrap())
            .await
            .unwrap();
        env.app_state
            .fs
            .as_fake()
            .insert_file(path.to_str().unwrap(), content.as_bytes().to_vec())
            .await;
    }

    #[gpui::test]
    async fn test_new_files_are_indexed_and_others_ignored(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();

        add_file(&env, "routes/admin.php", &route_source("admin.home")).await;
        add_file(&env, "resources/views/admin/index.blade.php", "<div>\n").await;
        add_file(&env, "config/mail.php", "<?php\nreturn ['a' => 1];\n").await;
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        assert_eq!(route_names(&index), ["admin.home", "api.login", "home"]);
        assert_eq!(view_names(&index), ["admin.index", "auth.login", "welcome"]);
        assert!(!read_index(&index).contains_file(&env.dir.path().join("config/mail.php")));
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 3));
    }

    #[gpui::test]
    async fn test_a_deleted_file_leaves_the_index_while_the_setting_is_off(
        cx: &mut TestAppContext,
    ) {
        let env = setup(cx, json!({}), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        assert_eq!(route_names(&index), ["api.login", "home"]);

        set_enabled(false, cx);
        cx.run_until_parked();
        for name in ["routes/web.php", "resources/views/welcome.blade.php"] {
            std::fs::remove_file(env.dir.path().join(name)).unwrap();
            let deleted = project.update(cx, |project, cx| {
                let worktree_id = project.worktrees(cx).next().unwrap().read(cx).id();
                project.delete_file(
                    project::ProjectPath {
                        worktree_id,
                        path: util::rel_path::rel_path(name).into(),
                    },
                    cx,
                )
            });
            deleted.expect("the file is in the project").await.unwrap();
        }
        cx.run_until_parked();

        assert_eq!(route_names(&index), ["api.login"]);
        assert_eq!(view_names(&index), ["auth.login"]);
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 1));
    }

    #[gpui::test]
    async fn test_removing_an_indexed_worktree_empties_the_panel(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        assert_eq!(route_names(&index).len() + view_names(&index).len(), 4);

        let id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        assert!(route_names(&index).is_empty() && view_names(&index).is_empty());
        panel.read_with(cx, |panel, _| assert!(panel.rows.is_empty()));
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

    fn queue_web(
        wiring: &Entity<Wiring>,
        project: &Entity<Project>,
        env: &Env,
        cx: &mut gpui::VisualTestContext,
    ) -> WorktreeId {
        let (id, root) = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            (worktree.id(), worktree.abs_path())
        });
        let web = env.dir.path().join("routes/web.php");
        wiring.update(cx, |wiring, cx| {
            wiring.laravel.insert(id, root);
            wiring.queue([(web, true)], false, cx);
        });
        id
    }

    fn indexed(wiring: &Entity<Wiring>, env: &Env, cx: &mut gpui::VisualTestContext) -> bool {
        let index = wiring.read_with(cx, |wiring, _| wiring.index.clone());
        let found = read_index(&index).contains_file(&env.dir.path().join("routes/web.php"));
        found
    }

    #[gpui::test]
    async fn test_removing_a_worktree_drops_the_files_still_queued(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "laravel": { "enabled": false } }), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_web(&wiring, &project, &env, cx);
        assert_eq!(wiring.read_with(cx, |wiring, _| wiring.pending.len()), 1);

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        // Dropped on the removal itself, not merely skipped by the worker later.
        assert!(wiring.read_with(cx, |wiring, _| wiring.pending.is_empty()));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.pending.is_empty());
            assert!(wiring.worker.is_none(), "the worker has nothing left to do");
            assert!(route_names(&wiring.index).is_empty());
        });
    }

    #[gpui::test]
    async fn test_removing_a_worktree_during_a_running_batch_forgets_what_it_adds(
        cx: &mut TestAppContext,
    ) {
        let env = setup(cx, json!({ "laravel": { "enabled": false } }), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_web(&wiring, &project, &env, cx);

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
        assert!(!indexed(&wiring, &env, cx), "the batch is still running");

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.stale_roots.is_empty());
            assert!(route_names(&wiring.index).is_empty());
            assert!(
                !read_index(&wiring.index).contains_file(&env.dir.path().join("routes/web.php"))
            );
        });
    }
}
