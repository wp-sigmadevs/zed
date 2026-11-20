use gpui::prelude::*;
use gpui::{
    Action, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Pixels,
    Render, SharedString, Styled, WeakEntity, Window, px,
};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use ui::prelude::*;
use ui::{ButtonStyle, Color, Icon, IconName, Label, LabelCommon};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use super::LaravelIndex;

gpui::actions!(zenith_laravel, [ToggleLaravelPanel]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaravelPanelTab {
    Routes,
    Views,
}

pub struct LaravelPanel {
    focus_handle: FocusHandle,
    index: Arc<RwLock<LaravelIndex>>,
    search_query: String,
    active_tab: LaravelPanelTab,
    workspace: WeakEntity<Workspace>,
}

impl LaravelPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<LaravelIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            index,
            search_query: String::new(),
            active_tab: LaravelPanelTab::Routes,
            workspace,
        }
    }

    pub fn set_tab(&mut self, tab: LaravelPanelTab, cx: &mut Context<Self>) {
        self.active_tab = tab;
        cx.notify();
    }

    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        cx.notify();
    }
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
        7
    }
}

impl Render for LaravelPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let index_guard = read_index(&self.index);
        let query = self.search_query.to_lowercase();
        let active_tab = self.active_tab;

        let content_view = match active_tab {
            LaravelPanelTab::Routes => {
                let matching_routes = index_guard.route_completions("");
                let filtered_routes: Vec<_> = matching_routes
                    .into_iter()
                    .filter_map(|name| index_guard.find_route(&name))
                    .filter(|r| {
                        if query.is_empty() {
                            return true;
                        }
                        r.name.to_lowercase().contains(&query)
                            || r.uri.to_lowercase().contains(&query)
                            || r.action
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&query)
                    })
                    .cloned()
                    .collect();

                v_flex()
                    .id("laravel-routes-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .children(filtered_routes.into_iter().map(|route| {
                        let path = route.file_path.clone();
                        let line = route.line;
                        let workspace = self.workspace.clone();

                        let method_color = match route.method.as_str() {
                            "GET" => Color::Accent,
                            "POST" => Color::Info,
                            "DELETE" => Color::Error,
                            _ => Color::Muted,
                        };

                        h_flex()
                            .id(SharedString::from(format!("route-{}", route.name)))
                            .p_1()
                            .gap_2()
                            .rounded_sm()
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    let path = path.clone();
                                    workspace.update(cx, |workspace, cx| {
                                        workspace
                                            .open_abs_path(path, OpenOptions::default(), window, cx)
                                            .detach_and_log_err(cx);
                                    });
                                }
                            })
                            .child(
                                Label::new(route.method)
                                    .size(LabelSize::XSmall)
                                    .weight(gpui::FontWeight::BOLD)
                                    .color(method_color),
                            )
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        Label::new(route.uri)
                                            .size(LabelSize::Small)
                                            .weight(gpui::FontWeight::MEDIUM),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "name: {} | {}",
                                            route.name,
                                            route.action.as_deref().unwrap_or("closure")
                                        ))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(
                                h_flex().flex_1().justify_end().child(
                                    Label::new(format!(
                                        "{}:{}",
                                        route
                                            .file_path
                                            .file_name()
                                            .map(|f| f.to_string_lossy())
                                            .unwrap_or_default(),
                                        line
                                    ))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                            )
                    }))
                    .into_any_element()
            }
            LaravelPanelTab::Views => {
                let matching_views = index_guard.view_completions("");
                let filtered_views: Vec<_> = matching_views
                    .into_iter()
                    .filter_map(|name| index_guard.find_view(&name))
                    .filter(|v| {
                        if query.is_empty() {
                            return true;
                        }
                        v.dot_name.to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect();

                v_flex()
                    .id("laravel-views-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .children(filtered_views.into_iter().map(|view| {
                        let path = view.file_path.clone();
                        let workspace = self.workspace.clone();

                        h_flex()
                            .id(SharedString::from(format!("view-{}", view.dot_name)))
                            .p_1()
                            .gap_2()
                            .rounded_sm()
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    let path = path.clone();
                                    workspace.update(cx, |workspace, cx| {
                                        workspace
                                            .open_abs_path(path, OpenOptions::default(), window, cx)
                                            .detach_and_log_err(cx);
                                    });
                                }
                            })
                            .child(
                                Icon::new(IconName::FileCode)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(view.dot_name).size(LabelSize::Small))
                            .child(
                                h_flex().flex_1().justify_end().child(
                                    Label::new(if view.is_blade { ".blade.php" } else { ".php" })
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                            )
                    }))
                    .into_any_element()
            }
        };

        v_flex()
            .id("laravel-panel")
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
                    ),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("tab-routes", "Routes")
                            .style(if self.active_tab == LaravelPanelTab::Routes {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_tab(LaravelPanelTab::Routes, cx)
                            })),
                    )
                    .child(
                        Button::new("tab-views", "Views")
                            .style(if self.active_tab == LaravelPanelTab::Views {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_tab(LaravelPanelTab::Views, cx)
                            })),
                    ),
            )
            .child(content_view)
    }
}

/// Reads the shared index; a lock poisoned by a panicked indexer still yields the last index
/// instead of panicking the render.
pub(crate) fn read_index<T>(index: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    index.read().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
