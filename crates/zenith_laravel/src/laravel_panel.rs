use editor::Editor;
use gpui::prelude::*;
use gpui::{
    Action, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Role, ScrollStrategy, SharedString, Styled,
    UniformListScrollHandle, WeakEntity, Window, px, uniform_list,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use text::Point;
use ui::prelude::*;
use ui::{ButtonStyle, Color, Icon, IconName, Label, LabelCommon, ListItem, ListItemSpacing};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use super::{LaravelIndex, RouteDefinition, ViewDefinition};

gpui::actions!(
    zenith_laravel,
    [
        /// Toggles focus on the Laravel panel.
        ToggleLaravelPanel,
        /// Shows the named routes in the Laravel panel.
        ShowRoutes,
        /// Shows the views in the Laravel panel.
        ShowViews
    ]
);

/// Every row of the list has this height, which is what lets `uniform_list` draw only what is
/// on screen.
const ROW_HEIGHT: f32 = 26.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaravelPanelTab {
    Routes,
    Views,
}

pub struct LaravelPanel {
    focus_handle: FocusHandle,
    pub(crate) index: Arc<RwLock<LaravelIndex>>,
    search_query: String,
    active_tab: LaravelPanelTab,
    workspace: WeakEntity<Workspace>,
    /// Every route and view of the index, sorted by name. Rebuilt when the index changes, not
    /// per frame.
    routes: Vec<RouteDefinition>,
    views: Vec<ViewDefinition>,
    /// What the list shows: positions in `routes` or `views` (whichever tab is active) that pass
    /// the query.
    pub(crate) rows: Vec<usize>,
    /// The keyboard-selected line of `rows`.
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
}

impl LaravelPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<LaravelIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            index,
            search_query: String::new(),
            active_tab: LaravelPanelTab::Routes,
            workspace,
            routes: Vec::new(),
            views: Vec::new(),
            rows: Vec::new(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
        };
        panel.reload();
        panel
    }

    /// The index changed (the indexer finished a round): take its routes and views again.
    pub(crate) fn index_changed(&mut self, cx: &mut Context<Self>) {
        self.reload();
        cx.notify();
    }

    fn reload(&mut self) {
        let selected = self.selected_key();
        {
            let index = read_index(&self.index);
            self.routes = index.routes_sorted();
            self.views = index.views_sorted();
        }
        self.build_rows(selected);
    }

    pub fn set_tab(&mut self, tab: LaravelPanelTab, cx: &mut Context<Self>) {
        if self.active_tab != tab {
            self.active_tab = tab;
            // A selection of the other tab means nothing here.
            self.selected = None;
            self.build_rows(None);
        }
        cx.notify();
    }

    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        let selected = self.selected_key();
        self.build_rows(selected);
        cx.notify();
    }

    /// Who the selected line is (a route's name, a view's dot name), so the selection can follow
    /// it when `rows` changes.
    fn selected_key(&self) -> Option<String> {
        let ix = *self.rows.get(self.selected?)?;
        Some(self.key_at(ix).to_string())
    }

    fn key_at(&self, ix: usize) -> &str {
        match self.active_tab {
            LaravelPanelTab::Routes => &self.routes[ix].name,
            LaravelPanelTab::Views => &self.views[ix].dot_name,
        }
    }

    /// Rebuilds `rows`, then selects `selected` again: its own line, else (it is gone) the old
    /// position kept within the list.
    fn build_rows(&mut self, selected: Option<String>) {
        let query = self.search_query.to_lowercase();
        let contains = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        self.rows = match self.active_tab {
            LaravelPanelTab::Routes => self
                .routes
                .iter()
                .enumerate()
                .filter(|(_, route)| {
                    contains(&route.name)
                        || contains(&route.uri)
                        || contains(route.action.as_deref().unwrap_or(""))
                })
                .map(|(ix, _)| ix)
                .collect(),
            LaravelPanelTab::Views => self
                .views
                .iter()
                .enumerate()
                .filter(|(_, view)| contains(&view.dot_name))
                .map(|(ix, _)| ix)
                .collect(),
        };
        let previous = self.selected;
        self.selected = selected
            .and_then(|key| self.rows.iter().position(|&ix| self.key_at(ix) == key))
            .or_else(|| {
                let last = self.rows.len().checked_sub(1)?;
                previous.map(|ix| ix.min(last))
            });
        if let Some(ix) = self.selected.filter(|ix| Some(*ix) != previous) {
            self.scroll_handle
                .scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        self.select(self.selected.map_or(0, |ix| (ix + 1).min(last)), cx);
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
        self.select(self.selected.map_or(0, |ix| ix.saturating_sub(1)), cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select(0, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.select(last, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, window, cx);
        }
    }

    /// Escape: hand the keyboard back to the editor.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.focus_center_pane(window, cx));
        }
    }

    /// Enter or click: a route opens its file at its line, a view opens its file.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.select(ix, cx);
        let Some(&row) = self.rows.get(ix) else {
            return;
        };
        match self.active_tab {
            LaravelPanelTab::Routes => {
                let route = &self.routes[row];
                open(
                    &self.workspace,
                    route.file_path.clone(),
                    Some(route.line),
                    window,
                    cx,
                );
            }
            LaravelPanelTab::Views => {
                let view = &self.views[row];
                open(&self.workspace, view.file_path.clone(), None, window, cx);
            }
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self.rows[ix];
        let item = |id: SharedString| {
            ListItem::new(id)
                .spacing(ListItemSpacing::Dense)
                .height(px(ROW_HEIGHT))
                .rounded()
                .toggle_state(self.selected == Some(ix))
                .aria_role(Role::ListItem)
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
        };
        let location = |path: &Path| {
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match self.active_tab {
            LaravelPanelTab::Routes => {
                let route = &self.routes[row];
                let place = format!("{}:{}", location(&route.file_path), route.line);
                item(format!("route-{}", route.name).into())
                    .aria_label(format!(
                        "Route {} {}, name {}, at {place}",
                        route.method, route.uri, route.name
                    ))
                    .start_slot(
                        // The method is spelled out; the colour only adds to it.
                        Label::new(route.method.clone())
                            .size(LabelSize::XSmall)
                            .weight(gpui::FontWeight::BOLD)
                            .color(match route.method.as_str() {
                                "GET" => Color::Accent,
                                "POST" => Color::Info,
                                "DELETE" => Color::Error,
                                _ => Color::Muted,
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new(route.uri.clone()).size(LabelSize::Small))
                            .child(
                                Label::new(route.name.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .end_slot(
                        Label::new(place)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
            LaravelPanelTab::Views => {
                let view = &self.views[row];
                item(format!("view-{}", view.dot_name).into())
                    .aria_label(format!(
                        "View {}, {}",
                        view.dot_name,
                        location(&view.file_path)
                    ))
                    .start_slot(
                        Icon::new(IconName::FileCode)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(view.dot_name.clone()).size(LabelSize::Small))
                    .end_slot(
                        Label::new(if view.is_blade { ".blade.php" } else { ".php" })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Where a 1-based `line` of the index sits in the editor: column 0 of that line.
/// The column stored in the index is a byte offset into a scan that may be stale, so it is
/// not used.
pub(crate) fn line_to_point(line: usize) -> Point {
    Point::new(line.saturating_sub(1).try_into().unwrap_or(u32::MAX), 0)
}

/// Opens `path`, with the cursor on `line` (1-based, as in the index) when there is one.
fn open(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    line: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            let item = open.await?;
            if let (Some(line), Some(editor)) = (line, item.downcast::<Editor>()) {
                cx.update(|window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point(line_to_point(line), window, cx)
                    })
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

impl Focusable for LaravelPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for LaravelPanel {}

impl Panel for LaravelPanel {
    fn persistent_name() -> &'static str {
        "LaravelPanel"
    }

    fn panel_key() -> &'static str {
        "laravel_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Laravel Routes & Views")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleLaravelPanel)
    }

    fn activation_priority(&self) -> u32 {
        ACTIVATION_PRIORITY
    }
}

impl Render for LaravelPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `Button` defaults to `Role::Button`; the toggle state reaches assistive technology as
        // "pressed" and the label says it in words.
        let tab_button =
            |id: &'static str, label: &'static str, what: &'static str, tab: LaravelPanelTab| {
                let active = self.active_tab == tab;
                Button::new(id, label)
                    .style(if active {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Subtle
                    })
                    .toggle_state(active)
                    .aria_label(if active {
                        format!("{what}, selected")
                    } else {
                        what.to_string()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            };
        let (noun, empty) = match self.active_tab {
            LaravelPanelTab::Routes => ("routes", "No named routes found"),
            LaravelPanelTab::Views => ("views", "No views found"),
        };
        v_flex()
            .id("laravel-panel")
            .key_context("LaravelPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(
                cx.listener(|this, _: &ShowRoutes, _, cx| {
                    this.set_tab(LaravelPanelTab::Routes, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShowViews, _, cx| this.set_tab(LaravelPanelTab::Views, cx)),
            )
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .p_2()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Server).color(Color::Accent))
                            .child(Label::new("Laravel").weight(gpui::FontWeight::SEMIBOLD)),
                    )
                    .child(
                        Label::new(format!("{} {noun}", self.rows.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(tab_button(
                        "tab-routes",
                        "Routes",
                        "Show routes",
                        LaravelPanelTab::Routes,
                    ))
                    .child(tab_button(
                        "tab-views",
                        "Views",
                        "Show views",
                        LaravelPanelTab::Views,
                    )),
            )
            .when(self.rows.is_empty(), |this| {
                this.child(
                    div()
                        .p_2()
                        .child(Label::new(empty).size(LabelSize::Small).color(Color::Muted)),
                )
            })
            .child(
                // Only the rows in view are built; an app can have thousands of views.
                uniform_list(
                    "laravel-rows",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .flex_1()
                .px_1(),
            )
    }
}

/// Dock panels need unique priorities (a debug build panics on a clash). Taken: 0 agent,
/// 1 project, 2 terminal, 3 git, 5 collab, 6 outline, 7 debugger, 8 todo, 9 problems,
/// 10 scratches, 11 WordPress hooks, 12 Laravel, 13 Next.js, 99 search, 200 workspace tests.
const ACTIVATION_PRIORITY: u32 = 12;

/// Reads the shared index; a lock poisoned by a panicked indexer still yields the last index
/// instead of panicking the render.
pub(crate) fn read_index<T>(index: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    index.read().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    const WEB: &str = "<?php\nRoute::get('/b', 'C@m')->name('b.two');\nRoute::get('/d', 'C@m')->name('d.four');\n";

    fn index_with(files: &[(&str, &str)]) -> Arc<RwLock<LaravelIndex>> {
        let index = Arc::new(RwLock::new(LaravelIndex::new()));
        for (path, source) in files {
            index
                .write()
                .unwrap()
                .index_route_file(PathBuf::from(path), source);
        }
        index
    }

    fn selected_key(panel: &LaravelPanel) -> Option<String> {
        panel.selected_key()
    }

    #[gpui::test]
    fn test_laravel_panel_state(cx: &mut App) {
        let index = Arc::new(RwLock::new(LaravelIndex::new()));
        let panel = cx.new(|cx| LaravelPanel::new(WeakEntity::new_invalid(), index, cx));

        panel.update(cx, |this, cx| {
            assert_eq!(this.active_tab, LaravelPanelTab::Routes);
            this.set_tab(LaravelPanelTab::Views, cx);
            assert_eq!(this.active_tab, LaravelPanelTab::Views);

            assert_eq!(this.search_query, "");
            this.set_search_query("users".to_string(), cx);
            assert_eq!(this.search_query, "users");
        });
    }

    #[gpui::test]
    fn test_rows_follow_the_index_the_query_and_the_tab(cx: &mut App) {
        let index = index_with(&[("/p/routes/web.php", WEB)]);
        index.write().unwrap().index_view_path(
            Path::new("/p/resources/views"),
            Path::new("/p/resources/views/home.blade.php"),
        );
        let panel = cx.new(|cx| LaravelPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 2));

        // A new route shows up only once the panel is told the index changed.
        index.write().unwrap().index_route_file(
            PathBuf::from("/p/routes/api.php"),
            &WEB.replace("b.two", "a.one"),
        );
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 2));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), 3));

        panel.update(cx, |panel, cx| {
            panel.set_search_query("D.F".into(), cx);
            assert_eq!(panel.rows.len(), 1);
            panel.set_search_query("/b".into(), cx);
            // The uri matches too: a.one and b.two both answer to /b.
            assert_eq!(panel.rows.len(), 2);
            panel.set_search_query(String::new(), cx);
            panel.set_tab(LaravelPanelTab::Views, cx);
            assert_eq!(panel.rows.len(), 1);
        });
    }

    #[gpui::test]
    fn test_selection_follows_its_route_when_the_index_changes(cx: &mut App) {
        let index = index_with(&[("/p/routes/web.php", WEB)]);
        let panel = cx.new(|cx| LaravelPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.update(cx, |panel, _| {
            // Rows: b.two, d.four. Select d.four.
            panel.selected = Some(1);
            assert_eq!(selected_key(panel).as_deref(), Some("d.four"));
        });

        // A route sorting above the selected one arrives: the selected row moves down by one.
        index.write().unwrap().index_route_file(
            PathBuf::from("/p/routes/api.php"),
            &WEB.replace("b.two", "a.one").replace("d.four", "a.two"),
        );
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.rows.len(), 4);
            assert_eq!(panel.selected, Some(3));
            assert_eq!(selected_key(panel).as_deref(), Some("d.four"));
        });

        // The query filters rows out and back: the selection stays on its route.
        panel.update(cx, |panel, cx| {
            panel.set_search_query("d.f".into(), cx);
            assert_eq!((panel.rows.len(), panel.selected), (1, Some(0)));
            panel.set_search_query(String::new(), cx);
            assert_eq!((panel.rows.len(), panel.selected), (4, Some(3)));
        });

        // The selected route goes away: the position is kept within the list.
        index
            .write()
            .unwrap()
            .remove_file(Path::new("/p/routes/web.php"));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!((panel.rows.len(), panel.selected), (2, Some(1)));
        });
    }

    #[gpui::test]
    fn test_selection_follows_its_view_and_is_cleared_by_a_tab_change(cx: &mut App) {
        let index = Arc::new(RwLock::new(LaravelIndex::new()));
        let base = Path::new("/p/resources/views");
        for name in ["b", "c"] {
            index
                .write()
                .unwrap()
                .index_view_path(base, &base.join(format!("{name}.blade.php")));
        }
        // The routes tab has rows too, so a selection kept by mistake would show.
        index
            .write()
            .unwrap()
            .index_route_file(PathBuf::from("/p/routes/web.php"), WEB);
        let panel = cx.new(|cx| LaravelPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.update(cx, |panel, cx| {
            panel.set_tab(LaravelPanelTab::Views, cx);
            panel.selected = Some(1);
            assert_eq!(selected_key(panel).as_deref(), Some("c"));
        });
        index
            .write()
            .unwrap()
            .index_view_path(base, &base.join("a.blade.php"));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.selected, Some(2));
            assert_eq!(selected_key(panel).as_deref(), Some("c"));
        });
        panel.update(cx, |panel, cx| {
            panel.set_tab(LaravelPanelTab::Routes, cx);
            assert_eq!(panel.selected, None);
        });
    }

    struct Fixture {
        workspace: Entity<Workspace>,
        panel: Entity<LaravelPanel>,
    }

    /// A workspace over `/p` (routes `b.two` at line 2 and `d.four` at line 3 of routes/web.php,
    /// view `home`) with the panel added by hand and the keys a keymap file would give it.
    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            let panel = Some("LaravelPanel");
            cx.bind_keys([
                gpui::KeyBinding::new("down", menu::SelectNext, panel),
                gpui::KeyBinding::new("up", menu::SelectPrevious, panel),
                gpui::KeyBinding::new("enter", menu::Confirm, panel),
                gpui::KeyBinding::new("escape", menu::Cancel, panel),
            ]);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/p",
                serde_json::json!({
                    "routes": { "web.php": WEB },
                    "resources": { "views": { "home.blade.php": "<html>\n<body>\n" } },
                }),
            )
            .await;
        let project = project::Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let index = index_with(&[("/p/routes/web.php", WEB)]);
        index.write().unwrap().index_view_path(
            Path::new("/p/resources/views"),
            Path::new("/p/resources/views/home.blade.php"),
        );
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.entity().downgrade();
            let panel = cx.new(|cx| LaravelPanel::new(weak, index, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        (Fixture { workspace, panel }, cx)
    }

    fn selected(fixture: &Fixture, cx: &mut VisualTestContext) -> (Option<usize>, usize) {
        fixture
            .panel
            .read_with(cx, |panel, _| (panel.selected, panel.rows.len()))
    }

    fn cursor(fixture: &Fixture, cx: &mut VisualTestContext) -> (String, Point) {
        let editor = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("a file is open");
        let path = editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| buffer.read(cx).file())
                .map(|file| file.path().as_unix_str().to_string())
                .unwrap_or_default()
        });
        let head = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        (path, head)
    }

    #[gpui::test]
    async fn keyboard_selects_switches_tabs_and_opens(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        // The toggle action puts the keyboard on the panel itself.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<LaravelPanel>(window, cx);
        });
        cx.run_until_parked();
        let focused = |cx: &mut VisualTestContext| {
            fixture.panel.update_in(cx, |panel, window, cx| {
                panel.focus_handle(cx).is_focused(window)
            })
        };
        assert!(focused(cx));

        // Rows: b.two, d.four.
        assert_eq!(selected(&fixture, cx), (None, 2));
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&fixture, cx), (Some(0), 2));
        cx.simulate_keystrokes("down down down up");
        assert_eq!(selected(&fixture, cx), (Some(0), 2));

        // Enter on a route opens its file with the cursor on the route's line (3, 1-based).
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(
            cursor(&fixture, cx),
            ("routes/web.php".to_string(), Point::new(2, 0))
        );

        // Escape hands the keyboard to the editor.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<LaravelPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(focused(cx));
        cx.simulate_keystrokes("escape");
        assert!(!focused(cx));

        // The tab actions switch the list; a view opens at the top.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<LaravelPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(ShowViews);
        assert_eq!(selected(&fixture, cx), (None, 1));
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(
            cursor(&fixture, cx),
            (
                "resources/views/home.blade.php".to_string(),
                Point::new(0, 0)
            )
        );
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<LaravelPanel>(window, cx);
        });
        cx.dispatch_action(ShowRoutes);
        assert_eq!(selected(&fixture, cx), (None, 2));
    }

    /// The shipped Zenith keymap gives the panel its toggle key.
    #[gpui::test]
    fn test_zenith_keymap_binds_the_panel(cx: &mut App) {
        // The file itself, not the asset loader: a dev build reads assets from the checkout that
        // owns the test binary, which is not this one when the target dir is shared.
        let bindings = match settings::KeymapFile::load(
            include_str!("../../../assets/keymaps/zenith.json"),
            cx,
        ) {
            settings::KeymapFileLoadResult::Success { key_bindings }
            | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
            settings::KeymapFileLoadResult::JsonParseFailure { error } => panic!("{error:#}"),
        };
        let bound = |action: &dyn Action, key: &str, context: &str| {
            bindings.iter().any(|binding| {
                binding.action().partial_eq(action)
                    && binding.keystrokes().len() == 1
                    && binding.keystrokes()[0].inner().key == key
                    && binding.predicate().map(|p| p.to_string()).as_deref() == Some(context)
            })
        };
        assert!(bound(&ToggleLaravelPanel, "8", "Workspace"));
    }

    #[test]
    fn test_index_lines_are_one_based_editor_rows_are_zero_based() {
        assert_eq!(line_to_point(1), Point::new(0, 0));
        assert_eq!(line_to_point(42), Point::new(41, 0));
        // A line of 0 never occurs in the index; it must not underflow.
        assert_eq!(line_to_point(0), Point::new(0, 0));
    }

    #[test]
    fn test_read_index_survives_a_poisoned_lock() {
        let index = Arc::new(RwLock::new(7));
        let writer = index.clone();
        std::thread::spawn(move || {
            let _guard = writer.write().unwrap();
            panic!("indexer died");
        })
        .join()
        .unwrap_err();
        assert!(index.is_poisoned());
        assert_eq!(*read_index(&index), 7);
    }

    #[test]
    fn test_activation_priority_is_not_taken_by_another_panel() {
        const TAKEN_BY_OTHERS: [u32; 12] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 99];
        assert!(!TAKEN_BY_OTHERS.contains(&ACTIVATION_PRIORITY));
        assert_ne!(ACTIVATION_PRIORITY, 200);
        assert_eq!(ACTIVATION_PRIORITY, 12);
    }
}
