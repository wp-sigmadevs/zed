//! Puts the Next.js panel into a workspace and keeps its index current.
//!
//! The panel exists only for a local project with a trusted, visible folder that looks like
//! Next.js. Nothing of an untrusted folder is read until the user trusts it. Only the App Router
//! special files (`page`, `layout`, `route`, ...) below `app/` or `src/app/` are indexed, and of
//! those only the `route` files are read (for their HTTP methods); the rest are paths only.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
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
use zenith_php_tools::{
    StringCompletionSource, add_string_completions, read_regular, worktree_trusted,
};

use crate::{
    AppRouterKind, NextJsIndex, NextJsPanel, ToggleNextJsPanel, classify, nextjs_panel::read_index,
    route_path_prefix,
};

/// Most App Router files indexed per worktree; the rest are left out.
/// ponytail: a large app has a few hundred. Raise it if a monorepo needs more.
const MAX_APP_FILES: usize = 20_000;

/// Most routes indexed over all worktrees of a workspace.
/// ponytail: five full worktrees. Raise it if people open more at once.
const MAX_TOTAL_ROUTES: usize = 100_000;

/// A route handler larger than this is not read (and is left out of the index).
/// ponytail: handlers are a few KB. Raise it if a generated one is cut off.
const MAX_ROUTE_FILE_BYTES: usize = 1024 * 1024;

/// Files the worker handles per round. The panel fills round by round, and a removed folder or a
/// closed workspace stops the work between rounds.
const FILES_PER_BATCH: usize = 500;

#[derive(Clone, Copy)]
struct Caps {
    per_worktree: usize,
    total: usize,
}

const CAPS: Caps = Caps {
    per_worktree: MAX_APP_FILES,
    total: MAX_TOTAL_ROUTES,
};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct NextJsSettings {
    pub enabled: bool,
}

