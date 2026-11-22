//! Turn tracking: finds agent threads, counts their turns and records a
//! local-history checkpoint of every file a turn changes, before the change.

use std::time::{SystemTime, UNIX_EPOCH};

use acp_thread::{AcpThread, AcpThreadEvent, ThreadStatus};
use action_log::ActionLog;
use gpui::{App, AppContext as _, Context, Entity, EntityId, SharedString};
use settings::Settings as _;

use crate::AgentReviewSettings;
use crate::queue::{AgentReview, GlobalAgentReview, HeldFile, ThreadReview, Turn, TurnFile, label};

pub(crate) fn init(cx: &mut App) {
    let review = cx.new(|_| AgentReview::default());
    cx.set_global(GlobalAgentReview(review));
    cx.observe_new(|thread: &mut AcpThread, _, cx: &mut Context<AcpThread>| {
        // Subagents are reviewed through their parent thread.
        if thread.parent_session_id().is_some() || !AgentReviewSettings::get_global(cx).enabled {
            return;
        }
        // The new thread is mid-update here: register it once that is over.
        let thread = cx.weak_entity();
        cx.defer(move |cx| {
            let (Some(thread), Some(review)) = (thread.upgrade(), AgentReview::global(cx)) else {
                return;
            };
            review.update(cx, |review, cx| review.register(thread, cx));
        });
    })
    .detach();
}

impl AgentReview {
    fn register(&mut self, thread: Entity<AcpThread>, cx: &mut Context<Self>) {
        let thread_id = thread.entity_id();
        if self.index(thread_id).is_some() {
            return;
        }
        let (project_id, session_id, action_log) = {
            let thread = thread.read(cx);
            (
                thread.project().entity_id(),
                SharedString::from(thread.session_id().to_string()),
                thread.action_log().clone(),
            )
        };
        let subscriptions = vec![
            cx.subscribe(&thread, |this, thread, event: &AcpThreadEvent, cx| {
                if matches!(
                    event,
                    AcpThreadEvent::StatusChanged
                        | AcpThreadEvent::Stopped { .. }
                        | AcpThreadEvent::TitleUpdated
                        | AcpThreadEvent::ToolAuthorizationRequested(_)
                        | AcpThreadEvent::ToolAuthorizationReceived(_)
                        | AcpThreadEvent::ElicitationRequested(_)
                        | AcpThreadEvent::ElicitationResponded(_)
                ) {
                    this.refresh(&thread, cx);
                }
            }),
            // ActionLog has no events; it notifies after each diff update.
            cx.observe(&action_log, move |this, action_log, cx| {
                this.action_log_changed(thread_id, &action_log, cx);
            }),
            cx.observe_release(&thread, move |this, _, cx| {
                if let Some(ix) = this.index(thread_id) {
                    let project_id = this.threads.remove(ix).project_id;
                    this.update_summary(project_id, cx);
                    cx.notify();
                }
            }),
        ];
        self.threads.push(ThreadReview {
            thread: thread.downgrade(),
            project_id,
            session_id,
            run: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
            title: None,
            status: crate::ReviewStatus::Idle,
            pending_files: 0,
            turns: Vec::new(),
            generating: false,
            waiting: false,
            _subscriptions: subscriptions,
        });
        self.refresh(&thread, cx);
    }

    /// Re-reads the thread's status; a switch to generating starts a turn.
    fn refresh(&mut self, thread: &Entity<AcpThread>, cx: &mut Context<Self>) {
        let Some(ix) = self.index(thread.entity_id()) else {
            return;
        };
        let (generating, waiting, title, action_log) = {
            let thread = thread.read(cx);
            let generating = thread.status() == ThreadStatus::Generating;
            (
                generating,
                generating && thread.is_waiting_for_confirmation(),
                thread.title(),
                thread.action_log().clone(),
            )
        };
        let review = &mut self.threads[ix];
        if generating && !review.generating && AgentReviewSettings::get_global(cx).enabled {
            start_turn(review, &action_log, cx);
        }
        review.generating = generating;
        review.waiting = waiting;
        review.title = title;
        review.pending_files = action_log.read(cx).changed_buffers(cx).count();
        review.update_status();
        let project_id = review.project_id;
        self.update_summary(project_id, cx);
        cx.notify();
    }

    /// Checkpoints the pre-turn text of files the current turn changed, and
    /// refreshes the pending count. Always notifies: the diff counts changed.
    fn action_log_changed(
        &mut self,
        thread_id: EntityId,
        action_log: &Entity<ActionLog>,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.index(thread_id) else {
            return;
        };
        let changed: Vec<_> = action_log.read(cx).changed_buffers(cx).collect();
        let enabled = AgentReviewSettings::get_global(cx).enabled;
        let review = &mut self.threads[ix];
        // Not gated on `generating`: the diff behind `changed_buffers` lands
        // asynchronously, possibly after the turn ended.
        if let Some(turn) = review.turns.last_mut().filter(|_| enabled) {
            let label = label(&review.session_id, review.run, turn.number);
            // A held file kept or rejected meanwhile is no longer this turn's
            // to restore; a later agent edit shows up below as a new file.
            turn.held.retain(|held| {
                changed
                    .iter()
                    .any(|(buffer, _)| buffer.entity_id() == held.buffer.entity_id())
            });
            for (buffer, diff) in &changed {
                if turn
                    .files
                    .iter()
                    .any(|file| file.buffer.entity_id() == buffer.entity_id())
                {
                    continue;
                }
                if let Some(ix) = turn
                    .held
                    .iter()
                    .position(|held| held.buffer.entity_id() == buffer.entity_id())
                {
                    if !buffer
                        .read(cx)
                        .has_edits_since(turn.held[ix].snapshot.version())
                    {
                        continue;
                    }
                    let held = turn.held.swap_remove(ix);
                    turn.files.push(TurnFile {
                        checkpointed: zenith_local_history::checkpoint(
                            buffer,
                            held.snapshot.as_rope().clone(),
                            &label,
                            cx,
                        ),
                        buffer: buffer.downgrade(),
                        created: false,
                    });
                    continue;
                }
                // The diff base is the text before the agent's first unkept edit.
                // An empty base is a file the agent created (or an empty file,
                // where reject restores the same result).
                let base = diff
                    .read(cx)
                    .base_text_string(cx)
                    .filter(|text| !text.is_empty());
                let created = base.is_none();
                let checkpointed = base.is_some_and(|text| {
                    zenith_local_history::checkpoint(buffer, text.into(), &label, cx)
                });
                turn.files.push(TurnFile {
                    buffer: buffer.downgrade(),
                    created,
                    checkpointed,
                });
            }
        }
        if review.pending_files != changed.len() {
            review.pending_files = changed.len();
            review.update_status();
            let project_id = review.project_id;
            self.update_summary(project_id, cx);
        }
        cx.notify();
    }
}

/// Starts the next turn holding the current text of every file that already
/// has pending agent edits (from earlier turns); `action_log_changed`
/// records it once the file changes during the turn.
fn start_turn(review: &mut ThreadReview, action_log: &Entity<ActionLog>, cx: &App) {
    let number = review.turn() + 1;
    let held = action_log
        .read(cx)
        .changed_buffers(cx)
        .map(|(buffer, _)| HeldFile {
            snapshot: buffer.read(cx).text_snapshot(),
            buffer: buffer.downgrade(),
        })
        .collect();
    review.turns.push(Turn {
        number,
        files: Vec::new(),
        held,
    });
}
