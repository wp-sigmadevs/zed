//! Startup splash (plan 34): a borderless vector card shown while the app starts,
//! closed when the first workspace window opens. Board: `mockups/zenith-splash.html`.
//! Draws with fixed colours and the system font, so it needs no theme or settings.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyWindowHandle, App, Bounds, Context, FontWeight, Global,
    IntoElement, ParentElement, PathBuilder, Pixels, Render, Styled, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, canvas, div, hsla,
    linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb, size,
};
use release_channel::{AppVersion, ReleaseChannel};
use workspace::Workspace;

const WIDTH: f32 = 640.;
const HEIGHT: f32 = 400.;
/// Safety net: a start that never opens a workspace must not leave the card up.
const MAX_SHOWN: Duration = Duration::from_secs(20);

struct Splash(AnyWindowHandle);

impl Global for Splash {}

/// Shows the card and arranges for it to close with the first workspace window.
pub fn show(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let version = format!(
        "{} {}",
        ReleaseChannel::global(cx).display_name(),
        AppVersion::global(cx)
    );
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None::<TitlebarOptions>,
            kind: WindowKind::PopUp,
            focus: false,
            show: true,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        move |_, cx| cx.new(|_| SplashView { version }),
    );
    let Ok(handle) = handle else { return };
    cx.set_global(Splash(handle.into()));

    cx.observe_new(|_: &mut Workspace, _, cx| close(cx))
        .detach();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(MAX_SHOWN).await;
        cx.update(close);
    })
    .detach();
}

/// Closes the card if it is still up.
pub fn close(cx: &mut App) {
    if cx.has_global::<Splash>() {
        let Splash(handle) = cx.remove_global::<Splash>();
        handle
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
}

struct SplashView {
    version: String,
}

impl Render for SplashView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let white = |alpha: f32| hsla(0., 0., 1., alpha);
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .rounded(px(12.))
            .font_family(".SystemUIFont")
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x1b1446), 0.),
                linear_color_stop(rgb(0xff7a3d), 1.),
            ))
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, _| paint_scene(bounds, window),
                )
                .size_full()
                .absolute(),
            )
            .child(
                div()
                    .absolute()
                    .left(px(36.))
                    .top(px(44.))
                    .text_color(white(1.))
                    .child(
                        div()
                            .text_size(px(56.))
                            .font_weight(FontWeight::BOLD)
                            .line_height(px(56.))
                            .child("Zenith"),
                    )
                    .child(
                        div()
                            .mt(px(10.))
                            .text_size(px(15.))
                            .text_color(white(0.8))
                            .child("IDE depth. Editor speed."),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(38.))
                    .top(px(150.))
                    .text_size(px(15.))
                    .text_color(white(0.7))
                    .child(format!("Version {}", self.version)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(36.))
                    .bottom(px(22.))
                    .text_size(px(11.5))
                    .text_color(white(0.72))
                    .child("Starting up…"),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(3.))
                    .bg(white(0.12))
                    .child(
                        div()
                            .h_full()
                            .w(relative(0.3))
                            .bg(rgb(0xff7a3d))
                            .with_animation(
                                "zenith-splash-progress",
                                Animation::new(Duration::from_millis(1600)).repeat(),
                                |bar, delta| bar.ml(relative(delta * 0.7)),
                            ),
                    ),
            )
    }
}

/// Sun, glow and two ridges, in the 640×400 space of the board.
fn paint_scene(bounds: Bounds<Pixels>, window: &mut Window) {
    let at = |x: f32, y: f32| bounds.origin + point(px(x), px(y));
    let fill = |points: &[(f32, f32)], color: gpui::Hsla, window: &mut Window| {
        let mut path = PathBuilder::fill();
        path.move_to(at(points[0].0, points[0].1));
        for &(x, y) in &points[1..] {
            path.line_to(at(x, y));
        }
        path.close();
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    };
    // Sun glow: stacked translucent discs, since a radial gradient needs no more than this.
    for (radius, alpha) in [(170., 0.07), (120., 0.1), (80., 0.14), (52., 0.2)] {
        let mut disc = PathBuilder::fill();
        let steps = 48;
        for i in 0..steps {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            let p = at(470. + radius * a.cos(), 215. + radius * a.sin());
            if i == 0 {
                disc.move_to(p)
            } else {
                disc.line_to(p)
            }
        }
        disc.close();
        if let Ok(disc) = disc.build() {
            window.paint_path(disc, hsla(0.11, 1., 0.74, alpha));
        }
    }
    let mut sun = PathBuilder::fill();
    for i in 0..48 {
        let a = i as f32 / 48. * std::f32::consts::TAU;
        let p = at(470. + 34. * a.cos(), 215. + 34. * a.sin());
        if i == 0 {
            sun.move_to(p)
        } else {
            sun.line_to(p)
        }
    }
    sun.close();
    if let Ok(sun) = sun.build() {
        window.paint_path(sun, rgb(0xffe3a8));
    }
    fill(
        &[
            (250., 400.),
            (410., 160.),
            (470., 250.),
            (520., 190.),
            (640., 400.),
        ],
        hsla(0.7, 0.6, 0.27, 0.85),
        window,
    );
    fill(
        &[
            (330., 400.),
            (470., 235.),
            (540., 330.),
            (580., 280.),
            (640., 400.),
        ],
        rgb(0x1b1446).into(),
        window,
    );
    fill(
        &[(410., 160.), (384., 200.), (410., 188.), (432., 210.)],
        hsla(0., 0., 1., 0.9),
        window,
    );
    fill(
        &[
            (0., 400.),
            (0., 345.),
            (190., 335.),
            (380., 350.),
            (640., 335.),
            (640., 400.),
        ],
        rgb(0x120d33).into(),
        window,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_splash_shows_then_closes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 1, 0), cx);
            show(cx);
            assert_eq!(cx.windows().len(), 1);
            close(cx);
            assert_eq!(cx.windows().len(), 0);
            // Closing twice is a no-op.
            close(cx);
        });
    }
}
