//! The project switcher: the popover under the project-name button (plan 44, B).
//! Rows `Open…` and `Clone Repository…`, the open projects, the recent projects, and
//! the footer rows that keep upstream's remote and add-to-project flows reachable.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use fuzzy_nucleo::{MaxResults, StringMatchCandidate, StringMatchOptions, match_strings};
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Hsla, IntoElement, Render, Role, Subscription, Task,
    WeakEntity, Window, WindowHandle,
};
use picker::{Picker, PickerDelegate};
use project::ProjectGroupKey;
use recent_projects::RecentProjects;
use settings::Settings as _;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenMode, RecentWorkspace, SerializedWorkspaceLocation, Workspace, WorkspaceDb,
};
use zenith_welcome::{
    DetectCache,
    detect::{self, ProjectInfo},
};

use crate::{
    ZenithTitleBarSettings, badge,
    switcher_rows::{Entry, badge_color, display_path, entries, initials},
};

/// Detection runs for this many rows per open; the list shows what is cached for the rest.
const MAX_DETECT_ROOTS: usize = 30;

/// The popover body: Zenith's switcher, or upstream's list with `zenith.title_bar.enabled` off.
pub struct ProjectSwitcher {
    body: Body,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

enum Body {
    Zenith(Entity<Picker<SwitcherDelegate>>),
    Upstream(Entity<RecentProjects>),
}

impl EventEmitter<DismissEvent> for ProjectSwitcher {}

impl Focusable for ProjectSwitcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.body {
            Body::Zenith(picker) => picker.focus_handle(cx),
            Body::Upstream(recent) => recent.focus_handle(cx),
        }
    }
}

impl Render for ProjectSwitcher {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.body {
            Body::Zenith(picker) => picker.clone().into_any_element(),
            Body::Upstream(recent) => recent.clone().into_any_element(),
        }
    }
}

/// Same arguments as upstream's `RecentProjects::popover`, which this replaces.
pub fn project_switcher(
    workspace: WeakEntity<Workspace>,
    window_project_groups: Vec<ProjectGroupKey>,
    create_new_window: Option<bool>,
    focus_handle: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ProjectSwitcher> {
    if !ZenithTitleBarSettings::get_global(cx).enabled {
        let recent = RecentProjects::popover(
            workspace,
            window_project_groups,
            create_new_window,
            focus_handle,
            window,
            cx,
        );
        return cx.new(|cx| ProjectSwitcher {
            _subscriptions: vec![
                cx.subscribe(&recent, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            ],
            body: Body::Upstream(recent),
            _tasks: Vec::new(),
        });
    }
    let open = open_projects(window, cx);
    cx.new(|cx| ProjectSwitcher::zenith(workspace, open, focus_handle, window, cx))
}

impl ProjectSwitcher {
    fn zenith(
        workspace: WeakEntity<Workspace>,
        open: Vec<(ProjectGroupKey, OpenTarget)>,
        focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fs = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().fs.clone());
        let info = cx
            .try_global::<DetectCache>()
            .map(|cache| cache.0.clone())
            .unwrap_or_default();
        let delegate = SwitcherDelegate::new(workspace, focus_handle, open, info, fs.clone());
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .list_measure_all()
                .initial_width(rems(20.))
                .show_scrollbar(true)
        });
        let picker_focus = picker.focus_handle(cx);
        picker_focus.focus(window, cx);
        let subscriptions = vec![
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            cx.on_focus_out(&picker_focus, window, |_, _, _, cx| cx.emit(DismissEvent)),
        ];
        // The database read is async: the open projects show at once, recent ones follow.
        let db = WorkspaceDb::global(cx);
        let load = cx.spawn_in(window, async move |this, cx| {
            let Some(fs) = fs else { return };
            let recent = db
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            this.update_in(cx, |this, window, cx| this.set_recent(recent, window, cx))
                .log_err();
        });
        Self {
            body: Body::Zenith(picker),
            _subscriptions: subscriptions,
            _tasks: vec![load],
        }
    }

    /// Shows the recent projects, then refreshes branches in the background.
    fn set_recent(
        &mut self,
        recent: Vec<RecentWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Body::Zenith(picker) = &self.body else {
            return;
        };
        let picker = picker.clone();
        let (roots, fs) = picker.update(cx, |picker, cx| {
            picker.delegate.set_recent(recent);
            picker.refresh(window, cx);
            (
                picker.delegate.local_roots(MAX_DETECT_ROOTS),
                picker.delegate.fs.clone(),
            )
        });
        let Some(fs) = fs else { return };
        // Never a file read on the main thread: `detect` runs on the background executor.
        let detect = cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let mut found = Vec::with_capacity(roots.len());
                    for root in roots {
                        let info = detect::detect(fs.as_ref(), &root).await;
                        found.push((root, info));
                    }
                    found
                })
                .await;
            this.update(cx, |_, cx| {
                cx.default_global::<DetectCache>().0.extend(found.clone());
                picker.update(cx, |picker, cx| {
                    picker.delegate.info.extend(found);
                    cx.notify();
                });
            })
            .log_err();
        });
        self._tasks.push(detect);
    }
}

