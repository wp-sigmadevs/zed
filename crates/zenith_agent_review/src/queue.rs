//! The review queue: one [`ThreadReview`] per tracked agent thread, a cached
//! [`Summary`] per project, and the keep / reject / revert operations the
//! review UI calls.

use std::ops::Range;

use acp_thread::AcpThread;
use agent_ui::AgentDiffPane;
use anyhow::{Result, anyhow};
use collections::HashMap;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, Global, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use language::{Anchor, Buffer, TextBufferSnapshot};
use util::ResultExt as _;
use workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewStatus {
    Running,
    /// Running, but waiting for the user to allow a tool call or answer.
    NeedsInput,
    /// Idle with agent edits nobody has kept or rejected yet.
    Review,
    Idle,
}

/// Agent activity in one project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Threads generating, including those waiting for input.
    pub running: usize,
    pub needs_input: usize,
    /// Files with pending agent edits, summed over threads.
    pub review: usize,
}

#[derive(Clone, Debug)]
pub struct TurnFile {
    pub buffer: WeakEntity<Buffer>,
    /// The agent created the file this turn: there is no text to restore.
    pub created: bool,
    /// Local history accepted the checkpoint. Revert still reports the file
    /// when the text is gone from the store; when this is false it never looks.
    pub checkpointed: bool,
}

/// A file with pending agent edits when its turn started: its text then, kept
/// in memory and recorded only if the turn edits the file.
#[derive(Clone)]
pub(crate) struct HeldFile {
    pub(crate) buffer: WeakEntity<Buffer>,
    pub(crate) snapshot: TextBufferSnapshot,
}

impl std::fmt::Debug for HeldFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldFile")
            .field("buffer", &self.buffer)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct Turn {
    /// 1-based, counted since the thread was first seen in this app session.
    pub number: u32,
    /// Files this turn changed, checkpointed under [`ThreadReview::label`].
    pub files: Vec<TurnFile>,
    pub(crate) held: Vec<HeldFile>,
}

pub struct ThreadReview {
    pub thread: WeakEntity<AcpThread>,
    pub project_id: EntityId,
    pub session_id: SharedString,
    /// Tells this app session's turns apart from earlier ones of the same
    /// session (turn numbers restart per thread entity): the time the thread
    /// was first seen, in milliseconds.
    pub(crate) run: u64,
    pub title: Option<SharedString>,
    pub status: ReviewStatus,
    /// Files with pending agent edits in this thread's action log.
    pub pending_files: usize,
    pub turns: Vec<Turn>,
    pub(crate) generating: bool,
    pub(crate) waiting: bool,
    pub(crate) _subscriptions: Vec<Subscription>,
}

impl ThreadReview {
    /// The latest turn number; 0 before the first turn.
    pub fn turn(&self) -> u32 {
        self.turns.last().map_or(0, |turn| turn.number)
    }

    pub fn label(&self, turn: u32) -> String {
        label(&self.session_id, self.run, turn)
    }

    /// The agent may still edit files: a revert now would race it.
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            ReviewStatus::Running | ReviewStatus::NeedsInput
        )
    }

    pub(crate) fn update_status(&mut self) {
        self.status = match (self.generating, self.waiting) {
            (true, true) => ReviewStatus::NeedsInput,
            (true, false) => ReviewStatus::Running,
            _ if self.pending_files > 0 => ReviewStatus::Review,
            _ => ReviewStatus::Idle,
        };
    }
}

/// `agent:<session>:<run>:<turn>`; the history view shows the last part.
pub(crate) fn label(session_id: &str, run: u64, turn: u32) -> String {
    format!("agent:{session_id}:{run}:{turn}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentReviewEvent {
    /// The summary of this project changed; read it with [`AgentReview::summary`].
    SummaryChanged(EntityId),
}

/// Also notifies whenever any thread's status, title, turns or pending files
/// change, and after every action-log update (the +N −M counts).
#[derive(Default)]
pub struct AgentReview {
    pub(crate) threads: Vec<ThreadReview>,
    pub(crate) summaries: HashMap<EntityId, Summary>,
}

impl EventEmitter<AgentReviewEvent> for AgentReview {}

pub(crate) struct GlobalAgentReview(pub(crate) Entity<AgentReview>);

impl Global for GlobalAgentReview {}

impl AgentReview {
    /// `None` until [`crate::init`] ran.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAgentReview>()
            .map(|global| global.0.clone())
    }

    pub fn threads(&self) -> &[ThreadReview] {
        &self.threads
    }

    pub fn thread(&self, thread_id: EntityId) -> Option<&ThreadReview> {
        self.threads
            .iter()
            .find(|review| review.thread.entity_id() == thread_id)
    }

    /// Cached; recomputed only when a thread of this project changes.
    pub fn summary(&self, project_id: EntityId) -> Summary {
        self.summaries.get(&project_id).copied().unwrap_or_default()
    }

    pub(crate) fn index(&self, thread_id: EntityId) -> Option<usize> {
        self.threads
            .iter()
            .position(|review| review.thread.entity_id() == thread_id)
    }

    pub(crate) fn update_summary(&mut self, project_id: EntityId, cx: &mut Context<Self>) {
        let mut summary = Summary::default();
        for review in self.threads.iter().filter(|r| r.project_id == project_id) {
            match review.status {
                ReviewStatus::Running => summary.running += 1,
                ReviewStatus::NeedsInput => {
                    summary.running += 1;
                    summary.needs_input += 1;
                }
                ReviewStatus::Review | ReviewStatus::Idle => {}
            }
            summary.review += review.pending_files;
        }
        if self.summaries.insert(project_id, summary) != Some(summary) {
            cx.emit(AgentReviewEvent::SummaryChanged(project_id));
        }
    }
}

