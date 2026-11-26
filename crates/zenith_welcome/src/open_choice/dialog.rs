//! The "Open Project" dialog: This Window (default) / New Window / Attach / Cancel, and a
//! "Don't ask again" box that is saved only together with a real choice.

use std::{path::PathBuf, sync::Arc};

use fs::Fs;
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Role, WeakEntity, Window,
};
use ui::{Checkbox, Tooltip, prelude::*};
use util::paths::PathExt as _;
use workspace::{ModalView, Workspace};

use super::{Choice, FocusNextControl, FocusPreviousControl, apply};

pub struct OpenProjectDialog {
    paths: Vec<PathBuf>,
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    remember: bool,
    pub(super) focus_handle: FocusHandle,
    /// "This Window", the default button: where focus lands on open.
    pub(super) default_focus: FocusHandle,
}

impl OpenProjectDialog {
    pub fn new(
        paths: Vec<PathBuf>,
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_focus = cx.focus_handle();
        default_focus.focus(window, cx);
        // The layer that shows this focuses the root; focus still starts on the default button.
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |this, window, cx| {
            this.default_focus.focus(window, cx)
        })
        .detach();
        Self {
            paths,
            workspace,
            fs,
            remember: false,
            focus_handle,
            default_focus,
        }
    }

    fn choose(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        if self.remember {
            let window_setting = choice.content();
            settings::update_settings_file(self.fs.clone(), cx, move |content, _| {
                content
                    .zenith
                    .get_or_insert_default()
                    .open_project
                    .get_or_insert_default()
                    .window = Some(window_setting);
            });
        }
        let paths = std::mem::take(&mut self.paths);
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window
            .spawn(cx, async move |cx| {
                apply(choice, paths, workspace, cx).await
            })
            .detach();
    }

    /// Tab and Shift-Tab stay inside the dialog: past the last control they wrap.
    fn move_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        if !self.focus_handle.contains_focused(window, cx) {
            // Focusing the root would bounce to the first button (`on_focus`): go there directly.
            self.default_focus.focus(window, cx);
        }
    }

    fn body(&self) -> String {
        let names = self
            .paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let places = self
            .paths
            .iter()
            .map(|path| path.compact().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        format!("Open “{names}” ({places}) in:")
    }
}

impl ModalView for OpenProjectDialog {}

impl EventEmitter<DismissEvent> for OpenProjectDialog {}

impl Focusable for OpenProjectDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OpenProjectDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let choice_button = |id: &'static str,
                             label: &'static str,
                             tooltip: &'static str,
                             tab: isize,
                             choice: Choice,
                             cx: &mut Context<Self>| {
            div().debug_selector(|| id.into()).child(
                Button::new(id, label)
                    .tab_index(tab)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose(choice, window, cx)),
                    ),
            )
        };
        v_flex()
            .id("zenith-open-project")
            .key_context("ZenithOpenProject")
            .role(Role::Dialog)
            .aria_label("Open Project")
            .tab_group()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            // The base keymap's Tab is `menu::SelectNext`: `assets/keymaps/zenith.json` takes it
            // over here, so Tab moves focus.
            .on_action(cx.listener(|this, _: &FocusNextControl, window, cx| {
                this.move_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPreviousControl, window, cx| {
                this.move_focus(false, window, cx)
            }))
            .elevation_3(cx)
            .w(rems(32.))
            .p_3()
            .gap_3()
            .child(Headline::new("Open Project").size(HeadlineSize::Small))
            .child(Label::new(self.body()))
            .child(
                h_flex()
                    .id("zenith-open-remember")
                    .debug_selector(|| "zenith-open-remember".into())
                    .role(Role::CheckBox)
                    .aria_label("Don't ask again")
                    .aria_toggled(if self.remember {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .tab_index(5isize)
                    .gap_1()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.remember = !this.remember;
                        cx.notify();
                    }))
                    .child(
                        Checkbox::new("zenith-open-remember-box", self.remember.into())
                            .label("Don't ask again"),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        div().debug_selector(|| "zenith-open-this".into()).child(
                            Button::new("zenith-open-this", "This Window")
                                .style(ButtonStyle::Filled)
                                .track_focus(&self.default_focus)
                                .tab_index(1isize)
                                .tooltip(Tooltip::text(
                                    "Add the folder and switch to it in this window",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.choose(Choice::ThisWindow, window, cx)
                                })),
                        ),
                    )
                    .child(choice_button(
                        "zenith-open-new",
                        "New Window",
                        "Open the folder in a new window",
                        2,
                        Choice::NewWindow,
                        cx,
                    ))
                    .child(choice_button(
                        "zenith-open-attach",
                        "Attach",
                        "Add the folder to this window's project",
                        3,
                        Choice::Attach,
                        cx,
                    ))
                    .child(
                        div().debug_selector(|| "zenith-open-cancel".into()).child(
                            Button::new("zenith-open-cancel", "Cancel")
                                .tab_index(4isize)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                        ),
                    ),
            )
    }
}
