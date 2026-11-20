use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
    px,
};
use ui::{
    Button, ButtonCommon, ButtonStyle, Checkbox, Color,
    Headline, HeadlineSize, Icon, IconName, Label, LabelCommon, LabelSize, SharedString,
    ToggleState, v_flex, h_flex, prelude::*,
};
use workspace::{ModalView, Workspace};
use crate::{RefactorKind, RefactorPlan};

pub fn open_refactor_preview(
    workspace: &mut Workspace,
    plan: RefactorPlan,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        RefactorPreviewModal::new(plan, workspace_handle, window, cx)
    });
}

pub struct RefactorPreviewModal {
    pub plan: RefactorPlan,
    pub selected_file_index: usize,
    pub focus_handle: FocusHandle,
    pub workspace: WeakEntity<Workspace>,
    pub is_applying: bool,
    pub status_message: Option<String>,
}

impl RefactorPreviewModal {
    pub fn new(
        plan: RefactorPlan,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            plan,
            selected_file_index: 0,
            focus_handle,
            workspace,
            is_applying: false,
            status_message: None,
        }
    }

    pub fn toggle_file_inclusion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(diff) = self.plan.file_diffs.get_mut(index) {
            diff.is_included = !diff.is_included;
            cx.notify();
        }
    }

    pub fn set_all_files_included(&mut self, included: bool, cx: &mut Context<Self>) {
        for diff in &mut self.plan.file_diffs {
            diff.is_included = included;
        }
        cx.notify();
    }

    pub fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.plan.file_diffs.len() {
            self.selected_file_index = index;
            cx.notify();
        }
    }

    pub fn cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    pub fn apply(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_applying {
            return;
        }

        let included_count = self.plan.file_diffs.iter().filter(|d| d.is_included).count();
        if included_count == 0 {
            self.status_message = Some("No files selected to apply".to_string());
            cx.notify();
            return;
        }

        self.is_applying = true;
        cx.notify();

        match self.plan.apply_to_disk() {
            Ok(affected) => {
                log::info!("Applied refactoring to {} files", affected.len());
                cx.emit(DismissEvent);
            }
            Err(err) => {
                self.is_applying = false;
                self.status_message = Some(format!("Failed to apply: {err}"));
                cx.notify();
            }
        }
    }
}

impl ModalView for RefactorPreviewModal {}

impl Focusable for RefactorPreviewModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for RefactorPreviewModal {}

