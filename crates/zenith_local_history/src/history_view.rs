//! "Show History": a center tab with the revisions of one file on the left and a
//! diff of the selected revision against the file's current text on the right.
//!
//! The diff is [`FileDiffView`], so Revert Hunk comes with it: the old side is a
//! scratch buffer holding the revision, the new side is the project buffer.
//! Selecting another revision replaces the text of the scratch buffer.

use crate::capture::{self, Entry};
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff};
use gpui::{
    Action as _, App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Render, Role, SharedString, Styled, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use language::Buffer;
use project::{Project, ProjectPath};
use project_panel::ProjectPanel;
use std::ops::Range;
use time::{OffsetDateTime, UtcOffset};
use time_format::TimestampFormat;
use ui::{KeyBinding, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::size::format_file_size;
use workspace::{Item, Workspace, item::SaveOptions};
use zed_actions::zenith_local_history::ShowFileHistory;

actions!(
    zenith_local_history,
    [
        /// Replaces the file's text with the selected local history revision, as one undoable edit. Save to keep it.
        RevertToRevision,
        /// Copies the text of the selected local history revision.
        CopyRevisionText,
        /// Moves focus to the next control of the history tab: the revision list, then its buttons.
        FocusNextControl,
        /// Moves focus to the previous control of the history tab.
        FocusPreviousControl,
    ]
);

const LIST_WIDTH: f32 = 320.;
const ROW_HEIGHT: f32 = 44.;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowFileHistory, window, cx| {
            show_file_history(workspace, window, cx);
        });
    })
    .detach();
}

/// The file the user means: the project panel's selection while the panel has
/// focus (its context menu), otherwise the active tab.
fn target(workspace: &Workspace, window: &Window, cx: &App) -> Option<ProjectPath> {
    let project = workspace.project().read(cx);
    let from_panel = workspace
        .panel::<ProjectPanel>(cx)
        .filter(|panel| panel.focus_handle(cx).contains_focused(window, cx))
        .and_then(|panel| panel.read(cx).selected_entry_project_path(cx))
        .filter(|path| {
            project
                .entry_for_path(path, cx)
                .is_some_and(|entry| entry.is_file())
        });
    from_panel.or_else(|| workspace.active_item(cx)?.project_path(cx))
}

fn show_file_history(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(project_path) = target(workspace, window, cx) else {
        return;
    };
    if !workspace.project().read(cx).is_local() {
        return; // snapshots are recorded for local projects only
    }
    let existing = workspace
        .items_of_type::<LocalHistoryView>(cx)
        .find(|view| view.read(cx).project_path == project_path);
    let view = match existing {
        Some(view) => {
            workspace.activate_item(&view, true, true, window, cx);
            view
        }
        None => {
            let view = cx.new(|cx| {
                LocalHistoryView::new(workspace.weak_handle(), project_path, workspace, cx)
            });
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        }
    };
    // Pressing the key again in the diff brings focus back to the list.
    view.update(cx, |view, cx| {
        window.focus(&view.focus_handle, cx);
        view.load(window, cx);
    });
}

enum State {
    Loading,
    Ready,
    Failed(SharedString),
}

/// The mounted diff and the scratch buffer holding the selected revision.
struct DiffPane {
    view: Entity<FileDiffView>,
    old_buffer: Entity<Buffer>,
    #[cfg_attr(not(test), allow(dead_code))]
    diff: Entity<BufferDiff>,
}

pub struct LocalHistoryView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    project_path: ProjectPath,
    name: SharedString,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    state: State,
    entries: Vec<Entry>,
    selected: usize,
    /// The project buffer: the "new" side of the diff.
    current: Option<Entity<Buffer>>,
    diff: Option<DiffPane>,
    /// Text of the selected revision, once read. `None` while it loads or when it is gone.
    revision_text: Option<SharedString>,
    /// What the last Revert / Copy did or why it did nothing; read out by screen readers.
    status: Option<SharedString>,
    /// Why the diff pane is empty, when it is not just loading.
    note: Option<SharedString>,
    revert_focus: FocusHandle,
    copy_focus: FocusHandle,
    load_task: Task<()>,
    content_task: Task<()>,
    revert_task: Task<()>,
}

