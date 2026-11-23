//! PhpStorm's Remote Host tool window (plan 37 M1b): a host dropdown with `<None>`, a toolbar
//! (collapse, refresh, disconnect, settings), and a lazy tree of the server's root. A directory
//! is listed only when expanded; a file opens as a read-only cached copy.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::Arc,
};

use anyhow::anyhow;
use editor::Editor;
use futures::{FutureExt as _, future::Shared};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render,
    SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions, px,
    uniform_list,
};
use settings::Settings as _;
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconButton, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    DeploySettings, EntryKind, RemoteEntry, ServerConfig, ServerRefused, ServerStore, Transport,
    dialog::{DeploymentDialog, project_root},
    download_entry,
    remote_path::{join_remote, validate_name, validate_remote_path},
    spawn_operation,
};

actions!(
    zenith_deploy,
    [
        /// Toggles focus on the Remote Host tool window.
        ToggleFocus,
        /// Lists the expanded remote folders again.
        Refresh,
        /// Closes the connection to the remote host.
        Disconnect,
        /// Collapses every folder in the Remote Host tool window.
        CollapseAll,
    ]
);

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RemoteHostPanel>(window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| RemoteHostPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

type Session = Arc<smol::lock::Mutex<Box<dyn Transport>>>;
type SharedSession = Shared<Task<Result<Session, SharedString>>>;

#[derive(Clone, Debug)]
struct Row {
    /// The remote folder the entry is in.
    directory: String,
    entry: RemoteEntry,
    depth: usize,
}

impl Row {
    fn path(&self) -> Option<String> {
        join_remote(&self.directory, &self.entry.name).ok()
    }
}

pub struct RemoteHostPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    store: Entity<ServerStore>,
    project: Option<String>,
    server_id: Option<String>,
    /// The config the open session was made with; a change in the dialog reconnects.
    connected_config: Option<ServerConfig>,
    picked_default: bool,
    session: Option<SharedSession>,
    /// Listed folders by remote path. A folder absent here has never been listed.
    children: HashMap<String, Vec<RemoteEntry>>,
    expanded: HashSet<String>,
    loading: HashSet<String>,
    rows: Vec<Row>,
    selected: Option<usize>,
    status: Option<SharedString>,
    /// The host dropdown's menu; dropped when the servers or the selection change.
    host_menu: Option<Entity<ContextMenu>>,
    active: bool,
    scroll_handle: UniformListScrollHandle,
    /// Running operations by key; dropping one cancels it.
    operations: HashMap<String, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl RemoteHostPanel {
    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ServerStore::global(cx);
        let subscriptions = vec![cx.observe_in(&store, window, |this, _, _, cx| {
            this.store_changed(cx);
        })];
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.weak_handle(),
            store,
            project: project_root(workspace, cx),
            server_id: None,
            connected_config: None,
            picked_default: false,
            session: None,
            children: HashMap::new(),
            expanded: HashSet::new(),
            loading: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            status: None,
            host_menu: None,
            active: false,
            scroll_handle: UniformListScrollHandle::new(),
            operations: HashMap::new(),
            _subscriptions: subscriptions,
        };
        this.store_changed(cx);
        this
    }

    /// The servers the dropdown offers after `<None>`.
    pub(crate) fn host_options(&self, cx: &App) -> Vec<ServerConfig> {
        self.store.read(cx).servers_for(self.project.as_deref())
    }

    fn server(&self, cx: &App) -> Option<ServerConfig> {
        let id = self.server_id.as_ref()?;
        self.store.read(cx).server_for(self.project.as_deref(), id)
    }

    fn root(&self, cx: &App) -> Option<String> {
        let server = self.server(cx)?;
        let root = if server.root_path.trim().is_empty() {
            "/".to_string()
        } else {
            server.root_path.trim().to_string()
        };
        Some(root)
    }

    fn store_changed(&mut self, cx: &mut Context<Self>) {
        self.host_menu = None;
        let store = self.store.read(cx);
        if !store.is_loaded() {
            return;
        }
        if !self.picked_default {
            self.picked_default = true;
            self.server_id = self
                .project
                .as_deref()
                .and_then(|project| store.default_for(project))
                .map(String::from);
        }
        let current = self.server(cx);
        match (&current, &self.connected_config) {
            (None, _) if self.server_id.is_some() => self.select_server(None, cx),
            (Some(current), Some(connected)) if current != connected => {
                self.reset(cx);
                if self.active {
                    self.list_root(cx);
                }
            }
            _ => cx.notify(),
        }
    }

    /// Picks a host from the dropdown; `None` is `<None>`.
    pub(crate) fn select_server(&mut self, server_id: Option<String>, cx: &mut Context<Self>) {
        self.host_menu = None;
        self.reset(cx);
        self.server_id = server_id;
        if self.server_id.is_some() {
            self.list_root(cx);
        }
        cx.notify();
    }

    /// Drops the session and the tree. Running operations are cancelled with their tasks.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.operations.clear();
        if let Some(session) = self.session.take() {
            // Only a session that finished connecting is told to quit.
            if let Some(Ok(session)) = session.peek().cloned() {
                spawn_operation(cx, async move { session.lock().await.disconnect().await })
                    .detach_and_log_err(cx);
            }
        }
        self.connected_config = None;
        self.children.clear();
        self.expanded.clear();
        self.loading.clear();
        self.rows.clear();
        self.selected = None;
        self.status = None;
        cx.notify();
    }

    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.reset(cx);
        if let Some(server_id) = &self.server_id {
            self.store.read(cx).clear_cache(server_id, cx);
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root(cx) else {
            return;
        };
        let expanded: Vec<String> = self
            .expanded
            .iter()
            .filter(|path| self.children.contains_key(*path))
            .cloned()
            .collect();
        self.list(root, cx);
        for path in expanded {
            self.list(path, cx);
        }
    }

    pub(crate) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.rebuild_rows(cx);
    }

    fn list_root(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.root(cx) {
            self.list(root, cx);
        }
    }

    fn session(&mut self, cx: &mut Context<Self>) -> Option<SharedSession> {
        if let Some(session) = &self.session {
            return Some(session.clone());
        }
        let server = self.server(cx)?;
        self.connected_config = Some(server.clone());
        let open = self.store.read(cx).open_session(server, cx);
        let session = cx
            .spawn(async move |_, _| {
                open.await
                    .map(|transport| Arc::new(smol::lock::Mutex::new(transport)))
                    .map_err(|error| SharedString::from(format!("{error:#}")))
            })
            .shared();
        self.session = Some(session.clone());
        Some(session)
    }

    fn fail(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        // A refusal (550 on one folder) leaves the connection good. A timeout or I/O error can
        // leave it mid-command: start over.
        if error.downcast_ref::<ServerRefused>().is_none() {
            self.session = None;
            self.connected_config = None;
        }
        self.status = Some(format!("{error:#}").into());
        cx.notify();
    }

    fn list(&mut self, path: String, cx: &mut Context<Self>) {
        if validate_remote_path(&path).is_err() {
            self.status = Some(format!("{path:?} is not an absolute remote path").into());
            cx.notify();
            return;
        }
        let Some(session) = self.session(cx) else {
            return;
        };
        self.loading.insert(path.clone());
        cx.notify();
        let task = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let result = async {
                    let session = session.await.map_err(|error| anyhow!(error))?;
                    let path = path.clone();
                    cx.update(|cx| {
                        spawn_operation(
                            cx,
                            async move { session.lock().await.list_dir(&path).await },
                        )
                    })
                    .await
                }
                .await;
                this.update(cx, |this, cx| {
                    this.loading.remove(&path);
                    match result {
                        Ok(mut entries) => {
                            entries.sort_by(|a, b| {
                                (a.kind != EntryKind::Directory, &a.name)
                                    .cmp(&(b.kind != EntryKind::Directory, &b.name))
                            });
                            this.children.insert(path, entries);
                            this.status = None;
                            this.rebuild_rows(cx);
                        }
                        Err(error) => {
                            this.expanded.remove(&path);
                            this.rebuild_rows(cx);
                            this.fail(error, cx);
                        }
                    }
                })
                .log_err();
            }
        });
        self.operations.insert(format!("list:{path}"), task);
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        if let Some(root) = self.root(cx) {
            self.push_rows(&root, 0, &mut rows);
        }
        self.rows = rows;
        if self
            .selected
            .is_some_and(|selected| selected >= self.rows.len())
        {
            self.selected = self.rows.len().checked_sub(1);
        }
        cx.notify();
    }

    fn push_rows(&self, directory: &str, depth: usize, rows: &mut Vec<Row>) {
        let Some(entries) = self.children.get(directory) else {
            return;
        };
        for entry in entries {
            let row = Row {
                directory: directory.to_string(),
                entry: entry.clone(),
                depth,
            };
            let path = row.path();
            rows.push(row);
            if let Some(path) = path
                && entry.kind == EntryKind::Directory
                && self.expanded.contains(&path)
            {
                self.push_rows(&path, depth + 1, rows);
            }
        }
    }

    /// Expands or collapses a folder (listing it the first time), or opens a file.
    pub(crate) fn activate_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(index).cloned() else {
            return;
        };
        self.selected = Some(index);
        let Some(path) = row.path() else {
            return;
        };
        match row.entry.kind {
            EntryKind::Directory => {
                if !self.expanded.remove(&path) {
                    self.expanded.insert(path.clone());
                    if !self.children.contains_key(&path) {
                        self.list(path, cx);
                    }
                }
                self.rebuild_rows(cx);
            }
            EntryKind::File => self.open_file(row, window, cx),
            EntryKind::Symlink => {
                self.status = Some("Symbolic links are not followed.".into());
                cx.notify();
            }
        }
    }

    fn open_file(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.server(cx) else {
            return;
        };
        if validate_name(&server.id).is_err() {
            self.status = Some("The server has an invalid id.".into());
            cx.notify();
            return;
        }
        let cache_root = self.store.read(cx).cache_dir.clone();
        let local_directory = cache_root
            .join(&server.id)
            .join(row.directory.trim_start_matches('/'));
        let Some(session) = self.session(cx) else {
            return;
        };
        let workspace = self.workspace.clone();
        let key = format!("open:{}", row.path().unwrap_or_default());
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let session = session.await.map_err(|error| anyhow!(error))?;
                let path = cx
                    .update(|_, cx| {
                        spawn_operation(cx, async move {
                            create_private_dir(&cache_root, &local_directory)?;
                            let mut transport = session.lock().await;
                            download_entry(
                                transport.as_mut(),
                                &row.directory,
                                &row.entry,
                                &local_directory,
                            )
                            .await
                        })
                    })?
                    .await?;
                let open = workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        path,
                        OpenOptions {
                            visible: Some(OpenVisible::None),
                            ..OpenOptions::default()
                        },
                        window,
                        cx,
                    )
                })?;
                if let Some(editor) = open.await?.downcast::<Editor>() {
                    cx.update(|_, cx| editor.update(cx, |editor, _| editor.set_read_only(true)))?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| this.fail(error, cx)).log_err();
            }
        });
        self.operations.insert(key, task);
    }

    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let server_id = self.server_id.clone();
        // The click arrives while the panel renders inside the workspace; open after it.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    DeploymentDialog::open(workspace, server_id, window, cx)
                })
                .log_err();
        });
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let next = self
            .selected
            .map_or(0, |selected| (selected + 1).min(self.rows.len() - 1));
        self.selected = Some(next);
        self.scroll_handle
            .scroll_to_item(next, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rows.is_empty() {
            return;
        }
        let previous = self
            .selected
            .map_or(0, |selected| selected.saturating_sub(1));
        self.selected = Some(previous);
        self.scroll_handle
            .scroll_to_item(previous, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected {
            self.activate_row(selected, window, cx);
        }
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let path = row.path().unwrap_or_default();
        let expanded = self.expanded.contains(&path);
        let (icon, kind) = match row.entry.kind {
            EntryKind::Directory if expanded => (IconName::FolderOpen, "folder"),
            EntryKind::Directory => (IconName::Folder, "folder"),
            EntryKind::File => (IconName::File, "file"),
            EntryKind::Symlink => (IconName::Link, "symbolic link"),
        };
        let loading = self.loading.contains(&path);
        ListItem::new(SharedString::from(format!("zenith-deploy-row-{path}")))
            .height(px(ROW_HEIGHT))
            .indent_level(row.depth)
            .indent_step_size(px(INDENT))
            .toggle_state(self.selected == Some(index))
            .aria_label(SharedString::from(format!("{} ({kind})", row.entry.name)))
            .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .child(Label::new(row.entry.name.clone()))
            .when(loading, |item| {
                item.end_slot(
                    Label::new("Loading…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.activate_row(index, window, cx)))
            .into_any_element()
    }

    fn host_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        if let Some(menu) = &self.host_menu {
            return menu.clone();
        }
        let options = self.host_options(cx);
        let selected = self.server_id.clone();
        let panel = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let choices = std::iter::once((None, "<None>".to_string())).chain(
                options
                    .into_iter()
                    .map(|server| (Some(server.id), server.name)),
            );
            for (id, name) in choices {
                let panel = panel.clone();
                let toggled = id == selected;
                menu = menu.toggleable_entry(
                    name,
                    toggled,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        panel
                            .update(cx, |panel, cx| panel.select_server(id.clone(), cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        self.host_menu = Some(menu.clone());
        menu
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .server(cx)
            .map(|server| server.name)
            .unwrap_or_else(|| "<None>".into());
        let menu = self.host_menu(window, cx);
        let connected = self.session.is_some();
        let has_server = self.server_id.is_some();
        h_flex()
            .p_1()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                DropdownMenu::new("zenith-deploy-host", label, menu)
                    .style(DropdownStyle::Subtle)
                    .aria_label("Remote host"),
            )
            .child(
                div()
                    .debug_selector(|| "zenith-deploy-open-dialog".into())
                    .child(
                        IconButton::new("zenith-deploy-open-dialog", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Deployment Settings…"))
                            .aria_label("Deployment Settings…")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_dialog(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("zenith-deploy-collapse", IconName::ListCollapse)
                    .icon_size(IconSize::Small)
                    .disabled(self.expanded.is_empty())
                    .tooltip(Tooltip::for_action_title("Collapse All", &CollapseAll))
                    .aria_label("Collapse All")
                    .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(!has_server)
                    .tooltip(Tooltip::for_action_title("Refresh", &Refresh))
                    .aria_label("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-disconnect", IconName::Disconnected)
                    .icon_size(IconSize::Small)
                    .disabled(!connected)
                    .tooltip(Tooltip::for_action_title("Disconnect", &Disconnect))
                    .aria_label("Disconnect")
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
            )
            .child(
                IconButton::new("zenith-deploy-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Deployment Settings…"))
                    .aria_label("Deployment Settings…")
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
    }
}

/// The cache holds copies of remote files (maybe `wp-config.php`): owner-only on unix.
fn create_private_dir(cache_root: &std::path::Path, directory: &PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(cache_root, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = cache_root;
    Ok(())
}

impl Render for RemoteHostPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.rows.is_empty() {
            let message: SharedString = if self.server_id.is_none() {
                "Select a host to browse it.".into()
            } else if !self.loading.is_empty() {
                "Connecting…".into()
            } else {
                "Nothing to show. Refresh to list the root again.".into()
            };
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element()
        } else {
            uniform_list(
                "zenith-deploy-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range
                        .map(|index| this.render_row(index, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .key_context("RemoteHostPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &Disconnect, _, cx| this.disconnect(cx)))
            .on_action(cx.listener(|this, _: &CollapseAll, _, cx| this.collapse_all(cx)))
            .size_full()
            .child(self.render_toolbar(window, cx))
            .when_some(self.status.clone(), |panel, status| {
                panel.child(
                    h_flex()
                        .px_2()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .color(Color::Error)
                                .size(IconSize::Small),
                        )
                        .child(
                            Label::new(status)
                                .color(Color::Error)
                                .size(LabelSize::Small),
                        ),
                )
            })
            .child(body)
    }
}

impl Focusable for RemoteHostPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RemoteHostPanel {}

impl Panel for RemoteHostPanel {
    fn persistent_name() -> &'static str {
        "Remote Host Panel"
    }

    fn panel_key() -> &'static str {
        "RemoteHostPanel"
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        DeploySettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    // ponytail: the dock comes from `zenith.deploy.dock`; dragging the panel does not write it.
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Remote Host")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active && self.rows.is_empty() && self.loading.is_empty() {
            // The dock calls this inside a workspace update; list after it.
            cx.defer_in(window, |this, _, cx| this.list_root(cx));
        }
    }

    fn activation_priority(&self) -> u32 {
        15
    }

    fn enabled(&self, cx: &App) -> bool {
        DeploySettings::get_global(cx).enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Servers,
        test_support::{FakeRemote, init, server, workspace},
    };
    use gpui::{TestAppContext, VisualTestContext};

    struct Fixture<'a> {
        workspace: Entity<Workspace>,
        panel: Entity<RemoteHostPanel>,
        cx: &'a mut VisualTestContext,
        servers: Vec<ServerConfig>,
        cache: tempfile::TempDir,
    }

    fn remote() -> FakeRemote {
        FakeRemote::new(&[
            (
                "/www",
                &[
                    ("index.php", EntryKind::File),
                    ("wp-content", EntryKind::Directory),
                    ("vendor", EntryKind::Directory),
                    ("evil", EntryKind::Symlink),
                ],
            ),
            (
                "/www/wp-content",
                &[("themes", EntryKind::Directory), ("a.css", EntryKind::File)],
            ),
        ])
    }

    async fn fixture<'a>(remote: &FakeRemote, cx: &'a mut TestAppContext) -> Fixture<'a> {
        // Anonymous FTP: no Keychain lookup, so the fake sees every connect.
        let servers: Vec<_> = [
            server("Everywhere", None),
            server("Here", Some("/p")),
            server("Elsewhere", Some("/other")),
        ]
        .into_iter()
        .map(|mut server| {
            server.anonymous = true;
            server
        })
        .collect();
        let mut saved = Servers {
            servers: servers.clone(),
            ..Servers::default()
        };
        saved
            .project_defaults
            .insert("/p".into(), servers[1].id.clone());
        let cache = tempfile::tempdir().expect("tempdir");
        let app_state = init(cx, saved, remote, cache.path().to_path_buf());
        let (workspace, cx) = workspace(&app_state, cx).await;
        let panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<RemoteHostPanel>(cx))
            .expect("the panel is registered");
        Fixture {
            workspace,
            panel,
            cx,
            servers,
            cache,
        }
    }

    fn row_names(fixture: &mut Fixture) -> Vec<(String, usize)> {
        fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .rows
                .iter()
                .map(|row| (row.entry.name.clone(), row.depth))
                .collect()
        })
    }

    fn row_index(fixture: &mut Fixture, name: &str) -> usize {
        fixture.panel.read_with(fixture.cx, |panel, _| {
            panel
                .rows
                .iter()
                .position(|row| row.entry.name == name)
                .expect("row is shown")
        })
    }

    #[gpui::test]
    async fn dropdown_offers_none_and_this_projects_servers_and_starts_on_the_default(
        cx: &mut TestAppContext,
    ) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        let (options, selected) = fixture.panel.read_with(fixture.cx, |panel, cx| {
            let names: Vec<_> = panel
                .host_options(cx)
                .into_iter()
                .map(|server| server.name)
                .collect();
            (names, panel.server_id.clone())
        });
        assert_eq!(
            options,
            ["Everywhere", "Here"],
            "Elsewhere is limited to /other"
        );
        assert_eq!(selected, Some(fixture.servers[1].id.clone()));
        assert!(
            remote.log().is_empty(),
            "nothing connects before the panel is used"
        );

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(None, cx));
        fixture.cx.run_until_parked();
        assert_eq!(
            fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.server_id.clone()),
            None,
            "<None>"
        );
    }

    #[gpui::test]
    async fn tree_lists_a_folder_only_when_it_is_expanded(cx: &mut TestAppContext) {
        let remote = remote();
        let mut fixture = fixture(&remote, cx).await;
        let id = fixture.servers[0].id.clone();
        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.select_server(Some(id), cx));
        fixture.cx.run_until_parked();
        assert_eq!(
            remote.log(),
            ["connect everywhere.example.com password=None", "list /www"]
        );
        assert_eq!(
            row_names(&mut fixture),
            [
                ("vendor".to_string(), 0),
                ("wp-content".to_string(), 0),
                ("evil".to_string(), 0),
                ("index.php".to_string(), 0)
            ]
        );

        let wp_content = row_index(&mut fixture, "wp-content");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(wp_content, window, cx)
        });
        fixture.cx.run_until_parked();
        assert_eq!(
            remote.log().last().map(String::as_str),
            Some("list /www/wp-content")
        );
        assert!(row_names(&mut fixture).contains(&("themes".to_string(), 1)));

        // Collapsing and expanding again uses the listing already fetched.
        for _ in 0..2 {
            let wp_content = row_index(&mut fixture, "wp-content");
            fixture.panel.update_in(fixture.cx, |panel, window, cx| {
                panel.activate_row(wp_content, window, cx)
            });
            fixture.cx.run_until_parked();
        }
        let lists: Vec<_> = remote
            .log()
            .into_iter()
            .filter(|line| line.starts_with("list"))
            .collect();
        assert_eq!(
            lists,
            ["list /www", "list /www/wp-content"],
            "vendor is never listed"
        );

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.collapse_all(cx));
        assert!(!row_names(&mut fixture).contains(&("themes".to_string(), 1)));

        fixture
            .panel
            .update(fixture.cx, |panel, cx| panel.disconnect(cx));
        fixture.cx.run_until_parked();
        assert_eq!(remote.log().last().map(String::as_str), Some("disconnect"));
        assert!(row_names(&mut fixture).is_empty());
    }

    #[gpui::test]
    async fn opening_a_file_opens_a_read_only_cached_copy(cx: &mut TestAppContext) {
        let remote = remote();
        let mut fixture = fixture(&remote, cx).await;
        let server = fixture.servers[0].clone();
        fixture.panel.update(fixture.cx, |panel, cx| {
            panel.select_server(Some(server.id.clone()), cx)
        });
        fixture.cx.run_until_parked();
        let cached = fixture
            .cache
            .path()
            .join(&server.id)
            .join("www")
            .join("index.php");
        // The project runs on a fake file system; mirror the copy the panel writes to disk.
        let fs = fixture.workspace.read_with(fixture.cx, |workspace, cx| {
            workspace.project().read(cx).fs().clone()
        });
        fs.as_fake()
            .insert_tree(
                cached.parent().expect("parent"),
                serde_json::json!({ "index.php": "<?php" }),
            )
            .await;

        let index = row_index(&mut fixture, "index.php");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(index, window, cx)
        });
        // The download writes the real cache on smol's blocking pool, outside the test
        // scheduler: give it real time.
        fixture.cx.executor().allow_parking();
        for _ in 0..200 {
            fixture.cx.run_until_parked();
            let opened = fixture.workspace.read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx).is_some()
            });
            if opened {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            remote.log().last().map(String::as_str),
            Some("get /www/index.php")
        );
        assert_eq!(
            std::fs::read_to_string(&cached).expect("cached copy"),
            "<?php"
        );
        let editor = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx)
            })
            .expect("the file opens in an editor");
        assert!(editor.read_with(fixture.cx, |editor, cx| editor.read_only(cx)));
        assert_eq!(
            fixture
                .panel
                .read_with(fixture.cx, |panel, _| panel.status.clone()),
            None
        );

        let evil = row_index(&mut fixture, "evil");
        fixture.panel.update_in(fixture.cx, |panel, window, cx| {
            panel.activate_row(evil, window, cx)
        });
        fixture.cx.run_until_parked();
        assert!(
            !remote.log().iter().any(|line| line == "get /www/evil"),
            "symlinks are never downloaded"
        );
    }

    #[gpui::test]
    async fn the_ellipsis_button_opens_the_deployment_dialog(cx: &mut TestAppContext) {
        let remote = remote();
        let fixture = fixture(&remote, cx).await;
        fixture
            .workspace
            .update_in(fixture.cx, |workspace, window, cx| {
                workspace.focus_panel::<RemoteHostPanel>(window, cx);
            });
        fixture.cx.run_until_parked();
        let bounds = fixture
            .cx
            .debug_bounds("zenith-deploy-open-dialog")
            .expect("the toolbar draws the … button");
        fixture
            .cx
            .simulate_click(bounds.center(), gpui::Modifiers::none());
        fixture.cx.run_until_parked();
        let dialog = fixture
            .workspace
            .read_with(fixture.cx, |workspace, cx| {
                workspace.active_modal::<DeploymentDialog>(cx)
            })
            .expect("the dialog opens");
        let selected = dialog.read_with(fixture.cx, |dialog, _| {
            dialog.selected_server().map(|server| server.id.clone())
        });
        assert_eq!(selected, Some(fixture.servers[1].id.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn the_cache_root_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let cache = root.path().join("cache");
        create_private_dir(&cache, &cache.join("server").join("www")).expect("created");
        let mode = std::fs::metadata(&cache)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
