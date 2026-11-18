//! Search Everywhere (plan 34, step 4): one modal, a tab per source. All merges
//! Classes, Files, Symbols and Actions; Hooks lights up in 4e.

use command_palette::{
    action_last_invoked, humanize_action_name, normalize_action_query, record_command_invocation,
};
use command_palette_hooks::CommandPaletteFilter;
use editor::{Bias, Editor, SelectionEffects, scroll::Autoscroll};
use file_icons::FileIcons;
use fuzzy_nucleo::{
    MaxResults, PathMatchOptions, StringMatch, StringMatchCandidate, StringMatchOptions,
};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, WeakEntity, Window, actions,
};
use language::SymbolKind;
use open_path_prompt::file_finder_settings::FileFinderSettings;
use picker::{Picker, PickerDelegate};
use project::{
    Candidates, PathMatchCandidateSet, Project, ProjectPath, Symbol, WorktreeId,
    lsp_store::SymbolLocation,
};
use settings::Settings as _;
use std::{
    cmp::Reverse,
    collections::HashSet,
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use text::Point;
use ui::{Divider, HighlightedLabel, ListItem, ListItemSpacing, TintColor, prelude::*};
use util::{
    ResultExt as _,
    paths::{PathStyle, PathWithPosition},
    rel_path::RelPath,
};
use workspace::{
    ModalView, Workspace,
    notifications::{NotifyResultExt as _, NotifyTaskExt as _},
};

actions!(
    zenith_search,
    [
        /// Opens Search Everywhere.
        Toggle,
        /// Opens Search Everywhere on the Actions tab (PhpStorm's Find Action).
        FindAction,
        /// Opens Search Everywhere on the Classes tab (PhpStorm's Go to Class).
        GoToClass,
        /// Opens Search Everywhere on the Files tab (PhpStorm's Go to File).
        GoToFile,
        /// Opens Search Everywhere on the Symbols tab (PhpStorm's Go to Symbol).
        GoToSymbol,
        /// Opens Search Everywhere on the Files tab, which lists recent files
        /// first (PhpStorm's Recent Files).
        RecentFiles,
        /// Opens the places recently visited, newest first (PhpStorm's Recent Locations).
        RecentLocations,
        /// Moves to the next Search Everywhere tab.
        NextTab,
        /// Moves to the previous Search Everywhere tab.
        PreviousTab
    ]
);

const RECENT_FILES_LIMIT: usize = 20;
const RECENT_LOCATIONS_LIMIT: usize = 50;
const SYMBOL_QUERY_DEBOUNCE: Duration = Duration::from_millis(100);
/// Results per group on the All tab, so one source can't push the others off.
const ALL_TAB_GROUP_LIMIT: usize = 8;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            SearchEverywhere::open(workspace, Tab::All, window, cx)
        });
        workspace.register_action(|workspace, _: &FindAction, window, cx| {
            SearchEverywhere::open(workspace, Tab::Actions, window, cx)
        });
        workspace.register_action(|workspace, _: &GoToClass, window, cx| {
            SearchEverywhere::open(workspace, Tab::Classes, window, cx)
        });
        workspace.register_action(|workspace, _: &GoToFile, window, cx| {
            SearchEverywhere::open(workspace, Tab::Files, window, cx)
        });
        workspace.register_action(|workspace, _: &GoToSymbol, window, cx| {
            SearchEverywhere::open(workspace, Tab::Symbols, window, cx)
        });
        workspace.register_action(|workspace, _: &RecentLocations, window, cx| {
            SearchEverywhere::open(workspace, Tab::Locations, window, cx)
        });
        workspace.register_action(|workspace, _: &RecentFiles, window, cx| {
            SearchEverywhere::open(workspace, Tab::Recent, window, cx)
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
    /// Recent Locations: opened by its own key, not part of the tab strip.
    Locations,
    /// Recent Files (⌘E): PhpStorm's popup with tool windows beside the files.
    Recent,
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
            Tab::Locations => "Recent Locations",
            Tab::Recent => "Recent Files",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Tab::Classes => "Search classes…",
            Tab::Files => "Search files…",
            Tab::Symbols => "Search symbols…",
            Tab::Actions => "Search actions…",
            Tab::Locations => "Search recent locations…",
            Tab::Recent => "Search recent files…",
            Tab::All | Tab::Hooks => "Classes, files, symbols, actions",
        }
    }

    /// Sources that exist so far; the rest render disabled.
    fn is_ready(self) -> bool {
        self != Tab::Hooks
    }

    /// The next ready tab `step` places along, wrapping around.
    fn cycle(self, step: isize) -> Tab {
        let count = Self::ORDER.len() as isize;
        let mut ix = Self::ORDER.iter().position(|tab| *tab == self).unwrap_or(0) as isize;
        loop {
            ix = (ix + step).rem_euclid(count);
            let tab = Self::ORDER[ix as usize];
            if tab.is_ready() {
                return tab;
            }
        }
    }
}

pub struct SearchEverywhere {
    picker: Entity<Picker<SearchDelegate>>,
    /// Recent Files' left column: every panel, as PhpStorm lists tool windows.
    tool_windows: Vec<ToolWindow>,
}

struct ToolWindow {
    name: SharedString,
    icon: IconName,
    toggle: Box<dyn Action>,
}

fn tool_windows(workspace: &Workspace, window: &Window, cx: &App) -> Vec<ToolWindow> {
    [workspace.left_dock(), workspace.bottom_dock(), workspace.right_dock()]
        .into_iter()
        .flat_map(|dock| dock.read(cx).zenith_panels().cloned().collect::<Vec<_>>())
        .filter(|panel| panel.enabled(cx))
        .filter_map(|panel| {
            Some(ToolWindow {
                name: panel
                    .icon_tooltip(window, cx)?
                    .trim_end_matches(" Panel")
                    .to_string()
                    .into(),
                icon: panel.icon(window, cx)?,
                toggle: panel.toggle_action(window, cx),
            })
        })
        .collect()
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
    /// for another switches to it (⇧⇧ then ⌘⇧A goes All → Actions).
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
        let recent_locations = recent_locations(workspace, cx);
        let tool_windows = tool_windows(workspace, window, cx);
        workspace.toggle_modal(window, cx, move |window, cx| {
            let mut this = Self::new(
                tab,
                workspace_handle,
                project,
                recent_files,
                recent_locations,
                previous_focus_handle,
                window,
                cx,
            );
            this.tool_windows = tool_windows;
            this
        });
    }

    fn new(
        tab: Tab,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        recent_files: Vec<ProjectPath>,
        recent_locations: Vec<Location>,
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
            recent_locations,
            previous_focus_handle,
            tab,
            commands,
            symbols: Vec::new(),
            matches: Vec::new(),
            selected_ix: 0,
            latest_query: String::new(),
            file_target: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            edited_only: false,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());
        Self {
            picker,
            tool_windows: Vec::new(),
        }
    }

    fn switch_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.tab = tab;
            picker.refresh_placeholder(window, cx);
            picker.refresh(window, cx);
        });
        cx.notify();
    }

    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.picker.read(cx).delegate.tab.cycle(step);
        self.switch_tab(tab, window, cx);
    }
}