impl LocalHistoryView {
    fn new(
        workspace_handle: WeakEntity<Workspace>,
        project_path: ProjectPath,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().read(cx);
        // A single-file worktree has an empty relative path: use the worktree's name.
        let name = project_path
            .path
            .file_name()
            .map(str::to_owned)
            .or_else(|| {
                let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
                let abs_path = worktree.read(cx).abs_path();
                Some(abs_path.file_name()?.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "file".to_owned());
        Self {
            workspace: workspace_handle,
            project: workspace.project().clone(),
            project_path,
            name: name.into(),
            focus_handle: cx.focus_handle().tab_stop(true),
            scroll_handle: UniformListScrollHandle::new(),
            state: State::Loading,
            entries: Vec::new(),
            selected: 0,
            current: None,
            diff: None,
            revision_text: None,
            status: None,
            note: None,
            revert_focus: cx.focus_handle(),
            copy_focus: cx.focus_handle(),
            load_task: Task::ready(()),
            content_task: Task::ready(()),
            revert_task: Task::ready(()),
        }
    }

    /// Opens the file's buffer, then reads its revisions. The buffer goes first:
    /// opening it records the "Opened" baseline, which the list must include.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The old text stays unusable until the gate has been re-checked below.
        self.revision_text = None;
        self.status = None;
        let Some(worktree) = self
            .project
            .read(cx)
            .worktree_for_id(self.project_path.worktree_id, cx)
        else {
            self.state = State::Failed("The folder is no longer open.".into());
            return cx.notify();
        };
        let Some(root) = capture::root_key(&worktree.read(cx).abs_path()) else {
            self.state = State::Failed(
                "This folder's path is not valid UTF-8, so it has no local history.".into(),
            );
            return cx.notify();
        };
        let rel_path = self.project_path.path.as_unix_str().to_owned();
        let open = self.project.update(cx, |project, cx| {
            project.open_buffer(self.project_path.clone(), cx)
        });
        self.load_task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<_> = async {
                let buffer = open.await.context("opening the file")?;
                // Checked when shown, not only when recorded: a file that is private,
                // ignored or a symlink by now must not expose its old revisions.
                let viewable = this.update(cx, |_, cx| {
                    worktree::File::from_dyn(buffer.read(cx).file())
                        .is_some_and(|file| capture::is_viewable(file, cx))
                })?;
                if !viewable {
                    return Ok((buffer, Vec::new()));
                }
                let list = this.update(cx, |_, cx| capture::list(cx, root, rel_path))?;
                Ok((buffer, list.await.context("reading the history")?))
            }
            .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((buffer, entries)) => {
                    this.current = Some(buffer);
                    this.entries = entries;
                    this.state = State::Ready;
                    this.select(0, window, cx);
                }
                Err(error) => {
                    log::error!("local history: {error:#}");
                    this.state =
                        State::Failed(format!("Could not load the history: {error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        self.selected = ix;
        self.revision_text = None;
        self.status = None;
        self.scroll_handle
            .scroll_to_item(ix, gpui::ScrollStrategy::Center);
        let content = capture::content(cx, entry.revision.blob_hash.clone());
        // A newer selection replaces this task, which cancels it.
        self.content_task = cx.spawn_in(window, async move |this, cx| {
            let text = match content.await {
                Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                other => {
                    let reason = other.err().map_or_else(
                        || "This revision was removed by the retention limits.".to_string(),
                        |error| format!("{error:#}"),
                    );
                    this.update(cx, |this, cx| {
                        this.diff = None;
                        this.note = Some(reason.into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Ok(Some((existing, current))) = this.update(cx, |this, cx| {
                this.revision_text = Some(text.clone().into());
                cx.notify();
                let current = this.current.clone()?;
                Some((
                    this.diff.as_ref().map(|pane| pane.old_buffer.clone()),
                    current,
                ))
            }) else {
                return;
            };
            if let Some(old_buffer) = existing {
                // FileDiffView re-diffs on the buffer edit, after its debounce.
                old_buffer.update(cx, |buffer, cx| {
                    buffer.set_text(text, cx);
                });
                this.update(cx, |this, cx| {
                    this.note = None;
                    cx.notify();
                })
                .ok();
                return;
            }
            let language = current.read_with(cx, |buffer, _| buffer.language().cloned());
            let old_buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(text, cx);
                buffer.set_language(language, cx);
                buffer
            });
            let diff = match build_buffer_diff(&old_buffer, &current, cx).await {
                Ok(diff) => diff,
                Err(error) => {
                    this.update(cx, |this, cx| this.diff_failed(&error, cx))
                        .ok();
                    return;
                }
            };
            this.update_in(cx, |this, window, cx| {
                this.mount_diff(old_buffer, current, diff, window, cx)
            })
            .ok();
        });
        cx.notify();
    }

    /// The selected revision's text, or why it cannot be used: not loaded yet, or the
    /// file is not viewable any more (private, ignored or a symlink by now).
    fn revision_to_use(&self, cx: &App) -> Result<SharedString, &'static str> {
        let Some(text) = &self.revision_text else {
            return Err(if self.entries.get(self.selected).is_none() {
                "No revision is selected."
            } else {
                "This revision's text is not loaded yet."
            });
        };
        let viewable = self.current.as_ref().is_some_and(|buffer| {
            worktree::File::from_dyn(buffer.read(cx).file())
                .is_some_and(|file| capture::is_viewable(file, cx))
        });
        if viewable {
            Ok(text.clone())
        } else {
            Err("This file's history can no longer be shown.")
        }
    }

    /// Why Revert cannot run now, if it cannot.
    fn revert_blocker(&self, cx: &App) -> Option<&'static str> {
        match self.revision_to_use(cx) {
            Err(reason) => Some(reason),
            Ok(_) if self.current.as_ref().is_none_or(|b| b.read(cx).read_only()) => {
                Some("The file is read-only.")
            }
            Ok(_) => None,
        }
    }

    fn set_status(&mut self, status: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some(status.into());
        cx.notify();
    }

    /// Replaces the project buffer's text with the revision: one undoable edit, never
    /// a file write, so the editor shows it and the user saves with the usual key.
    /// A diff keeps cursors and folds outside the changed lines.
    fn revert(&mut self, _: &RevertToRevision, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.revert_blocker(cx) {
            return self.set_status(reason, cx);
        }
        let (Some(text), Some(buffer)) = (self.revision_text.clone(), self.current.clone()) else {
            return;
        };
        if buffer.read(cx).text() == text.as_ref() {
            return self.set_status("The file already matches this revision.", cx);
        }
        let when = self
            .entries
            .get(self.selected)
            .map(|entry| format_time(entry.revision.ts))
            .unwrap_or_default();
        // The text about to be replaced may never have been saved: with recording on,
        // keep it as a revision first.
        capture::capture(&buffer, "before-revert", cx);
        let diff = buffer.read(cx).diff(text.to_string(), cx);
        self.revert_task = cx.spawn(async move |this, cx| {
            let diff = diff.await;
            buffer.update(cx, |buffer, cx| {
                let line_ending = buffer.line_ending();
                // Closed on both sides so the revert neither merges into earlier typing
                // nor swallows later typing: one undo step brings the old text back.
                buffer.finalize_last_transaction();
                buffer.start_transaction();
                buffer.apply_diff(diff, cx);
                // apply_diff drops hunks that clash with edits made while the diff was
                // computed; whatever is left over is replaced as part of the same edit.
                if buffer.text() != text.as_ref() {
                    buffer.set_text(text.as_ref(), cx);
                }
                // The diff carries the revision's line ending; the file keeps its own.
                if buffer.line_ending() != line_ending {
                    buffer.set_line_ending(line_ending, cx);
                }
                buffer.end_transaction(cx);
                buffer.finalize_last_transaction();
            });
            this.update(cx, |this, cx| {
                this.set_status(
                    format!(
                        "Reverted to the {when} revision. Undo with the editor's undo; save to keep."
                    ),
                    cx,
                )
            })
            .ok();
        });
    }

    fn copy_revision(&mut self, _: &CopyRevisionText, _: &mut Window, cx: &mut Context<Self>) {
        match self.revision_to_use(cx) {
            Ok(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                self.set_status("Copied revision text.", cx);
            }
            Err(reason) => self.set_status(reason, cx),
        }
    }

    fn diff_failed(&mut self, error: &anyhow::Error, cx: &mut Context<Self>) {
        self.note = Some(format!("Could not compare: {error:#}").into());
        cx.notify();
    }

    fn mount_diff(
        &mut self,
        old_buffer: Entity<Buffer>,
        current: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = self.project.clone();
        let view = cx.new(|cx| {
            FileDiffView::new(
                old_buffer.clone(),
                current,
                diff.clone(),
                project,
                workspace,
                window,
                cx,
            )
        });
        self.diff = Some(DiffPane {
            view,
            old_buffer,
            diff,
        });
        self.note = None;
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.entries.len().saturating_sub(1);
        self.select((self.selected + 1).min(last), window, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select(self.selected.saturating_sub(1), window, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select(0, window, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.entries.len().saturating_sub(1), window, cx);
    }

    /// Enter moves into the diff, so hunks can be reverted from the keyboard.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = &self.diff {
            window.focus(&pane.view.focus_handle(cx), cx);
        }
    }

    /// Escape steps back: from the diff or a button to the list, from the list closes the tab.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.is_focused(window) {
            window.dispatch_action(workspace::CloseActiveItem::default().boxed_clone(), cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
    }

    fn focus_next_control(
        &mut self,
        _: &FocusNextControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
    }

    fn focus_previous_control(
        &mut self,
        _: &FocusPreviousControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let entry = &self.entries[ix];
        let selected = ix == self.selected;
        let title = row_title(entry);
        let when = format_time(entry.revision.ts);
        let detail = match entry.revision.label {
            Some(_) => format!("{} · {when}", source_name(&entry.revision.source)),
            None => when,
        };
        let delta = size_delta(entry, self.entries.get(ix + 1));
        ListItem::new(("zenith-history-row", ix))
            .spacing(ListItemSpacing::Dense)
            .height(px(ROW_HEIGHT))
            .rounded()
            .toggle_state(selected)
            .aria_role(Role::ListItem)
            // Selection is reported by the role (`aria_selected`), not repeated in the label.
            .aria_label(format!("{title}, {detail}, {delta}"))
            .when(selected, |item| item.aria_active_descendant())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.select(ix, window, cx);
            }))
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(Label::new(title).truncate())
                            .child(Label::new(delta).color(Color::Muted).size(LabelSize::Small)),
                    )
                    .child(
                        Label::new(detail)
                            .color(Color::Muted)
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
            )
    }

    /// A centred message that still takes focus, so Escape closes the tab.
    fn placeholder(&self, text: SharedString, hint: Option<&'static str>) -> gpui::AnyElement {
        v_flex()
            .id("zenith-history-placeholder")
            .role(Role::Status)
            .aria_label(match hint {
                Some(hint) => format!("{text}. {hint}"),
                None => text.to_string(),
            })
            .key_context("LocalHistoryList")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(Label::new(text).color(Color::Muted))
            .when_some(hint, |this, hint| {
                this.child(Label::new(hint).color(Color::Muted).size(LabelSize::Small))
            })
            .into_any_element()
    }

    fn render_split(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let list = uniform_list(
            "zenith-history-rows",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                range
                    .map(|ix| this.render_row(ix, cx).into_any_element())
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll_handle)
        .flex_1();
        let selected = self.entries.get(self.selected);
        let revert_blocker = self.revert_blocker(cx);
        let copy_blocker = self.revision_to_use(cx).err();
        // A disabled button is not a tab stop. (The handle's own flag counts: a tracked handle
        // ignores the element's `tab_index`.)
        let revert_focus = self.revert_focus.clone().tab_stop(revert_blocker.is_none());
        let copy_focus = self.copy_focus.clone().tab_stop(copy_blocker.is_none());
        let description = |blocker: Option<&str>, available: &str| match blocker {
            Some(reason) => format!("Unavailable: {reason}"),
            None => available.to_string(),
        };
        h_flex()
            .size_full()
            .child(
                div()
                    .w(px(LIST_WIDTH))
                    .h_full()
                    .flex_none()
                    .border_r_1()
                    .border_color(colors.border)
                    .p_0p5()
                    .child(
                        v_flex()
                            .id("zenith-history-list")
                            .role(Role::List)
                            .aria_label(format!("Revisions of {}", self.name))
                            .key_context("LocalHistoryList")
                            .track_focus(&self.focus_handle)
                            .on_action(cx.listener(Self::select_next))
                            .on_action(cx.listener(Self::select_previous))
                            .on_action(cx.listener(Self::select_first))
                            .on_action(cx.listener(Self::select_last))
                            .on_action(cx.listener(Self::confirm))
                            .size_full()
                            .p_0p5()
                            // A visible focus ring for keyboard users.
                            .border_1()
                            .border_color(colors.border_transparent)
                            .rounded_sm()
                            .focus_visible(|style| style.border_color(colors.border_focused))
                            .child(list),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .when_some(selected, |pane, entry| {
                        pane.child(
                            h_flex()
                                .flex_none()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .justify_between()
                                .border_b_1()
                                .border_color(colors.border_variant)
                                .child(
                                    Label::new(format!(
                                        "{} · {} compared with the current file",
                                        row_title(entry),
                                        format_time(entry.revision.ts)
                                    ))
                                    .color(Color::Muted)
                                    .size(LabelSize::Small)
                                    .truncate(),
                                )
                                .child(
                                    h_flex()
                                        .flex_none()
                                        .gap_1()
                                        .child(
                                            Button::new(
                                                "zenith-history-revert",
                                                "Revert to this revision",
                                            )
                                            .label_size(LabelSize::Small)
                                            .track_focus(&revert_focus)
                                            .disabled(revert_blocker.is_some())
                                            .aria_description(description(
                                                revert_blocker,
                                                "Replaces the file's text with this revision. Undo to go back; save to keep.",
                                            ))
                                            .key_binding(KeyBinding::for_action_in(
                                                &RevertToRevision,
                                                &self.focus_handle,
                                                cx,
                                            ))
                                            .tooltip(Tooltip::for_action_title(
                                                "Replace the file's text with this revision (undo to go back, save to keep)",
                                                &RevertToRevision,
                                            ))
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.revert(&RevertToRevision, window, cx)
                                                },
                                            )),
                                        )
                                        .child(
                                            Button::new(
                                                "zenith-history-copy",
                                                "Copy revision text",
                                            )
                                            .label_size(LabelSize::Small)
                                            .track_focus(&copy_focus)
                                            .disabled(copy_blocker.is_some())
                                            .aria_description(description(
                                                copy_blocker,
                                                "Copies this revision's text to the clipboard.",
                                            ))
                                            .key_binding(KeyBinding::for_action_in(
                                                &CopyRevisionText,
                                                &self.focus_handle,
                                                cx,
                                            ))
                                            .tooltip(Tooltip::for_action_title(
                                                "Copy this revision's text",
                                                &CopyRevisionText,
                                            ))
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.copy_revision(
                                                        &CopyRevisionText,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                        ),
                                ),
                        )
                    })
                    .child(match &self.diff {
                        Some(pane) => div()
                            .flex_1()
                            .min_h_0()
                            .child(pane.view.clone())
                            .into_any_element(),
                        None => message(
                            self.note
                                .clone()
                                .unwrap_or_else(|| "Loading revision…".into()),
                        ),
                    }),
            )
    }
}