/// What activating an open project needs.
#[derive(Clone)]
struct OpenTarget {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
}

/// Every project open in any window, one row per project group. Windows without a
/// `MultiWorkspace` root (welcome-only) are skipped.
fn open_projects(window: &mut Window, cx: &mut App) -> Vec<(ProjectGroupKey, OpenTarget)> {
    let current = window.window_handle();
    let mut open: Vec<(ProjectGroupKey, OpenTarget)> = Vec::new();
    let mut add = |handle: WindowHandle<MultiWorkspace>, multi: &MultiWorkspace, cx: &App| {
        for workspace in multi.workspaces() {
            let key = workspace.read(cx).project_group_key(cx);
            if key.path_list().paths().is_empty() || open.iter().any(|(known, _)| *known == key) {
                continue;
            }
            let target = OpenTarget {
                window: handle,
                workspace: workspace.clone(),
            };
            open.push((key, target));
        }
    };
    // The current window is being updated, so it is read through `window`, not its handle.
    if let (Some(handle), Some(Some(multi))) = (
        current.downcast::<MultiWorkspace>(),
        window.root::<MultiWorkspace>(),
    ) {
        add(handle, multi.read(cx), cx);
    }
    let cx: &App = cx;
    for handle in cx.windows() {
        if handle == current {
            continue;
        }
        let Some(handle) = handle.downcast::<MultiWorkspace>() else {
            continue;
        };
        // Err while that window is itself mid-update: skipped.
        if let Ok(multi) = handle.read(cx) {
            add(handle, multi, cx);
        }
    }
    open
}

pub struct SwitcherDelegate {
    workspace: WeakEntity<Workspace>,
    /// The workspace's focus: actions are dispatched from there, not from the closing popover.
    focus_handle: FocusHandle,
    open: Vec<(ProjectGroupKey, OpenTarget)>,
    recent: Vec<RecentWorkspace>,
    rows: Vec<Entry<OpenTarget>>,
    /// Indices into `rows` that match the query.
    shown: Vec<usize>,
    info: HashMap<PathBuf, ProjectInfo>,
    fs: Option<Arc<dyn fs::Fs>>,
    selected_index: usize,
}

/// One project row, ready to draw.
struct Row {
    names: String,
    path: String,
    branch: Option<String>,
    initials: String,
    color: Hsla,
    /// What a screen reader says: `<names>, <path>, branch <b>`.
    label: String,
}

