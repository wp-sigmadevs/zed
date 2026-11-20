//! PhpStorm's Problems tool window: a small tree of files and their diagnostics, with a
//! "File" tab (the active editor, every severity) and a "Project Errors" tab (errors only).
//! Not a wrapper of the diagnostics multibuffer: that view is an editor, this is a list.

use std::{collections::HashSet, ops::Range};

use editor::Editor;
use file_icons::FileIcons;
use gpui::{
    Action, AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render, Role, ScrollStrategy,
    SharedString, Styled, Task, UniformListScrollHandle, WeakEntity, Window, actions, px,
    uniform_list,
};
use language::BufferSnapshot;
use project::{Project, ProjectItem as _, ProjectPath};
use std::path::Path;
use text::Point;
use ui::{ButtonLike, ContextMenu, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    zenith_problems,
    [
        /// Toggles focus on the Problems tool window.
        ToggleFocus,
        /// Shows the problems of the file in the active editor.
        ShowFileProblems,
        /// Shows the errors of every file in the project.
        ShowProjectErrors,
        /// Expands every file in the Problems tool window.
        ExpandAll,
        /// Collapses every file in the Problems tool window.
        CollapseAll,
    ]
);

const HEADER_HEIGHT: f32 = 38.;
const ROW_HEIGHT: f32 = 28.;
const PROBLEM_INDENT: f32 = 24.;
const STRIP_WIDTH: f32 = 36.;

/// Project Errors opens one buffer per file with errors; this bounds that memory.
const MAX_FILES: usize = 200;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ProblemsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &ShowFileProblems, window, cx| {
            show_tab(workspace, Tab::File, window, cx);
        });
        workspace.register_action(|workspace, _: &ShowProjectErrors, window, cx| {
            show_tab(workspace, Tab::Project, window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let panel = cx.new(|cx| ProblemsPanel::new(workspace, cx));
        workspace.add_panel(panel, window, cx);
    })
    .detach();
}

