use std::sync::{Arc, RwLock};
use gpui::prelude::*;
use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, SharedString, Styled, WeakEntity,
    Window, px,
};
use ui::prelude::*;
use ui::{ButtonStyle, Color, Icon, IconName, Label, LabelCommon};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use super::{AppRouterKind, NextJsIndex};

gpui::actions!(zenith_nextjs, [ToggleNextJsPanel]);

pub struct NextJsPanel {
    focus_handle: FocusHandle,
    index: Arc<RwLock<NextJsIndex>>,
    search_query: String,
    filter_kind: Option<AppRouterKind>,
    workspace: WeakEntity<Workspace>,
}

impl NextJsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<NextJsIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            index,
            search_query: String::new(),
            filter_kind: None,
            workspace,
        }
    }

    pub fn set_filter_kind(&mut self, kind: Option<AppRouterKind>, cx: &mut Context<Self>) {
        self.filter_kind = kind;
        cx.notify();
    }

    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        cx.notify();
    }
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
        matches!(position, DockPosition::Left | DockPosition::Right | DockPosition::Bottom)
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {}

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
        8
    }
}

impl Render for NextJsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let index_guard = self.index.read().unwrap();
        let query = self.search_query.to_lowercase();
        let filter_kind = self.filter_kind;

        let all_paths = index_guard.route_completions("");
        let mut routes = Vec::new();

        for path in all_paths {
            for route in index_guard.find_routes_for_path(&path) {
                if let Some(kind) = filter_kind {
                    if route.kind != kind {
                        continue;
                    }
                }
                if !query.is_empty() && !route.url_path.to_lowercase().contains(&query) {
                    continue;
                }
                routes.push(route.clone());
            }
        }

        let route_count = routes.len();

        v_flex()
            .id("nextjs-panel")
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
                            .child(Label::new("Next.js App Router").weight(gpui::FontWeight::SEMIBOLD))
                    )
                    .child(
                        Label::new(format!("{route_count} routes"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    )
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("filter-all", "All")
                            .style(if self.filter_kind.is_none() { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .on_click(cx.listener(|this, _, _, cx| this.set_filter_kind(None, cx)))
                    )
                    .child(
                        Button::new("filter-pages", "Pages")
                            .style(if self.filter_kind == Some(AppRouterKind::Page) { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .on_click(cx.listener(|this, _, _, cx| this.set_filter_kind(Some(AppRouterKind::Page), cx)))
                    )
                    .child(
                        Button::new("filter-routes", "APIs")
                            .style(if self.filter_kind == Some(AppRouterKind::RouteHandler) { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .on_click(cx.listener(|this, _, _, cx| this.set_filter_kind(Some(AppRouterKind::RouteHandler), cx)))
                    )
            )
            .child(
                v_flex()
                    .id("nextjs-routes-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .children(routes.into_iter().map(|route| {
                        let path = route.file_path.clone();
                        let workspace = self.workspace.clone();

                        let (badge_text, badge_color) = match route.kind {
                            AppRouterKind::Page => ("PAGE", Color::Accent),
                            AppRouterKind::RouteHandler => ("API", Color::Info),
                            AppRouterKind::Layout => ("LAYOUT", Color::Muted),
                            AppRouterKind::Loading => ("LOADING", Color::Muted),
                            AppRouterKind::Error => ("ERROR", Color::Error),
                            AppRouterKind::NotFound => ("404", Color::Warning),
                            AppRouterKind::Template => ("TEMPLATE", Color::Muted),
                        };

                        let http_methods_str = if !route.http_methods.is_empty() {
                            format!(" [{}]", route.http_methods.join(", "))
                        } else {
                            String::new()
                        };

                        h_flex()
                            .id(SharedString::from(format!("route-{}-{:?}", route.url_path, route.kind)))
                            .p_1()
                            .gap_2()
                            .rounded_sm()
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    let path = path.clone();
                                    workspace.update(cx, |workspace, cx| {
                                        workspace.open_abs_path(path, OpenOptions::default(), window, cx).detach_and_log_err(cx);
                                    });
                                }
                            })
                            .child(
                                Label::new(badge_text)
                                    .size(LabelSize::XSmall)
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(badge_color)
                            )
                            .child(
                                Label::new(format!("{}{http_methods_str}", route.url_path))
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::MEDIUM)
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .justify_end()
                                    .child(
                                        Label::new(route.file_path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                    )
                            )
                    }))
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn test_nextjs_panel_state(cx: &mut App) {
        let index = Arc::new(RwLock::new(NextJsIndex::new()));
        let panel = cx.new(|cx| {
            NextJsPanel::new(WeakEntity::new_invalid(), index, cx)
        });

        panel.update(cx, |this, cx| {
            assert_eq!(this.filter_kind, None);
            this.set_filter_kind(Some(AppRouterKind::Page), cx);
            assert_eq!(this.filter_kind, Some(AppRouterKind::Page));

            assert_eq!(this.search_query, "");
            this.set_search_query("about".to_string(), cx);
            assert_eq!(this.search_query, "about");
        });
    }
}