impl SwitcherDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        focus_handle: FocusHandle,
        open: Vec<(ProjectGroupKey, OpenTarget)>,
        info: HashMap<PathBuf, ProjectInfo>,
        fs: Option<Arc<dyn fs::Fs>>,
    ) -> Self {
        let mut this = Self {
            workspace,
            focus_handle,
            open,
            recent: Vec::new(),
            rows: Vec::new(),
            shown: Vec::new(),
            info,
            fs,
            selected_index: 0,
        };
        this.rebuild("");
        this
    }

    fn set_recent(&mut self, recent: Vec<RecentWorkspace>) {
        self.recent = recent;
    }

    /// The row list for an empty query; `shown` is then filtered by [`Self::rebuild`].
    fn rebuild(&mut self, query: &str) {
        self.rows = entries(self.open.clone(), self.recent.clone());
        self.rows.push(Entry::OpenRemote);
        self.rows.push(Entry::More);
        let query = query.trim();
        self.shown = if query.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.filter(query)
        };
        self.selected_index = self
            .shown
            .iter()
            .position(|&ix| !matches!(self.rows[ix], Entry::Header(_)))
            .unwrap_or(0);
    }

    /// Project rows whose names or path match; headers only above a kept row, and no
    /// action rows while searching.
    fn filter(&self, query: &str) -> Vec<usize> {
        let candidates: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| {
                let row = self.row(entry)?;
                Some(StringMatchCandidate::new(
                    ix,
                    &format!("{} {}", row.names, row.path),
                ))
            })
            .collect();
        let matched: std::collections::HashSet<usize> = match_strings(
            &candidates,
            query,
            StringMatchOptions {
                max_results: MaxResults::limit::<100>(),
                ..Default::default()
            },
        )
        .into_iter()
        .map(|hit| hit.candidate_id)
        .collect();
        let mut shown = Vec::new();
        let mut header = None;
        for (ix, entry) in self.rows.iter().enumerate() {
            match entry {
                Entry::Header(_) => header = Some(ix),
                Entry::OpenProject(..) | Entry::Recent(_) if matched.contains(&ix) => {
                    shown.extend(header.take());
                    shown.push(ix);
                }
                _ => {}
            }
        }
        shown
    }

    /// First root of each local project row, for branch detection.
    fn local_roots(&self, max: usize) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter_map(|entry| match entry {
                Entry::OpenProject(key, _) if key.host().is_none() => {
                    key.path_list().ordered_paths().next().cloned()
                }
                Entry::Recent(project)
                    if matches!(project.location, SerializedWorkspaceLocation::Local) =>
                {
                    project.paths.paths().first().cloned()
                }
                _ => None,
            })
            .take(max)
            .collect()
    }

    fn row(&self, entry: &Entry<OpenTarget>) -> Option<Row> {
        let (paths, root, host) = match entry {
            Entry::OpenProject(key, _) => (
                key.path_list().ordered_paths().cloned().collect::<Vec<_>>(),
                key.path_list().ordered_paths().next().cloned(),
                key.host().map(|host| host.display_name()),
            ),
            Entry::Recent(project) => (
                project.identity_paths.ordered_paths().cloned().collect(),
                project.paths.paths().first().cloned(),
                match &project.location {
                    SerializedWorkspaceLocation::Local => None,
                    SerializedWorkspaceLocation::Remote(options) => Some(options.display_name()),
                },
            ),
            _ => return None,
        };
        let names: Vec<String> = paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let names = names.join(", ");
        let mut path = paths
            .iter()
            .map(|path| display_path(path))
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(host) = host.as_deref() {
            path = format!("{host}: {path}");
        }
        let branch = root
            .filter(|_| host.is_none())
            .and_then(|root| self.info.get(&root))
            .and_then(|info| info.branch.clone());
        let label = match &branch {
            Some(branch) => format!("{names}, {path}, branch {branch}"),
            None => format!("{names}, {path}"),
        };
        Some(Row {
            initials: initials(&name_refs).unwrap_or_default(),
            color: badge_color(&names),
            names,
            path,
            branch,
            label,
        })
    }

    /// Closes the popover, then runs `action` from the workspace's focus.
    fn dispatch(
        &self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let focus = self.focus_handle.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            focus.focus(window, cx);
            window.dispatch_action(action, cx);
        });
    }
}

/// The one place a recent project is opened: the open-choice dialog (plan 44, C) plugs
/// in here. `new_window` is the secondary confirm (cmd-enter): no dialog, new window.
pub fn open_recent_paths(
    workspace: &WeakEntity<Workspace>,
    paths: Vec<PathBuf>,
    new_window: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let open_mode = if new_window {
        OpenMode::NewWindow
    } else {
        zenith_welcome::open_mode(cx)
    };
    workspace
        .update(cx, |workspace, cx| {
            workspace
                .open_workspace_for_paths(open_mode, paths, window, cx)
                .detach_and_log_err(cx);
        })
        .log_err();
}

