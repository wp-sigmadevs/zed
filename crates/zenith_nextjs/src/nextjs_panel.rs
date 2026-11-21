use gpui::prelude::*;
use gpui::{
    Action, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Role, ScrollStrategy, SharedString, Styled,
    UniformListScrollHandle, WeakEntity, Window, px, uniform_list,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use ui::prelude::*;
use ui::{ButtonStyle, Color, Icon, IconName, Label, LabelCommon, ListItem, ListItemSpacing};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use super::{AppRoute, AppRouterKind, NextJsIndex};

gpui::actions!(
    zenith_nextjs,
    [
        /// Toggles focus on the Next.js panel.
        ToggleNextJsPanel,
        /// Shows every App Router file in the Next.js panel.
        ShowAllRoutes,
        /// Shows only the pages in the Next.js panel.
        ShowPages,
        /// Shows only the route handlers (API routes) in the Next.js panel.
        ShowRouteHandlers,
        /// Switches the Next.js panel to the previous filter.
        PreviousFilter,
        /// Switches the Next.js panel to the next filter.
        NextFilter
    ]
);

/// Every row of the list has this height, which is what lets `uniform_list` draw only what is
/// on screen.
const ROW_HEIGHT: f32 = 26.;

/// Which kinds of file the panel lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextJsFilter {
    All,
    Pages,
    RouteHandlers,
}

impl NextJsFilter {
    const ORDER: [Self; 3] = [Self::All, Self::Pages, Self::RouteHandlers];

    fn kind(self) -> Option<AppRouterKind> {
        match self {
            Self::All => None,
            Self::Pages => Some(AppRouterKind::Page),
            Self::RouteHandlers => Some(AppRouterKind::RouteHandler),
        }
    }

    /// The next (or previous) filter; the ends wrap round.
    fn step(self, forward: bool) -> Self {
        let n = Self::ORDER.len();
        let ix = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Self::ORDER[(if forward { ix + 1 } else { ix + n - 1 }) % n]
    }
}

pub struct NextJsPanel {
    focus_handle: FocusHandle,
    pub(crate) index: Arc<RwLock<NextJsIndex>>,
    filter: NextJsFilter,
    workspace: WeakEntity<Workspace>,
    /// Every route of the index, sorted. Rebuilt when the index changes, not per frame.
    routes: Vec<AppRoute>,
    /// What the list shows: positions in `routes` that pass the filter.
    pub(crate) rows: Vec<usize>,
    /// The keyboard-selected line of `rows`.
    selected: Option<usize>,
    scroll_handle: UniformListScrollHandle,
}