impl Render for RefactorPreviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total_files = self.plan.file_diffs.len();
        let selected_count = self.plan.file_diffs.iter().filter(|d| d.is_included).count();
        let total_replacements: usize = self.plan.file_diffs.iter().map(|d| d.replacements.len()).sum();

        let kind_label = match &self.plan.kind {
            RefactorKind::MoveClass { class_name, .. } => format!("Move Class: {class_name}"),
            RefactorKind::SafeDelete { symbol_name, .. } => format!("Safe Delete: {symbol_name}"),
            RefactorKind::ChangeSignature { function_name, .. } => {
                format!("Change Signature: {function_name}")
            }
        };

        v_flex()
            .id("refactor-preview-modal")
            .elevation_3(cx)
            .w(px(800.0))
            .h(px(560.0))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .overflow_hidden()
            .key_context("RefactorPreviewModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel(window, cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.apply(window, cx)))
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Sparkle).color(Color::Accent))
                            .child(Headline::new(kind_label).size(HeadlineSize::Small)),
                    )
                    .child(
                        Button::new("refactor-modal-close", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    ),
            )
            .when(!self.plan.warnings.is_empty(), |this| {
                this.child(
                    v_flex()
                        .px_4()
                        .py_2()
                        .bg(cx.theme().status().warning_background)
                        .border_b_1()
                        .border_color(cx.theme().status().warning_border)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Icon::new(IconName::Warning).color(Color::Warning))
                                .child(
                                    Label::new("Warning: Unsafe Refactoring Detected")
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                ),
                        )
                        .children(self.plan.warnings.iter().map(|w| {
                            Label::new(w.clone())
                                .size(LabelSize::Small)
                                .color(Color::Warning)
                        })),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .w(px(280.0))
                            .h_full()
                            .border_r_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().panel_background)
                            .child(
                                h_flex()
                                    .px_3()
                                    .py_2()
                                    .justify_between()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(cx.theme().colors().border)
                                    .child(
                                        Label::new(format!("Files ({selected_count}/{total_files})"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(
                                                Button::new("select-all-btn", "All")
                                                    .style(ButtonStyle::Subtle)
                                                    .on_click(cx.listener(|this, _, _window, cx| {
                                                        this.set_all_files_included(true, cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new("select-none-btn", "None")
                                                    .style(ButtonStyle::Subtle)
                                                    .on_click(cx.listener(|this, _, _window, cx| {
                                                        this.set_all_files_included(false, cx);
                                                    })),
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .id("refactor-files-scroll")
                                    .flex_1()
                                    .overflow_y_scroll()
                                    .children(self.plan.file_diffs.iter().enumerate().map(|(ix, diff)| {
                                        let is_selected = ix == self.selected_file_index;
                                        let file_name = diff
                                            .file_path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_else(|| "file".to_string());
                                        let parent_dir = diff
                                            .file_path
                                            .parent()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_default();
                                        let edit_count = diff.replacements.len();

                                        h_flex()
                                            .id(format!("file-row-{}", ix))
                                            .px_3()
                                            .py_2()
                                            .gap_2()
                                            .items_center()
                                            .cursor_pointer()
                                            .when(is_selected, |this| {
                                                this.bg(cx.theme().colors().element_selected)
                                            })
                                            .hover(|this| this.bg(cx.theme().colors().element_hover))
                                            .on_click(cx.listener(move |this, _, _window, cx| {
                                                this.select_file(ix, cx);
                                            }))
                                            .child(
                                                Checkbox::new(
                                                    SharedString::from(format!("check-{}", ix)),
                                                    if diff.is_included {
                                                        ToggleState::Selected
                                                    } else {
                                                        ToggleState::Unselected
                                                    },
                                                )
                                                .on_click(cx.listener(move |this, _, _window, cx| {
                                                    this.toggle_file_inclusion(ix, cx);
                                                })),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .overflow_hidden()
                                                    .child(Label::new(file_name).size(LabelSize::Small))
                                                    .child(
                                                        Label::new(parent_dir)
                                                            .size(LabelSize::XSmall)
                                                            .color(Color::Muted),
                                                    ),
                                            )
                                            .child(
                                                Label::new(format!("{edit_count}"))
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Accent),
                                            )
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .bg(cx.theme().colors().elevated_surface_background)
                            .children(
                                if let Some(diff) = self.plan.file_diffs.get(self.selected_file_index) {
                                    vec![
                                        h_flex()
                                            .px_4()
                                            .py_2()
                                            .justify_between()
                                            .items_center()
                                            .border_b_1()
                                            .border_color(cx.theme().colors().border)
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .items_center()
                                                    .child(Icon::new(IconName::File).color(Color::Muted))
                                                    .child(
                                                        Label::new(diff.file_path.to_string_lossy().to_string())
                                                            .size(LabelSize::Small),
                                                    ),
                                            )
                                            .child(
                                                Label::new(format!("{} replacement(s)", diff.replacements.len()))
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .into_any_element(),
                                        v_flex()
                                            .id("refactor-diff-scroll")
                                            .flex_1()
                                            .overflow_y_scroll()
                                            .p_3()
                                            .gap_3()
                                            .children(diff.replacements.iter().enumerate().map(|(rix, rep)| {
                                                v_flex()
                                                    .id(format!("diff-card-{rix}"))
                                                    .border_1()
                                                    .border_color(cx.theme().colors().border)
                                                    .rounded_md()
                                                    .overflow_hidden()
                                                    .child(
                                                        h_flex()
                                                            .px_2()
                                                            .py_1()
                                                            .bg(cx.theme().colors().panel_background)
                                                            .child(
                                                                Label::new(format!("Line {}", rep.line))
                                                                    .size(LabelSize::XSmall)
                                                                    .color(Color::Muted),
                                                            ),
                                                    )
                                                    .child(
                                                        v_flex()
                                                            .px_3()
                                                            .py_2()
                                                            .gap_1()
                                                            .bg(cx.theme().status().deleted_background)
                                                            .border_b_1()
                                                            .border_color(cx.theme().status().deleted_border)
                                                            .child(
                                                                Label::new(format!("- {}", rep.old_text))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Error),
                                                            ),
                                                    )
                                                    .child(
                                                        v_flex()
                                                            .px_3()
                                                            .py_2()
                                                            .gap_1()
                                                            .bg(cx.theme().status().created_background)
                                                            .child(
                                                                Label::new(format!("+ {}", rep.new_text))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Created),
                                                            ),
                                                    )
                                            }))
                                            .into_any_element(),
                                    ]
                                } else {
                                    vec![v_flex()
                                        .flex_1()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            Label::new("No file selected")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .into_any_element()]
                                },
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .justify_between()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new(format!(
                                    "{selected_count} of {total_files} files selected ({total_replacements} total changes)"
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .when_some(self.status_message.clone(), |this, msg| {
                                this.child(Label::new(msg).size(LabelSize::Small).color(Color::Error))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("refactor-cancel-btn", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                            )
                            .child(
                                Button::new(
                                    "refactor-apply-btn",
                                    if self.is_applying {
                                        "Applying..."
                                    } else {
                                        "Apply Refactoring"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .disabled(selected_count == 0 || self.is_applying)
                                .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                            ),
                    ),
            )
    }
}
