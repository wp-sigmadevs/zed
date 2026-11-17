//! Search Everywhere (plan 34, step 4): one modal, a tab per source. Files and
//! Actions are live; the other tabs light up in 4c–4e.

use command_palette::{
    action_last_invoked, humanize_action_name, normalize_action_query, record_command_invocation,
};
use command_palette_hooks::CommandPaletteFilter;
use editor::Editor;
use file_icons::FileIcons;
use fuzzy_nucleo::{
    MaxResults, PathMatchOptions, StringMatch, StringMatchCandidate, StringMatchOptions,
};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity, Window, actions,
};
use open_path_prompt::file_finder_settings::FileFinderSettings;
use picker::{Picker, PickerDelegate};
use project::{Candidates, PathMatchCandidateSet, Project, ProjectPath, WorktreeId};
use settings::Settings as _;
use std::{
    cmp::Reverse,
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use text::Point;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::{
    ResultExt as _,
    paths::{PathStyle, PathWithPosition},
    rel_path::RelPath,
};
use workspace::{ModalView, Workspace, notifications::NotifyResultExt as _};

actions!(
    zenith_search,
    [
        /// Opens Search Everywhere.
        Toggle,
        /// Opens Search Everywhere on the Actions tab (PhpStorm's Find Action).
        FindAction
    ]
);

const RECENT_FILES_LIMIT: usize = 20;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        // Toggle opens Files until the All tab exists (step 4d).
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            SearchEverywhere::open(workspace, Tab::Files, window, cx)
        });
        workspace.register_action(|workspace, _: &FindAction, window, cx| {
            SearchEverywhere::open(workspace, Tab::Actions, window, cx)
        });
    })
    .detach();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    fn placeholder(self) -> &'static str {
        match self {
            Tab::Files => "Search files…",
            Tab::Actions => "Search actions…",
            _ => "Search…",
        }
    }

    /// Sources that exist so far; the rest render disabled.
    fn is_ready(self) -> bool {
        matches!(self, Tab::Files | Tab::Actions)
    }
}

pub struct SearchEverywhere {
    picker: Entity<Picker<SearchDelegate>>,
}

impl ModalView for SearchEverywhere {}
impl EventEmitter<DismissEvent> for SearchEverywhere {}

impl Focusable for SearchEverywhere {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl SearchEverywhere {
    /// Opens on `tab`; asking for the tab already shown closes the modal, asking
    /// for another switches to it (⇧⇧ then ⌘⇧A goes Files → Actions).
    fn open(workspace: &mut Workspace, tab: Tab, window: &mut Window, cx: &mut Context<Workspace>) {
        if let Some(modal) = workspace.active_modal::<Self>(cx) {
            if modal.read(cx).picker.read(cx).delegate.tab == tab {
                workspace.hide_modal(window, cx);
            } else {
                modal.update(cx, |modal, cx| modal.switch_tab(tab, window, cx));
            }
            return;
        }
        // Nothing focused (e.g. opened from the title bar): return focus to the workspace.
        let previous_focus_handle = window
            .focused(cx)
            .unwrap_or_else(|| workspace.focus_handle(cx));
        let workspace_handle = cx.entity().downgrade();
        let project = workspace.project().clone();
        // Read now: the delegate first matches while this workspace is mid-update.
        let recent_files = workspace
            .recent_navigation_history(Some(RECENT_FILES_LIMIT), cx)
            .into_iter()
            .map(|(project_path, _)| project_path)
            .collect();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(
                tab,
                workspace_handle,
                project,
                recent_files,
                previous_focus_handle,
                window,
                cx,
            )
        });
    }

    fn new(
        tab: Tab,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        recent_files: Vec<ProjectPath>,
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
        let delegate = SearchDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            project,
            recent_files,
            previous_focus_handle,
            tab,
            commands,
            matches: Vec::new(),
            selected_ix: 0,
            latest_query: String::new(),
            file_target: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        Self { picker }
    }

    fn switch_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.tab = tab;
            picker.refresh_placeholder(window, cx);
            picker.refresh(window, cx);
        });
        cx.notify();
    }
}

impl Render for SearchEverywhere {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.picker.read(cx).delegate.tab;
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
                            .toggle_state(tab == current_tab)
                            .disabled(!tab.is_ready())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.switch_tab(tab, window, cx)
                            }))
                    })),
            )
            .child(self.picker.clone())
    }
}

struct Command {
    name: SharedString,
    action: Box<dyn Action>,
}

enum SearchMatch {
    Action(StringMatch),
    File {
        project_path: ProjectPath,
        label: SharedString,
        positions: Vec<usize>,
    },
}

