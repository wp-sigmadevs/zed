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
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
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
        /// Moves to the next Search Everywhere tab.
        NextTab,
        /// Moves to the previous Search Everywhere tab.
        PreviousTab
    ]
);

const RECENT_FILES_LIMIT: usize = 20;
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
            Tab::Classes => "Search classes…",
            Tab::Files => "Search files…",
            Tab::Symbols => "Search symbols…",
            Tab::Actions => "Search actions…",
            Tab::All | Tab::Hooks => "Search everywhere…",
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
            symbols: Vec::new(),
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

    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.picker.read(cx).delegate.tab.cycle(step);
        self.switch_tab(tab, window, cx);
    }
}

impl Render for SearchEverywhere {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.picker.read(cx).delegate.tab;
        v_flex()
            .key_context("SearchEverywhere")
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)),
            )
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
}

pub struct SearchDelegate {
    modal: WeakEntity<SearchEverywhere>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    recent_files: Vec<ProjectPath>,
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
    fn kind_label(&self, search_match: &SearchMatch) -> &'static str {
        match search_match {
            SearchMatch::Action(_) => "Action",
            SearchMatch::File { .. } => "File",
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
        let search_match = self.matches.get(ix)?;
        let item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .end_slot::<Label>((self.tab == Tab::All).then(|| {
                Label::new(self.kind_label(search_match))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }));
        match search_match {
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
                Some(
                    item.child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .gap_2()
                            .child(
                                HighlightedLabel::new(
                                    symbol.label.text.clone(),
                                    string_match.positions.clone(),
                                )
                                .truncate(),
                            )
                            .child(
                                Label::new(format!("{location}:{}", symbol.range.start.0.row + 1))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
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
                    SearchMatch::Action(_) | SearchMatch::Symbol(_) => None,
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
        });
    }
}
