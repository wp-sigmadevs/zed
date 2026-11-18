//! Tool window rails: a slim icon strip down each side of the window, as in
//! PhpStorm. The left rail holds the left dock's panels at the top and the bottom
//! dock's at the foot; the right rail holds the right dock's panels. They reuse
//! the status bar's panel buttons, which then stop drawing there.

use gpui::{App, Context, Entity, IntoElement, Render, Window};
use theme::ActiveTheme as _;
use ui::{Tooltip, prelude::*};
use workspace::{
    IslandsActive, Workspace,
    dock::{DockPosition, PanelButtons, RailsActive},
};

pub fn init(cx: &mut App) {
    cx.set_global(RailsActive);
    cx.set_global(IslandsActive);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        if window.is_none() {
            return;
        }
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
            .py_1()
            .items_center()
            .justify_between()
            .bg(colors.status_bar_background)
            .border_color(colors.border_variant)
            .when(left, |this| this.border_r_1())
            .when(!left, |this| this.border_l_1())
            .child(self.top.clone())
            .when(left, |this| {
                this.child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .child(problems_button())
                        .children(self.foot.clone()),
                )
            })
    }
}

/// PhpStorm's Problems tool window: Zed's project diagnostics, which also hold
/// the inspections' findings. Built by name so this crate needs no `diagnostics` dependency.
fn problems_button() -> impl IntoElement {
    div().debug_selector(|| "zenith-rail-problems".into()).child(
        IconButton::new("zenith-rail-problems", IconName::Warning)
            .icon_size(IconSize::Small)
            .aria_label("Problems")
            .tooltip(Tooltip::text("Problems"))
            .on_click(|_, window, cx| {
                if let Ok(action) = cx.build_action("diagnostics::Deploy", None) {
                    window.dispatch_action(action, cx);
                }
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::Project;
    use workspace::{AppState, MultiWorkspace};

    async fn open_window(cx: &mut TestAppContext, with_rails: bool) -> gpui::VisualTestContext {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::init(app_state.clone(), cx);
            if with_rails {
                init(cx);
            }
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (_, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
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
        let problems = cx
            .debug_bounds("zenith-rail-problems")
            .expect("problems button on a rail");
        assert!(problems.right() <= left.right() && problems.top() >= left.top());
    }

    #[gpui::test]
    async fn test_no_rails_unless_enabled(cx: &mut TestAppContext) {
        let cx = &mut open_window(cx, false).await;
        assert!(cx.debug_bounds("zenith-left-rail").is_none());
        assert!(cx.debug_bounds("zenith-right-rail").is_none());
        assert!(cx.debug_bounds("zenith-center-island").is_none());
    }
}