impl Render for SearchEverywhere {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.picker.read(cx).delegate.tab;
        let border = cx.theme().colors().border_variant;
        let header = match current_tab {
            Tab::Recent | Tab::Locations => self.render_popup_header(current_tab, cx).into_any_element(),
            _ => h_flex()
                .p_1()
                .gap_1()
                .border_b_1()
                .border_color(border)
                .children(Tab::ORDER.into_iter().map(|tab| {
                    Button::new(tab.label(), tab.label())
                        .label_size(LabelSize::Small)
                        .toggle_state(tab == current_tab)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .disabled(!tab.is_ready())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.switch_tab(tab, window, cx)
                        }))
                }))
                .into_any_element(),
        };
        let body = if current_tab == Tab::Recent {
            h_flex()
                .items_start()
                .child(self.render_tool_windows(cx))
                .child(div().flex_1().min_w_0().child(self.picker.clone()))
                .into_any_element()
        } else {
            self.picker.clone().into_any_element()
        };
        v_flex()
            .key_context("SearchEverywhere")
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)),
            )
            .w(if current_tab == Tab::Recent {
                rems(48.)
            } else {
                rems(38.)
            })
            .elevation_3(cx)
            .overflow_hidden()
            .child(header)
            .child(body)
    }
}

impl SearchEverywhere {
    /// PhpStorm's popup title row: "Recent Files  [ ] Show edited only".
    fn render_popup_header(&self, tab: Tab, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let edited_only = self.picker.read(cx).delegate.edited_only;
        h_flex()
            .debug_selector(|| "zenith-popup-header".into())
            .h_9()
            .px_3()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(tab.label())
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .when(tab == Tab::Recent, |this| {
                this.child(
                    ui::Checkbox::new("zenith-edited-only", edited_only.into())
                        .label("Show edited only")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.picker.update(cx, |picker, cx| {
                                picker.delegate.edited_only = !picker.delegate.edited_only;
                                picker.refresh(window, cx);
                            });
                            cx.notify();
                        })),
                )
            })
    }

    /// PhpStorm lists the tool windows beside the recent files.
    fn render_tool_windows(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .debug_selector(|| "zenith-tool-windows".into())
            .w(rems(12.))
            .flex_none()
            .py_1()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .children(self.tool_windows.iter().enumerate().map(|(ix, tool_window)| {
                let toggle = tool_window.toggle.boxed_clone();
                ListItem::new(("zenith-tool-window", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(tool_window.icon)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(tool_window.name.clone()).size(LabelSize::Small))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        cx.emit(DismissEvent);
                        window.dispatch_action(toggle.boxed_clone(), cx);
                    }))
            }))
    }
}

struct Command {
    name: SharedString,
    action: Box<dyn Action>,
}

#[derive(Clone)]
enum SearchMatch {
    Action(StringMatch),
    /// Index into `SearchDelegate::symbols`; positions are in the label text.
    Symbol(StringMatch),
    File {
        project_path: ProjectPath,
        label: SharedString,
        positions: Vec<usize>,
    },
    /// Index into `SearchDelegate::recent_locations`.
    Location {
        ix: usize,
        label: SharedString,
        positions: Vec<usize>,
    },
}

/// A place visited: file and 0-based row, newest first in `recent_locations`.
#[derive(Clone)]
struct Location {
    project_path: ProjectPath,
    row: u32,
    label: SharedString,
}

/// Every place the panes' navigation histories recorded, newest first, one per
/// file and row.
fn recent_locations(workspace: &Workspace, cx: &App) -> Vec<Location> {
    let path_style = workspace.project().read(cx).path_style(cx);
    let mut visited = Vec::new();
    for pane in workspace.panes() {
        pane.read(cx)
            .nav_history()
            .for_each_entry(cx, &mut |entry, (project_path, _)| {
                if let Some(row) = entry.row {
                    visited.push((entry.timestamp, project_path, row));
                }
            });
    }
    visited.sort_by_key(|(timestamp, ..)| Reverse(*timestamp));
    // Where you are now is the newest place; the history only holds places left.
    if let Some(editor) = workspace.active_item_as::<Editor>(cx)
        && let Some(project_path) = workspace::item::ItemHandle::project_path(&editor, cx)
    {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let row = snapshot
            .summary_for_anchor::<Point>(&editor.selections.newest_anchor().head())
            .row;
        visited.insert(0, (usize::MAX, project_path, row));
    }
    let mut seen = HashSet::<(ProjectPath, u32)>::default();
    visited
        .into_iter()
        .filter(|(_, project_path, row)| seen.insert((project_path.clone(), *row)))
        .take(RECENT_LOCATIONS_LIMIT)
        .map(|(_, project_path, row)| Location {
            label: format!(
                "{}:{}",
                file_label(RelPath::empty(), &project_path.path, path_style),
                row + 1
            )
            .into(),
            project_path,
            row,
        })
        .collect()
}

pub struct SearchDelegate {
    modal: WeakEntity<SearchEverywhere>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    recent_files: Vec<ProjectPath>,
    recent_locations: Vec<Location>,
    previous_focus_handle: FocusHandle,
    tab: Tab,
    commands: Vec<Command>,
    symbols: Vec<Symbol>,
    matches: Vec<SearchMatch>,
    selected_ix: usize,
    latest_query: String,
    /// Where to land from a `file:row:column` or `file:start-end` query.
    file_target: Option<FileTarget>,
    cancel_flag: Arc<AtomicBool>,
    /// Recent Files' "Show edited only".
    edited_only: bool,
}

