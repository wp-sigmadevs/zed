//! Zenith additions to the title bar (plan 34, step 2).

mod project_switcher;
mod run;
mod switcher_rows;

use std::collections::HashMap;

use gpui::{Action, AnyElement, Empty, Entity, EntityId, FontWeight, Global, Hsla};
use project::Project;
use settings::{RegisterSetting, Settings, SettingsContent};
use ui::{ButtonLike, IconButton, Tooltip, Vector, VectorName, prelude::*};

pub use project_switcher::{ProjectSwitcher, open_recent_paths, project_switcher};
pub use run::{DebugSelected, RerunLast, RunConfig, RunModel, RunSelected, StopRun, init};

/// `zenith.title_bar.enabled`: off, the title bar is upstream's. Read on every render.
#[derive(Clone, Copy, RegisterSetting)]
pub struct ZenithTitleBarSettings {
    pub enabled: bool,
}

impl Settings for ZenithTitleBarSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let title_bar = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.title_bar.as_ref());
        Self {
            enabled: title_bar
                .and_then(|title_bar| title_bar.enabled)
                .unwrap_or(true),
        }
    }
}

/// Badge shown before the project name, e.g. "LG" for `lumen-gallery` (rule in
/// [`switcher_rows::initials`]), in a colour taken from the name.
pub fn project_badge(project_name: &str, cx: &App) -> Option<impl IntoElement + use<>> {
    if !ZenithTitleBarSettings::get_global(cx).enabled {
        return None;
    }
    let initials = switcher_rows::initials(&[project_name])?;
    let color = switcher_rows::badge_color(project_name);
    // Board: 22 px mark, then a 20 px badge.
    let mark = show_mark(cx).then(|| Vector::square(VectorName::ZedLogo, rems_from_px(22_f32)));
    Some(
        h_flex()
            .flex_none()
            .gap(px(8.))
            .children(mark)
            .child(badge(&initials, color)),
    )
}

/// The project-name button always shows its chevron under Zenith's switcher; upstream's
/// rule (several roots) applies with `zenith.title_bar.enabled` off.
pub fn switcher_chevron(worktree_count: usize, cx: &App) -> bool {
    ZenithTitleBarSettings::get_global(cx).enabled || worktree_count > 1
}

/// 20 px badge, radius 5, 9.5 px bold white letters on `color`.
fn badge(initials: &str, color: Hsla) -> impl IntoElement + use<> {
    div()
        .flex_none()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .bg(color)
        .text_color(gpui::white())
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .child(SharedString::from(initials.to_string()))
}

/// JetBrains' title bar has no logo: the mark is dropped under the islands/rails chrome.
fn show_mark(cx: &App) -> bool {
    !cx.has_global::<workspace::IslandsActive>() && !cx.has_global::<workspace::dock::RailsActive>()
}

/// Zenith's title bar middle: search, run controls, agent chip. One element, because
/// the upstream title bar holds its sections in a fixed-size `ArrayVec`.
pub fn middle(project: &Entity<Project>, cx: &App) -> AnyElement {
    if !ZenithTitleBarSettings::get_global(cx).enabled {
        return Empty.into_any_element();
    }
    // Board gaps: run controls 2 px apart, 8 px before the chip, 4 px before settings.
    h_flex()
        .gap(px(2.))
        .children(
            cx.build_action("zenith_search::Toggle", None)
                .ok()
                .map(|action| search_box(action, cx)),
        )
        .child(div().w(px(10.)))
        .child(run::run_controls(project, cx))
        .child(div().w(px(8.)))
        .child(agent_chip(project, cx))
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
        .into_any_element()
}

/// "Search Everywhere" box for the middle of the title bar; dispatches `action`.
fn search_box(action: Box<dyn gpui::Action>, cx: &App) -> impl IntoElement + use<> {
    // Board: 340×28, radius 7, 12.5 px placeholder, ⇧⇧ key cap; theme colours so light
    // themes work.
    let colors = cx.theme().colors();
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
                .bg(colors.element_background)
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
                        .text_color(colors.text_placeholder)
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
                        .border_color(colors.border_variant)
                        .bg(colors.editor_background)
                        .text_size(px(11.))
                        .text_color(colors.text_placeholder)
                        .child("⇧⇧"),
                ),
        )
        .tooltip({
            let action = action.boxed_clone();
            move |_window, cx| Tooltip::for_action("Search Everywhere", action.as_ref(), cx)
        })
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// Plan 34: orange marks anything an agent did; nothing else in the UI uses it.
pub const AGENT_ORANGE: u32 = 0xff7a3d;

/// Agent activity of one project, as the title-bar chip shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCounts {
    pub running: usize,
    /// Files with agent edits waiting for review.
    pub review: usize,
}

/// Per-project agent counts, written by the agent review queue. A global, so the title bar
/// needs no dependency on the agent crates.
#[derive(Default)]
pub struct AgentChip(HashMap<EntityId, AgentCounts>);

impl Global for AgentChip {}

impl AgentChip {
    pub fn counts(project_id: EntityId, cx: &App) -> AgentCounts {
        cx.try_global::<Self>()
            .and_then(|chip| chip.0.get(&project_id).copied())
            .unwrap_or_default()
    }

    /// Callers notify the workspace afterwards so the title bar repaints.
    pub fn set(project_id: EntityId, counts: AgentCounts, cx: &mut App) {
        let chip = cx.default_global::<Self>();
        if counts == AgentCounts::default() {
            chip.0.remove(&project_id);
        } else {
            chip.0.insert(project_id, counts);
        }
    }
}

