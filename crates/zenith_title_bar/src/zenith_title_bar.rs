//! Zenith additions to the title bar (plan 34, step 2).

use gpui::{Action, Entity, FontWeight, Hsla};
use project::{Inventory, Project};
use ui::{ButtonLike, IconButton, Tooltip, prelude::*};

/// Two-letter badge shown before the project name, e.g. "LG" for `lumen-gallery`.
pub fn project_badge(project_name: &str, cx: &App) -> Option<impl IntoElement + use<>> {
    let initials = initials(project_name)?;
    let colors = cx.theme().colors();
    Some(
        div()
            .flex_none()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                Label::new(initials)
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::BOLD),
            ),
    )
}

/// Zenith's title bar middle: search, run controls, agent chip. One element, because
/// the upstream title bar holds its sections in a fixed-size `ArrayVec`.
pub fn middle(project: &Entity<Project>, cx: &App) -> impl IntoElement + use<> {
    h_flex()
        .gap_2()
        .child(search_box(Box::new(zenith_search::Toggle)))
        .child(run_controls(project, cx))
        .child(agent_chip())
}

/// "Search Everywhere" box for the middle of the title bar; dispatches `action`.
fn search_box(action: Box<dyn gpui::Action>) -> impl IntoElement {
    ButtonLike::new("zenith-search-everywhere")
        .style(ButtonStyle::Filled)
        .child(
            h_flex()
                .w_48()
                .min_w_0()
                .gap_1()
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Search Everywhere")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
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
    ButtonLike::new("zenith-agent-chip")
        .child(
            h_flex()
                .gap_1()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(orange.opacity(0.5))
                .bg(orange.opacity(0.12))
                .child(
                    Icon::new(IconName::Sparkle)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(orange)),
                )
                .child(
                    Label::new("Agents")
                        .size(LabelSize::Small)
                        .color(Color::Custom(orange)),
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