impl SearchDelegate {
    fn search_actions(
        &self,
        query: &str,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<Vec<SearchMatch>> {
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
        let normalized_query = normalize_action_query(query);
        let is_empty_query = query.is_empty();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
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
            matches.into_iter().map(SearchMatch::Action).collect()
        })
    }

    /// Language-server workspace symbols, as project symbols: the server filters
    /// by `query`, then a fuzzy pass ranks; symbols in the project lead.
    fn search_symbols(
        &self,
        query: String,
        classes_only: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<(Vec<Symbol>, Vec<StringMatch>)> {
        if query.is_empty() {
            return Task::ready(Default::default());
        }
        // rust-analyzer takes `path::Name` queries; rank on the last segment only.
        let filter_query = query
            .rsplit_once("::")
            .map_or(&*query, |(_, suffix)| suffix)
            .to_owned();
        cx.spawn_in(window, async move |picker, cx| {
            // Each keystroke replaces this task, so the server only sees the query
            // once typing pauses.
            cx.background_executor().timer(SYMBOL_QUERY_DEBOUNCE).await;
            let Ok(symbols) = picker.update(cx, |picker, cx| {
                picker
                    .delegate
                    .project
                    .update(cx, |project, cx| project.symbols(&query, cx))
            }) else {
                return Default::default();
            };
            let Some(symbols) = symbols.await.log_err() else {
                return Default::default();
            };
            let symbols = symbols
                .into_iter()
                .filter(|symbol| !classes_only || is_class_like(symbol))
                .collect::<Vec<_>>();
            let Ok(in_project) = picker.update(cx, |picker, cx| {
                let project = picker.delegate.project.read(cx);
                symbols
                    .iter()
                    .map(|symbol| match &symbol.path {
                        SymbolLocation::InProject(path) => project
                            .entry_for_path(path, cx)
                            .is_some_and(|entry| !entry.is_ignored),
                        SymbolLocation::OutsideProject { .. } => false,
                    })
                    .collect::<Vec<_>>()
            }) else {
                return Default::default();
            };
            let candidates = symbols
                .iter()
                .enumerate()
                .map(|(ix, symbol)| StringMatchCandidate::new(ix, symbol.label.filter_text()))
                .collect::<Vec<_>>();
            let mut matches = fuzzy_nucleo::match_strings_async(
                &candidates,
                &filter_query,
                StringMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    ..Default::default()
                },
                cx.background_executor().clone(),
            )
            .await;
            // Stable: keeps fuzzy order within each group.
            matches.sort_by_key(|string_match| !in_project[string_match.candidate_id]);
            for string_match in &mut matches {
                let filter_start = symbols[string_match.candidate_id].label.filter_range.start;
                for position in &mut string_match.positions {
                    *position += filter_start;
                }
            }
            (symbols, matches)
        })
    }

    /// Recent locations by fuzzy label; all of them, newest first, when nothing is typed.
    fn search_locations(
        &self,
        query: String,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<Vec<SearchMatch>> {
        let locations = self
            .recent_locations
            .iter()
            .enumerate()
            .map(|(ix, location)| (ix, location.label.clone()))
            .collect::<Vec<_>>();
        if query.is_empty() {
            return Task::ready(
                locations
                    .into_iter()
                    .map(|(ix, label)| SearchMatch::Location {
                        ix,
                        label,
                        positions: Vec::new(),
                    })
                    .collect(),
            );
        }
        let candidates = locations
            .iter()
            .map(|(ix, label)| StringMatchCandidate::new(*ix, label))
            .collect::<Vec<_>>();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            fuzzy_nucleo::match_strings_async(
                &candidates,
                &query,
                StringMatchOptions::default(),
                executor,
            )
            .await
            .into_iter()
            .map(|string_match| SearchMatch::Location {
                ix: string_match.candidate_id,
                label: string_match.string,
                positions: string_match.positions,
            })
            .collect()
        })
    }

    /// Recent Files: the recently opened files, fuzzy-filtered by the query; with
    /// "Show edited only", just those changed this session.
    fn search_recent(&self, query: String, cx: &mut Context<Picker<Self>>) -> Task<Vec<SearchMatch>> {
        let project = self.project.read(cx);
        let path_style = project.path_style(cx);
        let include_root_name = project.visible_worktrees(cx).count() > 1;
        let recent = self
            .recent_files
            .iter()
            .filter(|project_path| {
                !self.edited_only
                    || project.get_open_buffer(project_path, cx).is_some_and(|buffer| {
                        let buffer = buffer.read(cx);
                        buffer.is_dirty() || buffer.peek_undo_stack().is_some()
                    })
            })
            .filter_map(|project_path| {
                let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
                let worktree = worktree.read(cx);
                let prefix: &RelPath = if include_root_name || worktree.is_single_file() {
                    worktree.root_name()
                } else {
                    RelPath::empty()
                };
                Some((
                    project_path.clone(),
                    SharedString::from(file_label(prefix, &project_path.path, path_style)),
                ))
            })
            .collect::<Vec<_>>();
        if query.is_empty() {
            return Task::ready(
                recent
                    .into_iter()
                    .map(|(project_path, label)| SearchMatch::File {
                        project_path,
                        label,
                        positions: Vec::new(),
                    })
                    .collect(),
            );
        }
        let candidates = recent
            .iter()
            .enumerate()
            .map(|(ix, (_, label))| StringMatchCandidate::new(ix, label))
            .collect::<Vec<_>>();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            fuzzy_nucleo::match_strings_async(
                &candidates,
                &query,
                StringMatchOptions::default(),
                executor,
            )
            .await
            .into_iter()
            .map(|string_match| SearchMatch::File {
                project_path: recent[string_match.candidate_id].0.clone(),
                label: string_match.string,
                positions: string_match.positions,
            })
            .collect()
        })
    }

    /// Project files by fuzzy path, as the file finder; recently opened files
    /// when nothing is typed.
    fn search_files(
        &mut self,
        query: String,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<Vec<SearchMatch>> {
        let project = self.project.read(cx);
        let path_style = project.path_style(cx);
        let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
        let include_root_name = worktrees.len() > 1;
        let include_ignored = FileFinderSettings::get_global(cx).include_ignored;

        if query.is_empty() {
            let recent = self
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
            return Task::ready(recent);
        }

        // Single-file worktrees: the matcher reports the root name as the path,
        // but the file's project path is empty.
        let single_file_worktrees = worktrees
            .iter()
            .filter(|worktree| worktree.read(cx).is_single_file())
            .map(|worktree| worktree.read(cx).id())
            .collect::<HashSet<_>>();
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
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let matches = fuzzy_nucleo::match_path_sets(
                candidate_sets.as_slice(),
                &query,
                &None,
                PathMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    cancel_flag: Some(&cancel_flag),
                    ..Default::default()
                },
                executor,
            )
            .await;
            matches
                .into_iter()
                .map(|path_match| {
                    let worktree_id = WorktreeId::from_usize(path_match.worktree_id);
                    let path = if single_file_worktrees.contains(&worktree_id) {
                        RelPath::empty_arc()
                    } else {
                        path_match.path.clone()
                    };
                    SearchMatch::File {
                        label: file_label(&path_match.path_prefix, &path_match.path, path_style)
                            .into(),
                        project_path: ProjectPath { worktree_id, path },
                        positions: path_match.positions,
                    }
                })
                .collect()
        })
    }

    fn set_matches(
        &mut self,
        symbols: Vec<Symbol>,
        matches: Vec<SearchMatch>,
        reset_selection: bool,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.symbols = symbols;
        self.matches = matches;
        self.selected_ix = if reset_selection {
            0
        } else {
            self.selected_ix.min(self.matches.len().saturating_sub(1))
        };
        cx.notify();
    }

    /// Short kind shown on each All-tab row.
    /// Opens `project_path` (split when `secondary`) and lands on `file_target`.
    fn open_file(
        &mut self,
        project_path: ProjectPath,
        file_target: Option<FileTarget>,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            if secondary {
                workspace.split_path_preview(project_path, false, None, window, cx)
            } else {
                workspace.open_path(project_path, None, true, window, cx)
            }
        });
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

    fn kind_label(&self, search_match: &SearchMatch) -> &'static str {
        match search_match {
            SearchMatch::Action(_) => "Action",
            SearchMatch::File { .. } => "File",
            SearchMatch::Location { .. } => "Location",
            SearchMatch::Symbol(string_match) => {
                match self.symbols.get(string_match.candidate_id) {
                    Some(symbol) if is_class_like(symbol) => "Class",
                    _ => "Symbol",
                }
            }
        }
    }
}

