//! Tool window rails: a slim icon strip down each side of the window, as in
//! PhpStorm. The left rail holds the left dock's panels at the top and the bottom
//! dock's at the foot; the right rail holds the right dock's panels. They reuse
//! the status bar's panel buttons, which then stop drawing there.

mod tab_bar_end;

pub use tab_bar_end::{draws_tab_bar_end, quick_actions_tab_bar_end, tab_bar_end};

use gpui::{AnyElement, App, Context, Entity, IntoElement, Render, Window};
use settings::{RegisterSetting, Settings, SettingsContent};
use theme::ActiveTheme as _;
use ui::prelude::*;
use workspace::{
    IslandsActive, Workspace,
    dock::{DockPosition, DraggedPanelButton, PanelButtons, RailsActive},
};

/// `zenith.rails.enabled` and `zenith.ui.islands`: read once at init, so a change takes
/// effect after a restart.
#[derive(Clone, Copy, RegisterSetting)]
pub struct RailsSettings {
    pub enabled: bool,
    pub islands: bool,
}

impl Settings for RailsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let zenith = content.zenith.as_ref();
        Self {
            enabled: zenith
                .and_then(|zenith| zenith.rails.as_ref()?.enabled)
                .unwrap_or(true),
            islands: zenith
                .and_then(|zenith| zenith.ui.as_ref()?.islands)
                .unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    let settings = *RailsSettings::get_global(cx);
    if settings.islands {
        cx.set_global(IslandsActive);
    }
    if !settings.enabled {
        return;
    }
    cx.set_global(RailsActive);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let buttons = |position, workspace: &Workspace, cx: &mut Context<Workspace>| {
            let dock = workspace.dock_at_position(position).clone();
            cx.new(|cx| PanelButtons::new(dock, cx).vertical())
        };
        let left = Rail {
            side: DockPosition::Left,
            top: buttons(DockPosition::Left, workspace, cx),
            foot: Some(buttons(DockPosition::Bottom, workspace, cx)),
        };
        let right = Rail {
            side: DockPosition::Right,
            top: buttons(DockPosition::Right, workspace, cx),
            foot: None,
        };
        let (left, right) = (cx.new(|_| left), cx.new(|_| right));
        workspace.set_rails(left.into(), right.into(), cx);
        for pane in workspace.panes() {
            tab_bar_end::install(pane, cx);
        }
        cx.subscribe_in(&cx.entity(), window, |_, _, event, _, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                tab_bar_end::install(pane, cx);
            }
        })
        .detach();
    })
    .detach();
}

struct Rail {
    side: DockPosition,
    top: Entity<PanelButtons>,
    foot: Option<Entity<PanelButtons>>,
}

impl Render for Rail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let left = self.side == DockPosition::Left;
        v_flex()
            .debug_selector(move || {
                if left {
                    "zenith-left-rail".into()
                } else {
                    "zenith-right-rail".into()
                }
            })
            .h_full()
            .w(workspace::RAIL_WIDTH)
            .flex_none()
            // Board: rails sit on the window background, no border; 4 px top, 6 px bottom.
            .pt(px(4.))
            .pb(px(6.))
            .items_center()
            .justify_between()
            .bg(colors.title_bar_background)
            .child(drop_zone(
                self.side,
                "top",
                true,
                self.top.clone().into_any_element(),
                cx,
            ))
            .when(left, |this| {
                this.child(drop_zone(
                    DockPosition::Bottom,
                    "foot",
                    false,
                    v_flex()
                        .items_center()
                        .gap_1()
                        .children(self.foot.clone())
                        .into_any_element(),
                    cx,
                ))
            })
    }
}

