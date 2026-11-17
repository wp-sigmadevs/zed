//! Search Everywhere (plan 34, step 4): one modal, a tab per source. Step 4a ships
//! the tab strip and the Actions source; the other tabs light up in 4b–4e.

use command_palette::{
    command_usage, humanize_action_name, normalize_action_query, record_command_invocation,
};
use command_palette_hooks::CommandPaletteFilter;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, StringMatchOptions};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use std::{cmp::Reverse, sync::Arc};
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

actions!(
    zenith_search,
    [
        /// Opens Search Everywhere.
        Toggle
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(SearchEverywhere::toggle);
    })
    .detach();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    All,
    Classes,
    Files,
    Symbols,
    Hooks,
    Actions,
}

impl Tab {
    const ORDER: [Tab; 6] = [
        Tab::All,
        Tab::Classes,
        Tab::Files,
        Tab::Symbols,
        Tab::Hooks,
        Tab::Actions,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Classes => "Classes",
            Tab::Files => "Files",
            Tab::Symbols => "Symbols",
            Tab::Hooks => "Hooks",
            Tab::Actions => "Actions",
        }
    }

    /// Sources that exist so far; the rest render disabled.
    fn is_ready(self) -> bool {
        self == Tab::Actions
    }
}

pub struct SearchEverywhere {
    picker: Entity<Picker<ActionsDelegate>>,
}

impl ModalView for SearchEverywhere {}
impl EventEmitter<DismissEvent> for SearchEverywhere {}

impl Focusable for SearchEverywhere {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl SearchEverywhere {
    fn toggle(
        workspace: &mut Workspace,
        _: &Toggle,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace.active_modal::<Self>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return;
        }
        let Some(previous_focus_handle) = window.focused(cx) else {
            return;
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(previous_focus_handle, window, cx)
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);
        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter(|action| !filter.is_some_and(|filter| filter.is_hidden(&**action)))
            .map(|action| Command {
                name: humanize_action_name(action.name()).into(),
                action,
            })
            .collect();
        let delegate = ActionsDelegate {
            modal: cx.entity().downgrade(),
            previous_focus_handle,
            commands,
            matches: Vec::new(),
            selected_ix: 0,
            latest_query: String::new(),
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        Self { picker }
    }
}

impl Render for SearchEverywhere {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SearchEverywhere")
            .w(rems(38.))
            .elevation_3(cx)
            .overflow_hidden()
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .children(Tab::ORDER.into_iter().map(|tab| {
                        Button::new(tab.label(), tab.label())
                            .label_size(LabelSize::Small)
                            .toggle_state(tab == Tab::Actions)
                            .disabled(!tab.is_ready())
                    })),
            )
            .child(self.picker.clone())
    }
}

struct Command {
    name: SharedString,
    action: Box<dyn Action>,
}

pub struct ActionsDelegate {
    modal: WeakEntity<SearchEverywhere>,
    previous_focus_handle: FocusHandle,
    commands: Vec<Command>,
    matches: Vec<StringMatch>,
    selected_ix: usize,
    latest_query: String,
}

impl PickerDelegate for ActionsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "search everywhere"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search actions…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_ix = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let usage = command_usage(cx);
        let candidates = self
            .commands
            .iter()
            .enumerate()
            .map(|(ix, command)| StringMatchCandidate::from_shared(ix, command.name.clone()))
            .collect::<Vec<_>>();
        let usage = self
            .commands
            .iter()
            .map(|command| usage.get(&command.name).copied())
            .collect::<Vec<_>>();
        let normalized_query = normalize_action_query(&query);
        let is_empty_query = query.is_empty();
        self.latest_query = query;
        let executor = cx.background_executor().clone();
        let matching = cx.background_spawn(async move {
            let mut matches = fuzzy_nucleo::match_strings_async(
                &candidates,
                &normalized_query,
                StringMatchOptions::default(),
                executor,
            )
            .await;
            // Recently used first, as in the command palette; the rest keep fuzzy
            // order, or name order when nothing is typed.
            matches.sort_by_cached_key(|string_match| {
                (
                    Reverse(usage.get(string_match.candidate_id).copied().flatten()),
                    is_empty_query.then(|| string_match.string.clone()),
                )
            });
            matches
        });
        cx.spawn_in(window, async move |picker, cx| {
            let matches = matching.await;
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_ix = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(command_ix) = self.matches.get(self.selected_ix).map(|m| m.candidate_id) else {
            self.dismissed(window, cx);
            return;
        };
        let Some(command) = self.commands.get(command_ix) else {
            return;
        };
        record_command_invocation(command.name.clone(), self.latest_query.clone(), cx);
        let action = command.action.boxed_clone();
        window.focus(&self.previous_focus_handle, cx);
        self.dismissed(window, cx);
        window.dispatch_action(action, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(ix)?;
        let command = self.commands.get(string_match.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .child(
                            HighlightedLabel::new(
                                command.name.clone(),
                                string_match.positions.clone(),
                            )
                            .truncate(),
                        )
                        .child(KeyBinding::for_action_in(
                            &*command.action,
                            &self.previous_focus_handle,
                            cx,
                        )),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::Editor;
    use gpui::TestAppContext;
    use project::Project;
    use settings::KeymapFile;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    async fn test_actions_tab_runs_the_picked_action(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            workspace::init(app_state.clone(), cx);
            command_palette::init(cx);
            init(cx);
            cx.bind_keys(KeymapFile::load_panic_on_failure(
                r#"[{ "bindings": { "enter": "menu::Confirm" } }]"#,
                cx,
            ));
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.dispatch_action(Toggle);
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<SearchEverywhere>(cx)
                .expect("Search Everywhere opens")
                .read(cx)
                .picker
                .clone()
        });
        picker.read_with(cx, |picker, _| assert!(picker.delegate.matches.len() > 5));

        cx.simulate_input("bcksp");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let first = &picker.delegate.matches[0];
            assert_eq!(
                picker.delegate.commands[first.candidate_id].name,
                "editor: backspace"
            );
        });

        cx.dispatch_action(menu::Confirm);
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<SearchEverywhere>(cx).is_none());
            assert_eq!(editor.read(cx).text(cx), "ab");
        });

        // The pick is remembered and leads the list next time.
        cx.run_until_parked();
        cx.dispatch_action(Toggle);
        cx.run_until_parked();
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<SearchEverywhere>(cx)
                .expect("Search Everywhere reopens")
                .read(cx)
                .picker
                .clone()
        });
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.matches[0].string, "editor: backspace");
        });
    }

    #[gpui::test]
    fn test_keymap_opens_search_like_phpstorm(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            workspace::init(AppState::test(cx), cx);
            init(cx);
            let bindings = KeymapFile::load_asset(settings::ZENITH_KEYMAP_PATH, None, cx)
                .expect("Zenith keymap loads");
            let bound_to = |action: &dyn Action, keys: &[&str]| {
                bindings.iter().any(|binding| {
                    binding.action().partial_eq(action)
                        && binding
                            .keystrokes()
                            .iter()
                            .map(|keystroke| keystroke.inner().key.as_str())
                            .eq(keys.iter().copied())
                })
            };
            // ⇧⇧ stays on the file finder until Search Everywhere has Files (step 4b).
            assert!(bound_to(
                &workspace::ToggleFileFinder::default(),
                &["shift", "shift"]
            ));
            assert!(bound_to(&Toggle, &["a"]));
        });
    }
}
