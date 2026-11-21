//! A small list modal: pick one entry and a callback receives its index.

use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

type OnChoose = Box<dyn FnOnce(usize, &mut Window, &mut App)>;

pub struct ChoicePicker {
    picker: Entity<Picker<ChoiceDelegate>>,
}

impl ChoicePicker {
    pub fn show(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        placeholder: &'static str,
        items: Vec<String>,
        on_choose: impl FnOnce(usize, &mut Window, &mut App) + 'static,
    ) {
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = ChoiceDelegate {
                modal: cx.entity().downgrade(),
                placeholder,
                items,
                selected: 0,
                on_choose: Some(Box::new(on_choose)),
            };
            let picker = cx.new(|cx| Picker::nonsearchable_uniform_list(delegate, window, cx));
            ChoicePicker { picker }
        });
    }
}

impl Render for ChoicePicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(24.)).child(self.picker.clone())
    }
}

impl Focusable for ChoicePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ChoicePicker {}
impl ModalView for ChoicePicker {}

struct ChoiceDelegate {
    modal: WeakEntity<ChoicePicker>,
    placeholder: &'static str,
    items: Vec<String>,
    selected: usize,
    on_choose: Option<OnChoose>,
}

impl PickerDelegate for ChoiceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "zenith choice picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn match_count(&self) -> usize {
        self.items.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        _: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let selected = self.selected;
        if let Some(on_choose) = self.on_choose.take() {
            on_choose(selected, window, cx);
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let label = self.items.get(ix)?.clone();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(label)),
        )
    }
}
