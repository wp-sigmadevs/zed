//! The Agent Review tool window: every agent thread of the project with its status, the
//! files it changed that nobody kept or rejected yet, and keep / reject / revert controls.

use std::{collections::HashMap, ops::Range};

use acp_thread::AcpThread;
use action_log::DiffStats;
use agent_ui::{Keep, KeepAll, Reject, RejectAll};
use gpui::{
    Action, AnyElement, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Pixels,
    PromptLevel, Role, ScrollStrategy, Subscription, UniformListScrollHandle, WeakEntity,
    uniform_list,
};
use language::Buffer;
use settings::Settings as _;
use ui::{ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::DetachAndPromptErr as _,
};
use zenith_title_bar::AGENT_ORANGE;

use crate::{
    AgentReview, AgentReviewSettings, RevertTurn, ReviewStatus, ToggleFocus, keep_all, keep_file,
    open_diff, reject_all, reject_file, revert_turn,
};

const ROW_HEIGHT: f32 = 28.;

type Handler = fn(&mut AgentReviewPanel, &mut Window, &mut Context<AgentReviewPanel>);
const FILE_INDENT: f32 = 20.;
// Unique among dock panels (debug builds panic on a duplicate); 8-13 are other Zenith panels.
const ACTIVATION_PRIORITY: u32 = 14;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<AgentReviewPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RevertTurn, window, cx| {
            if let Some(panel) = workspace.panel::<AgentReviewPanel>(cx) {
                panel.update(cx, |panel, cx| panel.revert_latest_turn(window, cx));
            }
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| AgentReviewPanel::new(workspace, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

fn status_word(status: ReviewStatus) -> &'static str {
    match status {
        ReviewStatus::Running => "Running",
        ReviewStatus::NeedsInput => "Needs input",
        ReviewStatus::Review => "Review",
        ReviewStatus::Idle => "Done",
    }
}

fn files_word(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

/// One line of the list; a thread is followed by its pending files and its action buttons,
/// all flat so the list keeps one row height.
#[derive(Clone)]
enum Row {
    Thread {
        thread: WeakEntity<AcpThread>,
        title: SharedString,
        status: ReviewStatus,
        turn: u32,
        pending: usize,
    },
    File {
        thread: WeakEntity<AcpThread>,
        buffer: WeakEntity<Buffer>,
        name: SharedString,
        directory: SharedString,
        added: u32,
        removed: u32,
    },
    Actions {
        thread: WeakEntity<AcpThread>,
        turn: u32,
        pending: usize,
    },
}

impl Row {
    fn thread(&self) -> &WeakEntity<AcpThread> {
        match self {
            Row::Thread { thread, .. } | Row::File { thread, .. } | Row::Actions { thread, .. } => {
                thread
            }
        }
    }

    /// The line read in tests and by screen readers.
    fn text(&self) -> String {
        match self {
            Row::Thread {
                title,
                status,
                turn,
                pending,
                ..
            } => format!(
                "{title}, {}, turn {turn}, {} to review",
                status_word(*status),
                files_word(*pending)
            ),
            Row::File {
                name,
                directory,
                added,
                removed,
                ..
            } if directory.is_empty() => format!("{name}, +{added} −{removed}"),
            Row::File {
                name,
                directory,
                added,
                removed,
                ..
            } => format!("{name} in {directory}, +{added} −{removed}"),
            Row::Actions { turn, .. } => format!("Actions for turn {turn}"),
        }
    }
}

pub struct AgentReviewPanel {
    project_id: EntityId,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    rows: Vec<Row>,
    selected: Option<usize>,
    /// Pending files of all threads, for the tool-window button badge.
    pending_files: usize,
    _review_subscription: Option<Subscription>,
    /// Per thread's action log: the +N −M counts change without a queue notification.
    action_log_subscriptions: HashMap<EntityId, Subscription>,
}

impl AgentReviewPanel {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let review_subscription = AgentReview::global(cx)
            .map(|review| cx.observe(&review, |this, _, cx| this.rebuild(cx)));
        let mut panel = Self {
            project_id: workspace.project().entity_id(),
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            rows: Vec::new(),
            selected: None,
            pending_files: 0,
            _review_subscription: review_subscription,
            action_log_subscriptions: HashMap::new(),
        };
        panel.rebuild(cx);
        panel
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        let mut action_logs = Vec::new();
        if let Some(review) = AgentReview::global(cx) {
            // Newest thread first.
            for thread_review in review.read(cx).threads().iter().rev() {
                if thread_review.project_id != self.project_id {
                    continue;
                }
                let Some(thread) = thread_review.thread.upgrade() else {
                    continue;
                };
                let turn = thread_review.turn();
                // A thread that never ran a turn has nothing to show.
                if turn == 0 && thread_review.status == ReviewStatus::Idle {
                    continue;
                }
                let action_log = thread.read(cx).action_log().clone();
                let files: Vec<Row> = action_log
                    .read(cx)
                    .changed_buffers(cx)
                    .map(|(buffer, diff)| {
                        let path = buffer.read(cx).file().map(|file| file.path().clone());
                        let name = path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .unwrap_or("untitled")
                            .to_string();
                        let directory = path
                            .as_ref()
                            .and_then(|path| path.parent())
                            .map(|parent| parent.as_unix_str().to_string())
                            .unwrap_or_default();
                        let stats = DiffStats::single_file(diff.read(cx));
                        Row::File {
                            thread: thread_review.thread.clone(),
                            buffer: buffer.downgrade(),
                            name: name.into(),
                            directory: directory.into(),
                            added: stats.lines_added,
                            removed: stats.lines_removed,
                        }
                    })
                    .collect();
                let pending = files.len();
                rows.push(Row::Thread {
                    thread: thread_review.thread.clone(),
                    title: thread_review
                        .title
                        .clone()
                        .unwrap_or_else(|| "Agent thread".into()),
                    status: thread_review.status,
                    turn,
                    pending,
                });
                rows.extend(files);
                rows.push(Row::Actions {
                    thread: thread_review.thread.clone(),
                    turn,
                    pending,
                });
                action_logs.push(action_log);
            }
        }

        self.action_log_subscriptions
            .retain(|id, _| action_logs.iter().any(|log| log.entity_id() == *id));
        for action_log in action_logs {
            self.action_log_subscriptions
                .entry(action_log.entity_id())
                .or_insert_with(|| cx.observe(&action_log, |this, _, cx| this.rebuild(cx)));
        }
        self.pending_files = rows
            .iter()
            .filter(|row| matches!(row, Row::File { .. }))
            .count();
        self.rows = rows;
        self.selected = self
            .rows
            .len()
            .checked_sub(1)
            .and_then(|last| self.selected.map(|ix| ix.min(last)));
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        self.select(self.selected.map_or(0, |ix| (ix + 1).min(last)), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.rows.is_empty() {
            self.select(self.selected.map_or(0, |ix| ix.saturating_sub(1)), cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select(0, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.select(last, cx);
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.selected.and_then(|ix| self.rows.get(ix))
    }

    /// Enter: the thread's per-hunk review tab.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.selected_row().and_then(|row| row.thread().upgrade()) {
            open_diff(thread, self.workspace.clone(), window, cx);
        }
    }

    /// Escape: hand the keyboard back to the editor.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.focus_center_pane(window, cx));
        }
    }

    /// On a file: keeps that file; on a thread's lines: keeps all of the thread.
    fn keep(&mut self, _: &Keep, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected_row().cloned() {
            Some(Row::File { thread, buffer, .. }) => {
                if let (Some(thread), Some(buffer)) = (thread.upgrade(), buffer.upgrade()) {
                    keep_file(&thread, &buffer, cx);
                }
            }
            Some(row) => {
                if let Some(thread) = row.thread().upgrade() {
                    keep_all(&thread, cx);
                }
            }
            None => {}
        }
    }

    /// Only on a file: rejecting a whole thread is Reject All, never a stray key.
    fn reject(&mut self, _: &Reject, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::File { thread, buffer, .. }) = self.selected_row().cloned() else {
            return;
        };
        if let (Some(thread), Some(buffer)) = (thread.upgrade(), buffer.upgrade()) {
            reject_file(&thread, &buffer, cx).detach_and_prompt_err(
                "Failed to reject the agent's edits",
                window,
                cx,
                |_, _, _| None,
            );
        }
    }

    fn keep_all(&mut self, _: &KeepAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.selected_row().and_then(|row| row.thread().upgrade()) {
            keep_all(&thread, cx);
        }
    }

    fn reject_all(&mut self, _: &RejectAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.selected_row().and_then(|row| row.thread().upgrade()) {
            reject_all(&thread, cx).detach();
        }
    }

    /// The selected thread's latest turn, else the newest thread's.
    fn revert_latest_turn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self
            .selected_row()
            .or_else(|| self.rows.first())
            .map(|row| row.thread().clone());
        let turn = thread.as_ref().and_then(|thread| {
            let review = AgentReview::global(cx)?;
            let turn = review.read(cx).thread(thread.entity_id())?.turn();
            (turn > 0).then_some(turn)
        });
        if let (Some(thread), Some(turn)) = (thread, turn) {
            self.revert(thread, turn, window, cx);
        }
    }

    fn revert(
        &mut self,
        thread: WeakEntity<AcpThread>,
        turn: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = thread.upgrade() else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Revert agent turn {turn}?"),
            Some(
                "Every file this turn changed goes back to its text from before the turn and is \
                 saved. This also undoes your own edits to those files made during that turn or \
                 since. Each file can be brought back with Undo.",
            ),
            &["Revert Turn", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return anyhow::Ok(());
            }
            let missing = cx.update(|_, cx| revert_turn(&thread, turn, cx))?.await?;
            if missing.is_empty() {
                return Ok(());
            }
            let names = cx.update(|_, cx| {
                missing
                    .iter()
                    .map(|buffer| {
                        buffer
                            .read(cx)
                            .file()
                            .map(|file| file.path().as_unix_str().to_string())
                            .unwrap_or_else(|| "untitled".to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })?;
            let shown = this.update_in(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Info,
                    "Some files were left as they are",
                    Some(&format!(
                        "Local History has no checkpoint of these files for turn {turn}: {names}."
                    )),
                    &["OK"],
                    cx,
                )
            })?;
            shown.await.log_err();
            Ok(())
        })
        .detach_and_prompt_err(
            "Failed to revert the agent turn",
            window,
            cx,
            |_, _, _| None,
        );
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let orange: Hsla = gpui::rgb(AGENT_ORANGE).into();
        let thread_id = row.thread().entity_id();
        let item = |id: String| {
            ListItem::new(SharedString::from(id))
                .height(px(ROW_HEIGHT))
                .rounded()
                .toggle_state(self.selected == Some(ix))
                .aria_role(Role::TreeItem)
                .aria_label(row.text())
                .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
        };
        // A click calls the handler directly: a dispatched action would go to whatever has
        // focus (often the editor). Selecting the row first makes the handler act on it.
        let button = |id: String,
                      label: &'static str,
                      aria: String,
                      action: &dyn Action,
                      handler: Handler| {
            Button::new(SharedString::from(id), label)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .aria_label(aria)
                .tooltip(Tooltip::for_action_title(label, action))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.selected = Some(ix);
                    handler(this, window, cx);
                }))
        };
        match row {
            Row::Thread {
                title,
                status,
                turn,
                ..
            } => item(format!("agent-review-thread-{thread_id}"))
                .start_slot(
                    Icon::new(IconName::ZenithBot)
                        .size(IconSize::Small)
                        .color(Color::Custom(orange)),
                )
                .child(
                    Label::new(title.clone())
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                )
                .end_slot(
                    h_flex()
                        .gap_2()
                        // The status is a word, never the colour alone.
                        .child(
                            div()
                                .px_1p5()
                                .rounded_full()
                                .bg(orange.opacity(0.14))
                                .child(
                                    Label::new(status_word(*status))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Custom(orange)),
                                ),
                        )
                        .child(
                            Label::new(format!("turn {turn}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
            Row::File {
                buffer,
                name,
                directory,
                added,
                removed,
                ..
            } => {
                let buffer_id = buffer.entity_id();
                item(format!("agent-review-file-{thread_id}-{buffer_id}"))
                    .indent_level(1)
                    .indent_step_size(px(FILE_INDENT))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1p5()
                            .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
                            .child(
                                Label::new(directory.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("+{added}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Created),
                            )
                            .child(
                                Label::new(format!("−{removed}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Deleted),
                            )
                            .child(button(
                                format!("agent-review-keep-{thread_id}-{buffer_id}"),
                                "Keep",
                                format!("Keep the agent's edits to {name}"),
                                &Keep,
                                |this, window, cx| this.keep(&Keep, window, cx),
                            ))
                            .child(button(
                                format!("agent-review-reject-{thread_id}-{buffer_id}"),
                                "Reject",
                                format!("Reject the agent's edits to {name}"),
                                &Reject,
                                |this, window, cx| this.reject(&Reject, window, cx),
                            )),
                    )
                    .into_any_element()
            }
            Row::Actions { turn, pending, .. } => item(format!("agent-review-actions-{thread_id}"))
                .indent_level(1)
                .indent_step_size(px(FILE_INDENT))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            button(
                                format!("agent-review-keep-all-{thread_id}"),
                                "Keep All",
                                format!("Keep all {} of this thread", files_word(*pending)),
                                &KeepAll,
                                |this, window, cx| this.keep_all(&KeepAll, window, cx),
                            )
                            .disabled(*pending == 0),
                        )
                        .child(
                            button(
                                format!("agent-review-reject-all-{thread_id}"),
                                "Reject All",
                                format!("Reject all {} of this thread", files_word(*pending)),
                                &RejectAll,
                                |this, window, cx| this.reject_all(&RejectAll, window, cx),
                            )
                            .disabled(*pending == 0),
                        )
                        .child(button(
                            format!("agent-review-open-diff-{thread_id}"),
                            "Open Diff",
                            "Open this thread's changes hunk by hunk".to_string(),
                            &menu::Confirm,
                            |this, window, cx| this.confirm(&menu::Confirm, window, cx),
                        ))
                        .child(
                            Button::new(
                                SharedString::from(format!("agent-review-revert-{thread_id}")),
                                format!("Revert Turn {turn}"),
                            )
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .aria_label(format!(
                                "Revert turn {turn}, including your own edits from that turn"
                            ))
                            .tooltip(Tooltip::for_action_title("Revert Turn", &RevertTurn))
                            .disabled(*turn == 0)
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.selected = Some(ix);
                                    this.revert_latest_turn(window, cx);
                                },
                            )),
                        ),
                )
                .into_any_element(),
        }
    }
}