/// Chip text and the review count for its badge: "Agents" when idle, else the non-zero
/// parts, e.g. "1 running · review" with badge 2.
pub fn agent_chip_label(counts: AgentCounts) -> (String, Option<usize>) {
    let running = (counts.running > 0).then(|| format!("{} running", counts.running));
    let review = (counts.review > 0).then(|| "review".to_string());
    let text = [running, review].into_iter().flatten().collect::<Vec<_>>();
    if text.is_empty() {
        return ("Agents".to_string(), None);
    }
    (
        text.join(" · "),
        (counts.review > 0).then_some(counts.review),
    )
}

/// What a screen reader says for the chip, e.g. "Agents: 1 running, 2 files to review".
fn agent_chip_aria_label(counts: AgentCounts) -> String {
    let mut parts = Vec::new();
    if counts.running > 0 {
        parts.push(format!("{} running", counts.running));
    }
    if counts.review > 0 {
        let plural = if counts.review == 1 { "" } else { "s" };
        parts.push(format!("{} file{plural} to review", counts.review));
    }
    if parts.is_empty() {
        "Agents".to_string()
    } else {
        format!("Agents: {}", parts.join(", "))
    }
}

/// Agent chip with live counts; toggles the agent panel.
fn agent_chip(project: &Entity<Project>, cx: &App) -> impl IntoElement + use<> {
    let counts = AgentChip::counts(project.entity_id(), cx);
    let (text, badge) = agent_chip_label(counts);
    let orange: Hsla = gpui::rgb(AGENT_ORANGE).into();
    // Board: 26 px pill, rgba(255,122,61,.14), 12 px / 500 orange text; with a badge the
    // right padding is 6 px and the badge is a 16 px orange pill, 10.5 px / 700 dark text.
    ButtonLike::new("zenith-agent-chip")
        .style(ButtonStyle::Transparent)
        .aria_label(agent_chip_aria_label(counts))
        .child(
            h_flex()
                .h(px(26.))
                .pl(px(9.))
                .pr(px(if badge.is_some() { 6. } else { 9. }))
                .gap(px(6.))
                .rounded_full()
                .bg(orange.opacity(0.14))
                .child(
                    Icon::new(IconName::ZenithBot)
                        .size(IconSize::XSmall)
                        .color(Color::Custom(orange)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(orange)
                        .child(text),
                )
                .when_some(badge, |this, count| {
                    this.child(
                        div()
                            .h(px(16.))
                            .min_w(px(16.))
                            .px(px(4.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(orange)
                            .text_color(gpui::rgb(0x1e1f22))
                            .text_size(px(10.5))
                            .font_weight(FontWeight::BOLD)
                            .child(count.to_string()),
                    )
                }),
        )
        .tooltip(action_tooltip(
            "Agents",
            Box::new(zed_actions::assistant::ToggleFocus),
        ))
        .on_click(dispatch(Box::new(zed_actions::assistant::ToggleFocus)))
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

#[cfg(test)]
mod tests {
    use super::{AgentChip, AgentCounts, agent_chip_aria_label, agent_chip_label, show_mark};
    use gpui::{EntityId, TestAppContext};

    #[gpui::test]
    fn test_mark_hidden_under_islands_or_rails(cx: &mut TestAppContext) {
        cx.update(|cx| assert!(show_mark(cx)));
        cx.update(|cx| cx.set_global(workspace::IslandsActive));
        cx.update(|cx| assert!(!show_mark(cx)));
    }

    #[gpui::test]
    fn test_mark_hidden_under_rails(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(workspace::dock::RailsActive));
        cx.update(|cx| assert!(!show_mark(cx)));
    }

    #[gpui::test]
    fn test_setting_off_drops_the_badge(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            assert!(super::project_badge("lumen-gallery", cx).is_some());
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(settings::ZenithSettingsContent {
                        title_bar: Some(settings::ZenithTitleBarSettingsContent {
                            enabled: Some(false),
                        }),
                        ..Default::default()
                    });
                });
            });
            assert!(super::project_badge("lumen-gallery", cx).is_none());
        });
    }

    #[test]
    fn test_agent_chip_label() {
        let counts = |running, review| AgentCounts { running, review };
        assert_eq!(agent_chip_label(counts(0, 0)), ("Agents".to_string(), None));
        assert_eq!(
            agent_chip_label(counts(1, 2)),
            ("1 running · review".to_string(), Some(2))
        );
        assert_eq!(
            agent_chip_label(counts(2, 0)),
            ("2 running".to_string(), None)
        );
        assert_eq!(
            agent_chip_label(counts(0, 1)),
            ("review".to_string(), Some(1))
        );
        assert_eq!(
            agent_chip_aria_label(counts(1, 2)),
            "Agents: 1 running, 2 files to review"
        );
        assert_eq!(agent_chip_aria_label(counts(0, 0)), "Agents");
    }

    #[gpui::test]
    fn test_agent_chip_counts_per_project(cx: &mut TestAppContext) {
        let (first, second) = (EntityId::from(1u64), EntityId::from(2u64));
        let busy = AgentCounts {
            running: 1,
            review: 0,
        };
        cx.update(|cx| {
            assert_eq!(AgentChip::counts(first, cx), AgentCounts::default());
            AgentChip::set(first, busy, cx);
            assert_eq!(AgentChip::counts(first, cx), busy);
            assert_eq!(AgentChip::counts(second, cx), AgentCounts::default());
            AgentChip::set(first, AgentCounts::default(), cx);
            assert_eq!(AgentChip::counts(first, cx), AgentCounts::default());
        });
    }
}
