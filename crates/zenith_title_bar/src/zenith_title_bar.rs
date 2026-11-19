//! Zenith additions to the title bar (plan 34, step 2).

use gpui::{Action, Entity, FontWeight, Hsla};
use project::{Inventory, Project};
use ui::{ButtonLike, IconButton, Tooltip, Vector, VectorName, prelude::*};

/// Two-letter badge shown before the project name, e.g. "LG" for `lumen-gallery`.
pub fn project_badge(project_name: &str, _cx: &App) -> Option<impl IntoElement + use<>> {
    let initials = initials(project_name)?;
    // Board: 22 px mark, then a 20 px #4a5b8c badge, radius 5, 9.5 px bold white.
    let mark = Vector::square(VectorName::ZedLogo, rems_from_px(22_f32));
    Some(
        h_flex().flex_none().gap(px(8.)).child(mark).child(
            div()
                .flex_none()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .bg(gpui::rgb(0x4a5b8c))
                .text_color(gpui::white())
                .text_size(px(9.5))
                .font_weight(FontWeight::BOLD)
                .child(initials),
        ),
    )
}

/// Zenith's title bar middle: search, run controls, agent chip. One element, because
/// the upstream title bar holds its sections in a fixed-size `ArrayVec`.
pub fn middle(project: &Entity<Project>, cx: &App) -> impl IntoElement + use<> {
    // Board gaps: run controls 2 px apart, 8 px before the chip, 4 px before settings.
    h_flex()
        .gap(px(2.))
        .child(search_box(Box::new(zenith_search::Toggle)))
        .child(div().w(px(10.)))
        .child(run_controls(project, cx))
        .child(div().w(px(8.)))
        .child(agent_chip())
        .child(div().w(px(4.)))
        .child(
            IconButton::new("zenith-settings", IconName::Settings)
                .icon_size(IconSize::Small)
                .tooltip(action_tooltip(
                    "Settings",
                    Box::new(zed_actions::OpenSettings),
                ))
                .on_click(dispatch(Box::new(zed_actions::OpenSettings))),
        )
}