fn whole(buffer: &Entity<Buffer>, cx: &App) -> Range<Anchor> {
    Anchor::min_max_range_for_buffer(buffer.read(cx).remote_id())
}

/// Marks every agent edit in `buffer` as reviewed; the text stays.
pub fn keep_file(thread: &Entity<AcpThread>, buffer: &Entity<Buffer>, cx: &mut App) {
    let range = whole(buffer, cx);
    let action_log = thread.read(cx).action_log().clone();
    action_log.update(cx, |log, cx| {
        log.keep_edits_in_range(buffer.clone(), range, None, cx)
    });
}

/// Undoes every pending agent edit in `buffer` and saves it; a file the agent
/// created is deleted.
pub fn reject_file(
    thread: &Entity<AcpThread>,
    buffer: &Entity<Buffer>,
    cx: &mut App,
) -> Task<Result<()>> {
    let range = whole(buffer, cx);
    let action_log = thread.read(cx).action_log().clone();
    action_log
        .update(cx, |log, cx| {
            log.reject_edits_in_ranges(buffer.clone(), vec![range], None, cx)
        })
        .0
}

pub fn keep_all(thread: &Entity<AcpThread>, cx: &mut App) {
    let action_log = thread.read(cx).action_log().clone();
    action_log.update(cx, |log, cx| log.keep_all_edits(None, cx));
}

pub fn reject_all(thread: &Entity<AcpThread>, cx: &mut App) -> Task<()> {
    let action_log = thread.read(cx).action_log().clone();
    action_log.update(cx, |log, cx| log.reject_all_edits(None, cx))
}

/// [`keep_all`] for every tracked thread of the project.
pub fn keep_all_in_project(project_id: EntityId, cx: &mut App) {
    let Some(review) = AgentReview::global(cx) else {
        return;
    };
    let threads: Vec<_> = review
        .read(cx)
        .threads
        .iter()
        .filter(|review| review.project_id == project_id)
        .filter_map(|review| review.thread.upgrade())
        .collect();
    for thread in threads {
        keep_all(&thread, cx);
    }
}

/// What [`revert_turn`] could not do.
#[derive(Debug, Default)]
pub struct RevertOutcome {
    /// Files with no checkpoint text for the turn, left as they are.
    pub missing: Vec<Entity<Buffer>>,
    /// Files reverted in the buffer but not saved, or created files that could
    /// not be deleted, with the error.
    pub failed: Vec<(Entity<Buffer>, anyhow::Error)>,
}

/// Restores every file turn `turn` changed to its checkpoint, each as one
/// undoable edit, and saves it; files the agent created that turn are rejected
/// (deleted). Also undoes user edits made to those files since. A failing
/// file does not stop the others. Refused while the agent is running.
pub fn revert_turn(
    thread: &Entity<AcpThread>,
    turn: u32,
    cx: &mut App,
) -> Task<Result<RevertOutcome>> {
    let files = AgentReview::global(cx).and_then(|review| {
        let review = review.read(cx);
        let thread = review.thread(thread.entity_id())?;
        let files = thread
            .turns
            .iter()
            .find(|t| t.number == turn)?
            .files
            .clone();
        Some((files, thread.label(turn), thread.is_running()))
    });
    let Some((files, label, running)) = files else {
        return Task::ready(Err(anyhow!("agent turn {turn} is not tracked")));
    };
    // apply_diff would drop the hunks the agent edits meanwhile.
    if running {
        return Task::ready(Err(anyhow!(
            "the agent is still working; revert once the turn has ended"
        )));
    }
    let action_log = thread.read(cx).action_log().clone();
    let project = thread.read(cx).project().clone();
    cx.spawn(async move |cx| {
        let mut outcome = RevertOutcome::default();
        for file in files {
            // A closed buffer has no pending edits left to revert.
            let Some(buffer) = file.buffer.upgrade() else {
                continue;
            };
            if file.created {
                let reject = action_log.update(cx, |log, cx| {
                    let range = whole(&buffer, cx);
                    log.reject_edits_in_ranges(buffer.clone(), vec![range], None, cx)
                        .0
                });
                if let Err(error) = reject.await {
                    outcome.failed.push((buffer, error));
                }
                continue;
            }
            // An earlier run's record may share the label: never look one up
            // for a file this turn did not checkpoint.
            let text = if file.checkpointed {
                cx.update(|cx| zenith_local_history::labeled_text(&buffer, &label, cx))
                    .await
                    .log_err()
                    .flatten()
            } else {
                None
            };
            let Some(text) = text else {
                outcome.missing.push(buffer);
                continue;
            };
            let diff = buffer
                .read_with(cx, |buffer, cx| buffer.diff(text, cx))
                .await;
            buffer.update(cx, |buffer, cx| {
                // Keep the revert its own undo step, apart from the agent's edit.
                buffer.finalize_last_transaction();
                buffer.apply_diff(diff, cx);
                buffer.finalize_last_transaction();
            });
            let save = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
            if let Err(error) = save.await {
                outcome.failed.push((buffer, error));
            }
        }
        Ok(outcome)
    })
}

/// Opens the thread's per-hunk review tab. Deferred: it reads the workspace,
/// which may be mid-update when this is called from one of its actions.
pub fn open_diff(
    thread: Entity<AcpThread>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    window.defer(cx, move |window, cx| {
        AgentDiffPane::deploy(thread, workspace, window, cx).log_err();
    });
}