fn show_tab(workspace: &mut Workspace, tab: Tab, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(panel) = workspace.focus_panel::<ProblemsPanel>(window, cx) {
        panel.update(cx, |panel, cx| panel.set_tab(tab, cx));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    File,
    Project,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    fn word(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    fn icon(self) -> Icon {
        // Shape and colour both differ, so status is never colour alone.
        match self {
            Severity::Error => Icon::new(IconName::XCircle).color(Color::Error),
            Severity::Warning => Icon::new(IconName::Warning).color(Color::Warning),
            // JetBrains draws weak warnings and typos as a green check.
            Severity::Info => Icon::new(IconName::Check).color(Color::Success),
        }
    }
}

struct Problem {
    severity: Severity,
    message: SharedString,
    point: Point,
}

impl Problem {
    /// One-based line, as shown after the message.
    fn line_label(&self) -> String {
        format!(":{}", self.point.row + 1)
    }
}

/// "1 problem" / "10 problems", the dim text after a file's path.
fn problems_label(count: usize) -> String {
    match count {
        1 => "1 problem".to_string(),
        count => format!("{count} problems"),
    }
}

struct FileNode {
    path: ProjectPath,
    problems: Vec<Problem>,
}

impl FileNode {
    fn name(&self) -> &str {
        self.path.path.file_name().unwrap_or_default()
    }

    fn dir(&self) -> &str {
        self.path
            .path
            .parent()
            .map(|dir| dir.as_unix_str())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    File(usize),
    Problem(usize, usize),
}

/// The primary diagnostics of `snapshot`, in buffer order; only errors when `errors_only`.
fn collect(snapshot: &BufferSnapshot, errors_only: bool) -> Vec<Problem> {
    snapshot
        .diagnostics_in_range::<_, Point>(0..snapshot.len(), false)
        .filter(|entry| entry.diagnostic.is_primary)
        .filter_map(|entry| {
            let severity = match entry.diagnostic.severity {
                lsp::DiagnosticSeverity::ERROR => Severity::Error,
                _ if errors_only => return None,
                lsp::DiagnosticSeverity::WARNING => Severity::Warning,
                _ => Severity::Info,
            };
            Some(Problem {
                severity,
                message: entry
                    .diagnostic
                    .message
                    .as_str()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
                    .into(),
                point: entry.range.start,
            })
        })
        .collect()
}

pub struct ProblemsPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    tab: Tab,
    /// The active editor's file; absent when it has no problems.
    file: Option<FileNode>,
    project_files: Vec<FileNode>,
    project_truncated: bool,
    /// Project-wide error count from the diagnostic summaries (no buffers opened).
    error_count: usize,
    active: bool,
    collapsed: HashSet<ProjectPath>,
    rows: Vec<Row>,
    selected: Option<usize>,
    project_task: Task<()>,
}

impl ProblemsPanel {
    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        cx.subscribe(&project, |this, _, event, cx| {
            if matches!(event, project::Event::DiagnosticsUpdated { .. }) {
                this.diagnostics_changed(cx);
            }
        })
        .detach();
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            cx.subscribe(&workspace_entity, |this, _, event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    this.refresh_file(cx);
                }
            })
            .detach();
        }
        let error_count = project.read(cx).diagnostic_summary(false, cx).error_count;
        Self {
            project,
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            tab: Tab::File,
            file: None,
            project_files: Vec::new(),
            project_truncated: false,
            error_count,
            active: false,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            project_task: Task::ready(()),
        }
    }

    fn files(&self) -> &[FileNode] {
        match self.tab {
            Tab::File => self.file.as_slice(),
            Tab::Project => &self.project_files,
        }
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.selected = None;
        self.rebuild_rows(cx);
    }

    fn diagnostics_changed(&mut self, cx: &mut Context<Self>) {
        self.error_count = self
            .project
            .read(cx)
            .diagnostic_summary(false, cx)
            .error_count;
        self.refresh_file(cx);
        if self.active {
            self.refresh_project(cx);
        }
    }

    fn active_buffer(&self, cx: &App) -> Option<Entity<language::Buffer>> {
        self.workspace
            .upgrade()?
            .read(cx)
            .active_item_as::<Editor>(cx)?
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
    }

    /// One buffer's diagnostics are cheap to read, so this runs inline and the tab follows
    /// the editor without a flicker.
    fn refresh_file(&mut self, cx: &mut Context<Self>) {
        self.file = self.active_buffer(cx).and_then(|buffer| {
            let path = buffer.read(cx).project_path(cx)?;
            let problems = collect(&buffer.read(cx).snapshot(), false);
            (!problems.is_empty()).then_some(FileNode { path, problems })
        });
        self.rebuild_rows(cx);
    }

    fn refresh_project(&mut self, cx: &mut Context<Self>) {
        let mut paths: Vec<ProjectPath> = self
            .project
            .read(cx)
            .diagnostic_summaries(false, cx)
            .filter(|(_, _, summary)| summary.error_count > 0)
            .map(|(path, _, _)| path)
            .collect();
        paths.sort();
        paths.dedup();
        let truncated = paths.len() > MAX_FILES;
        paths.truncate(MAX_FILES);
        let opens: Vec<_> = paths
            .into_iter()
            .map(|path| {
                let open = self
                    .project
                    .update(cx, |project, cx| project.open_buffer(path.clone(), cx));
                (path, open)
            })
            .collect();
        self.project_task = cx.spawn(async move |this, cx| {
            let mut files = Vec::new();
            for (path, open) in opens {
                let Some(buffer) = open.await.log_err() else {
                    continue;
                };
                let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let problems = cx
                    .background_spawn(async move { collect(&snapshot, true) })
                    .await;
                if !problems.is_empty() {
                    files.push(FileNode { path, problems });
                }
            }
            this.update(cx, |this, cx| {
                this.project_files = files;
                this.project_truncated = truncated;
                this.rebuild_rows(cx);
            })
            .log_err();
        });
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        for (file_ix, file) in self.files().iter().enumerate() {
            rows.push(Row::File(file_ix));
            if !self.collapsed.contains(&file.path) {
                rows.extend((0..file.problems.len()).map(|ix| Row::Problem(file_ix, ix)));
            }
        }
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
        if self.rows.is_empty() {
            return;
        }
        self.select(self.selected.map_or(0, |ix| ix.saturating_sub(1)), cx);
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

    /// Collapses a file, or moves from a problem to its file.
    fn select_parent(&mut self, _: &menu::SelectParent, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.and_then(|ix| self.rows.get(ix).copied()) {
            Some(Row::File(file_ix)) => self.set_collapsed(file_ix, true, cx),
            Some(Row::Problem(file_ix, _)) => {
                if let Some(ix) = self.rows.iter().position(|row| *row == Row::File(file_ix)) {
                    self.select(ix, cx);
                }
            }
            None => {}
        }
    }

    fn select_child(&mut self, _: &menu::SelectChild, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(Row::File(file_ix)) = self.selected.and_then(|ix| self.rows.get(ix).copied()) {
            self.set_collapsed(file_ix, false, cx);
        }
    }

    fn set_collapsed(&mut self, file_ix: usize, collapsed: bool, cx: &mut Context<Self>) {
        let Some(path) = self.files().get(file_ix).map(|file| file.path.clone()) else {
            return;
        };
        if collapsed {
            self.collapsed.insert(path);
        } else {
            self.collapsed.remove(&path);
        }
        self.rebuild_rows(cx);
    }

    fn expand_all(&mut self, _: &ExpandAll, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.rebuild_rows(cx);
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        self.collapsed = self.files().iter().map(|file| file.path.clone()).collect();
        self.rebuild_rows(cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, window, cx);
        }
    }

    /// Enter or click: a file toggles, a problem opens its file at the problem.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.select(ix, cx);
        match self.rows.get(ix).copied() {
            Some(Row::File(file_ix)) => {
                let collapsed = self
                    .files()
                    .get(file_ix)
                    .is_some_and(|file| !self.collapsed.contains(&file.path));
                self.set_collapsed(file_ix, collapsed, cx);
            }
            Some(Row::Problem(file_ix, problem_ix)) => {
                let Some(file) = self.files().get(file_ix) else {
                    return;
                };
                let point = file.problems[problem_ix].point;
                self.open(file.path.clone(), point, window, cx);
            }
            None => {}
        }
    }

    fn open(&self, path: ProjectPath, point: Point, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_path(path, None, true, window, cx)
        });
        window
            .spawn(cx, async move |cx| {
                let editor = open.await?.downcast::<Editor>();
                if let Some(editor) = editor {
                    cx.update(|window, cx| {
                        editor.update(cx, |editor, cx| {
                            editor.go_to_singleton_buffer_point(point, window, cx)
                        })
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    /// Plain text of a row: also its accessible label.
    fn row_text(&self, row: Row) -> String {
        match row {
            Row::File(file_ix) => {
                let file = &self.files()[file_ix];
                format!("{} ({})", file.name(), file.problems.len())
            }
            Row::Problem(file_ix, ix) => {
                let problem = &self.files()[file_ix].problems[ix];
                format!(
                    "{}: {}{}",
                    problem.severity.word(),
                    problem.message,
                    problem.line_label()
                )
            }
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self.rows[ix];
        let selected = self.selected == Some(ix);
        let item = ListItem::new(("zenith-problems-row", ix))
            .spacing(ListItemSpacing::Dense)
            .height(px(ROW_HEIGHT))
            .rounded()
            .toggle_state(selected)
            .aria_role(Role::TreeItem)
            .aria_label(self.row_text(row))
            .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)));
        match row {
            Row::File(file_ix) => {
                let file = &self.files()[file_ix];
                let icon = FileIcons::get_icon(Path::new(file.name()), cx)
                    .map(|path| Icon::from_path(path).size(IconSize::Small));
                item.start_slot::<Icon>(icon)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(file.name().to_string()).weight(gpui::FontWeight::BOLD),
                            )
                            .child(Label::new(file.dir().to_string()).color(Color::Muted))
                            .child(
                                Label::new(problems_label(file.problems.len())).color(Color::Muted),
                            ),
                    )
                    .into_any_element()
            }
            Row::Problem(file_ix, problem_ix) => {
                let problem = &self.files()[file_ix].problems[problem_ix];
                item.indent_level(1)
                    .indent_step_size(px(PROBLEM_INDENT))
                    .start_slot(problem.severity.icon())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(problem.message.clone()).truncate())
                            .child(Label::new(problem.line_label()).color(Color::Muted)),
                    )
                    .into_any_element()
            }
        }
    }

    /// A rounded chip: the label with its count inline after it (a `CountBadge` is absolutely
    /// positioned and would overlap the label). The active chip is outlined and tinted.
    fn tab_chip(
        &self,
        id: &'static str,
        label: &'static str,
        count: usize,
        tab: Tab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.tab == tab;
        let border = if active {
            cx.theme().colors().border_focused
        } else {
            gpui::transparent_black()
        };
        div().rounded(px(6.)).border_1().border_color(border).child(
            ButtonLike::new(id)
                .style(ButtonStyle::Subtle)
                .toggle_state(active)
                .aria_role(Role::Tab)
                .aria_label(if count > 0 {
                    format!("{label} {count}")
                } else {
                    label.to_string()
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_tab(tab, cx)))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(label))
                        .when(count > 0, |this| {
                            this.child(Label::new(count.to_string()).color(Color::Muted))
                        }),
                ),
        )
    }

    /// JetBrains' tool window ⋮ menu: every entry is an action, so each is also bindable.
    fn header_menu(&self) -> impl IntoElement {
        let focus = self.focus_handle.clone();
        PopoverMenu::new("zenith-problems-menu")
            .trigger(
                IconButton::new("zenith-problems-menu-button", IconName::ZenithMoreVertical)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Warning)
                    .tooltip(Tooltip::text("More")),
            )
            .menu(move |window, cx| {
                let focus = focus.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.context(focus)
                        .action("File Problems", ShowFileProblems.boxed_clone())
                        .action("Project Errors", ShowProjectErrors.boxed_clone())
                        .separator()
                        .action("Hide", workspace::ToggleBottomDock.boxed_clone())
                }))
            })
    }

    /// Title, tab chips, then ⋮ and hide at the right, like the Project tool window's header.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file_count = self.file.as_ref().map_or(0, |file| file.problems.len());
        h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new("Problems").weight(gpui::FontWeight::BOLD))
            .child(self.tab_chip("zenith-problems-file", "File", file_count, Tab::File, cx))
            .child(self.tab_chip(
                "zenith-problems-project",
                "Project Errors",
                self.error_count,
                Tab::Project,
                cx,
            ))
            .child(div().flex_1())
            .child(self.header_menu())
            .child(
                div()
                    .debug_selector(|| "zenith-problems-hide".into())
                    .child(
                        IconButton::new("zenith-problems-hide", IconName::ZenithMinus)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Warning)
                            .tooltip(Tooltip::text("Hide"))
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(workspace::ToggleBottomDock.boxed_clone(), cx)
                            }),
                    ),
            )
    }

    /// The left icon strip; only controls that do something.
    fn render_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(STRIP_WIDTH))
            .flex_none()
            .items_center()
            .pt_2()
            .gap_1()
            .child(
                IconButton::new("zenith-problems-expand-all", IconName::ZenithExpandAll)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Expand All"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.expand_all(&ExpandAll, window, cx)),
                    ),
            )
            .child(
                IconButton::new("zenith-problems-collapse-all", IconName::ZenithCollapseAll)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Collapse All"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.collapse_all(&CollapseAll, window, cx)
                    })),
            )
    }
}