/// The All tab: classes, files, other symbols, actions, as PhpStorm groups them.
fn merge_all_tab(
    symbols: &[Symbol],
    symbol_matches: Vec<StringMatch>,
    files: Vec<SearchMatch>,
    actions: Vec<SearchMatch>,
) -> Vec<SearchMatch> {
    let (classes, other_symbols): (Vec<_>, Vec<_>) = symbol_matches
        .into_iter()
        .partition(|string_match| is_class_like(&symbols[string_match.candidate_id]));
    classes
        .into_iter()
        .map(SearchMatch::Symbol)
        .take(ALL_TAB_GROUP_LIMIT)
        .chain(files.into_iter().take(ALL_TAB_GROUP_LIMIT))
        .chain(
            other_symbols
                .into_iter()
                .map(SearchMatch::Symbol)
                .take(ALL_TAB_GROUP_LIMIT),
        )
        .chain(actions.into_iter().take(ALL_TAB_GROUP_LIMIT))
        .collect()
}

/// What the Classes tab keeps. LSP has no trait kind: PHPantom reports PHP
/// traits as Class, Intelephense as Module, so Module counts only in PHP files
/// (elsewhere it means Rust modules, TypeScript namespaces and the like).
fn is_class_like(symbol: &Symbol) -> bool {
    match symbol.kind {
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum | SymbolKind::Struct => true,
        SymbolKind::Module => {
            let extension = match &symbol.path {
                SymbolLocation::InProject(project_path) => project_path.path.extension(),
                SymbolLocation::OutsideProject { abs_path, .. } => abs_path
                    .extension()
                    .and_then(|extension| extension.to_str()),
            };
            extension == Some("php")
        }
        _ => false,
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
        let tab = self.tab;
        if tab == Tab::Hooks {
            self.set_matches(Vec::new(), Vec::new(), true, cx);
            return Task::ready(());
        }
        if tab == Tab::Recent {
            let recent = self.search_recent(query, cx);
            return cx.spawn_in(window, async move |picker, cx| {
                let matches = recent.await;
                picker
                    .update(cx, |picker, cx| {
                        picker.delegate.set_matches(Vec::new(), matches, true, cx)
                    })
                    .log_err();
            });
        }
        if tab == Tab::Locations {
            let locations = self.search_locations(query, cx);
            return cx.spawn_in(window, async move |picker, cx| {
                let matches = locations.await;
                picker
                    .update(cx, |picker, cx| {
                        picker.delegate.set_matches(Vec::new(), matches, true, cx)
                    })
                    .log_err();
            });
        }
        let is_empty_query = query.is_empty();
        let (file_query, file_target) = parse_file_query(&query);
        self.file_target = file_target;
        let files = if matches!(tab, Tab::All | Tab::Files) {
            self.search_files(file_query, cx)
        } else {
            Task::ready(Vec::new())
        };
        // Every action matches an empty query; on All that would bury the recent files.
        let actions = if tab == Tab::Actions || (tab == Tab::All && !is_empty_query) {
            self.search_actions(&query, cx)
        } else {
            Task::ready(Vec::new())
        };
        let symbols = if matches!(tab, Tab::All | Tab::Classes | Tab::Symbols) {
            self.search_symbols(query, tab == Tab::Classes, window, cx)
        } else {
            Task::ready(Default::default())
        };
        cx.spawn_in(window, async move |picker, cx| {
            let files = files.await;
            let actions = actions.await;
            if tab == Tab::All && !is_empty_query {
                // Files and actions are local; show them before the language server answers.
                let matches = merge_all_tab(&[], Vec::new(), files.clone(), actions.clone());
                picker
                    .update(cx, |picker, cx| {
                        picker.delegate.set_matches(Vec::new(), matches, true, cx)
                    })
                    .log_err();
            }
            let (symbols, symbol_matches) = symbols.await;
            let matches = if tab == Tab::All {
                merge_all_tab(&symbols, symbol_matches, files, actions)
            } else {
                files
                    .into_iter()
                    .chain(symbol_matches.into_iter().map(SearchMatch::Symbol))
                    .chain(actions)
                    .collect()
            };
            let reset_selection = tab != Tab::All || is_empty_query;
            picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .set_matches(symbols, matches, reset_selection, cx)
                })
                .log_err();
        })
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
            Some(SearchMatch::Symbol(string_match)) => {
                let Some(symbol) = self.symbols.get(string_match.candidate_id).cloned() else {
                    return;
                };
                let buffer = self.project.update(cx, |project, cx| {
                    project.open_buffer_for_symbol(&symbol, cx)
                });
                let workspace = self.workspace.clone();
                cx.spawn_in(window, async move |_, cx| {
                    let buffer = buffer.await?;
                    workspace.update_in(cx, |workspace, window, cx| {
                        let position = buffer
                            .read(cx)
                            .clip_point_utf16(symbol.range.start, Bias::Left);
                        let pane = secondary.then(|| workspace.adjacent_pane(window, cx));
                        let editor = workspace.open_project_item::<Editor>(
                            pane, buffer, true, true, true, true, window, cx,
                        );
                        editor.update(cx, |editor, cx| {
                            let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                            let Some(buffer_snapshot) = multibuffer_snapshot.as_singleton() else {
                                return;
                            };
                            let text_anchor = buffer_snapshot.anchor_before(position);
                            let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(text_anchor)
                            else {
                                return;
                            };
                            editor.change_selections(
                                SelectionEffects::scroll(Autoscroll::center()).unfold(),
                                window,
                                cx,
                                |selections| selections.select_ranges([anchor..anchor]),
                            );
                        });
                    })
                })
                .detach_and_notify_err(self.workspace.clone(), window, cx);
                self.dismissed(window, cx);
            }
            Some(SearchMatch::File { project_path, .. }) => {
                let (project_path, file_target) = (project_path.clone(), self.file_target);
                self.open_file(project_path, file_target, secondary, window, cx);
            }
            Some(SearchMatch::Location { ix, .. }) => {
                let Some(location) = self.recent_locations.get(*ix).cloned() else {
                    return;
                };
                let target = FileTarget::Point {
                    row: location.row + 1,
                    column: None,
                };
                self.open_file(location.project_path, Some(target), secondary, window, cx);
            }
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_editor(
        &self,
        editor: &Arc<dyn picker::ErasedEditor>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::Div> {
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .child(
                    h_flex()
                        .debug_selector(|| "zenith-search-field".into())
                        .h_10()
                        .px_3()
                        .gap_2()
                        .flex_none()
                        .child(
                            Icon::new(IconName::MagnifyingGlass)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1().child(editor.render(window, cx)))
                        .child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(colors.border_variant)
                                .child(
                                    Label::new("esc")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .child(Divider::horizontal()),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let search_match = self.matches.get(ix)?;
        // PhpStorm labels each group once, on its first row.
        let kind = self.kind_label(search_match);
        let group_starts = starts_group(
            ix.checked_sub(1)
                .and_then(|previous| self.matches.get(previous))
                .map(|previous| self.kind_label(previous)),
            kind,
        );
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .end_slot::<Label>((self.tab == Tab::All && group_starts).then(|| {
                Label::new(kind.to_uppercase())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }));
        let file_icon = |path: &str, cx: &App| {
            FileFinderSettings::get_global(cx)
                .file_icons
                .then(|| FileIcons::get_icon(Path::new(path), cx))
                .flatten()
                .map(|icon| Icon::from_path(icon).color(Color::Muted).into_any_element())
        };
        let (icon, name, positions, detail) = match search_match {
            SearchMatch::Action(string_match) => {
                let command = self.commands.get(string_match.candidate_id)?;
                let keys = ui::text_for_action(&*command.action, window, cx);
                (
                    Some(
                        Icon::new(IconName::Command)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .into_any_element(),
                    ),
                    command.name.clone(),
                    string_match.positions.clone(),
                    keys,
                )
            }
            SearchMatch::Symbol(string_match) => {
                let symbol = self.symbols.get(string_match.candidate_id)?;
                let project = self.project.read(cx);
                let location = match &symbol.path {
                    SymbolLocation::InProject(project_path) => {
                        let worktree = project.worktree_for_id(project_path.worktree_id, cx);
                        let prefix = match worktree {
                            Some(worktree) if project.visible_worktrees(cx).count() > 1 => {
                                worktree.read(cx).root_name()
                            }
                            _ => RelPath::empty(),
                        };
                        file_label(prefix, &project_path.path, project.path_style(cx))
                    }
                    SymbolLocation::OutsideProject { abs_path, .. } => {
                        abs_path.to_string_lossy().into_owned()
                    }
                };
                let file_name = location.rsplit(['/', '\\']).next().unwrap_or(&location);
                (
                    Some(symbol_badge(symbol.kind, cx).into_any_element()),
                    symbol.label.text.clone().into(),
                    string_match.positions.clone(),
                    Some(format!("{file_name}:{}", symbol.range.start.0.row + 1)),
                )
            }
            SearchMatch::Location {
                label, positions, ..
            }
            | SearchMatch::File {
                label, positions, ..
            } => {
                let (name, dir, name_positions) = split_path_label(label, positions);
                let file = name.rsplit_once(':').map_or(name.as_str(), |(file, _)| file);
                (
                    file_icon(file, cx),
                    name.clone().into(),
                    name_positions,
                    Some(dir.unwrap_or_else(|| "project root".to_string())),
                )
            }
        };
        Some(
            item.start_slot::<gpui::AnyElement>(icon).child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(
                        HighlightedLabel::new(name, positions)
                            .weight(gpui::FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .children(detail.map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                    })),
            ),
        )
    }
}


/// A row opens a group when the row before it is of another kind (or there is none).
fn starts_group(previous_kind: Option<&str>, kind: &str) -> bool {
    previous_kind != Some(kind)
}

/// "dir/sub/file.php" → ("file.php", Some("dir/sub"), positions inside the name).
fn split_path_label(label: &str, positions: &[usize]) -> (String, Option<String>, Vec<usize>) {
    match label.rfind(['/', '\\']) {
        Some(separator) => {
            let start = separator + 1;
            (
                label[start..].to_string(),
                Some(label[..separator].to_string()),
                positions
                    .iter()
                    .filter(|&&position| position >= start)
                    .map(|position| position - start)
                    .collect(),
            )
        }
        None => (label.to_string(), None, positions.to_vec()),
    }
}

/// PhpStorm's round kind badge: C class, I interface, E enum, T trait, m method, f function…
fn symbol_badge(kind: SymbolKind, cx: &App) -> impl IntoElement + use<> {
    let (letter, color) = symbol_badge_letter(kind);
    let color = match color {
        BadgeColor::Type => cx.theme().colors().text_accent,
        BadgeColor::Member => cx.theme().status().success,
        BadgeColor::Value => cx.theme().status().warning,
    };
    div()
        .size(rems(1.))
        .flex_none()
        .rounded_full()
        .bg(color.opacity(0.25))
        .flex()
        .items_center()
        .justify_center()
        .child(
            Label::new(letter)
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::BOLD)
                .color(Color::Custom(color)),
        )
}