impl NextJsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<NextJsIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            index,
            filter: NextJsFilter::All,
            workspace,
            routes: Vec::new(),
            rows: Vec::new(),
            selected: None,
            scroll_handle: UniformListScrollHandle::new(),
        };
        panel.reload();
        panel
    }

    /// The index changed (the indexer finished a round): take its routes again.
    pub(crate) fn index_changed(&mut self, cx: &mut Context<Self>) {
        self.reload();
        cx.notify();
    }

    fn reload(&mut self) {
        let selected = self.selected_key();
        self.routes = read_index(&self.index).routes_sorted();
        self.build_rows(selected);
    }

    pub fn set_filter(&mut self, filter: NextJsFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            let selected = self.selected_key();
            self.build_rows(selected);
        }
        cx.notify();
    }

    /// Who the selected line is (its file, which only one route has), so the selection can
    /// follow it when `rows` changes.
    fn selected_key(&self) -> Option<PathBuf> {
        let ix = *self.rows.get(self.selected?)?;
        Some(self.routes[ix].file_path.clone())
    }

    /// Rebuilds `rows`, then selects `selected` again: its own line, else (it is gone) the old
    /// position kept within the list, else (there was none) the first row, so Enter works on
    /// arrival.
    fn build_rows(&mut self, selected: Option<PathBuf>) {
        let kind = self.filter.kind();
        self.rows = self
            .routes
            .iter()
            .enumerate()
            .filter(|(_, route)| kind.is_none_or(|kind| route.kind == kind))
            .map(|(ix, _)| ix)
            .collect();
        let previous = self.selected;
        self.selected = selected
            .and_then(|key| {
                self.rows
                    .iter()
                    .position(|&ix| self.routes[ix].file_path == key)
            })
            .or_else(|| {
                let last = self.rows.len().checked_sub(1)?;
                Some(previous.map_or(0, |ix| ix.min(last)))
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

    /// Enter or click: opens the row's file at the top.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&row) = self.rows.get(ix) else {
            return;
        };
        self.select(ix, cx);
        let path = self.routes[row].file_path.clone();
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            });
        }
    }

    /// A row's element id: the file is unique to the route, so two rows never share one.
    fn row_id(&self, ix: usize) -> SharedString {
        format!("route-{}", self.routes[self.rows[ix]].file_path.display()).into()
    }

    /// What assistive technology reads for a row; the selection is said in words, not only by
    /// the row's colour.
    fn row_label(&self, ix: usize) -> String {
        let route = &self.routes[self.rows[ix]];
        let methods = if route.http_methods.is_empty() {
            String::new()
        } else {
            format!(", methods {}", route.http_methods.join(" "))
        };
        let selected = if self.selected == Some(ix) {
            ", selected"
        } else {
            ""
        };
        format!(
            "{} {}{methods}, {}{selected}",
            kind_name(route.kind),
            route.url_path,
            file_name(&route.file_path)
        )
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let route = &self.routes[self.rows[ix]];
        let (badge_text, badge_color) = match route.kind {
            AppRouterKind::Page => ("PAGE", Color::Accent),
            AppRouterKind::RouteHandler => ("API", Color::Info),
            AppRouterKind::Layout => ("LAYOUT", Color::Muted),
            AppRouterKind::Loading => ("LOADING", Color::Muted),
            AppRouterKind::Error => ("ERROR", Color::Error),
            AppRouterKind::NotFound => ("404", Color::Warning),
            AppRouterKind::Template => ("TEMPLATE", Color::Muted),
        };
        let methods = if route.http_methods.is_empty() {
            String::new()
        } else {
            format!(" [{}]", route.http_methods.join(", "))
        };
        ListItem::new(self.row_id(ix))
            .spacing(ListItemSpacing::Dense)
            .height(px(ROW_HEIGHT))
            .rounded()
            .toggle_state(self.selected == Some(ix))
            .aria_role(Role::ListItem)
            .aria_label(self.row_label(ix))
            .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
            .start_slot(
                // The kind is spelled out; the colour only adds to it.
                Label::new(badge_text)
                    .size(LabelSize::XSmall)
                    .weight(gpui::FontWeight::BOLD)
                    .color(badge_color),
            )
            .child(
                Label::new(format!("{}{methods}", route.url_path))
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .end_slot(
                Label::new(file_name(&route.file_path))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

fn kind_name(kind: AppRouterKind) -> &'static str {
    match kind {
        AppRouterKind::Page => "Page",
        AppRouterKind::Layout => "Layout",
        AppRouterKind::Loading => "Loading",
        AppRouterKind::Error => "Error",
        AppRouterKind::RouteHandler => "API route",
        AppRouterKind::NotFound => "Not found",
        AppRouterKind::Template => "Template",
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Focusable for NextJsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for NextJsPanel {}

impl Panel for NextJsPanel {
    fn persistent_name() -> &'static str {
        "NextJsPanel"
    }

    fn panel_key() -> &'static str {
        "nextjs_panel"
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
        Some(IconName::Code)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Next.js App Router")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleNextJsPanel)
    }

    fn activation_priority(&self) -> u32 {
        ACTIVATION_PRIORITY
    }
}

impl Render for NextJsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `Button` defaults to `Role::Button`; the toggle state reaches assistive technology as
        // "pressed" and the label says it in words.
        let filter_button =
            |id: &'static str, label: &'static str, what: &'static str, filter: NextJsFilter| {
                let active = self.filter == filter;
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
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            };
        v_flex()
            .id("nextjs-panel")
            .key_context("NextJsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(
                cx.listener(|this, _: &ShowAllRoutes, _, cx| {
                    this.set_filter(NextJsFilter::All, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ShowPages, _, cx| this.set_filter(NextJsFilter::Pages, cx)),
            )
            .on_action(cx.listener(|this, _: &ShowRouteHandlers, _, cx| {
                this.set_filter(NextJsFilter::RouteHandlers, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousFilter, _, cx| {
                this.set_filter(this.filter.step(false), cx)
            }))
            .on_action(cx.listener(|this, _: &NextFilter, _, cx| {
                this.set_filter(this.filter.step(true), cx)
            }))
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
                            .child(Icon::new(IconName::Code).color(Color::Accent))
                            .child(
                                Label::new("Next.js App Router").weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(format!("{} routes", self.rows.len()))
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
                    .child(filter_button(
                        "filter-all",
                        "All",
                        "All routes filter",
                        NextJsFilter::All,
                    ))
                    .child(filter_button(
                        "filter-pages",
                        "Pages",
                        "Pages filter",
                        NextJsFilter::Pages,
                    ))
                    .child(filter_button(
                        "filter-routes",
                        "APIs",
                        "API routes filter",
                        NextJsFilter::RouteHandlers,
                    )),
            )
            .when(self.rows.is_empty(), |this| {
                this.child(
                    div().p_2().child(
                        Label::new("No App Router routes found")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(
                // Only the rows in view are built; an app can have thousands of files.
                uniform_list(
                    "nextjs-rows",
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
const ACTIVATION_PRIORITY: u32 = 13;

/// Reads the shared index; a lock poisoned by a panicked indexer still yields the last index
/// instead of panicking the render.
pub(crate) fn read_index<T>(index: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    index.read().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    fn route(url: &str, kind: AppRouterKind, file: &str, methods: &[&str]) -> AppRoute {
        AppRoute {
            url_path: url.to_string(),
            kind,
            file_path: PathBuf::from(file),
            is_dynamic: false,
            dynamic_params: Vec::new(),
            http_methods: methods.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn index_with(routes: Vec<AppRoute>) -> Arc<RwLock<NextJsIndex>> {
        let index = Arc::new(RwLock::new(NextJsIndex::new()));
        for route in routes {
            index.write().unwrap().insert(route);
        }
        index
    }

    /// Rows (sorted by URL then file): /about page, /api/hook route (GET, POST), / layout (the
    /// root), and so on: see the order in the assertions.
    fn sample() -> Vec<AppRoute> {
        vec![
            route("/", AppRouterKind::Layout, "/p/app/layout.tsx", &[]),
            route("/", AppRouterKind::Page, "/p/app/page.tsx", &[]),
            route("/about", AppRouterKind::Page, "/p/app/about/page.tsx", &[]),
            route(
                "/api/hook",
                AppRouterKind::RouteHandler,
                "/p/app/api/hook/route.ts",
                &["GET", "POST"],
            ),
        ]
    }

    fn selected_file(panel: &NextJsPanel) -> Option<String> {
        panel
            .selected_key()
            .map(|key| key.to_string_lossy().into_owned())
    }

    #[gpui::test]
    fn test_nextjs_panel_state(cx: &mut App) {
        let panel =
            cx.new(|cx| NextJsPanel::new(WeakEntity::new_invalid(), index_with(sample()), cx));
        panel.update(cx, |this, cx| {
            assert_eq!((this.filter, this.rows.len()), (NextJsFilter::All, 4));
            this.set_filter(NextJsFilter::Pages, cx);
            assert_eq!((this.filter, this.rows.len()), (NextJsFilter::Pages, 2));
            this.set_filter(NextJsFilter::RouteHandlers, cx);
            assert_eq!(this.rows.len(), 1);
            this.set_filter(NextJsFilter::All, cx);
            assert_eq!(this.rows.len(), 4);
        });
    }

    #[test]
    fn test_filters_cycle_both_ways() {
        use NextJsFilter::*;
        assert_eq!(All.step(true), Pages);
        assert_eq!(Pages.step(true), RouteHandlers);
        assert_eq!(RouteHandlers.step(true), All);
        assert_eq!(All.step(false), RouteHandlers);
        assert_eq!(RouteHandlers.step(false), Pages);
        assert_eq!(Pages.step(false), All);
    }

    #[gpui::test]
    fn test_first_row_is_selected_on_arrival_and_none_when_empty(cx: &mut App) {
        let index = index_with(Vec::new());
        let panel = cx.new(|cx| NextJsPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.read_with(cx, |panel, _| assert_eq!(panel.selected, None));
        index.write().unwrap().insert(sample().remove(2));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.selected, Some(0));
            assert_eq!(
                selected_file(panel).as_deref(),
                Some("/p/app/about/page.tsx")
            );
        });
    }

    #[gpui::test]
    fn test_selection_follows_its_file_across_rebuilds(cx: &mut App) {
        let index = index_with(sample());
        let panel = cx.new(|cx| NextJsPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.update(cx, |panel, _| {
            // Rows: / layout, / page, /about, /api/hook. Select /about.
            panel.selected = Some(2);
            assert_eq!(
                selected_file(panel).as_deref(),
                Some("/p/app/about/page.tsx")
            );
        });

        // A route sorting above it arrives: the selected row moves down by one.
        index
            .write()
            .unwrap()
            .insert(route("/a", AppRouterKind::Page, "/p/app/a/page.tsx", &[]));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.selected, Some(3));
            assert_eq!(
                selected_file(panel).as_deref(),
                Some("/p/app/about/page.tsx")
            );
        });

        // The filter hides rows and shows them again: the selection stays on its file.
        panel.update(cx, |panel, cx| {
            panel.set_filter(NextJsFilter::Pages, cx);
            assert_eq!((panel.rows.len(), panel.selected), (3, Some(2)));
            panel.set_filter(NextJsFilter::All, cx);
            assert_eq!((panel.rows.len(), panel.selected), (5, Some(3)));
        });

        // The selected file goes away: the position is kept within the list.
        index
            .write()
            .unwrap()
            .remove_file(Path::new("/p/app/about/page.tsx"));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!((panel.rows.len(), panel.selected), (4, Some(3)));
        });
    }

    #[gpui::test]
    fn test_two_files_with_one_url_are_both_listed_and_told_apart(cx: &mut App) {
        // `app/` and `src/app/` both give /about; the same name must not merge or share an id.
        let index = index_with(vec![
            route("/about", AppRouterKind::Page, "/p/app/about/page.tsx", &[]),
            route(
                "/about",
                AppRouterKind::Page,
                "/p/src/app/about/page.tsx",
                &[],
            ),
        ]);
        let panel = cx.new(|cx| NextJsPanel::new(WeakEntity::new_invalid(), index, cx));
        panel.update(cx, |panel, _| {
            assert_eq!(panel.rows.len(), 2);
            assert_ne!(panel.row_id(0), panel.row_id(1));
            panel.selected = Some(1);
            assert_eq!(
                selected_file(panel).as_deref(),
                Some("/p/src/app/about/page.tsx")
            );
        });
    }

    #[gpui::test]
    fn test_row_labels_say_kind_methods_file_and_selection(cx: &mut App) {
        let panel =
            cx.new(|cx| NextJsPanel::new(WeakEntity::new_invalid(), index_with(sample()), cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.row_label(0), "Layout /, layout.tsx, selected");
            assert_eq!(panel.row_label(1), "Page /, page.tsx");
            assert_eq!(
                panel.row_label(3),
                "API route /api/hook, methods GET POST, route.ts"
            );
        });
    }

    struct Fixture {
        workspace: Entity<Workspace>,
        panel: Entity<NextJsPanel>,
    }

    /// A workspace over `/p` with the panel added by hand and the keys a keymap file would
    /// give it. Rows: / layout, / page, /about page, /api/hook route.
    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            let panel = Some("NextJsPanel");
            cx.bind_keys([
                gpui::KeyBinding::new("down", menu::SelectNext, panel),
                gpui::KeyBinding::new("up", menu::SelectPrevious, panel),
                gpui::KeyBinding::new("enter", menu::Confirm, panel),
                gpui::KeyBinding::new("escape", menu::Cancel, panel),
                gpui::KeyBinding::new("left", PreviousFilter, panel),
                gpui::KeyBinding::new("right", NextFilter, panel),
            ]);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/p",
                serde_json::json!({
                    "app": {
                        "layout.tsx": "export default function L() {}\n",
                        "page.tsx": "export default function P() {}\n",
                        "about": { "page.tsx": "export default function A() {}\n" },
                        "api": { "hook": { "route.ts": "export async function GET() {}\n" } },
                    },
                }),
            )
            .await;
        let project = project::Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.entity().downgrade();
            let panel = cx.new(|cx| NextJsPanel::new(weak, index_with(sample()), cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        (Fixture { workspace, panel }, cx)
    }

    fn state(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
    ) -> (NextJsFilter, Option<usize>, usize) {
        fixture.panel.read_with(cx, |panel, _| {
            (panel.filter, panel.selected, panel.rows.len())
        })
    }

    fn active_file(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
        let editor = fixture
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<editor::Editor>(cx)
            })
            .expect("a file is open");
        editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| buffer.read(cx).file())
                .map(|file| file.path().as_unix_str().to_string())
                .unwrap_or_default()
        })
    }

    fn focus_panel(fixture: &Fixture, cx: &mut VisualTestContext) {
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<NextJsPanel>(window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn keyboard_selects_filters_opens_and_hands_back(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        // The toggle action puts the keyboard on the panel itself.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<NextJsPanel>(window, cx);
        });
        cx.run_until_parked();
        let focused = |cx: &mut VisualTestContext| {
            fixture.panel.update_in(cx, |panel, window, cx| {
                panel.focus_handle(cx).is_focused(window)
            })
        };
        assert!(focused(cx));

        // The first row is selected on arrival; the ends stop the selection.
        assert_eq!(state(&fixture, cx), (NextJsFilter::All, Some(0), 4));
        cx.simulate_keystrokes("down down down down down");
        assert_eq!(state(&fixture, cx).1, Some(3));
        cx.simulate_keystrokes("up up up up up");
        assert_eq!(state(&fixture, cx).1, Some(0));

        // Enter opens the file at the top.
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(active_file(&fixture, cx), "app/page.tsx");

        // Escape hands the keyboard to the editor.
        focus_panel(&fixture, cx);
        assert!(focused(cx));
        cx.simulate_keystrokes("escape");
        assert!(!focused(cx));

        // Right and left cycle the filter: All -> Pages -> APIs -> All, and back.
        focus_panel(&fixture, cx);
        cx.simulate_keystrokes("right");
        assert_eq!(state(&fixture, cx).0, NextJsFilter::Pages);
        assert_eq!(state(&fixture, cx).2, 2);
        cx.simulate_keystrokes("right");
        assert_eq!(state(&fixture, cx).0, NextJsFilter::RouteHandlers);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(active_file(&fixture, cx), "app/api/hook/route.ts");
        focus_panel(&fixture, cx);
        cx.simulate_keystrokes("right");
        assert_eq!(state(&fixture, cx).0, NextJsFilter::All);
        cx.simulate_keystrokes("left");
        assert_eq!(state(&fixture, cx).0, NextJsFilter::RouteHandlers);
    }

    #[gpui::test]
    async fn filter_actions_set_the_filter(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        focus_panel(&fixture, cx);
        cx.dispatch_action(ShowPages);
        assert_eq!(state(&fixture, cx).0, NextJsFilter::Pages);
        cx.dispatch_action(ShowRouteHandlers);
        assert_eq!(
            state(&fixture, cx),
            (NextJsFilter::RouteHandlers, Some(0), 1)
        );
        cx.dispatch_action(ShowAllRoutes);
        assert_eq!(state(&fixture, cx).0, NextJsFilter::All);
    }

    /// A click on a row that is no longer there (the list shrank) selects nothing.
    #[gpui::test]
    async fn activating_a_missing_row_changes_nothing(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.activate(99, window, cx);
        });
        assert_eq!(state(&fixture, cx), (NextJsFilter::All, Some(0), 4));
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
        const TAKEN_BY_OTHERS: [u32; 13] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 99];
        assert!(!TAKEN_BY_OTHERS.contains(&ACTIVATION_PRIORITY));
        assert_ne!(ACTIVATION_PRIORITY, 200);
        assert_eq!(ACTIVATION_PRIORITY, 13);
    }
}