fn message(text: impl Into<SharedString>) -> gpui::AnyElement {
    let text = text.into();
    v_flex()
        .id("zenith-history-message")
        .role(Role::Status)
        .aria_label(text.clone())
        .size_full()
        .items_center()
        .justify_center()
        .gap_1()
        .child(Label::new(text).color(Color::Muted))
        .into_any_element()
}

fn source_name(source: &str) -> String {
    match source {
        "open" => "Opened".to_string(),
        "save" => "Saved".to_string(),
        "before-revert" => "Before revert".to_string(),
        "external" => "External change".to_string(),
        "agent" => "Agent".to_string(),
        other => other.to_string(),
    }
}

fn row_title(entry: &Entry) -> String {
    let label = entry.revision.label.as_deref();
    // Agent checkpoints are labelled `agent:<session>:<turn>`; the session id is noise.
    match label.and_then(|label| label.rsplit_once(':')) {
        Some((_, turn)) if entry.revision.source == "agent" => format!("Agent · turn {turn}"),
        _ => label
            .map(str::to_owned)
            .unwrap_or_else(|| source_name(&entry.revision.source)),
    }
}

/// Size change against the next older revision; the oldest shows its size.
fn size_delta(entry: &Entry, older: Option<&Entry>) -> String {
    let Some(older) = older else {
        return format_file_size(entry.size, false);
    };
    match entry.size.cmp(&older.size) {
        std::cmp::Ordering::Greater => {
            format!("+{}", format_file_size(entry.size - older.size, false))
        }
        std::cmp::Ordering::Less => {
            format!("-{}", format_file_size(older.size - entry.size, false))
        }
        std::cmp::Ordering::Equal => "no size change".to_string(),
    }
}

