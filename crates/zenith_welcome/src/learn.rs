//! Learn page. Docs links point at upstream Zed documentation, named as such.

use gpui::{Action, App, IntoElement};
use ui::{ListItem, ListItemSpacing, prelude::*};
use zed_actions::{OpenKeymap, OpenSettings, command_palette};

enum Target {
    Url(&'static str),
    Action(fn() -> Box<dyn Action>),
}

const ENTRIES: &[(&str, &str, Target)] = &[
    (
        "Command Palette",
        "Run any command by name",
        Target::Action(|| command_palette::Toggle.boxed_clone()),
    ),
    (
        "Keymap",
        "View and edit key bindings",
        Target::Action(|| OpenKeymap.boxed_clone()),
    ),
    (
        "Settings",
        "Open the settings editor",
        Target::Action(|| OpenSettings.boxed_clone()),
    ),
    (
        "Editor Documentation",
        "Upstream editor docs (zed.dev)",
        Target::Url("https://zed.dev/docs"),
    ),
    (
        "Key Bindings Guide",
        "Upstream docs (zed.dev)",
        Target::Url("https://zed.dev/docs/key-bindings"),
    ),
    (
        "Vim Mode Guide",
        "Upstream docs (zed.dev)",
        Target::Url("https://zed.dev/docs/vim"),
    ),
    (
        "Agent Panel Guide",
        "Upstream docs (zed.dev)",
        Target::Url("https://zed.dev/docs/ai/agent-panel"),
    ),
];

pub fn render_learn(_cx: &mut App) -> impl IntoElement {
    v_flex()
        .gap_2()
        .max_w(rems(40.))
        .children(
            ENTRIES
                .iter()
                .enumerate()
                .map(|(ix, (title, description, target))| {
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            v_flex().child(Label::new(*title)).child(
                                Label::new(*description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                        .on_click(move |_, window, cx| match target {
                            Target::Url(url) => cx.open_url(url),
                            Target::Action(action) => window.dispatch_action(action(), cx),
                        })
                }),
        )
}
