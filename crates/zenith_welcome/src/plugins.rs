//! Plugins page: the extensions zed-kit installs for PHP/web work.

use gpui::{Action as _, App, IntoElement};
use ui::{ListItem, ListItemSpacing, prelude::*};
use zed_actions::Extensions;

/// Mirrors `auto_install_extensions` in zed-kit's `settings.global.jsonc`.
pub const RECOMMENDED: &[(&str, &str, &str)] = &[
    ("php", "PHP", "Language server, syntax and snippets"),
    ("blade", "Blade", "Laravel Blade templates"),
    ("twig", "Twig", "Twig templates"),
    ("scss", "SCSS", "Sass stylesheets"),
    ("dockerfile", "Dockerfile", "Docker build files"),
    ("make", "Make", "Makefiles"),
    ("http", "HTTP", "Send requests from .http files"),
];

pub fn render_plugins(_cx: &mut App) -> impl IntoElement {
    v_flex()
        .gap_2()
        .max_w(rems(40.))
        .child(Label::new("Recommended for PHP and web projects").color(Color::Muted))
        .children(RECOMMENDED.iter().map(|(id, name, description)| {
            ListItem::new(*id)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    v_flex().child(Label::new(*name)).child(
                        Label::new(*description)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .on_click(move |_, window, cx| {
                    let action = Extensions {
                        id: Some(id.to_string()),
                        ..Default::default()
                    };
                    window.dispatch_action(action.boxed_clone(), cx);
                })
        }))
        .child(h_flex().pt_2().child(
            Button::new("browse-extensions", "Browse All Extensions").on_click(|_, window, cx| {
                window.dispatch_action(Extensions::default().boxed_clone(), cx)
            }),
        ))
}