impl Settings for NextJsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.nextjs.as_ref())
                .and_then(|nextjs| nextjs.enabled)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    NextJsSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleNextJsPanel, window, cx| {
            workspace.toggle_panel_focus::<NextJsPanel>(window, cx);
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

/// Per-workspace state: which worktrees are Next.js, the shared index and the panel.
struct Wiring {
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    project: Entity<Project>,
    index: Arc<RwLock<NextJsIndex>>,
    panel: Option<Entity<NextJsPanel>>,
    /// One detection-and-scan task per worktree that was trusted when seen.
    scans: HashMap<WorktreeId, Task<()>>,
    /// Worktrees found to be Next.js, with their root.
    nextjs: HashMap<WorktreeId, Arc<Path>>,
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
        let index: Arc<RwLock<NextJsIndex>> = Arc::default();
        let route_paths = Rc::new(RoutePaths {
            wiring: cx.weak_entity(),
            index: index.clone(),
        });
        let mut subscriptions = vec![
            cx.subscribe(&project, Self::on_project_event),
            cx.observe_global::<SettingsStore>(|this, cx| this.refresh(cx)),
            add_string_completions(&project, route_paths, cx),
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
            index,
            panel: None,
            scans: HashMap::new(),
            nextjs: HashMap::new(),
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
        if !NextJsSettings::get_global(cx).enabled || !self.project.read(cx).is_local() {
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

    /// Detects Next.js off the main thread; if it is one, adds the panel and queues its files.
    fn scan(
        &self,
        worktree: Entity<Worktree>,
        root: Arc<Path>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let detect_root = root.clone();
            let is_nextjs = cx
                .background_spawn(async move { NextJsIndex::is_nextjs_project(&detect_root) })
                .await;
            if !is_nextjs {
                return;
            }
            // The snapshot is taken in the same step that starts following the worktree's
            // changes, so no update falls in between.
            let Ok(snapshot) = this.update(cx, |this, cx| {
                let (id, snapshot) =
                    worktree.read_with(cx, |worktree, _| (worktree.id(), worktree.snapshot()));
                this.nextjs.insert(id, root);
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
            let panel = cx.new(|cx| NextJsPanel::new(workspace.clone(), index, cx));
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
                if let Some(root) = self.nextjs.remove(id) {
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
        if !self.nextjs.contains_key(&id) {
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
        let roots: Vec<Arc<Path>> = self.nextjs.values().cloned().collect();
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

/// Route paths inside `href="…"` and `router.push('…')`, in a JS or TS file of a trusted folder
/// this workspace found to be Next.js, while the setting is on.
struct RoutePaths {
    wiring: WeakEntity<Wiring>,
    index: Arc<RwLock<NextJsIndex>>,
}

impl StringCompletionSource for RoutePaths {
    fn prefix<'a>(
        &self,
        worktree_id: WorktreeId,
        path: &str,
        line: &'a str,
        cx: &mut App,
    ) -> Option<&'a str> {
        let is_script = path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| matches!(extension, "js" | "jsx" | "ts" | "tsx" | "mjs"));
        if !NextJsSettings::get_global(cx).enabled || !is_script {
            return None;
        }
        let prefix = route_path_prefix(line)?;
        let wiring = self.wiring.upgrade()?;
        let wiring = wiring.read(cx);
        if !wiring.nextjs.contains_key(&worktree_id) {
            return None;
        }
        let project = wiring.project.clone();
        worktree_trusted(&project, worktree_id, cx).then_some(prefix)
    }

    fn candidates(&self, prefix: &str) -> Vec<String> {
        read_index(&self.index).route_completions(prefix)
    }
}

/// The first `cap` paths that the index takes.
fn indexable_paths<'a>(paths: impl Iterator<Item = &'a RelPath>, cap: usize) -> Vec<&'a RelPath> {
    paths
        .filter(|path| classify(path.as_unix_str()).is_some())
        .take(cap)
        .collect()
}

/// Absolute paths of a worktree's App Router files, ignored entries left out.
fn indexable_files(snapshot: &Snapshot, caps: Caps) -> Vec<(PathBuf, bool)> {
    indexable_paths(
        snapshot.files(false, 0).map(|entry| entry.path.as_ref()),
        caps.per_worktree,
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

/// Indexes (`true`) or forgets (`false`) each file. A route handler is read, bounded, refusing
/// pipes and devices (a project can plant `route.ts -> /dev/zero`); one that cannot be read or is
/// over the size cap leaves the index. Every other file is a path only, nothing is read. A file
/// outside `roots` or that is no App Router file is ignored, and a new file is left out once its
/// root holds the per-worktree cap or the index the total, counted against what is really
/// indexed. The parse runs without the lock; the lock is held only to insert or remove.
fn apply_changes(
    index: &RwLock<NextJsIndex>,
    batch: HashMap<PathBuf, bool>,
    roots: &[Arc<Path>],
    caps: Caps,
) {
    // Room left per root and in total, from what is really indexed (the initial listing, branch
    // switches and later events all end up here).
    let (mut room, mut total_room) = {
        let index = read_index(index);
        let room: Vec<usize> = roots
            .iter()
            .map(|root| {
                caps.per_worktree
                    .saturating_sub(index.route_count_under(root))
            })
            .collect();
        (room, caps.total.saturating_sub(index.route_count()))
    };
    for (path, live) in removals_first(batch) {
        let Some(slot) = roots.iter().position(|root| path.starts_with(root)) else {
            continue;
        };
        let root = &roots[slot];
        let classified = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| RelPath::new(rel, PathStyle::local()).ok())
            .and_then(|rel| classify(rel.as_unix_str()));
        let Some((app_dir, kind)) = classified else {
            continue;
        };
        let known = read_index(index).contains_file(&path);
        if live && !known && (room[slot] == 0 || total_room == 0) {
            continue;
        }
        let route = live
            .then(|| {
                let source = if kind == AppRouterKind::RouteHandler {
                    read_regular(&path, MAX_ROUTE_FILE_BYTES as u64 + 1)
                        .filter(|bytes| bytes.len() <= MAX_ROUTE_FILE_BYTES)
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())?
                } else {
                    String::new()
                };
                NextJsIndex::route_for(&root.join(app_dir), &path, &source)
            })
            .flatten();
        let mut index = write_index(index);
        match route {
            Some(route) => index.insert(route),
            None => index.remove_file(&path),
        }
        match (known, index.contains_file(&path)) {
            (false, true) => {
                room[slot] -= 1;
                total_room -= 1;
            }
            (true, false) => {
                room[slot] += 1;
                total_room += 1;
            }
            _ => {}
        }
    }
}

fn write_index(index: &RwLock<NextJsIndex>) -> RwLockWriteGuard<'_, NextJsIndex> {
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
        assert!(NextJsSettings::from_settings(&SettingsContent::default()).enabled);
        let mut content = SettingsContent::default();
        content.zenith = Some(settings::settings_content::ZenithSettingsContent {
            nextjs: Some(settings::settings_content::ZenithNextJsSettingsContent {
                enabled: Some(false),
            }),
            ..Default::default()
        });
        assert!(!NextJsSettings::from_settings(&content).enabled);
    }

    #[test]
    fn test_listing_stops_at_the_cap_and_skips_what_is_not_indexed() {
        let names: Vec<String> = (0..4)
            .map(|n| format!("app/p{n}/page.tsx"))
            .chain(
                [
                    "app/Button.tsx",
                    "pages/index.tsx",
                    "node_modules/a/app/page.tsx",
                ]
                .map(String::from),
            )
            .chain((0..4).map(|n| format!("src/app/q{n}/route.ts")))
            .collect();
        let paths: Vec<_> = names.iter().map(|name| rel(name)).collect();
        let kept = indexable_paths(paths.iter().map(|path| path.as_ref()), 5);
        assert_eq!(kept.len(), 5);
        // The files that are not indexed use up no cap: the fifth is the first `src/app` one.
        assert_eq!(kept[4].as_unix_str(), "src/app/q0/route.ts");
        let all = indexable_paths(paths.iter().map(|path| path.as_ref()), 100);
        assert_eq!(all.len(), 8);
    }

    fn apply(
        index: &RwLock<NextJsIndex>,
        roots: &[&Path],
        batch: HashMap<PathBuf, bool>,
        caps: Caps,
    ) {
        let roots: Vec<Arc<Path>> = roots.iter().map(|root| Arc::from(*root)).collect();
        apply_changes(index, batch, &roots, caps);
    }

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    /// The indexed files, relative to `root`.
    fn files(index: &RwLock<NextJsIndex>, root: &Path) -> Vec<String> {
        let mut files: Vec<String> = read_index(index)
            .routes_sorted()
            .into_iter()
            .filter_map(|route| {
                let rel = route.file_path.strip_prefix(root).ok()?;
                Some(rel.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        files.sort();
        files
    }

    const HANDLER: &str = "export async function GET() {}\nexport async function POST() {}\n";

    #[test]
    fn test_apply_changes_reads_only_route_handlers_and_bounds_them() {
        let dir = tempfile::tempdir().unwrap();
        let hook = write(dir.path(), "app/api/hook/route.ts", HANDLER);
        let huge = write(
            dir.path(),
            "app/api/huge/route.ts",
            &format!("{HANDLER}{}", " ".repeat(MAX_ROUTE_FILE_BYTES)),
        );
        let missing_route = dir.path().join("app/api/gone/route.ts");
        // A page is a path only: it is indexed although it cannot be read (it is not on disk).
        let page = dir.path().join("app/about/page.tsx");
        let index = RwLock::new(NextJsIndex::new());
        apply(
            &index,
            &[dir.path()],
            HashMap::from([
                (hook, true),
                (huge, true),
                (missing_route, true),
                (page.clone(), true),
            ]),
            CAPS,
        );
        assert_eq!(
            files(&index, dir.path()),
            ["app/about/page.tsx", "app/api/hook/route.ts"],
            "the oversized and the missing handler are left out"
        );
        let index = read_index(&index);
        assert_eq!(
            index.find_routes_for_path("/api/hook")[0].http_methods,
            ["GET", "POST"]
        );
        assert!(index.contains_file(&page));
    }

    #[test]
    fn test_apply_changes_forgets_a_deleted_or_vanished_file() {
        let dir = tempfile::tempdir().unwrap();
        let hook = write(dir.path(), "app/api/hook/route.ts", HANDLER);
        let about = dir.path().join("app/about/page.tsx");
        let index = RwLock::new(NextJsIndex::new());
        let go = |batch| apply(&index, &[dir.path()], batch, CAPS);
        go(HashMap::from([(hook.clone(), true), (about.clone(), true)]));
        assert_eq!(files(&index, dir.path()).len(), 2);

        // Deleted: the event says it is gone.
        go(HashMap::from([(about, false)]));
        assert_eq!(files(&index, dir.path()), ["app/api/hook/route.ts"]);

        // Vanished between the event and the read.
        std::fs::remove_file(&hook).unwrap();
        go(HashMap::from([(hook.clone(), true)]));
        assert!(files(&index, dir.path()).is_empty());
        assert!(!read_index(&index).contains_file(&hook));
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_changes_does_not_read_a_device() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app/api/route.ts");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Would never finish, were it read to the end.
        std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
        let index = RwLock::new(NextJsIndex::new());
        apply(
            &index,
            &[dir.path()],
            HashMap::from([(path.clone(), true)]),
            CAPS,
        );
        assert!(!read_index(&index).contains_file(&path));
    }

    #[test]
    fn test_apply_changes_leaves_everything_else_alone() {
        let dir = tempfile::tempdir().unwrap();
        let others: Vec<PathBuf> = [
            "app/components/Button.tsx",
            "app/page.test.tsx",
            "app/page.css",
            "pages/index.tsx",
            "page.tsx",
            "app/node_modules/pkg/page.tsx",
            "node_modules/next/app/page.tsx",
            ".next/app/page.tsx",
            "app/.next/page.tsx",
            "app/vendor/x/route.ts",
        ]
        .map(|name| write(dir.path(), name, HANDLER))
        .into();
        let index = RwLock::new(NextJsIndex::new());
        apply(
            &index,
            &[dir.path()],
            others.iter().map(|path| (path.clone(), true)).collect(),
            CAPS,
        );
        assert!(files(&index, dir.path()).is_empty());
    }

    #[test]
    fn test_a_file_outside_every_root_is_ignored() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let outside = write(b.path(), "app/page.tsx", "");
        let index = RwLock::new(NextJsIndex::new());
        apply(&index, &[a.path()], HashMap::from([(outside, true)]), CAPS);
        assert_eq!(read_index(&index).route_count(), 0);
    }

    #[test]
    fn test_files_with_one_url_survive_the_removal_of_one() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app/about/page.tsx");
        let src = dir.path().join("src/app/about/page.tsx");
        let layout = dir.path().join("app/about/layout.tsx");
        let index = RwLock::new(NextJsIndex::new());
        let go = |batch| apply(&index, &[dir.path()], batch, CAPS);
        go(HashMap::from([
            (app.clone(), true),
            (src.clone(), true),
            (layout, true),
        ]));
        assert_eq!(read_index(&index).find_routes_for_path("/about").len(), 3);
        go(HashMap::from([(app, false)]));
        let index = read_index(&index);
        assert_eq!(index.find_routes_for_path("/about").len(), 2);
        assert!(index.contains_file(&src));
    }

    #[test]
    fn test_new_files_stop_at_the_caps_whatever_path_brings_them() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Two files per worktree, three over both.
        let caps = Caps {
            per_worktree: 2,
            total: 3,
        };
        let page = |dir: &Path, n: &str| dir.join(format!("app/{n}/page.tsx"));
        let roots = [a.path(), b.path()];
        let index = RwLock::new(NextJsIndex::new());
        let count_under = |dir: &Path| read_index(&index).route_count_under(dir);

        // The first batch (the initial listing) fills worktree A up to its cap.
        let first = ["a1", "a2", "a3"].map(|n| (page(a.path(), n), true));
        apply(&index, &roots, first.into(), caps);
        assert_eq!(count_under(a.path()), 2);

        // A later event for a new file finds no room in A ...
        apply(
            &index,
            &roots,
            HashMap::from([(page(a.path(), "a4"), true)]),
            caps,
        );
        assert_eq!(count_under(a.path()), 2);

        // ... B takes one more (the total is 3), then the total stops it.
        let more = ["b1", "b2"].map(|n| (page(b.path(), n), true));
        apply(&index, &roots, more.into(), caps);
        assert_eq!(
            (count_under(b.path()), read_index(&index).route_count()),
            (1, 3)
        );

        // An indexed file may still change, and deleting one makes room again.
        let known = page(a.path(), "a1");
        let known = [known, page(a.path(), "a2"), page(a.path(), "a3")]
            .into_iter()
            .find(|path| read_index(&index).contains_file(path))
            .unwrap();
        apply(&index, &roots, HashMap::from([(known.clone(), true)]), caps);
        assert_eq!(read_index(&index).route_count(), 3);
        apply(&index, &roots, HashMap::from([(known, false)]), caps);
        // Which of b1/b2 got the slot depends on HashMap order: add the other one.
        let (indexed_b, refused_b) = if read_index(&index).contains_file(&page(b.path(), "b1")) {
            ("b1", "b2")
        } else {
            ("b2", "b1")
        };
        apply(
            &index,
            &roots,
            HashMap::from([(page(b.path(), refused_b), true)]),
            caps,
        );
        assert_eq!(
            (count_under(b.path()), read_index(&index).route_count()),
            (2, 3)
        );

        // A batch that deletes one file and adds another nets out: the delete is counted first.
        let held = page(b.path(), indexed_b);
        let swap = HashMap::from([(held, false), (page(b.path(), "b3"), true)]);
        apply(&index, &roots, swap, caps);
        assert!(read_index(&index).contains_file(&page(b.path(), "b3")));
        assert_eq!(read_index(&index).route_count(), 3);
    }

    #[test]
    fn test_removals_come_before_additions() {
        // A HashMap lists in no fixed order; 64 mixed entries put an addition first unless sorted.
        let batch: HashMap<PathBuf, bool> = (0..64)
            .map(|n| (PathBuf::from(format!("/p/app/{n}/page.tsx")), n % 2 == 0))
            .collect();
        let ordered = removals_first(batch);
        assert_eq!(ordered.len(), 64);
        let first_addition = ordered.iter().position(|(_, live)| *live).unwrap();
        assert_eq!(first_addition, 32);
        assert!(ordered[first_addition..].iter().all(|(_, live)| *live));
    }

    fn next_tree() -> Value {
        let handler = HANDLER;
        json!({
            "package.json": "{\"dependencies\": {\"next\": \"14\"}}",
            "app": {
                "layout.tsx": "export default function L() {}\n",
                "page.tsx": "export default function P() {}\n",
                "about": { "page.tsx": "export default function A() {}\n" },
                "api": { "hook": { "route.ts": handler } },
                "components": { "Button.tsx": "export const B = 1;\n" },
                "node_modules": { "pkg": { "page.tsx": "x\n" } },
            },
            "src": { "app": { "blog": { "[slug]": { "page.tsx": "export default function S() {}\n" } } } },
            ".next": { "app": { "page.tsx": "x\n" } },
            "node_modules": { "next": { "app": { "page.tsx": "x\n" } } },
            "pages": { "index.tsx": "x\n" },
        })
    }

    const NEXT_FILES: [&str; 5] = [
        "app/about/page.tsx",
        "app/api/hook/route.ts",
        "app/layout.tsx",
        "app/page.tsx",
        "src/app/blog/[slug]/page.tsx",
    ];

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

    /// A Next.js app on the real disk (detection and reads use it) mirrored in the fake one
    /// (the project's worktree lives there). Without `nextjs` the `package.json` is missing.
    async fn setup(
        cx: &mut TestAppContext,
        settings: serde_json::Value,
        trust_store: bool,
        nextjs: bool,
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
        let mut tree = next_tree();
        if !nextjs {
            tree.as_object_mut().unwrap().remove("package.json");
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
    ) -> Option<Entity<NextJsPanel>> {
        workspace.read_with(cx, |workspace, cx| workspace.panel::<NextJsPanel>(cx))
    }

    fn index_of(
        panel: &Entity<NextJsPanel>,
        cx: &mut gpui::VisualTestContext,
    ) -> Arc<RwLock<NextJsIndex>> {
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
                        &json!({ "zenith": { "nextjs": { "enabled": enabled } } }).to_string(),
                        cx,
                    )
                    .unwrap();
            });
        });
    }

    #[gpui::test]
    async fn test_panel_is_added_for_a_trusted_nextjs_worktree(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        // Only App Router files below app/ and src/app/; the copies in node_modules/ and .next/
        // and the plain components stay out.
        assert_eq!(files(&index, env.dir.path()), NEXT_FILES);
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 5));

        // The command palette action focuses the panel through the keyboard path.
        cx.dispatch_action(ToggleNextJsPanel);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn test_no_panel_for_a_project_that_is_not_nextjs(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, false).await;
        let (_, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        assert!(panel_of(&workspace, cx).is_none());
    }

    #[gpui::test]
    async fn test_no_panel_when_the_setting_is_off(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "nextjs": { "enabled": false } }), false, true).await;
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
        assert_eq!(files(&index_of(&panel, cx), env.dir.path()), NEXT_FILES);
    }

    /// The route paths the editor's provider adds at `offset` (the custom ones, so the language
    /// server and buffer words are left out).
    async fn route_completions_at(
        editor: &Entity<editor::Editor>,
        buffer: &Entity<language::Buffer>,
        offset: usize,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<String> {
        let task = editor.update_in(cx, |editor, window, cx| {
            let provider = editor.completion_provider().expect("a completion provider");
            let position = buffer.read(cx).anchor_before(offset);
            let trigger = editor::CompletionContext {
                trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            };
            provider.completions(buffer, position, trigger, window, cx)
        });
        task.await
            .unwrap()
            .into_iter()
            .flat_map(|response| response.completions)
            .filter(|completion| matches!(completion.source, project::CompletionSource::Custom))
            .map(|completion| completion.new_text)
            .collect()
    }

    #[gpui::test]
    async fn test_route_paths_complete_inside_href_strings_in_an_editor(cx: &mut TestAppContext) {
        use gpui::VisualContext as _;

        let env = setup(cx, json!({}), false, true).await;
        let (project, _workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let buffer = project
            .update(cx, |project, cx| {
                let worktree_id = project.worktrees(cx).next().unwrap().read(cx).id();
                project.open_buffer(
                    project::ProjectPath {
                        worktree_id,
                        path: util::rel_path::rel_path("app/components/Button.tsx").into(),
                    },
                    cx,
                )
            })
            .await
            .unwrap();
        let text = "export const B = () => <Link href=\"/a\">x</Link>;\n";
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
        });
        let editor = cx.new_window_entity(|window, cx| {
            editor::Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx)
        });
        let inside = text.find("\"/a").unwrap() + 3;
        let outside = text.find("const").unwrap() + 3;

        // `/` and `/blog/:slug` are in the index too; the typed `/a` leaves them out.
        assert_eq!(
            route_completions_at(&editor, &buffer, inside, cx).await,
            ["/about", "/api/hook"]
        );
        assert!(
            route_completions_at(&editor, &buffer, outside, cx)
                .await
                .is_empty()
        );

        set_enabled(false, cx);
        cx.run_until_parked();
        assert!(
            route_completions_at(&editor, &buffer, inside, cx)
                .await
                .is_empty()
        );
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

        add_file(
            &env,
            "app/admin/page.tsx",
            "export default function X() {}\n",
        )
        .await;
        add_file(
            &env,
            "app/admin/route.ts",
            "export async function DELETE() {}\n",
        )
        .await;
        add_file(&env, "app/admin/Form.tsx", "export const F = 1;\n").await;
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        let mut expected: Vec<&str> = NEXT_FILES.to_vec();
        expected.extend(["app/admin/page.tsx", "app/admin/route.ts"]);
        expected.sort();
        assert_eq!(files(&index, env.dir.path()), expected);
        assert_eq!(read_index(&index).find_routes_for_path("/admin").len(), 2);
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 7));
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
        assert_eq!(files(&index, env.dir.path()).len(), 5);

        set_enabled(false, cx);
        cx.run_until_parked();
        for name in ["app/page.tsx", "app/api/hook/route.ts"] {
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

        assert_eq!(
            files(&index, env.dir.path()),
            [
                "app/about/page.tsx",
                "app/layout.tsx",
                "src/app/blog/[slug]/page.tsx"
            ]
        );
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 3));
    }

    #[gpui::test]
    async fn test_removing_an_indexed_worktree_empties_the_panel(cx: &mut TestAppContext) {
        let env = setup(cx, json!({}), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let panel = panel_of(&workspace, cx).expect("panel added");
        cx.run_until_parked();
        let index = index_of(&panel, cx);
        assert_eq!(files(&index, env.dir.path()).len(), 5);

        let id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        assert_eq!(read_index(&index).route_count(), 0);
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

    fn queue_hook(
        wiring: &Entity<Wiring>,
        project: &Entity<Project>,
        env: &Env,
        cx: &mut gpui::VisualTestContext,
    ) -> WorktreeId {
        let (id, root) = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap().read(cx);
            (worktree.id(), worktree.abs_path())
        });
        let hook = env.dir.path().join("app/api/hook/route.ts");
        wiring.update(cx, |wiring, cx| {
            wiring.nextjs.insert(id, root);
            wiring.queue([(hook, true)], false, cx);
        });
        id
    }

    fn hook_indexed(wiring: &Entity<Wiring>, env: &Env, cx: &mut gpui::VisualTestContext) -> bool {
        let index = wiring.read_with(cx, |wiring, _| wiring.index.clone());
        let found = read_index(&index).contains_file(&env.dir.path().join("app/api/hook/route.ts"));
        found
    }

    #[gpui::test]
    async fn test_removing_a_worktree_drops_the_files_still_queued(cx: &mut TestAppContext) {
        let env = setup(cx, json!({ "nextjs": { "enabled": false } }), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_hook(&wiring, &project, &env, cx);
        assert_eq!(wiring.read_with(cx, |wiring, _| wiring.pending.len()), 1);

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        // Dropped on the removal itself, not merely skipped by the worker later.
        assert!(wiring.read_with(cx, |wiring, _| wiring.pending.is_empty()));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.pending.is_empty());
            assert!(wiring.worker.is_none(), "the worker has nothing left to do");
            assert_eq!(read_index(&wiring.index).route_count(), 0);
        });
    }

    #[gpui::test]
    async fn test_removing_a_worktree_during_a_running_batch_forgets_what_it_adds(
        cx: &mut TestAppContext,
    ) {
        let env = setup(cx, json!({ "nextjs": { "enabled": false } }), false, true).await;
        let (project, workspace, cx) = open_workspace(&env, cx).await;
        cx.run_until_parked();
        let wiring = manual_wiring(&workspace, &project, cx);
        let id = queue_hook(&wiring, &project, &env, cx);

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
        assert!(
            !hook_indexed(&wiring, &env, cx),
            "the batch is still running"
        );

        project.update(cx, |project, cx| project.remove_worktree(id, cx));
        cx.run_until_parked();

        wiring.read_with(cx, |wiring, _| {
            assert!(wiring.stale_roots.is_empty());
            assert_eq!(read_index(&wiring.index).route_count(), 0);
        });
    }
}