pub struct SearchDelegate {
    modal: WeakEntity<SearchEverywhere>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    recent_files: Vec<ProjectPath>,
    previous_focus_handle: FocusHandle,
    tab: Tab,
    commands: Vec<Command>,
    matches: Vec<SearchMatch>,
    selected_ix: usize,
    latest_query: String,
    /// Where to land from a `file:row:column` or `file:start-end` query.
    file_target: Option<FileTarget>,
    cancel_flag: Arc<AtomicBool>,
}

impl SearchDelegate {
    fn match_actions(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let last_invoked = action_last_invoked(cx);
        let candidates = self
            .commands
            .iter()
            .enumerate()
            .map(|(ix, command)| StringMatchCandidate::from_shared(ix, command.name.clone()))
            .collect::<Vec<_>>();
        let last_invoked = self
            .commands
            .iter()
            .map(|command| last_invoked.get(&command.name).copied())
            .collect::<Vec<_>>();
        let normalized_query = normalize_action_query(&query);
        let is_empty_query = query.is_empty();
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
                    Reverse(
                        last_invoked
                            .get(string_match.candidate_id)
                            .copied()
                            .flatten(),
                    ),
                    is_empty_query.then(|| string_match.string.clone()),
                )
            });
            matches
        });
        cx.spawn_in(window, async move |picker, cx| {
            let matches = matching.await;
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches =
                        matches.into_iter().map(SearchMatch::Action).collect();
                    picker.delegate.selected_ix = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    /// Project files by fuzzy path, as the file finder; recently opened files
    /// when nothing is typed.
    fn match_files(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let project = self.project.read(cx);
        let path_style = project.path_style(cx);
        let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
        let include_root_name = worktrees.len() > 1;
        let include_ignored = FileFinderSettings::get_global(cx).include_ignored;
        let (query, file_target) = parse_file_query(&query);
        self.file_target = file_target;

        if query.is_empty() {
            self.matches = self
                .recent_files
                .iter()
                .filter_map(|project_path| {
                    let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
                    let worktree = worktree.read(cx);
                    let prefix: &RelPath = if include_root_name || worktree.is_single_file() {
                        worktree.root_name()
                    } else {
                        RelPath::empty()
                    };
                    Some(SearchMatch::File {
                        label: file_label(prefix, &project_path.path, path_style).into(),
                        project_path: project_path.clone(),
                        positions: Vec::new(),
                    })
                })
                .collect();
            self.selected_ix = 0;
            return Task::ready(());
        }

        let candidate_sets = worktrees
            .iter()
            .map(|worktree| {
                let worktree = worktree.read(cx);
                PathMatchCandidateSet {
                    snapshot: worktree.snapshot(),
                    include_ignored: include_ignored.unwrap_or_else(|| {
                        worktree.root_entry().is_some_and(|entry| entry.is_ignored)
                    }),
                    include_root_name,
                    candidates: Candidates::Files,
                }
            })
            .collect::<Vec<_>>();
        self.cancel_flag.store(true, Ordering::Release);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = self.cancel_flag.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy_nucleo::match_path_sets(
                candidate_sets.as_slice(),
                &query,
                &None,
                PathMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    cancel_flag: Some(&cancel_flag),
                    ..Default::default()
                },
                cx.background_executor().clone(),
            )
            .await;
            if cancel_flag.load(Ordering::Acquire) {
                return;
            }
            picker
                .update(cx, |picker, cx| {
                    let project = picker.delegate.project.read(cx);
                    let matches = matches
                        .into_iter()
                        .map(|path_match| {
                            let worktree_id = WorktreeId::from_usize(path_match.worktree_id);
                            // Single-file worktrees: the matcher reports the root name
                            // as the path, but the file's project path is empty.
                            let path = if project
                                .worktree_for_id(worktree_id, cx)
                                .is_some_and(|worktree| worktree.read(cx).is_single_file())
                            {
                                RelPath::empty_arc()
                            } else {
                                path_match.path.clone()
                            };
                            SearchMatch::File {
                                label: file_label(
                                    &path_match.path_prefix,
                                    &path_match.path,
                                    path_style,
                                )
                                .into(),
                                project_path: ProjectPath { worktree_id, path },
                                positions: path_match.positions,
                            }
                        })
                        .collect();
                    picker.delegate.matches = matches;
                    picker.delegate.selected_ix = 0;
                    cx.notify();
                })
                .log_err();
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileTarget {
    /// `file:row` or `file:row:column`, 1-based as typed.
    Point { row: u32, column: Option<u32> },
    /// `file:start-end`: whole lines, 1-based and inclusive.
    Lines { start: u32, end: u32 },
}

impl FileTarget {
    fn range(self, buffer: &text::BufferSnapshot) -> Range<Point> {
        match self {
            FileTarget::Point { row, column } => {
                let point = buffer.point_from_external_input(
                    row.saturating_sub(1),
                    column.unwrap_or(1).saturating_sub(1),
                );
                point..point
            }
            FileTarget::Lines { start, end } => {
                let max_point = buffer.max_point();
                let line_start = |row: u32| {
                    if row > max_point.row {
                        max_point
                    } else {
                        Point::new(row, 0)
                    }
                };
                line_start(start - 1)..line_start(end)
            }
        }
    }
}

/// Splits a Files query into the path to match and where to land, as the file
/// finder reads `path:row:column` and `path:start-end`.
fn parse_file_query(query: &str) -> (String, Option<FileTarget>) {
    let query = query.trim().trim_end_matches(':');
    if let Some((path, lines)) = query.rsplit_once(':')
        && !path.is_empty()
        && let Some((start, end)) = lines.split_once('-')
        && let Ok(start) = start.parse::<u32>()
        && start > 0
    {
        let target = match end.parse::<u32>() {
            Ok(end) if end >= start => FileTarget::Lines { start, end },
            _ => FileTarget::Point {
                row: start,
                column: None,
            },
        };
        return (path.to_owned(), Some(target));
    }
    let path_position = PathWithPosition::parse_str(query);
    let target = path_position.row.map(|row| FileTarget::Point {
        row,
        column: path_position.column,
    });
    (path_position.path.to_string_lossy().into_owned(), target)
}

/// The string the path matcher scores against, so match positions line up.
fn file_label(prefix: &RelPath, path: &RelPath, path_style: PathStyle) -> String {
    if prefix.is_empty() {
        path.display(path_style).into_owned()
    } else if path.is_empty() {
        prefix.display(path_style).into_owned()
    } else {
        format!(
            "{}{}{}",
            prefix.display(path_style),
            path_style.primary_separator(),
            path.display(path_style)
        )
    }
}

impl PickerDelegate for SearchDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "search everywhere"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.tab.placeholder().into()
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
        self.latest_query = query.clone();
        match self.tab {
            Tab::Actions => self.match_actions(query, window, cx),
            Tab::Files => self.match_files(query, window, cx),
            Tab::All | Tab::Classes | Tab::Symbols | Tab::Hooks => {
                self.matches.clear();
                Task::ready(())
            }
        }
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.matches.get(self.selected_ix) {
            None => self.dismissed(window, cx),
            Some(SearchMatch::Action(string_match)) => {
                let Some(command) = self.commands.get(string_match.candidate_id) else {
                    return;
                };
                record_command_invocation(command.name.clone(), self.latest_query.clone(), cx);
                let action = command.action.boxed_clone();
                window.focus(&self.previous_focus_handle, cx);
                self.dismissed(window, cx);
                window.dispatch_action(action, cx);
            }
            Some(SearchMatch::File { project_path, .. }) => {
                let Some(workspace) = self.workspace.upgrade() else {
                    return;
                };
                let project_path = project_path.clone();
                let open = workspace.update(cx, |workspace, cx| {
                    if secondary {
                        workspace.split_path_preview(project_path, false, None, window, cx)
                    } else {
                        workspace.open_path(project_path, None, true, window, cx)
                    }
                });
                let file_target = self.file_target;
                let workspace = self.workspace.clone();
                cx.spawn_in(window, async move |_, cx| {
                    let item = open.await.notify_workspace_async_err(workspace, cx)?;
                    let file_target = file_target?;
                    let editor = item.downcast::<Editor>()?;
                    editor
                        .update_in(cx, |editor, window, cx| {
                            let buffer = editor.buffer().read(cx).as_singleton()?;
                            let range = file_target.range(&buffer.read(cx).snapshot());
                            editor.go_to_singleton_buffer_range(range, window, cx);
                            Some(())
                        })
                        .ok()
                        .flatten()
                })
                .detach();
                self.dismissed(window, cx);
            }
        }
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
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected);
        match self.matches.get(ix)? {
            SearchMatch::Action(string_match) => {
                let command = self.commands.get(string_match.candidate_id)?;
                Some(
                    item.child(
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
            SearchMatch::File {
                label, positions, ..
            } => Some(
                item.start_slot::<Icon>(
                    FileFinderSettings::get_global(cx)
                        .file_icons
                        .then(|| FileIcons::get_icon(Path::new(label.as_ref()), cx))
                        .flatten()
                        .map(|icon| Icon::from_path(icon).color(Color::Muted)),
                )
                .child(HighlightedLabel::new(label.clone(), positions.clone()).truncate()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::Editor;
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use settings::KeymapFile;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
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
        })
    }

    fn open_picker(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<SearchDelegate>> {
        workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<SearchEverywhere>(cx)
                .expect("Search Everywhere is open")
                .read(cx)
                .picker
                .clone()
        })
    }

    #[gpui::test]
    async fn test_actions_tab_runs_the_picked_action(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
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

        cx.dispatch_action(FindAction);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| assert!(picker.delegate.matches.len() > 5));

        cx.simulate_input("bcksp");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let Some(SearchMatch::Action(first)) = picker.delegate.matches.first() else {
                panic!("expected an action match");
            };
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
        cx.dispatch_action(FindAction);
        cx.run_until_parked();
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let Some(SearchMatch::Action(first)) = picker.delegate.matches.first() else {
                panic!("expected an action match");
            };
            assert_eq!(first.string, "editor: backspace");
        });
    }

    #[gpui::test]
    async fn test_files_tab_opens_the_picked_file(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "src": { "main.rs": "fn main() {}", "lib.rs": "" },
                    "README.md": "",
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        cx.dispatch_action(Toggle);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.tab, Tab::Files));

        cx.simulate_input("mainrs");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let Some(SearchMatch::File {
                label, positions, ..
            }) = picker.delegate.matches.first()
            else {
                panic!("expected a file match");
            };
            assert_eq!(label.as_ref(), path!("src/main.rs"));
            assert!(!positions.is_empty());
        });

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<SearchEverywhere>(cx).is_none());
            let opened = workspace
                .active_item(cx)
                .and_then(|item| item.project_path(cx))
                .expect("a file is open");
            assert_eq!(opened.path.as_unix_str(), "src/main.rs");
        });

        // With nothing typed, Files lists what was opened recently.
        cx.dispatch_action(Toggle);
        cx.run_until_parked();
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let labels = picker
                .delegate
                .matches
                .iter()
                .filter_map(|search_match| match search_match {
                    SearchMatch::File { label, .. } => Some(label.to_string()),
                    SearchMatch::Action(_) => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(labels, [path!("src/main.rs")]);
        });
    }

    #[gpui::test]
    async fn test_shortcuts_switch_tabs_then_close(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let editor = cx.new_window_entity(|window, cx| Editor::single_line(window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.dispatch_action(Toggle);
        let picker = open_picker(&workspace, cx);
        cx.dispatch_action(FindAction);
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.tab, Tab::Actions);
            assert!(matches!(
                picker.delegate.matches.first(),
                Some(SearchMatch::Action(_))
            ));
        });

        cx.dispatch_action(FindAction);
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<SearchEverywhere>(cx).is_none());
        });
    }

    #[gpui::test]
    async fn test_files_tab_jumps_to_line_and_splits(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({ "main.rs": "fn main() {\n    let answer = 42;\n}\n" }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        cx.dispatch_action(Toggle);
        cx.simulate_input("main.rs:2:9");
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        let editor = workspace.update(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("main.rs opens in an editor")
        });
        editor.update(cx, |editor, cx| {
            let caret = editor
                .selections
                .all_adjusted(&editor.display_snapshot(cx))
                .into_iter()
                .next()
                .expect("one caret");
            assert_eq!((caret.start.row, caret.start.column), (1, 8));
        });

        cx.dispatch_action(Toggle);
        cx.simulate_input("main.rs:1-2");
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            let selection = editor
                .selections
                .all_adjusted(&editor.display_snapshot(cx))
                .into_iter()
                .next()
                .expect("one selection");
            assert_eq!(selection.start, Point::new(0, 0));
            assert_eq!(selection.end, Point::new(2, 0));
        });

        cx.dispatch_action(Toggle);
        cx.simulate_input("main");
        cx.run_until_parked();
        cx.dispatch_action(menu::SecondaryConfirm);
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| assert_eq!(workspace.panes().len(), 2));
    }

    #[test]
    fn test_parse_file_query() {
        assert_eq!(parse_file_query("main"), ("main".into(), None));
        assert_eq!(
            parse_file_query(" main.rs:2:9 "),
            (
                "main.rs".into(),
                Some(FileTarget::Point {
                    row: 2,
                    column: Some(9)
                })
            )
        );
        assert_eq!(
            parse_file_query("main.rs:2-3"),
            (
                "main.rs".into(),
                Some(FileTarget::Lines { start: 2, end: 3 })
            )
        );
        assert_eq!(
            parse_file_query("main.rs:3-"),
            (
                "main.rs".into(),
                Some(FileTarget::Point {
                    row: 3,
                    column: None
                })
            )
        );
    }

    #[gpui::test]
    async fn test_opens_with_nothing_focused(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            window.blur(cx);
            SearchEverywhere::open(workspace, Tab::Files, window, cx);
            assert!(workspace.active_modal::<SearchEverywhere>(cx).is_some());
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
            assert!(bound_to(&Toggle, &["shift", "shift"]));
            assert!(bound_to(&FindAction, &["a"]));
        });
    }
}