pub(crate) fn format_time(ts: i64) -> String {
    let Ok(timestamp) = OffsetDateTime::from_unix_timestamp(ts) else {
        return String::new();
    };
    time_format::format_localized_timestamp(
        timestamp,
        OffsetDateTime::now_utc(),
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
        TimestampFormat::EnhancedAbsolute,
    )
}

impl EventEmitter<()> for LocalHistoryView {}

impl Focusable for LocalHistoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for LocalHistoryView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        format!("History: {}", self.name).into()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZenithHistory).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(format!("Local history of {}", self.project_path.path.as_unix_str()).into())
    }

    fn active_project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    // Edits and Revert Hunk in the right pane change the project buffer, so
    // saving, the dirty dot, conflicts and the close prompt follow that buffer.
    fn is_dirty(&self, cx: &App) -> bool {
        self.current
            .as_ref()
            .is_some_and(|buffer| buffer.read(cx).is_dirty())
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.current
            .as_ref()
            .is_some_and(|buffer| buffer.read(cx).has_conflict())
    }

    fn can_save(&self, cx: &App) -> bool {
        self.diff
            .as_ref()
            .is_some_and(|pane| pane.view.read(cx).can_save(cx))
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        match &self.diff {
            Some(pane) => pane
                .view
                .update(cx, |view, cx| view.save(options, project, window, cx)),
            None => Task::ready(Ok(())),
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        if let Some(pane) = &self.diff {
            pane.view.read(cx).for_each_project_item(cx, f);
        }
    }

    /// Lets the diff's own toolbar (unified / split) show for this tab.
    fn act_as_type<'a>(
        &'a self,
        type_id: std::any::TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == std::any::TypeId::of::<Self>() {
            return Some(self_handle.clone().into());
        }
        let pane = self.diff.as_ref()?;
        pane.view.read(cx).act_as_type(type_id, &pane.view, cx)
    }
}

impl Render for LocalHistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            State::Loading => self.placeholder("Loading history…".into(), None),
            State::Failed(reason) => self.placeholder(reason.clone(), None),
            State::Ready if self.entries.is_empty() => self.placeholder(
                format!("No local history for {} yet.", self.name).into(),
                Some("A revision is recorded when a file is opened and each time it is saved."),
            ),
            State::Ready => self.render_split(cx).into_any_element(),
        };
        v_flex()
            .key_context("LocalHistoryView")
            // On the root, so the actions work from the list and from the diff.
            .on_action(cx.listener(Self::revert))
            .on_action(cx.listener(Self::copy_revision))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_next_control))
            .on_action(cx.listener(Self::focus_previous_control))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(div().flex_1().min_h_0().child(body))
            // Always present, so a screen reader sees the text change when it does.
            .child(
                div()
                    .id("zenith-history-status")
                    .role(Role::Status)
                    .aria_label(self.status.clone().unwrap_or_default())
                    .flex_none()
                    .when_some(self.status.clone(), |this, status| {
                        this.px_2()
                            .py_1()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                Label::new(status)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                    }),
            )
    }
}

#[cfg(test)]
#[path = "history_view_tests.rs"]
mod tests;
