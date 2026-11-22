//! Status-bar line for the most urgent agent thread of the workspace's project, and the
//! title-bar chip counts.

use gpui::{Action as _, AppContext as _, EntityId, Hsla, Subscription, WeakEntity};
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};
use zenith_title_bar::{AGENT_ORANGE, AgentChip, AgentCounts};

use crate::{AgentReview, AgentReviewEvent, ReviewStatus, ToggleFocus};

pub(crate) fn init(cx: &mut App) {
    if let Some(review) = AgentReview::global(cx) {
        cx.subscribe(&review, |review, event: &AgentReviewEvent, cx| {
            let AgentReviewEvent::SummaryChanged(project_id) = *event;
            let summary = review.read(cx).summary(project_id);
            AgentChip::set(
                project_id,
                AgentCounts {
                    running: summary.running,
                    review: summary.review,
                },
                cx,
            );
        })
        .detach();
    }
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let item = cx.new(|cx| AgentStatus::new(workspace, cx));
        workspace
            .status_bar()
            .update(cx, |bar, cx| bar.add_left_item(item, window, cx));
    })
    .detach();
}

/// "Claude Code running…", "… waiting for input", "… waiting for review (2 files)";
/// `None` when no thread of the project needs anything.
pub(crate) fn status_text(review: &AgentReview, project_id: EntityId) -> Option<String> {
    let urgency = |status: ReviewStatus| match status {
        ReviewStatus::NeedsInput => 0,
        ReviewStatus::Running => 1,
        ReviewStatus::Review => 2,
        ReviewStatus::Idle => 3,
    };
    let thread = review
        .threads()
        .iter()
        .filter(|thread| thread.project_id == project_id && thread.status != ReviewStatus::Idle)
        .min_by_key(|thread| urgency(thread.status))?;
    let name = thread.title.as_deref().unwrap_or("Agent");
    Some(match thread.status {
        ReviewStatus::NeedsInput => format!("{name} waiting for input"),
        ReviewStatus::Running => format!("{name} running…"),
        ReviewStatus::Review => {
            let files = thread.pending_files;
            let plural = if files == 1 { "" } else { "s" };
            format!("{name} waiting for review ({files} file{plural})")
        }
        ReviewStatus::Idle => return None,
    })
}

pub struct AgentStatus {
    project_id: EntityId,
    _subscriptions: Vec<Subscription>,
}

impl AgentStatus {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project_id = workspace.project().entity_id();
        let workspace = workspace.weak_handle();
        let mut subscriptions = Vec::new();
        if let Some(review) = AgentReview::global(cx) {
            // Any thread change can change the text (titles too), so repaint on every notify.
            subscriptions.push(cx.observe(&review, |_, _, cx| cx.notify()));
            subscriptions.push(
                cx.subscribe(&review, move |_, _, event: &AgentReviewEvent, cx| {
                    let AgentReviewEvent::SummaryChanged(changed) = *event;
                    if changed == project_id {
                        repaint_title_bar(workspace.clone(), cx);
                    }
                }),
            );
        }
        Self {
            project_id,
            _subscriptions: subscriptions,
        }
    }
}

/// The title bar observes the workspace, so a workspace notify redraws the agent chip.
/// Deferred: the event may arrive while the workspace is being updated.
fn repaint_title_bar(workspace: WeakEntity<Workspace>, cx: &mut App) {
    cx.defer(move |cx| {
        if let Some(workspace) = workspace.upgrade() {
            workspace.update(cx, |_, cx| cx.notify());
        }
    });
}

impl Render for AgentStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = AgentReview::global(cx)
            .and_then(|review| status_text(review.read(cx), self.project_id));
        let orange: Hsla = gpui::rgb(AGENT_ORANGE).into();
        div()
            .debug_selector(|| "zenith-agent-status".into())
            .when_some(text, |this, text| {
                this.child(
                    ButtonLike::new("zenith-agent-status")
                        .aria_label(format!("{text}. Open Agent Review"))
                        .child(
                            Label::new(text)
                                .size(LabelSize::Small)
                                .color(Color::Custom(orange)),
                        )
                        .tooltip(|_, cx| Tooltip::for_action("Agent Review", &ToggleFocus, cx))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(ToggleFocus.boxed_clone(), cx)
                        }),
                )
            })
    }
}

impl StatusItemView for AgentStatus {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