impl Render for AgentReviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.rows.is_empty() {
            v_flex()
                .flex_1()
                .p_4()
                .gap_1()
                .items_center()
                .justify_center()
                .child(Label::new("No agent activity in this project.").color(Color::Muted))
                .child(
                    Label::new(
                        "Agent threads show here while they run and while their edits wait \
                         for review.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        } else {
            uniform_list(
                "zenith-agent-review-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        let summary = match self.pending_files {
            0 => String::new(),
            count => format!("{} to review", files_word(count)),
        };
        v_flex()
            .id("zenith-agent-review")
            .debug_selector(|| "zenith-agent-review-panel".into())
            .role(Role::Tree)
            .aria_label("Agent Review")
            .key_context("AgentReviewPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::keep))
            .on_action(cx.listener(Self::reject))
            .on_action(cx.listener(Self::keep_all))
            .on_action(cx.listener(Self::reject_all))
            .size_full()
            .child(
                h_flex()
                    .h(px(32.))
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .child(Label::new("Agent Review").weight(FontWeight::MEDIUM))
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(body)
    }
}

impl Focusable for AgentReviewPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for AgentReviewPanel {}

impl Panel for AgentReviewPanel {
    fn persistent_name() -> &'static str {
        "Agent Review Panel"
    }

    fn panel_key() -> &'static str {
        "AgentReviewPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    // shortcut: right only; a dock setting needs `settings_content`, a full rebuild.
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    // Reads only this panel's own fields and settings: the dock may call these mid-update.
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZenithBot)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Agent Review")
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        (self.pending_files > 0).then(|| self.pending_files.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        ACTIVATION_PRIORITY
    }

    fn enabled(&self, cx: &App) -> bool {
        AgentReviewSettings::get_global(cx).enabled
    }
}

#[cfg(test)]
impl AgentReviewPanel {
    pub(crate) fn row_texts(&self) -> Vec<String> {
        self.rows.iter().map(Row::text).collect()
    }

    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected
    }
}
