use std::collections::HashSet;
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

use super::{CallbackTarget, HookKind, WordPressIndex};

gpui::actions!(zenith_wordpress, [ToggleWpHooksPanel]);

pub struct WpHooksPanel {
    focus_handle: FocusHandle,
    index: Arc<RwLock<WordPressIndex>>,
    search_query: String,
    filter_kind: Option<HookKind>,
    expanded_hooks: HashSet<String>,
    workspace: WeakEntity<Workspace>,
}

impl WpHooksPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<WordPressIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            index,
            search_query: String::new(),
            filter_kind: None,
            expanded_hooks: HashSet::new(),
            workspace,
        }
    }

    pub fn toggle_expand(&mut self, hook_name: &str, cx: &mut Context<Self>) {
        if self.expanded_hooks.contains(hook_name) {
            self.expanded_hooks.remove(hook_name);
        } else {
            self.expanded_hooks.insert(hook_name.to_string());
        }
        cx.notify();
    }

    pub fn set_filter_kind(&mut self, kind: Option<HookKind>, cx: &mut Context<Self>) {
        self.filter_kind = kind;
        cx.notify();
    }

    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        cx.notify();
    }
}

impl Focusable for WpHooksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WpHooksPanel {}

impl Panel for WpHooksPanel {
    fn persistent_name() -> &'static str {
        "WpHooksPanel"
    }

    fn panel_key() -> &'static str {
        "wp_hooks_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right | DockPosition::Bottom)
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(340.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::BookCopy)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("WordPress Hooks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleWpHooksPanel)
    }

    fn activation_priority(&self) -> u32 {
        6
    }
}

impl Render for WpHooksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let index_guard = self.index.read().unwrap();
        let query = self.search_query.to_lowercase();
        let filter_kind = self.filter_kind;

        let mut hook_names: Vec<String> = index_guard
            .completions("")
            .into_iter()
            .filter(|c| {
                if let Some(kind) = filter_kind {
                    if c.kind != kind {
                        return false;
                    }
                }
                if !query.is_empty() && !c.name.to_lowercase().contains(&query) {
                    return false;
                }
                true
            })
            .map(|c| c.name)
            .collect();
        hook_names.sort();

        let total_count = hook_names.len();

        v_flex()
            .id("wp-hooks-panel")
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
                            .child(Icon::new(IconName::BookCopy).color(Color::Accent))
                            .child(Label::new("WordPress Hooks").weight(gpui::FontWeight::SEMIBOLD))
                    )
                    .child(
                        Label::new(format!("{total_count} hooks"))
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
                        Button::new("filter-actions", "Actions")
                            .style(if self.filter_kind == Some(HookKind::Action) { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .on_click(cx.listener(|this, _, _, cx| this.set_filter_kind(Some(HookKind::Action), cx)))
                    )
                    .child(
                        Button::new("filter-filters", "Filters")
                            .style(if self.filter_kind == Some(HookKind::Filter) { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                            .on_click(cx.listener(|this, _, _, cx| this.set_filter_kind(Some(HookKind::Filter), cx)))
                    )
            )
            .child(
                v_flex()
                    .id("hook-scroll-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .children(hook_names.into_iter().map(|hook_name| {
                        let is_expanded = self.expanded_hooks.contains(&hook_name);
                        let listeners = index_guard.find_listeners(&hook_name);
                        let defs = index_guard.find_definitions(&hook_name);
                        let listener_count = listeners.len();
                        let def_count = defs.len();

                        let hook_name_clone = hook_name.clone();
                        let chevron_icon = if is_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        };

                        let mut hook_item = v_flex()
                            .id(SharedString::from(format!("hook-{hook_name}")))
                            .rounded_md()
                            .child(
                                h_flex()
                                    .id(SharedString::from(format!("hook-header-{hook_name}")))
                                    .p_1()
                                    .gap_1()
                                    .items_center()
                                    .hover(|s| s.bg(cx.theme().colors().element_hover))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_expand(&hook_name_clone, cx);
                                    }))
                                    .child(Icon::new(chevron_icon).size(IconSize::Small).color(Color::Muted))
                                    .child(Label::new(hook_name.clone()).weight(gpui::FontWeight::MEDIUM))
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .justify_end()
                                            .gap_1()
                                            .child(
                                                Label::new(format!("{listener_count}L / {def_count}D"))
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted)
                                            )
                                    )
                            );

                        if is_expanded {
                            let mut details = v_flex().pl_4().pr_1().gap_1();

                            for def in defs {
                                let path = def.file_path.clone();
                                let line = def.line;
                                let workspace = self.workspace.clone();

                                details = details.child(
                                    h_flex()
                                        .id(SharedString::from(format!("def-{}-{line}", def.name)))
                                        .p_1()
                                        .gap_1()
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
                                        .child(Label::new("[def]").size(LabelSize::XSmall).color(Color::Accent))
                                        .child(
                                            Label::new(format!("{}:{}", def.file_path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(), line))
                                                .size(LabelSize::Small)
                                                .color(Color::Default)
                                        )
                                );
                            }

                            for listener in listeners {
                                let callback_str = match &listener.callback {
                                    CallbackTarget::Function(f) => format!("{f}()"),
                                    CallbackTarget::Method { class_name, method_name, is_static } => {
                                        let sep = if *is_static { "::" } else { "->" };
                                        format!("{}{sep}{method_name}()", class_name.as_deref().unwrap_or("$this"))
                                    }
                                    CallbackTarget::Closure => "Closure".to_string(),
                                    CallbackTarget::Unknown(u) => u.clone(),
                                };

                                let path = listener.file_path.clone();
                                let line = listener.line;
                                let workspace = self.workspace.clone();

                                details = details.child(
                                    h_flex()
                                        .id(SharedString::from(format!("listener-{}-{line}", listener.hook_name)))
                                        .p_1()
                                        .gap_1()
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
                                        .child(Label::new(format!("[{}]", listener.priority)).size(LabelSize::XSmall).color(Color::Muted))
                                        .child(Label::new(callback_str).size(LabelSize::Small).color(Color::Default))
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .justify_end()
                                                .child(
                                                    Label::new(format!("{}:{}", listener.file_path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(), line))
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted)
                                                )
                                        )
                                );
                            }

                            hook_item = hook_item.child(details);
                        }

                        hook_item
                    }))
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn test_wp_hooks_panel_state(cx: &mut App) {
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        let panel = cx.new(|cx| {
            WpHooksPanel {
                focus_handle: cx.focus_handle(),
                index,
                search_query: String::new(),
                filter_kind: None,
                expanded_hooks: HashSet::new(),
                workspace: WeakEntity::new_invalid(),
            }
        });

        panel.update(cx, |this, cx| {
            assert_eq!(this.filter_kind, None);
            this.set_filter_kind(Some(HookKind::Action), cx);
            assert_eq!(this.filter_kind, Some(HookKind::Action));

            assert_eq!(this.search_query, "");
            this.set_search_query("admin_".to_string(), cx);
            assert_eq!(this.search_query, "admin_");

            assert!(!this.expanded_hooks.contains("init"));
            this.toggle_expand("init", cx);
            assert!(this.expanded_hooks.contains("init"));
            this.toggle_expand("init", cx);
            assert!(!this.expanded_hooks.contains("init"));
        });
    }
}