fn activate(target: &OpenTarget, window: &mut Window, cx: &mut App) {
    let workspace = target.workspace.clone();
    if target.window.window_id() == window.window_handle().window_id() {
        if let Some(Some(multi)) = window.root::<MultiWorkspace>() {
            multi.update(cx, |multi, cx| multi.activate(workspace, None, window, cx));
        }
        return;
    }
    target
        .window
        .update(cx, |multi, window, cx| {
            multi.activate(workspace, None, window, cx);
            window.activate_window();
        })
        .log_err();
}

impl PickerDelegate for SwitcherDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "zenith project switcher"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn match_count(&self) -> usize {
        self.shown.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        self.shown
            .get(ix)
            .is_some_and(|&row| !matches!(self.rows[row], Entry::Header(_)))
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        self.shown
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(self.rows[**row], Entry::Clone | Entry::More))
            .map(|(ix, _)| ix)
            .chain(
                self.shown
                    .iter()
                    .position(|&row| matches!(self.rows[row], Entry::OpenRemote))
                    .and_then(|ix| ix.checked_sub(1)),
            )
            .collect()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.rebuild(&query);
        Task::ready(())
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some("No matching projects".into())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .shown
            .get(self.selected_index)
            .map(|&ix| &self.rows[ix])
        else {
            return;
        };
        match entry {
            Entry::Header(_) => {}
            Entry::Open => self.dispatch(
                workspace::Open {
                    create_new_window: None,
                }
                .boxed_clone(),
                window,
                cx,
            ),
            Entry::Clone => self.dispatch(git::Clone.boxed_clone(), window, cx),
            Entry::OpenRemote => {
                self.dispatch(zed_actions::OpenRemote::default().boxed_clone(), window, cx)
            }
            Entry::More => {
                self.dispatch(zed_actions::OpenRecent::default().boxed_clone(), window, cx)
            }
            Entry::OpenProject(_, target) => {
                activate(target, window, cx);
                cx.emit(DismissEvent);
            }
            Entry::Recent(project) => {
                if matches!(project.location, SerializedWorkspaceLocation::Local) {
                    let paths = project.paths.paths().to_vec();
                    open_recent_paths(&self.workspace, paths, secondary, window, cx);
                    cx.emit(DismissEvent);
                } else {
                    // Remote projects need upstream's connection flow.
                    self.dispatch(zed_actions::OpenRecent::default().boxed_clone(), window, cx);
                }
            }
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = &self.rows[*self.shown.get(ix)?];
        let action_row = |icon: IconName, label: &'static str| {
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .aria_role(Role::ListItem)
                .aria_label(label)
                .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(Label::new(label))
                .into_any_element()
        };
        Some(match entry {
            Entry::Open => action_row(IconName::FolderOpen, "Open…"),
            Entry::Clone => action_row(IconName::GitBranchPlus, "Clone Repository…"),
            Entry::OpenRemote => action_row(IconName::Server, "Open Remote…"),
            Entry::More => action_row(IconName::Ellipsis, "More…"),
            Entry::Header(title) => div()
                .id(("zenith-switcher-header", ix))
                .role(Role::Heading)
                .aria_label(*title)
                .px_3()
                .pt_2()
                .pb_1()
                .child(
                    Label::new(*title)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Entry::OpenProject(..) | Entry::Recent(_) => {
                let row = self.row(entry)?;
                ListItem::new(ix)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .aria_role(Role::ListItem)
                    .aria_label(row.label.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(badge(&row.initials, row.color))
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(
                                        Label::new(row.names).weight(FontWeight::BOLD).truncate(),
                                    )
                                    .child(
                                        Label::new(row.path)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .when_some(row.branch, |this, branch| {
                                        this.child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    Icon::new(IconName::GitBranch)
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(branch)
                                                        .size(LabelSize::Small)
                                                        .color(Color::Muted)
                                                        .truncate(),
                                                ),
                                        )
                                    }),
                            ),
                    )
                    .into_any_element()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use std::{cell::RefCell, path::Path, rc::Rc};
    use workspace::ModalView;

    /// Hosts the switcher in the workspace's modal layer: focus and key dispatch as in a window.
    struct TestModal(Entity<ProjectSwitcher>, Subscription);

    impl ModalView for TestModal {}
    impl EventEmitter<DismissEvent> for TestModal {}
    impl Focusable for TestModal {
        fn focus_handle(&self, cx: &App) -> FocusHandle {
            self.0.focus_handle(cx)
        }
    }
    impl Render for TestModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.clone()
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn recent(path: &str) -> RecentWorkspace {
        RecentWorkspace {
            workspace_id: workspace::WorkspaceId::from_i64(1),
            location: SerializedWorkspaceLocation::Local,
            paths: workspace::PathList::new(&[path]),
            identity_paths: workspace::PathList::new(&[path]),
            path_style: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// A workspace on `/code/app`, with the switcher open in its modal layer.
    async fn open_switcher(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let project = Project::test(fs, [Path::new("/code/app")], cx).await;
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        show(&workspace, cx);
        (workspace, cx)
    }

    /// Built outside a workspace update, as the popover is: it reads the workspace.
    fn build(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ProjectSwitcher> {
        let (focus, weak) = workspace.read_with(cx, |w, cx| (w.focus_handle(cx), w.weak_handle()));
        cx.update(|window, cx| project_switcher(weak, vec![], None, focus, window, cx))
    }

    fn show(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        let switcher = build(workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| {
                let forward = cx.subscribe(&switcher, |_, _, _: &DismissEvent, cx| {
                    cx.emit(DismissEvent)
                });
                TestModal(switcher, forward)
            });
        });
        cx.run_until_parked();
    }

    fn switcher(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<ProjectSwitcher> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<TestModal>(cx)
                .expect("switcher open")
                .read(cx)
                .0
                .clone()
        })
    }

    fn picker(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<SwitcherDelegate>> {
        switcher(workspace, cx).read_with(cx, |switcher, _| match &switcher.body {
            Body::Zenith(picker) => picker.clone(),
            Body::Upstream(_) => panic!("upstream body"),
        })
    }

    fn set_recent(workspace: &Entity<Workspace>, paths: &[&str], cx: &mut VisualTestContext) {
        let recent = paths.iter().map(|path| recent(path)).collect();
        switcher(workspace, cx).update_in(cx, |switcher, window, cx| {
            switcher.set_recent(recent, window, cx)
        });
    }

    fn selected(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
        picker(workspace, cx).read_with(cx, |picker, _| picker.delegate.selected_index())
    }

    /// Labels of the rows, headers as `# title`.
    fn rows(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<String> {
        picker(workspace, cx).read_with(cx, |picker, _| {
            let delegate = &picker.delegate;
            delegate
                .shown
                .iter()
                .map(|&ix| match &delegate.rows[ix] {
                    Entry::Open => "Open…".into(),
                    Entry::Clone => "Clone".into(),
                    Entry::OpenRemote => "Remote".into(),
                    Entry::More => "More".into(),
                    Entry::Header(title) => format!("# {title}"),
                    entry => delegate.row(entry).expect("project row").label,
                })
                .collect()
        })
    }

    fn actions_seen(cx: &mut VisualTestContext) -> Rc<RefCell<Vec<&'static str>>> {
        let seen = Rc::new(RefCell::new(Vec::new()));
        cx.update(|_, cx| {
            let log = seen.clone();
            cx.on_action(move |_: &workspace::Open, _| log.borrow_mut().push("workspace::Open"));
            let log = seen.clone();
            cx.on_action(move |_: &git::Clone, _| log.borrow_mut().push("git::Clone"));
        });
        seen
    }

    fn project_open(root: &str, cx: &mut VisualTestContext) -> bool {
        cx.cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .any(|window| {
                    window.read(cx).is_ok_and(|multi| {
                        multi.workspaces().any(|workspace| {
                            workspace
                                .read(cx)
                                .project()
                                .read(cx)
                                .visible_worktrees(cx)
                                .any(|tree| tree.read(cx).abs_path().as_ref() == Path::new(root))
                        })
                    })
                })
        })
    }

    #[gpui::test]
    async fn test_arrows_skip_headers(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = open_switcher(FakeFs::new(cx.executor()), cx).await;
        set_recent(&workspace, &["/code/b", "/code/c"], cx);
        cx.run_until_parked();
        assert_eq!(
            rows(&workspace, cx),
            [
                "Open…",
                "Clone",
                "# Open Projects",
                "app, /code/app",
                "# Recent Projects",
                "b, /code/b",
                "c, /code/c",
                "Remote",
                "More",
            ]
        );
        assert_eq!(selected(&workspace, cx), 0);
        for expected in [1, 3, 5, 6, 7, 8, 0] {
            cx.dispatch_action(menu::SelectNext);
            assert_eq!(selected(&workspace, cx), expected);
        }
        cx.dispatch_action(menu::SelectPrevious);
        assert_eq!(selected(&workspace, cx), 8);
        for expected in [7, 6, 5, 3] {
            cx.dispatch_action(menu::SelectPrevious);
            assert_eq!(selected(&workspace, cx), expected);
        }
    }

    #[gpui::test]
    async fn test_first_two_rows_dispatch_open_and_clone(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = open_switcher(FakeFs::new(cx.executor()), cx).await;
        let seen = actions_seen(cx);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(*seen.borrow(), ["workspace::Open"]);
        // The popover closed itself.
        assert!(workspace.read_with(cx, |w, cx| w.active_modal::<TestModal>(cx).is_none()));

        show(&workspace, cx);
        cx.dispatch_action(menu::SelectNext);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(*seen.borrow(), ["workspace::Open", "git::Clone"]);
    }

    #[gpui::test]
    async fn test_recent_row_shows_branch_and_cached_branch_at_once(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/code",
            serde_json::json!({
                "app": {},
                "b": { ".git": { "HEAD": "ref: refs/heads/master\n" } },
                "c": {},
            }),
        )
        .await;
        cx.update(|cx| {
            cx.set_global(DetectCache(
                [(
                    PathBuf::from("/code/c"),
                    ProjectInfo {
                        framework: None,
                        branch: Some("cached".into()),
                    },
                )]
                .into_iter()
                .collect(),
            ))
        });
        let (workspace, cx) = open_switcher(fs, cx).await;
        // The switcher was built after the cache was set: cached branch before any read.
        set_recent(&workspace, &["/code/b", "/code/c"], cx);
        let before = rows(&workspace, cx);
        assert_eq!(before[5], "b, /code/b");
        assert_eq!(before[6], "c, /code/c, branch cached");
        cx.run_until_parked();
        let after = rows(&workspace, cx);
        assert_eq!(after[5], "b, /code/b, branch master");
        // Detection replaced the cached entry with what is on disk.
        assert_eq!(after[6], "c, /code/c");
    }

    #[gpui::test]
    async fn test_setting_off_builds_the_upstream_body(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = open_switcher(FakeFs::new(cx.executor()), cx).await;
        assert!(matches!(
            switcher(&workspace, cx).read_with(cx, |s, _| matches!(s.body, Body::Zenith(_))),
            true
        ));
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{"zenith": {"title_bar": {"enabled": false}}}"#, cx)
                    .result()
                    .expect("valid settings");
            });
        });
        let upstream =
            build(&workspace, cx).read_with(cx, |s, _| matches!(s.body, Body::Upstream(_)));
        assert!(upstream);
    }

    #[gpui::test]
    async fn test_recent_row_confirm_opens_the_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/code", serde_json::json!({ "app": {}, "b": {}, "c": {} }))
            .await;
        let (workspace, cx) = open_switcher(fs, cx).await;
        set_recent(&workspace, &["/code/b", "/code/c"], cx);
        cx.run_until_parked();
        assert!(!project_open("/code/b", cx));
        for _ in 0..3 {
            cx.dispatch_action(menu::SelectNext);
        }
        assert_eq!(selected(&workspace, cx), 5);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert!(project_open("/code/b", cx));
        assert!(!project_open("/code/c", cx));
    }

    #[gpui::test]
    async fn test_search_drops_action_rows_and_empty_sections(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = open_switcher(FakeFs::new(cx.executor()), cx).await;
        set_recent(&workspace, &["/code/bravo", "/code/charlie"], cx);
        cx.run_until_parked();
        picker(&workspace, cx).update_in(cx, |picker, window, cx| {
            picker.set_query("char", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&workspace, cx),
            ["# Recent Projects", "charlie, /code/charlie"]
        );
        assert_eq!(selected(&workspace, cx), 1);
    }
}
