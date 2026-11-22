//! Agent review queue (plan 23): every agent thread's turns, the files each
//! turn changed with a local-history checkpoint taken before the change, and
//! keep / reject / revert-turn operations, shown in the Agent Review panel
//! (`panel.rs`), a status-bar item and the title-bar chip counts (`status.rs`).

mod checkpoint;
mod panel;
mod queue;
mod status;
#[cfg(test)]
mod tests;

use gpui::{App, actions};
use settings::{RegisterSetting, Settings, SettingsContent};
use workspace::Workspace;

pub use panel::AgentReviewPanel;
pub use queue::{
    AgentReview, AgentReviewEvent, RevertOutcome, ReviewStatus, Summary, ThreadReview, Turn,
    TurnFile, keep_all, keep_all_in_project, keep_file, open_diff, reject_all, reject_file,
    revert_turn,
};

actions!(
    zenith_agent_review,
    [
        /// Toggles focus on the agent review panel.
        ToggleFocus,
        /// Keeps every pending agent edit in this project.
        KeepAll,
        /// Reverts every file the selected agent turn changed.
        RevertTurn,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct AgentReviewSettings {
    pub enabled: bool,
}

impl Settings for AgentReviewSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.agent_review.as_ref())
                .and_then(|review| review.enabled)
                .unwrap_or(true),
        }
    }
}

/// Tracks agent threads from now on, with the review panel, status-bar item and title-bar
/// chip counts.
pub fn init(cx: &mut App) {
    AgentReviewSettings::register(cx);
    checkpoint::init(cx);
    panel::init(cx);
    status::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &KeepAll, _, cx| {
            keep_all_in_project(workspace.project().entity_id(), cx);
        });
    })
    .detach();
}