/// A stretch of rail that takes a dragged tool-window button and docks its panel at
/// `position`, as PhpStorm moves tool windows between its bars.
fn drop_zone(
    position: DockPosition,
    id: &'static str,
    grow: bool,
    child: AnyElement,
    cx: &App,
) -> impl IntoElement {
    let hint = cx.theme().colors().drop_target_background;
    v_flex()
        .id(SharedString::from(format!("zenith-drop-{position:?}-{id}")))
        .items_center()
        .w_full()
        .when(grow, |this| this.flex_1())
        .drag_over::<DraggedPanelButton>(move |style, _, _, _| style.bg(hint))
        .on_drop(move |dragged: &DraggedPanelButton, window, cx| {
            if dragged.panel.position_is_valid(position, cx) {
                dragged.panel.set_position(position, window, cx);
            }
        })
        .child(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::Project;
    use std::{cell::Cell, rc::Rc};
    use workspace::{AppState, MultiWorkspace};

    async fn open_window(cx: &mut TestAppContext, with_rails: bool) -> gpui::VisualTestContext {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            AppState::set_global(app_state.clone(), cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::init(app_state.clone(), cx);
            editor::init(cx);
            if with_rails {
                init(cx);
            }
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        // An empty pane draws no tab bar.
        multi.update_in(cx, |multi, window, cx| {
            multi.workspace().update(cx, |workspace, cx| {
                let item = cx.new(workspace::item::test::TestItem::new);
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            })
        });
        cx.run_until_parked();
        cx.clone()
    }

    #[gpui::test]
    async fn test_rails_flank_the_workspace(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, true).await;
        let left = cx
            .debug_bounds("zenith-left-rail")
            .expect("left rail is drawn");
        let right = cx
            .debug_bounds("zenith-right-rail")
            .expect("right rail is drawn");
        assert_eq!(left.size.width, workspace::RAIL_WIDTH);
        assert_eq!(right.size.width, workspace::RAIL_WIDTH);
        assert!(left.right() <= right.left());
        assert!(cx.debug_bounds("zenith-center-island").is_some());
    }

    fn set_zenith_settings(cx: &mut TestAppContext, zenith: settings::ZenithSettingsContent) {
        cx.update(|cx| {
            let mut store = settings::SettingsStore::test(cx);
            store.update_user_settings(cx, |content| content.zenith = Some(zenith));
            cx.set_global(store);
        });
    }

    #[gpui::test]
    async fn test_rails_setting_off_draws_no_rails(cx: &mut TestAppContext) {
        set_zenith_settings(
            cx,
            settings::ZenithSettingsContent {
                rails: Some(settings::ZenithRailsSettingsContent {
                    enabled: Some(false),
                }),
                ..Default::default()
            },
        );
        let cx = &mut open_window(cx, true).await;
        assert!(cx.debug_bounds("zenith-left-rail").is_none());
        assert!(cx.debug_bounds("zenith-right-rail").is_none());
        cx.update(|_, cx| {
            assert!(!cx.has_global::<RailsActive>());
            assert!(
                cx.has_global::<IslandsActive>(),
                "islands are their own setting"
            );
        });
    }

    #[gpui::test]
    async fn test_islands_setting_off_keeps_rails(cx: &mut TestAppContext) {
        set_zenith_settings(
            cx,
            settings::ZenithSettingsContent {
                ui: Some(settings::ZenithUiSettingsContent {
                    islands: Some(false),
                }),
                ..Default::default()
            },
        );
        let cx = &mut open_window(cx, true).await;
        assert!(cx.debug_bounds("zenith-left-rail").is_some());
        cx.update(|_, cx| assert!(!cx.has_global::<IslandsActive>()));
    }

    #[gpui::test]
    async fn test_no_rails_unless_enabled(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, false).await;
        assert!(cx.debug_bounds("zenith-left-rail").is_none());
        assert!(cx.debug_bounds("zenith-right-rail").is_none());
        assert!(cx.debug_bounds("zenith-center-island").is_none());
    }

    struct Probe(&'static str);

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let name = self.0;
            div()
                .debug_selector(move || name.into())
                .w(px(20.))
                .h(px(10.))
        }
    }

    fn add_probe(
        cx: &mut gpui::VisualTestContext,
        name: &'static str,
        order: i32,
        visible: Rc<Cell<bool>>,
    ) -> Entity<workspace::Pane> {
        cx.update(|window, cx| {
            let multi = window.root::<MultiWorkspace>().flatten().unwrap();
            let pane = multi.read(cx).workspace().read(cx).active_pane().clone();
            let probe = cx.new(|_| Probe(name));
            pane.update(cx, |pane, cx| {
                pane.toolbar().update(cx, |_, cx| {
                    tab_bar_end(probe, order, move |_, _| visible.get(), cx)
                })
            });
            pane
        })
    }

    #[gpui::test]
    async fn test_tab_bar_end_draws_contributions_in_order(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, true).await;
        add_probe(cx, "probe-right", 1, Rc::new(Cell::new(true)));
        add_probe(cx, "probe-left", 0, Rc::new(Cell::new(true)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let end = cx.debug_bounds("zenith-tab-bar-end").expect("tab bar end");
        let left = cx.debug_bounds("probe-left").expect("left probe");
        let right = cx.debug_bounds("probe-right").expect("right probe");
        assert!(left.right() <= right.left(), "order decides the position");
        assert!(right.right() <= end.right() && end.left() <= left.left());
    }

    #[gpui::test]
    async fn test_tab_bar_end_is_empty_without_rails(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, false).await;
        add_probe(cx, "probe", 0, Rc::new(Cell::new(true)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("probe").is_none());
        assert!(cx.debug_bounds("zenith-tab-bar-end").is_none());
    }

    fn draw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test]
    async fn test_no_end_slot_while_no_view_is_visible(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, true).await;
        let visible = Rc::new(Cell::new(false));
        let pane = add_probe(cx, "probe", 0, visible.clone());
        // What flips the state of the moment (settings, tab change) makes the pane draw again.
        let redraw = |cx: &mut gpui::VisualTestContext| {
            pane.update(cx, |_, cx| cx.notify());
            draw(cx);
        };
        redraw(cx);
        assert!(cx.debug_bounds("probe").is_none());
        assert!(
            cx.debug_bounds("zenith-tab-bar-end").is_none(),
            "no empty strip of ours while every view is hidden"
        );
        // Asked at every draw, so it follows the state of the moment.
        visible.set(true);
        redraw(cx);
        assert!(cx.debug_bounds("probe").is_some());
        assert!(cx.debug_bounds("zenith-tab-bar-end").is_some());
        visible.set(false);
        redraw(cx);
        assert!(cx.debug_bounds("zenith-tab-bar-end").is_none());
    }

    #[gpui::test]
    async fn test_end_is_drawn_only_while_the_tab_bar_and_its_buttons_are_shown(
        cx: &mut TestAppContext,
    ) {
        let cx = &mut open_window(cx, true).await;
        let draws = |cx: &mut gpui::VisualTestContext| cx.update(|_, cx| draws_tab_bar_end(cx));
        assert!(draws(cx));
        for (show, buttons) in [(true, false), (false, true), (false, false)] {
            cx.update(|_, cx| {
                cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |content| {
                        let tab_bar = content.tab_bar.get_or_insert_default();
                        tab_bar.show = Some(show);
                        tab_bar.show_tab_bar_buttons = Some(buttons);
                    });
                });
            });
            assert!(!draws(cx), "show {show}, buttons {buttons}");
        }
    }

    #[gpui::test]
    async fn test_no_end_without_rails(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, false).await;
        assert!(!cx.update(|_, cx| draws_tab_bar_end(cx)));
    }

    #[gpui::test]
    async fn test_quick_actions_end_follows_setting_and_active_tab(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, true).await;
        cx.update(|window, cx| {
            let multi = window.root::<MultiWorkspace>().flatten().unwrap();
            let pane = multi.read(cx).workspace().read(cx).active_pane().clone();
            let probe = cx.new(|_| Probe("quick-actions"));
            pane.update(cx, |pane, cx| {
                pane.toolbar()
                    .update(cx, |_, cx| quick_actions_tab_bar_end(probe, cx))
            });
        });
        let shown = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let shown = cx.debug_bounds("quick-actions").is_some();
            assert_eq!(shown, cx.debug_bounds("zenith-tab-bar-end").is_some());
            shown
        };
        let redraw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let multi = window.root::<MultiWorkspace>().flatten().unwrap();
                let pane = multi.read(cx).workspace().read(cx).active_pane().clone();
                pane.update(cx, |_, cx| cx.notify());
            });
        };
        assert!(
            !shown(cx),
            "a non-editor tab has no quick actions, and no empty strip"
        );
        cx.dispatch_action(workspace::NewFile);
        cx.run_until_parked();
        assert!(shown(cx), "an editor tab has them at the tab bar's end");
        for quick_actions in [false, true] {
            cx.update(|_, cx| {
                cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |content| {
                        content.editor.toolbar.get_or_insert_default().quick_actions =
                            Some(quick_actions);
                    });
                });
            });
            redraw(cx);
            assert_eq!(shown(cx), quick_actions);
        }
    }

    #[gpui::test]
    async fn test_tab_bar_end_is_dropped_with_its_toolbar(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, true).await;
        let probe = cx.new(|_| Probe("probe"));
        let toolbar = cx.new(|_| workspace::Toolbar::new());
        toolbar.update(cx, |_, cx| tab_bar_end(probe, 0, |_, _| true, cx));
        let id = toolbar.entity_id();
        let held = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| cx.global::<tab_bar_end::Views>().0.contains_key(&id))
        };
        assert!(held(cx));
        drop(toolbar);
        // Dropped entities are released when an update finishes.
        cx.update(|_, _| ());
        assert!(!held(cx), "a closed pane leaves nothing behind");
    }
}