#[derive(Debug, PartialEq, Eq)]
enum BadgeColor {
    Type,
    Member,
    Value,
}

fn symbol_badge_letter(kind: SymbolKind) -> (&'static str, BadgeColor) {
    match kind {
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Object => ("C", BadgeColor::Type),
        SymbolKind::Interface => ("I", BadgeColor::Type),
        SymbolKind::Enum => ("E", BadgeColor::Type),
        // PHP traits arrive as Module (Intelephense).
        SymbolKind::Module | SymbolKind::Namespace | SymbolKind::Package => ("T", BadgeColor::Type),
        SymbolKind::Method | SymbolKind::Constructor => ("m", BadgeColor::Member),
        SymbolKind::Function | SymbolKind::Operator => ("f", BadgeColor::Member),
        SymbolKind::Property | SymbolKind::Field | SymbolKind::EnumMember => {
            ("p", BadgeColor::Value)
        }
        SymbolKind::Constant => ("c", BadgeColor::Value),
        _ => ("v", BadgeColor::Value),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;
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

    fn open_on(tab: Tab, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        workspace.update_in(cx, |workspace, window, cx| {
            SearchEverywhere::open(workspace, tab, window, cx)
        });
        cx.run_until_parked();
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

        open_on(Tab::Files, &workspace, cx);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.tab, Tab::Files));
        // The board's search field: magnifier, query, `esc` hint.
        assert!(cx.debug_bounds("zenith-search-field").is_some());

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
        open_on(Tab::Files, &workspace, cx);
        cx.run_until_parked();
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let labels = picker
                .delegate
                .matches
                .iter()
                .filter_map(|search_match| match search_match {
                    SearchMatch::File { label, .. } => Some(label.to_string()),
                    SearchMatch::Action(_)
                    | SearchMatch::Symbol(_)
                    | SearchMatch::Location { .. } => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(labels, [path!("src/main.rs")]);
        });
    }

    #[gpui::test]
    async fn test_recent_files_popup(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({ "src": { "main.rs": "", "lib.rs": "" }, "README.md": "" }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        let worktree_id = workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).worktrees(cx).next().unwrap().read(cx).id()
        });
        for file in ["src/main.rs", "src/lib.rs"] {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let path = ProjectPath {
                        worktree_id,
                        path: RelPath::from_unix_str(file).unwrap().into(),
                    };
                    workspace.open_path(path, None, true, window, cx)
                })
                .await
                .unwrap();
        }
        // Edit lib.rs, the active file.
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        editor.update_in(cx, |editor, window, cx| editor.insert("x", window, cx));
        cx.run_until_parked();

        let file_labels = |picker: &Entity<Picker<SearchDelegate>>, cx: &mut VisualTestContext| {
            picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .matches
                    .iter()
                    .filter_map(|search_match| match search_match {
                        SearchMatch::File { label, .. } => Some(label.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
        };

        open_on(Tab::Recent, &workspace, cx);
        let picker = open_picker(&workspace, cx);
        assert!(cx.debug_bounds("zenith-popup-header").is_some(), "title row");
        assert!(cx.debug_bounds("zenith-tool-windows").is_some(), "tool windows column");
        // Only what was opened, newest first; README was never opened.
        assert_eq!(
            file_labels(&picker, cx),
            [path!("src/lib.rs"), path!("src/main.rs")]
        );

        cx.simulate_input("main");
        cx.run_until_parked();
        assert_eq!(file_labels(&picker, cx), [path!("src/main.rs")]);
        // Typing filters the recent files only, never the whole project.
        picker.update_in(cx, |picker, window, cx| picker.set_query("readme", window, cx));
        cx.run_until_parked();
        assert!(file_labels(&picker, cx).is_empty());

        // "Show edited only".
        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("", window, cx);
            picker.delegate.edited_only = true;
            picker.refresh(window, cx);
        });
        cx.run_until_parked();
        assert_eq!(file_labels(&picker, cx), [path!("src/lib.rs")]);
    }

    #[gpui::test]
    async fn test_recent_locations_lists_places_visited_newest_first(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let text = (0..30)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "main.rs": text }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        let editor = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    std::path::PathBuf::from(path!("/root/main.rs")),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        // Each jump records the place it leaves: row 0, then row 10; row 20 is where it ends.
        for row in [10, 20] {
            editor.update_in(cx, |editor, window, cx| {
                editor.go_to_singleton_buffer_range(
                    Point::new(row, 0)..Point::new(row, 0),
                    window,
                    cx,
                )
            });
        }

        open_on(Tab::Locations, &workspace, cx);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let labels = picker
                .delegate
                .matches
                .iter()
                .filter_map(|search_match| match search_match {
                    SearchMatch::Location { label, .. } => Some(label.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(labels, ["main.rs:21", "main.rs:11", "main.rs:1"]);
        });

        // Picking the older place jumps back to it.
        cx.dispatch_action(menu::SelectNext);
        cx.dispatch_action(menu::SelectNext);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            assert_eq!(head.row, 0);
        });
    }

    #[gpui::test]
    async fn test_recent_locations_after_only_opening_files(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({ "a.rs": "a\n", "b.rs": "b\n" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        for name in ["a.rs", "b.rs"] {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        std::path::PathBuf::from(path!("/root")).join(name),
                        workspace::OpenOptions::default(),
                        window,
                        cx,
                    )
                })
                .await
                .expect("file opens");
        }
        open_on(Tab::Locations, &workspace, cx);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            let labels = picker
                .delegate
                .matches
                .iter()
                .filter_map(|m| match m {
                    SearchMatch::Location { label, .. } => Some(label.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(labels, ["b.rs:1", "a.rs:1"]);
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
        picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.tab, Tab::All));
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

        open_on(Tab::Files, &workspace, cx);
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

        open_on(Tab::Files, &workspace, cx);
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

        open_on(Tab::Files, &workspace, cx);
        cx.simulate_input("main");
        cx.run_until_parked();
        cx.dispatch_action(menu::SecondaryConfirm);
        cx.run_until_parked();
        workspace.update(cx, |workspace, _| assert_eq!(workspace.panes().len(), 2));
    }

    #[gpui::test]
    async fn test_symbols_and_classes_tabs(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "widget.rs": "fn helper() {}\nstruct Widget;\nenum WidgetKind {}\nfn widget_new() {}\nmod widget_mod {}\n",
                    "widget_trait.php": "<?php\ntrait WidgetTrait {}\n",
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Rust".into(),
                matcher: language::LanguageMatcher {
                    path_suffixes: vec!["rs".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            None,
        )));
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            language::FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/root/widget.rs"), cx)
            })
            .await
            .expect("buffer opens");
        let fake_server = fake_servers.next().await.expect("language server starts");
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>({
            let requests = requests.clone();
            move |_, _| {
                requests.fetch_add(1, Ordering::SeqCst);
                async {
                    let rust = path!("/root/widget.rs");
                    Ok(Some(lsp::WorkspaceSymbolResponse::Flat(vec![
                        symbol("Widget", lsp::SymbolKind::STRUCT, rust, 1),
                        symbol("WidgetKind", lsp::SymbolKind::ENUM, rust, 2),
                        symbol("widget_new", lsp::SymbolKind::FUNCTION, rust, 3),
                        // A Rust module is not a class; an Intelephense PHP trait is.
                        symbol("widget_mod", lsp::SymbolKind::MODULE, rust, 4),
                        symbol(
                            "WidgetTrait",
                            lsp::SymbolKind::MODULE,
                            path!("/root/widget_trait.php"),
                            1,
                        ),
                    ])))
                }
            }
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let names = |picker: &Entity<Picker<SearchDelegate>>, cx: &mut VisualTestContext| {
            picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .matches
                    .iter()
                    .filter_map(|search_match| match search_match {
                        SearchMatch::Symbol(string_match) => Some(
                            picker.delegate.symbols[string_match.candidate_id]
                                .name
                                .clone(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
        };
        let switch_tab = |tab: Tab, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let modal = workspace
                    .active_modal::<SearchEverywhere>(cx)
                    .expect("Search Everywhere is open");
                modal.update(cx, |modal, cx| modal.switch_tab(tab, window, cx));
            });
        };

        cx.dispatch_action(Toggle);
        let picker = open_picker(&workspace, cx);
        switch_tab(Tab::Symbols, cx);
        // Typed one key at a time with the app idle in between, as a person types.
        for character in ["w", "i", "d", "g", "e", "t"] {
            cx.simulate_input(character);
            cx.run_until_parked();
        }
        cx.executor().advance_clock(SYMBOL_QUERY_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "typing one word sends one query"
        );
        let mut symbols = names(&picker, cx);
        symbols.sort();
        assert_eq!(
            symbols,
            [
                "Widget",
                "WidgetKind",
                "WidgetTrait",
                "widget_mod",
                "widget_new"
            ]
        );

        switch_tab(Tab::Classes, cx);
        cx.run_until_parked();
        cx.executor().advance_clock(SYMBOL_QUERY_DEBOUNCE);
        cx.run_until_parked();
        let mut classes = names(&picker, cx);
        classes.sort();
        assert_eq!(classes, ["Widget", "WidgetKind", "WidgetTrait"]);

        cx.simulate_input("kind");
        cx.run_until_parked();
        cx.executor().advance_clock(SYMBOL_QUERY_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(names(&picker, cx), ["WidgetKind"]);
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        let editor = workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<SearchEverywhere>(cx).is_none());
            workspace
                .active_item_as::<Editor>(cx)
                .expect("the symbol opens in an editor")
        });
        editor.update(cx, |editor, cx| {
            let caret = editor
                .selections
                .all_adjusted(&editor.display_snapshot(cx))
                .into_iter()
                .next()
                .expect("one caret");
            assert_eq!(caret.start, Point::new(2, 0));
        });

        // All: classes, files, other symbols, then actions, each row labelled.
        cx.dispatch_action(Toggle);
        let picker = open_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.tab, Tab::All));
        let kinds = |picker: &Entity<Picker<SearchDelegate>>, cx: &mut VisualTestContext| {
            picker.read_with(cx, |picker, _| {
                picker
                    .delegate
                    .matches
                    .iter()
                    .map(|search_match| picker.delegate.kind_label(search_match))
                    .collect::<Vec<_>>()
            })
        };
        cx.simulate_input("widget");
        cx.run_until_parked();
        // Files show before the language server answers.
        let early = kinds(&picker, cx);
        assert_eq!(early[..2], ["File", "File"]);
        assert!(!early.contains(&"Class") && !early.contains(&"Symbol"));

        cx.executor().advance_clock(SYMBOL_QUERY_DEBOUNCE);
        cx.run_until_parked();
        let all = kinds(&picker, cx);
        assert_eq!(
            all[..7],
            [
                "Class", "Class", "Class", "File", "File", "Symbol", "Symbol"
            ]
        );
        assert!(all[7..].iter().all(|kind| *kind == "Action"));
    }

    #[gpui::test]
    async fn test_tab_keys_cycle_ready_tabs(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        cx.update(|cx| {
            let bindings = KeymapFile::load_asset(settings::ZENITH_KEYMAP_PATH, None, cx)
                .expect("Zenith keymap loads");
            cx.bind_keys(bindings);
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.dispatch_action(Toggle);
        let picker = open_picker(&workspace, cx);
        let mut seen = Vec::new();
        for _ in 0..5 {
            cx.simulate_keystrokes("tab");
            seen.push(picker.read_with(cx, |picker, _| picker.delegate.tab));
        }
        // Hooks has no source yet, so Tab skips it.
        assert_eq!(
            seen,
            [
                Tab::Classes,
                Tab::Files,
                Tab::Symbols,
                Tab::Actions,
                Tab::All
            ]
        );
        cx.simulate_keystrokes("shift-tab");
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.tab, Tab::Actions)
        });
    }

    fn symbol(name: &str, kind: lsp::SymbolKind, path: &str, row: u32) -> lsp::SymbolInformation {
        #[allow(deprecated)]
        lsp::SymbolInformation {
            name: name.to_string(),
            kind,
            tags: None,
            deprecated: None,
            container_name: None,
            location: lsp::Location::new(
                lsp::Uri::from_file_path(path).expect("absolute path"),
                lsp::Range::new(lsp::Position::new(row, 0), lsp::Position::new(row, 0)),
            ),
        }
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
            assert!(bound_to(&NextTab, &["tab"]));
            assert!(bound_to(&PreviousTab, &["tab"]));
            // PhpStorm's Go to Class / File / Symbol and Recent Files.
            assert!(bound_to(&GoToClass, &["o"]));
            assert!(bound_to(&GoToFile, &["o"]));
            assert!(bound_to(&GoToSymbol, &["o"]));
            assert!(bound_to(&RecentFiles, &["e"]));
            assert!(bound_to(&RecentLocations, &["e"]));
        });
    }

    /// The real key presses, resolved across the keymap files in load order and the
    /// contexts they are pressed in: Editor, terminal and project panel.
    #[gpui::test]
    fn test_jetbrains_keys_win_in_every_context(cx: &mut TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
            workspace::init(AppState::test(cx), cx);
            init(cx);
            // Partial load: the default keymap names actions from crates this test
            // doesn't link, which are skipped. The ones competing for these keys are
            // linked (dev-dependencies); the first assertions below prove it.
            let load = |bindings: &mut Vec<_>, path: &str| {
                bindings.extend(
                    KeymapFile::load_asset_allow_partial_failure(path, cx).expect("keymap loads"),
                );
            };
            let jetbrains = settings::BaseKeymap::JetBrains
                .asset_path()
                .expect("JetBrains keymap path");
            let mut base = Vec::new();
            load(&mut base, settings::DEFAULT_KEYMAP_PATH);
            load(&mut base, jetbrains);
            let mut all = base.clone();
            // Same order as `load_default_keymap`.
            load(&mut all, settings::ZENITH_KEYMAP_PATH);
            load(&mut all, settings::SPECIFIC_OVERRIDES_KEYMAP_PATH);
            let (base, all) = (gpui::Keymap::new(base), gpui::Keymap::new(all));

            let os = if cfg!(target_os = "macos") {
                gpui::OperatingSystem::Mac
            } else {
                gpui::OperatingSystem::Linux
            };
            let resolve = |keymap: &gpui::Keymap, keys: &str, contexts: &[&str]| {
                let stack: Vec<_> = contexts
                    .iter()
                    .map(|context| gpui::KeyContext::parse(context).expect("context"))
                    .collect();
                let text = format!("secondary-{keys}");
                let keystroke = gpui::Keystroke::parse_with_os(&text, os).expect("keystroke");
                let (found, _) = keymap.bindings_for_input(&[keystroke], &stack);
                // Highest precedence first.
                found
                    .first()
                    .map(|binding| binding.action().name().to_string())
            };
            let editor = ["Workspace", "Pane", "Editor mode=full"];
            let terminal = ["Workspace", "Pane", "Terminal"];
            let project_panel = ["Workspace", "ProjectPanel"];

            // Not vacuous: without Zenith's file the competing bindings are in force.
            assert_eq!(
                resolve(&base, "e", &editor).as_deref(),
                Some("file_finder::Toggle")
            );
            assert_eq!(
                resolve(&base, "shift-o", &editor).as_deref(),
                Some("file_finder::Toggle")
            );
            assert_eq!(
                resolve(&base, "o", &editor).as_deref(),
                Some("project_symbols::Toggle")
            );

            assert_eq!(
                resolve(&base, "shift-e", &editor).as_deref(),
                Some("pane::RevealInProjectPanel")
            );

            for contexts in [&editor[..], &terminal, &project_panel] {
                for (keys, action) in [
                    ("o", "zenith_search::GoToClass"),
                    ("shift-o", "zenith_search::GoToFile"),
                    ("alt-o", "zenith_search::GoToSymbol"),
                    ("e", "zenith_search::RecentFiles"),
                    ("shift-e", "zenith_search::RecentLocations"),
                ] {
                    assert_eq!(
                        resolve(&all, keys, contexts).as_deref(),
                        Some(action),
                        "secondary-{keys} in {contexts:?}"
                    );
                }
            }
        });
    }
}