/// "Search Everywhere" box for the middle of the title bar; dispatches `action`.
fn search_box(action: Box<dyn gpui::Action>) -> impl IntoElement {
    // Board: 340×28, radius 7, #31343a, 12.5 px placeholder #8c8f96, ⇧⇧ key cap.
    ButtonLike::new("zenith-search-everywhere")
        .style(ButtonStyle::Transparent)
        .child(
            h_flex()
                .w(px(340.))
                .h(px(28.))
                .min_w_0()
                .pl(px(10.))
                .pr(px(8.))
                .gap(px(8.))
                .rounded(px(7.))
                .bg(gpui::rgb(0x31343a))
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Placeholder),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .text_color(gpui::rgb(0x8c8f96))
                        .child("Search everywhere"),
                )
                .child(
                    div()
                        .h(px(18.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(gpui::rgb(0x393b40))
                        .bg(gpui::rgb(0x2b2d30))
                        .text_size(px(11.))
                        .text_color(gpui::rgb(0x8c8f96))
                        .child("⇧⇧"),
                ),
        )
        .tooltip({
            let action = action.boxed_clone();
            move |_window, cx| Tooltip::for_action("Search Everywhere", action.as_ref(), cx)
        })
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// Run configuration dropdown with ▶ and 🐞, as in PhpStorm. The dropdown opens
/// Zed's task picker; ▶ reruns the last task; 🐞 starts the debugger.
fn run_controls(project: &Entity<Project>, cx: &App) -> impl IntoElement + use<> {
    let label = run_label(project.read(cx).task_store().read(cx).task_inventory(), cx);
    // Looked up by name so the title bar doesn't depend on debugger_ui; absent ⇒ no 🐞.
    let debug = cx.build_action("debugger::Start", None).ok();
    h_flex()
        .gap_0p5()
        .child(
            Button::new("zenith-run-configuration", label)
                .label_size(LabelSize::Small)
                .color(Color::Muted)
                .end_icon(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(action_tooltip(
                    "Run Configurations",
                    Box::new(zed_actions::Spawn::modal()),
                ))
                .on_click(dispatch(Box::new(zed_actions::Spawn::modal()))),
        )
        .child(
            IconButton::new("zenith-run", IconName::PlayFilled)
                .icon_size(IconSize::Small)
                .icon_color(Color::Success)
                .tooltip(action_tooltip(
                    "Run",
                    Box::new(zed_actions::Rerun::default()),
                ))
                .on_click(dispatch(Box::new(zed_actions::Rerun::default()))),
        )
        .when_some(debug, |this, debug| {
            this.child(
                IconButton::new("zenith-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .tooltip(action_tooltip("Debug", debug.boxed_clone()))
                    .on_click(dispatch(debug)),
            )
        })
}

/// Plan 34: orange marks anything an agent did; nothing else in the UI uses it.
pub const AGENT_ORANGE: u32 = 0xff7a3d;

/// Agent chip; toggles the agent panel. Live "1 running · review 2" counts come with plan 22.
fn agent_chip() -> impl IntoElement {
    let orange: Hsla = gpui::rgb(AGENT_ORANGE).into();
    // Board: 26 px pill, rgba(255,122,61,.14), 12 px / 500 orange text.
    ButtonLike::new("zenith-agent-chip")
        .style(ButtonStyle::Transparent)
        .child(
            h_flex()
                .h(px(26.))
                .pl(px(9.))
                .pr(px(9.))
                .gap(px(6.))
                .rounded_full()
                .bg(orange.opacity(0.14))
                .child(
                    Icon::new(IconName::Sparkle)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(orange)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(orange)
                        .child("Agents"),
                ),
        )
        .tooltip(action_tooltip(
            "Agents",
            Box::new(zed_actions::assistant::ToggleFocus),
        ))
        .on_click(dispatch(Box::new(zed_actions::assistant::ToggleFocus)))
}

/// Name of the last task run, or a prompt to pick one.
fn run_label(inventory: Option<&Entity<Inventory>>, cx: &App) -> SharedString {
    inventory
        .and_then(|inventory| inventory.read(cx).last_scheduled_task(None))
        .map(|(_, task)| task.resolved_label.into())
        .unwrap_or_else(|| "Add Configuration…".into())
}

fn action_tooltip(
    title: &'static str,
    action: Box<dyn Action>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_window, cx| Tooltip::for_action(title, action.as_ref(), cx)
}

fn dispatch(action: Box<dyn Action>) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) {
    move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
}

/// First letter of the first two words, or the first two letters of a single word.
fn initials(name: &str) -> Option<SharedString> {
    let mut words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let first = words.next()?;
    let letters: String = match words.next() {
        Some(second) => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
        None => first.chars().take(2).collect(),
    };
    Some(letters.to_uppercase().into())
}

#[cfg(test)]
mod tests {
    use super::{initials, run_label};
    use gpui::TestAppContext;
    use project::{Inventory, TaskSourceKind};
    use task::{TaskContext, TaskTemplate};

    #[gpui::test]
    fn test_run_label(cx: &mut TestAppContext) {
        let inventory = cx.update(Inventory::new);
        cx.update(|cx| {
            assert_eq!(run_label(None, cx), "Add Configuration…");
            assert_eq!(run_label(Some(&inventory), cx), "Add Configuration…");
        });
        let task = TaskTemplate {
            label: "Build".into(),
            command: "make".into(),
            ..TaskTemplate::default()
        }
        .resolve_task("test", &TaskContext::default())
        .expect("task resolves");
        inventory.update(cx, |inventory, _| {
            inventory.task_scheduled(TaskSourceKind::UserInput, task)
        });
        cx.update(|cx| assert_eq!(run_label(Some(&inventory), cx), "Build"));
    }

    #[test]
    fn test_initials() {
        assert_eq!(initials("lumen-gallery").as_deref(), Some("LG"));
        assert_eq!(initials("my_wp plugin").as_deref(), Some("MW"));
        assert_eq!(initials("zenith").as_deref(), Some("ZE"));
        assert_eq!(initials("a").as_deref(), Some("A"));
        assert_eq!(initials("ünïcode-tëst").as_deref(), Some("ÜT"));
        assert_eq!(initials("--"), None);
        assert_eq!(initials(""), None);
    }
}