impl Render for ProblemsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if self.rows.is_empty() {
            let message = match self.tab {
                Tab::File => "No problems in this file.",
                Tab::Project => "No errors in the project.",
            };
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element()
        } else {
            uniform_list(
                "zenith-problems-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .into_any_element()
        };
        v_flex()
            .key_context("ProblemsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::select_child))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(Self::collapse_all))
            .size_full()
            .child(self.render_header(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_strip(cx))
                    .child(v_flex().flex_1().min_w_0().h_full().child(body)),
            )
            .when(self.tab == Tab::Project && self.project_truncated, |this| {
                this.child(
                    div().px_2().child(
                        Label::new(format!("Showing the first {MAX_FILES} files."))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
                )
            })
    }
}

impl Focusable for ProblemsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ProblemsPanel {}

impl Panel for ProblemsPanel {
    fn persistent_name() -> &'static str {
        "Problems Panel"
    }

    fn panel_key() -> &'static str {
        "ProblemsPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    // ponytail: bottom only; a dock setting needs `settings_content`, a full rebuild.
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ZenithAlert)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Problems")
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        (self.error_count > 0).then(|| self.error_count.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            // The dock calls this inside a workspace update, which `refresh_file` reads.
            cx.defer_in(window, |this, _, cx| {
                this.refresh_file(cx);
                this.refresh_project(cx);
            });
        }
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, TestAppContext, VisualTestContext};
    use language::DiagnosticSourceKind;
    use lsp::LanguageServerId;
    use serde_json::json;
    use workspace::{AppState, MultiWorkspace, OpenOptions};

    struct Fixture {
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        panel: Entity<ProblemsPanel>,
    }

    fn diagnostic(
        line: u32,
        column: u32,
        severity: lsp::DiagnosticSeverity,
        message: &str,
    ) -> lsp::Diagnostic {
        lsp::Diagnostic {
            range: lsp::Range::new(
                lsp::Position::new(line, column),
                lsp::Position::new(line, column + 1),
            ),
            severity: Some(severity),
            message: message.into(),
            ..Default::default()
        }
    }

    fn publish(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
        path: &str,
        diagnostics: Vec<lsp::Diagnostic>,
    ) {
        let lsp_store = fixture
            .project
            .read_with(cx, |project, _| project.lsp_store());
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store
                .update_diagnostics(
                    LanguageServerId(0),
                    lsp::PublishDiagnosticsParams {
                        uri: lsp::Uri::from_file_path(path).unwrap(),
                        diagnostics,
                        version: None,
                    },
                    None,
                    DiagnosticSourceKind::Pushed,
                    &[],
                    cx,
                )
                .unwrap();
        });
        cx.run_until_parked();
    }

    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            cx.bind_keys([
                KeyBinding::new("down", menu::SelectNext, Some("ProblemsPanel")),
                KeyBinding::new("up", menu::SelectPrevious, Some("ProblemsPanel")),
                KeyBinding::new("enter", menu::Confirm, Some("ProblemsPanel")),
            ]);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/p",
                json!({
                    "a.rs": "fn a() {}\nlet x = 1;\nlet y = 2;\nlet z = 3;\n",
                    "b.rs": "fn b() {}\nlet w = 4;\n",
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let panel = workspace.read_with(cx, |workspace, cx| workspace.panel::<ProblemsPanel>(cx));
        let fixture = Fixture {
            project,
            workspace,
            panel: panel.expect("panel is registered"),
        };
        publish(
            &fixture,
            cx,
            "/p/a.rs",
            vec![
                diagnostic(1, 4, lsp::DiagnosticSeverity::ERROR, "bad x"),
                diagnostic(2, 0, lsp::DiagnosticSeverity::WARNING, "unused y"),
                diagnostic(3, 2, lsp::DiagnosticSeverity::HINT, "hint z"),
            ],
        );
        publish(
            &fixture,
            cx,
            "/p/b.rs",
            vec![diagnostic(0, 3, lsp::DiagnosticSeverity::ERROR, "bad b")],
        );
        (fixture, cx)
    }

    fn texts(fixture: &Fixture, cx: &mut VisualTestContext) -> Vec<String> {
        fixture.panel.read_with(cx, |panel, _| {
            panel.rows.iter().map(|row| panel.row_text(*row)).collect()
        })
    }

    async fn open_file(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
        path: &str,
    ) -> Entity<Editor> {
        let editor = fixture
            .workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path.into(), OpenOptions::default(), window, cx)
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.run_until_parked();
        editor
    }

    fn open_panel(fixture: &Fixture, cx: &mut VisualTestContext) {
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<ProblemsPanel>(window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn project_errors_tree_drops_warnings(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        fixture
            .panel
            .update(cx, |panel, cx| panel.set_tab(Tab::Project, cx));
        assert_eq!(
            texts(&fixture, cx),
            ["a.rs (1)", "error: bad x:2", "b.rs (1)", "error: bad b:1"]
        );
        // The rail foot draws the bottom dock's panel buttons, so this is its Problems button.
        fixture.workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.dock_at_position(DockPosition::Bottom).read(cx);
            assert!(dock.panel::<ProblemsPanel>().is_some());
        });
        let label = fixture
            .panel
            .update_in(cx, |panel, window, cx| panel.icon_label(window, cx));
        assert_eq!(label, Some("2".into()));
    }

    #[gpui::test]
    async fn file_tab_follows_active_editor(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        assert!(texts(&fixture, cx).is_empty());
        open_file(&fixture, cx, "/p/a.rs").await;
        assert_eq!(
            texts(&fixture, cx),
            [
                "a.rs (3)",
                "error: bad x:2",
                "warning: unused y:3",
                "info: hint z:4"
            ]
        );
        open_file(&fixture, cx, "/p/b.rs").await;
        assert_eq!(texts(&fixture, cx), ["b.rs (1)", "error: bad b:1"]);
    }

    #[gpui::test]
    async fn keyboard_selects_and_enter_lands_on_the_range(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        fixture
            .panel
            .update(cx, |panel, cx| panel.set_tab(Tab::Project, cx));
        // Rows: a.rs, error in a.rs, b.rs, error in b.rs.
        cx.simulate_keystrokes("down down");
        let selected = |cx: &mut VisualTestContext| fixture.panel.read_with(cx, |p, _| p.selected);
        assert_eq!(selected(cx), Some(1));
        cx.simulate_keystrokes("down up up");
        assert_eq!(selected(cx), Some(0));
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let editor = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("the file is open");
        let head = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        assert_eq!(head, Point::new(1, 4));
    }

    #[gpui::test]
    async fn enter_on_a_file_row_collapses_it(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        fixture
            .panel
            .update(cx, |panel, cx| panel.set_tab(Tab::Project, cx));
        cx.simulate_keystrokes("down enter");
        assert_eq!(
            texts(&fixture, cx),
            ["a.rs (1)", "b.rs (1)", "error: bad b:1"]
        );
    }

    fn bottom_dock_open(fixture: &Fixture, cx: &mut VisualTestContext) -> bool {
        fixture.workspace.read_with(cx, |workspace, cx| {
            workspace
                .dock_at_position(DockPosition::Bottom)
                .read(cx)
                .is_open()
        })
    }

    #[gpui::test]
    async fn header_hide_button_closes_the_dock(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        assert!(bottom_dock_open(&fixture, cx));
        let bounds = cx
            .debug_bounds("zenith-problems-hide")
            .expect("the header draws a hide button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!bottom_dock_open(&fixture, cx));
    }

    #[test]
    fn problems_label_pluralises() {
        assert_eq!(problems_label(1), "1 problem");
        assert_eq!(problems_label(10), "10 problems");
    }

    #[gpui::test]
    async fn expand_and_collapse_all(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        open_panel(&fixture, cx);
        fixture
            .panel
            .update(cx, |panel, cx| panel.set_tab(Tab::Project, cx));
        let full = ["a.rs (1)", "error: bad x:2", "b.rs (1)", "error: bad b:1"];
        assert_eq!(texts(&fixture, cx), full);
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.collapse_all(&CollapseAll, window, cx)
        });
        assert_eq!(texts(&fixture, cx), ["a.rs (1)", "b.rs (1)"]);
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.expand_all(&ExpandAll, window, cx)
        });
        assert_eq!(texts(&fixture, cx), full);
    }
}