#[cfg(test)]
mod zenith_look_tests {
    use super::*;

    #[test]
    fn test_split_path_label() {
        assert_eq!(
            split_path_label("includes/Core/Assets.php", &[0, 9, 14, 15]),
            ("Assets.php".to_string(), Some("includes/Core".to_string()), vec![0, 1])
        );
        assert_eq!(
            split_path_label("composer.json", &[0, 1]),
            ("composer.json".to_string(), None, vec![0, 1])
        );
    }

    #[test]
    fn test_symbol_badges() {
        assert_eq!(symbol_badge_letter(SymbolKind::Class), ("C", BadgeColor::Type));
        assert_eq!(symbol_badge_letter(SymbolKind::Interface).0, "I");
        assert_eq!(symbol_badge_letter(SymbolKind::Module).0, "T");
        assert_eq!(symbol_badge_letter(SymbolKind::Method), ("m", BadgeColor::Member));
        assert_eq!(symbol_badge_letter(SymbolKind::Function).0, "f");
        assert_eq!(symbol_badge_letter(SymbolKind::Property).0, "p");
    }

    #[test]
    fn test_group_label_only_on_first_row_of_a_kind() {
        let kinds = ["Class", "Class", "File", "File", "Action"];
        let starts: Vec<bool> = (0..kinds.len())
            .map(|ix| starts_group(ix.checked_sub(1).map(|p| kinds[p]), kinds[ix]))
            .collect();
        assert_eq!(starts, [true, false, true, false, true]);
    }
}
