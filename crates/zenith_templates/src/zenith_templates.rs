//! The project panel's "+" popup (plan 20): pick a file type, type a name, and the
//! file is created in the selected directory from that type's template
//! (`template.rs`; built-ins plus `~/.config/zenith/templates/` and, in a trusted
//! project, `.zenith/templates/`). "PHP Class" opens its own dialog
//! (`php_class.rs`). File and Directory hand over to the project panel's own
//! inline name editor.

mod php_class;
pub mod psr4;
pub mod template;

use command_palette_hooks::CommandPaletteFilter;
use editor::Editor;
use file_icons::FileIcons;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, StringMatchOptions, match_strings};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Task, WeakEntity, Window, actions, rems,
};
pub use php_class::{NewPhpClassDialog, PhpClassSpec, PhpKind};
use picker::{Picker, PickerDelegate};
use project::{Project, ProjectPath, WorktreeId, trusted_worktrees::TrustedWorktrees};
use project_panel::{NewDirectory, NewFile, ProjectPanel};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use template::{Rendered, Template};
use text::Point;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::rel_path::RelPath;
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt as _};

actions!(
    zenith_templates,
    [
        /// Opens the New popup: pick a file type and name, and it is created in the
        /// directory selected in the project panel.
        NewFromTemplate,
        /// Opens the New PHP Class dialog for the directory selected in the project
        /// panel: class, interface, trait or enum, namespaced from composer.json.
        NewPhpClass
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &NewFromTemplate, window, cx| {
            NewPopup::open(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &NewPhpClass, window, cx| {
            NewPhpClassDialog::open(workspace, window, cx)
        });
    })
    .detach();
}

#[derive(Clone)]
enum Kind {
    /// `project_panel::NewFile`
    File,
    /// `project_panel::NewDirectory`
    Directory,
    /// `agent::NewThread`
    AgentSession,
    /// `zenith_scratches::NewScratchFile`, looked up by name: no crate dependency.
    Scratch,
    /// [`NewPhpClass`]
    PhpClass,
    /// A named file rendered from a template.
    Template(Arc<Template>),
}

#[derive(Clone)]
struct Entry {
    label: SharedString,
    kind: Kind,
    /// A file name the icon theme can match, for the row's icon.
    icon_sample: SharedString,
}

/// Rendered by the PHP Class dialog, not listed as rows.
const PHP_KINDS: [&str; 4] = ["php-class", "php-interface", "php-trait", "php-enum"];
/// Built-in rows after Agent Session, in PhpStorm's order.
const LATER_BUILTINS: [&str; 8] = [
    "javascript",
    "typescript",
    "html",
    "stylesheet",
    "scss",
    "less",
    "json",
    "markdown",
];

/// PhpStorm's order, then templates with new ids from the user's and the
/// project's folders. Rows whose action is not in this build are left out.
fn entries(templates: &[Template], agent: bool, scratch: bool) -> Vec<Entry> {
    let fixed = |label: &'static str, kind: Kind, icon_sample: &'static str| Entry {
        label: label.into(),
        kind,
        icon_sample: icon_sample.into(),
    };
    let row = |template: &Template| Entry {
        label: template.name.clone().into(),
        icon_sample: format!("x.{}", template.extension).into(),
        kind: Kind::Template(Arc::new(template.clone())),
    };
    let by_id = |id: &str| templates.iter().find(|template| template.id == id).map(row);
    let mut entries = Vec::new();
    if scratch {
        entries.push(fixed("New Scratch File", Kind::Scratch, ""));
    }
    entries.extend(by_id("php-file"));
    entries.push(fixed("PHP Class", Kind::PhpClass, "x.php"));
    entries.push(fixed("File", Kind::File, "x"));
    entries.push(fixed("Directory", Kind::Directory, ""));
    if agent {
        entries.push(fixed("Agent Session", Kind::AgentSession, ""));
    }
    entries.extend(LATER_BUILTINS.into_iter().filter_map(|id| by_id(id)));
    entries.extend(
        templates
            .iter()
            .filter(|template| {
                let id = template.id.as_str();
                id != "php-file" && !PHP_KINDS.contains(&id) && !LATER_BUILTINS.contains(&id)
            })
            .map(row),
    );
    entries
}

fn agent_session_action(cx: &App) -> Option<Box<dyn Action>> {
    cx.build_action("agent::NewThread", None).ok()
}

/// `None` when the scratches crate is not built in: the row is then hidden.
fn scratch_action(cx: &App) -> Option<Box<dyn Action>> {
    cx.build_action("zenith_scratches::NewScratchFile", None)
        .ok()
}

/// The directory a new file goes in: the selected directory, the parent of a
/// selected file, or the first project folder when nothing is selected.
fn target_dir(project: &Project, selected: Option<ProjectPath>, cx: &App) -> Option<ProjectPath> {
    if let Some(selected) = selected
        && let Some(entry) = project.entry_for_path(&selected, cx)
    {
        if entry.is_dir() {
            return Some(selected);
        }
        let parent = selected.path.parent()?;
        return Some(ProjectPath {
            worktree_id: selected.worktree_id,
            path: parent.into_arc(),
        });
    }
    let worktree = project.visible_worktrees(cx).next()?;
    Some(ProjectPath {
        worktree_id: worktree.read(cx).id(),
        path: RelPath::empty_arc(),
    })
}

/// [`target_dir`] for the project panel's selection.
fn selected_target_dir(workspace: &Workspace, cx: &App) -> Option<ProjectPath> {
    let selected = workspace
        .panel::<ProjectPanel>(cx)
        .and_then(|panel| panel.read(cx).selected_entry_project_path(cx));
    target_dir(workspace.project().read(cx), selected, cx)
}

/// `<worktree>/.zenith/templates`, only for a local worktree the user trusts:
/// its templates are the project's content, written into new files.
fn project_templates_dir(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> Option<PathBuf> {
    if !project.read(cx).is_local() {
        return None;
    }
    if let Some(store) = TrustedWorktrees::try_get_global(cx) {
        let worktree_store = project.read(cx).worktree_store();
        if !store.update(cx, |store, cx| {
            store.can_trust(&worktree_store, worktree_id, cx)
        }) {
            return None;
        }
    }
    let worktree = project.read(cx).worktree_for_id(worktree_id, cx)?;
    Some(
        worktree
            .read(cx)
            .abs_path()
            .join(".zenith")
            .join("templates"),
    )
}

/// Turns what was typed into the path to create, or says why it can't be.
/// The name is a trust boundary: it must stay inside `dir` and must not name
/// anything that already exists.
fn resolve(
    project: &Project,
    dir: &ProjectPath,
    name: &str,
    extension: &str,
    cx: &App,
) -> Result<ProjectPath, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name");
    }
    if name.chars().any(char::is_control) {
        return Err("The name has a control character");
    }
    if name.contains('\\') {
        return Err("The name can't contain a backslash");
    }
    if name.starts_with('/') {
        return Err("The name can't be an absolute path");
    }
    if name
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err("The name can't contain empty, '.' or '..' parts");
    }
    let suffix = format!(".{extension}");
    let mut file_name = name.to_string();
    if !name.to_lowercase().ends_with(&suffix) {
        file_name.push_str(&suffix);
    }
    let relative = RelPath::new(Path::new(&file_name), project.path_style(cx))
        .map_err(|_| "That isn't a valid file name")?;
    let path = dir.path.join(&relative).into_arc();
    // A bug above must never let the file land outside the chosen directory.
    if !path.starts_with(&dir.path) {
        return Err("The file must stay inside the directory");
    }
    let worktree = project
        .worktree_for_id(dir.worktree_id, cx)
        .ok_or("The project folder is gone")?;
    let worktree = worktree.read(cx);
    if worktree.entry_for_path(&path).is_some() {
        return Err("A file or folder with that name already exists");
    }
    for ancestor in path.ancestors().skip(1) {
        if let Some(entry) = worktree.entry_for_path(ancestor)
            && !entry.is_dir()
        {
            return Err("A file is in the way of that folder");
        }
    }
    Ok(ProjectPath {
        worktree_id: dir.worktree_id,
        path,
    })
}

/// The file name without the template's extension: `${NAME}`.
fn name_stem<'a>(file_name: &'a str, extension: &str) -> &'a str {
    let cut = file_name.len().saturating_sub(extension.len() + 1);
    match (file_name.get(..cut), file_name.get(cut..)) {
        (Some(stem), Some(suffix))
            if !stem.is_empty() && suffix.eq_ignore_ascii_case(&format!(".{extension}")) =>
        {
            stem
        }
        _ => file_name,
    }
}

/// Where `${CURSOR}` was, as a buffer point (columns are bytes).
fn point_at(text: &str, offset: usize) -> Point {
    let before = text.get(..offset).unwrap_or(text);
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    Point::new(
        before.matches('\n').count() as u32,
        (before.len() - line_start) as u32,
    )
}

/// Renders `content`, writes it to `target` (never over an existing file) and
/// opens it with the cursor at `${CURSOR}`. Errors show as a workspace notification.
fn create_file(
    workspace: WeakEntity<Workspace>,
    project: &Entity<Project>,
    target: ProjectPath,
    content: impl Future<Output = anyhow::Result<Rendered>> + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let (abs_path, fs, worktree) = {
        let project = project.read(cx);
        let abs_path = project
            .is_local()
            .then(|| project.absolute_path(&target, cx))
            .flatten();
        let Some(worktree) = project.worktree_for_id(target.worktree_id, cx) else {
            return;
        };
        (abs_path, project.fs().clone(), worktree)
    };
    let task: Task<anyhow::Result<()>> = window.spawn(cx, {
        let workspace = workspace.clone();
        async move |cx| {
            let Rendered { text, cursor } = content.await?;
            // The worktree snapshot misses excluded and unscanned ignored folders
            // (`vendor/`), and `create_entry` overwrites: ask the disk too.
            if let Some(abs_path) = abs_path
                && fs.metadata(&abs_path).await?.is_some()
            {
                anyhow::bail!("{} already exists", abs_path.display());
            }
            let path = target.path.clone();
            let bytes = text.as_bytes().to_vec();
            worktree
                .update(cx, |worktree, cx| {
                    worktree.create_entry(path, false, Some(bytes), cx)
                })
                .await?;
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(target, None, true, window, cx)
                })?
                .await?;
            if let (Some(offset), Some(editor)) = (cursor, item.downcast::<Editor>()) {
                let point = point_at(&text, offset);
                cx.update(|window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point(point, window, cx)
                    })
                })?;
            }
            Ok(())
        }
    });
    task.detach_and_notify_err(workspace, window, cx);
}

pub struct NewPopup {
    picker: Entity<Picker<NewDelegate>>,
    _load_templates: Task<()>,
}

impl ModalView for NewPopup {}
impl EventEmitter<DismissEvent> for NewPopup {}

impl Focusable for NewPopup {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for NewPopup {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ZenithNewPopup")
            .child(self.picker.clone())
    }
}

impl NewPopup {
    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let project = workspace.project().clone();
        let Some(dir) = selected_target_dir(workspace, cx) else {
            return;
        };
        let project_templates = project_templates_dir(&project, dir.worktree_id, cx);
        let fs = project.read(cx).fs().clone();
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let agent = agent_session_action(cx).is_some_and(|action| {
                !CommandPaletteFilter::try_global(cx).is_some_and(|f| f.is_hidden(&*action))
            });
            let scratch = scratch_action(cx).is_some();
            let delegate = NewDelegate::new(
                cx.entity().downgrade(),
                workspace_handle,
                project,
                dir,
                entries(&template::builtin(), agent, scratch),
            );
            let picker = cx.new(|cx| {
                Picker::uniform_list(delegate, window, cx)
                    .initial_width(rems(20.))
                    .show_scrollbar(false)
            });
            // The user's and the project's templates come from disk: rows for them
            // appear once read.
            let load = cx.spawn_in(window, async move |popup, cx| {
                let templates = template::load_all(fs, project_templates).await;
                popup
                    .update_in(cx, |popup, window, cx| {
                        popup.picker.update(cx, |picker, cx| {
                            picker.delegate.entries = entries(&templates, agent, scratch);
                            picker.refresh(window, cx);
                        })
                    })
                    .ok();
            });
            NewPopup {
                picker,
                _load_templates: load,
            }
        });
    }
}

pub struct NewDelegate {
    popup: WeakEntity<NewPopup>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    dir: ProjectPath,
    entries: Vec<Entry>,
    matches: Vec<StringMatch>,
    selected: usize,
    /// Set once a template is chosen: the query is then the file's name, then
    /// one answer per prompt of the template.
    naming: Option<(Entry, Arc<Template>)>,
    name: String,
    answers: Vec<String>,
    answer: String,
}

impl NewDelegate {
    fn new(
        popup: WeakEntity<NewPopup>,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        dir: ProjectPath,
        entries: Vec<Entry>,
    ) -> Self {
        let matches = all_matches(&entries);
        Self {
            popup,
            workspace,
            project,
            dir,
            entries,
            matches,
            selected: 0,
            naming: None,
            name: String::new(),
            answers: Vec::new(),
            answer: String::new(),
        }
    }

    fn dismiss(&self, cx: &mut App) {
        self.popup.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    /// File, Directory and Agent Session are the panel's and the agent's own
    /// actions: close the popup, focus the panel, then dispatch.
    fn hand_over(
        &self,
        action: Option<Box<dyn Action>>,
        focus_panel: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(action) = action else { return };
        let workspace = self.workspace.clone();
        self.dismiss(cx);
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                workspace.hide_modal(window, cx);
                if focus_panel {
                    workspace.focus_panel::<ProjectPanel>(window, cx);
                }
            });
            window.dispatch_action(action, cx);
        });
    }

    /// The prompt being answered, when past the name.
    fn current_prompt(&self) -> Option<&template::Prompt> {
        let (_, template) = self.naming.as_ref()?;
        template.prompts.get(self.answers.len().checked_sub(1)?)
    }

    fn create(
        &mut self,
        template: Arc<Template>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Ok(target) = resolve(
            self.project.read(cx),
            &self.dir,
            &self.name,
            &template.extension,
            cx,
        ) else {
            return;
        };
        let stem = name_stem(
            target.path.file_name().unwrap_or_default(),
            &template.extension,
        )
        .to_string();
        let answers = template
            .prompts
            .iter()
            .map(|prompt| prompt.name.clone())
            .zip(self.answers.iter().skip(1).cloned())
            .collect::<Vec<_>>();
        let fs = self.project.read(cx).fs().clone();
        let (root, abs_dir) = {
            let project = self.project.read(cx);
            let root = project
                .worktree_for_id(target.worktree_id, cx)
                .map(|worktree| worktree.read(cx).abs_path());
            let abs_dir = project
                .is_local()
                .then(|| project.absolute_path(&target, cx))
                .flatten()
                .and_then(|path| path.parent().map(Path::to_path_buf));
            (root, abs_dir)
        };
        let content = async move {
            let namespace = match (template.body.contains("${NAMESPACE}"), root, abs_dir) {
                (true, Some(root), Some(abs_dir)) => {
                    psr4::resolve(fs.as_ref(), &root, &abs_dir).await
                }
                _ => None,
            };
            let mut variables =
                template::standard_variables(&stem, namespace.as_deref().unwrap_or_default());
            variables.extend(answers);
            anyhow::Ok(template::render(&template.body, &variables))
        };
        self.dismiss(cx);
        create_file(
            self.workspace.clone(),
            &self.project,
            target,
            content,
            window,
            cx,
        );
    }
}

fn all_matches(entries: &[Entry]) -> Vec<StringMatch> {
    entries
        .iter()
        .enumerate()
        .map(|(candidate_id, entry)| StringMatch {
            candidate_id,
            score: 0.,
            positions: Vec::new(),
            string: entry.label.clone(),
        })
        .collect()
}

fn entry_icon(entry: &Entry, cx: &App) -> Icon {
    let (themed, fallback) = match entry.kind {
        Kind::File => (
            FileIcons::get_icon(Path::new(entry.icon_sample.as_ref()), cx),
            IconName::File,
        ),
        Kind::Directory => (None, IconName::Folder),
        Kind::AgentSession => (None, IconName::Thread),
        Kind::Scratch => (None, IconName::FileCode),
        Kind::PhpClass | Kind::Template(_) => (
            FileIcons::get_icon(Path::new(entry.icon_sample.as_ref()), cx),
            IconName::FileCode,
        ),
    };
    match themed {
        Some(path) => Icon::from_path(path).color(Color::Muted),
        None => Icon::new(fallback).color(Color::Muted),
    }
    .size(IconSize::Small)
}

impl PickerDelegate for NewDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "zenith new popup"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        match (&self.naming, self.current_prompt()) {
            (_, Some(prompt)) => prompt.label().into(),
            (Some((entry, _)), None) => format!("New {} name", entry.label).into(),
            (None, None) => "Search file type".into(),
        }
    }

    fn match_count(&self) -> usize {
        if self.naming.is_some() {
            1
        } else {
            self.matches.len()
        }
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.selected = 0;
        if self.naming.is_some() {
            if self.answers.is_empty() {
                self.name = query;
            } else {
                self.answer = query;
            }
            return Task::ready(());
        }
        let query = query.trim();
        self.matches = if query.is_empty() {
            all_matches(&self.entries)
        } else {
            let candidates = self
                .entries
                .iter()
                .enumerate()
                .map(|(ix, entry)| StringMatchCandidate::new(ix, entry.label.clone()))
                .collect::<Vec<_>>();
            match_strings(&candidates, query, StringMatchOptions::default())
        };
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some((_, template)) = self.naming.clone() {
            // `answers[0]` is the name, then one per prompt.
            if self.answers.is_empty() {
                if resolve(
                    self.project.read(cx),
                    &self.dir,
                    &self.name,
                    &template.extension,
                    cx,
                )
                .is_err()
                {
                    return;
                }
                self.answers.push(self.name.clone());
            } else {
                self.answers.push(std::mem::take(&mut self.answer));
            }
            let next_default = template
                .prompts
                .get(self.answers.len() - 1)
                .map(|prompt| prompt.default.clone());
            match next_default {
                Some(default) => {
                    self.answer = default.clone();
                    cx.defer_in(window, move |picker, window, cx| {
                        picker.set_query(&default, window, cx);
                        picker.refresh_placeholder(window, cx);
                    });
                    cx.notify();
                }
                None => self.create(template, window, cx),
            }
            return;
        }
        let Some(entry) = self
            .matches
            .get(self.selected)
            .and_then(|found| self.entries.get(found.candidate_id))
            .cloned()
        else {
            return;
        };
        match entry.kind.clone() {
            Kind::File => self.hand_over(Some(Box::new(NewFile)), true, window, cx),
            Kind::Directory => self.hand_over(Some(Box::new(NewDirectory)), true, window, cx),
            Kind::AgentSession => self.hand_over(agent_session_action(cx), true, window, cx),
            Kind::Scratch => self.hand_over(scratch_action(cx), false, window, cx),
            Kind::PhpClass => self.hand_over(Some(Box::new(NewPhpClass)), false, window, cx),
            Kind::Template(template) => {
                self.naming = Some((entry, template));
                self.name.clear();
                self.answers.clear();
                self.selected = 0;
                cx.defer_in(window, |picker, window, cx| {
                    picker.set_query("", window, cx);
                    picker.refresh_placeholder(window, cx);
                });
                cx.notify();
            }
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dismiss(cx);
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some("No matching file type".into())
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
        if let Some((entry, template)) = &self.naming {
            let (text, color) = if let Some(prompt) = self.current_prompt() {
                (
                    format!("{}: {}", prompt.label(), self.answer),
                    Color::Default,
                )
            } else {
                let project = self.project.read(cx);
                match resolve(project, &self.dir, &self.name, &template.extension, cx) {
                    Ok(path) => (
                        path.path.display(project.path_style(cx)).into_owned(),
                        Color::Default,
                    ),
                    Err(message) => (message.to_string(), Color::Muted),
                }
            };
            return Some(
                item.start_slot(entry_icon(entry, cx))
                    .child(Label::new(text).color(color)),
            );
        }
        let found = self.matches.get(ix)?;
        let entry = self.entries.get(found.candidate_id)?;
        Some(
            item.start_slot(entry_icon(entry, cx))
                .child(HighlightedLabel::new(
                    entry.label.clone(),
                    found.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project_panel::ProjectPanel;
    use serde_json::json;
    use settings::KeymapFile;
    use std::{cell::Cell, rc::Rc};
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    /// The project panel's chrome test declares a stand-in action by this exact name
    /// (project_panel.rs, `zenith_tests`); the header menu builds it by name too.
    #[gpui::test]
    fn test_new_from_template_action_name(cx: &mut TestAppContext) {
        assert_eq!(
            NewFromTemplate::name_for_type(),
            "zenith_templates::NewFromTemplate"
        );
        cx.update(|cx| {
            let built = cx
                .build_action("zenith_templates::NewFromTemplate", None)
                .expect("registered under its namespaced name");
            assert!(built.as_any().is::<NewFromTemplate>());
        });
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            workspace::init(app_state.clone(), cx);
            project_panel::init(cx);
            init(cx);
            cx.bind_keys(KeymapFile::load_panic_on_failure(
                r#"[{ "bindings": { "enter": "menu::Confirm" } }]"#,
                cx,
            ));
            app_state
        })
    }

    struct Setup {
        workspace: Entity<Workspace>,
        panel: Entity<ProjectPanel>,
        project: Entity<Project>,
        fs: Arc<dyn fs::Fs>,
    }

    async fn setup(cx: &mut TestAppContext) -> (Setup, &mut VisualTestContext) {
        setup_with(cx, true).await
    }

    async fn setup_with(
        cx: &mut TestAppContext,
        with_panel: bool,
    ) -> (Setup, &mut VisualTestContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "src": { "a.txt": "a", "Foo.php": "keep me", "Models": {} },
                    "composer.json": r#"{"autoload":{"psr-4":{"App\\": "src/"}}}"#,
                    "vendor": { "autoload.php": "do not touch" },
                    ".gitignore": "vendor/\n",
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let weak = workspace.downgrade();
        let load = workspace.update_in(cx, |_, window, cx| {
            window.spawn(cx, async move |cx| {
                ProjectPanel::load(weak, cx.clone()).await
            })
        });
        let panel = load.await.unwrap();
        if with_panel {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_panel(panel.clone(), window, cx)
            });
        }
        cx.run_until_parked();
        (
            Setup {
                workspace,
                panel,
                project,
                fs: app_state.fs.clone(),
            },
            cx,
        )
    }

    fn select(setup: &Setup, path: &str, cx: &mut VisualTestContext) {
        let project_path = setup.project.read_with(cx, |project, cx| {
            let worktree = project.visible_worktrees(cx).next().unwrap();
            ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::new_test(path).into_arc(),
            }
        });
        setup
            .panel
            .update(cx, |panel, cx| panel.select_path_for_test(project_path, cx));
    }

    fn picker(setup: &Setup, cx: &mut VisualTestContext) -> Entity<Picker<NewDelegate>> {
        setup.workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<NewPopup>(cx)
                .expect("the New popup is open")
                .read(cx)
                .picker
                .clone()
        })
    }

    fn open(setup: &Setup, cx: &mut VisualTestContext) -> Entity<Picker<NewDelegate>> {
        cx.dispatch_action(NewFromTemplate);
        cx.run_until_parked();
        picker(setup, cx)
    }

    fn confirm(cx: &mut VisualTestContext) {
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    /// Picks the first match for `kind_query`, then types `name` and confirms.
    fn create_named(setup: &Setup, kind_query: &str, name: &str, cx: &mut VisualTestContext) {
        open(setup, cx);
        cx.simulate_input(kind_query);
        cx.run_until_parked();
        confirm(cx);
        cx.simulate_input(name);
        cx.run_until_parked();
        confirm(cx);
    }

    async fn read(setup: &Setup, path: &str) -> Option<String> {
        setup.fs.load(path.as_ref()).await.ok()
    }

    #[gpui::test]
    async fn test_fuzzy_ts_finds_typescript_first(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        let picker = open(&setup, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(&*picker.delegate.entries[0].label, "New Scratch File");
            assert_eq!(&*picker.delegate.entries[1].label, "PHP File");
            assert_eq!(&*picker.delegate.entries[2].label, "PHP Class");
            // No agent crate here.
            assert_eq!(
                picker.delegate.matches.len(),
                entries(&template::builtin(), false, true).len()
            );
        });
        cx.simulate_input("ts");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let first = &picker.delegate.matches[0];
            assert_eq!(
                &*picker.delegate.entries[first.candidate_id].label,
                "TypeScript File"
            );
        });
    }

    #[gpui::test]
    async fn test_php_file_goes_in_the_selected_directory(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src", cx);
        create_named(&setup, "php", "Bar", cx);
        let content = read(&setup, path!("/root/src/Bar.php")).await;
        assert_eq!(content.as_deref(), Some("<?php\n\n"));
        assert!(read(&setup, path!("/root/Bar.php")).await.is_none());
    }

    #[gpui::test]
    async fn test_a_selected_file_uses_its_parent_directory(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src/a.txt", cx);
        create_named(&setup, "json", "data.json", cx);
        // The `.json` the user typed is not doubled.
        assert_eq!(
            read(&setup, path!("/root/src/data.json")).await.as_deref(),
            Some("{}\n")
        );
    }

    #[gpui::test]
    async fn test_no_selection_uses_the_project_root(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        create_named(&setup, "ts", "index", cx);
        assert!(read(&setup, path!("/root/index.ts")).await.is_some());
    }

    #[gpui::test]
    async fn test_an_existing_file_is_never_overwritten(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src", cx);
        create_named(&setup, "php", "Foo", cx);
        assert_eq!(
            read(&setup, path!("/root/src/Foo.php")).await.as_deref(),
            Some("keep me")
        );
        // Refused in the popup: it stays open for another name.
        picker(&setup, cx);
        // The snapshot check alone says no (the disk check would too).
        let result = setup.project.read_with(cx, |project, cx| {
            let worktree_id = project.visible_worktrees(cx).next().unwrap().read(cx).id();
            let dir = ProjectPath {
                worktree_id,
                path: RelPath::new_test("src").into_arc(),
            };
            resolve(project, &dir, "Foo", "php", cx)
        });
        assert!(result.is_err(), "{result:?}");
    }

    #[gpui::test]
    async fn test_an_ignored_unscanned_file_is_never_overwritten(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        // `vendor/` is git-ignored, so the worktree snapshot does not list its files.
        create_named(&setup, "php", "vendor/autoload", cx);
        assert_eq!(
            read(&setup, path!("/root/vendor/autoload.php"))
                .await
                .as_deref(),
            Some("do not touch")
        );
    }

    #[gpui::test]
    async fn test_names_that_escape_the_directory_are_refused(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src", cx);
        for name in [
            "../x",
            "a/../../x",
            "a/../x",
            "a/..",
            "/abs",
            "a\\..\\x",
            "a\0b",
            "",
            "  ",
            "a//b",
            "./x",
        ] {
            let result = setup.project.read_with(cx, |project, cx| {
                let dir = target_dir(
                    project,
                    Some(ProjectPath {
                        worktree_id: project.visible_worktrees(cx).next().unwrap().read(cx).id(),
                        path: RelPath::new_test("src").into_arc(),
                    }),
                    cx,
                )
                .unwrap();
                resolve(project, &dir, name, "php", cx)
            });
            assert!(result.is_err(), "{name:?} was accepted: {result:?}");
        }
        create_named(&setup, "php", "../x", cx);
        assert!(read(&setup, path!("/root/x.php")).await.is_none());
        assert!(read(&setup, path!("/root/src/x.php")).await.is_none());
        assert!(read(&setup, path!("/x.php")).await.is_none());
        picker(&setup, cx);
    }

    #[gpui::test]
    async fn test_file_and_directory_dispatch_the_project_panel_actions(cx: &mut TestAppContext) {
        // No panel in the workspace, so a global listener sees what was dispatched.
        let (setup, cx) = setup_with(cx, false).await;
        let (files, dirs) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        cx.update(|_, cx| {
            let files = files.clone();
            cx.on_action(move |_: &NewFile, _| files.set(files.get() + 1));
            let dirs = dirs.clone();
            cx.on_action(move |_: &NewDirectory, _| dirs.set(dirs.get() + 1));
        });
        for (query, expect_files, expect_dirs) in [("File", 1, 0), ("Directory", 1, 1)] {
            open(&setup, cx);
            cx.simulate_input(query);
            cx.run_until_parked();
            confirm(cx);
            assert_eq!(
                (files.get(), dirs.get()),
                (expect_files, expect_dirs),
                "{query}"
            );
        }
    }

    mod zenith_scratches {
        gpui::actions!(zenith_scratches, [NewScratchFile]);
    }

    #[gpui::test]
    async fn test_new_scratch_file_row_dispatches_the_scratches_action(cx: &mut TestAppContext) {
        let (setup, cx) = setup_with(cx, false).await;
        let count = Rc::new(Cell::new(0));
        cx.update(|_, cx| {
            let count = count.clone();
            cx.on_action(move |_: &zenith_scratches::NewScratchFile, _| count.set(count.get() + 1));
        });
        open(&setup, cx);
        cx.simulate_input("Scratch");
        cx.run_until_parked();
        confirm(cx);
        assert_eq!(count.get(), 1);
        assert!(
            setup
                .workspace
                .read_with(cx, |workspace, cx| workspace.active_modal::<NewPopup>(cx))
                .is_none(),
            "the popup closes before the scratch picker opens"
        );
    }

    #[test]
    fn test_scratch_row_is_hidden_when_its_action_is_not_built() {
        let labels = |agent, scratch| {
            entries(&template::builtin(), agent, scratch)
                .iter()
                .map(|entry| entry.label.to_string())
                .collect::<Vec<_>>()
        };
        let has = |labels: Vec<String>, label: &str| labels.iter().any(|known| known == label);
        assert!(has(labels(true, true), "New Scratch File"));
        assert!(!has(labels(true, false), "New Scratch File"));
        assert!(!has(labels(false, true), "Agent Session"));
        assert!(has(labels(false, true), "PHP File"));
        // The four PHP kinds are the dialog's, not rows.
        assert!(!has(labels(true, true), "PHP Interface"));
    }

    #[gpui::test]
    async fn test_file_entry_focuses_the_project_panel(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        open(&setup, cx);
        cx.simulate_input("Directory");
        cx.run_until_parked();
        confirm(cx);
        let focused = setup.panel.update_in(cx, |panel, window, cx| {
            panel.focus_handle(cx).contains_focused(window, cx)
        });
        assert!(focused, "the panel takes focus for its inline name editor");
    }

    fn cursor(setup: &Setup, cx: &mut VisualTestContext) -> Point {
        let editor = setup
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("a file is open");
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        })
    }

    fn dialog_open(setup: &Setup, cx: &mut VisualTestContext) -> bool {
        setup.workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<NewPhpClassDialog>(cx).is_some()
        })
    }

    #[gpui::test]
    async fn test_new_php_class_uses_psr4_and_places_the_cursor(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src/Models", cx);
        cx.dispatch_action(NewPhpClass);
        cx.run_until_parked();
        assert!(dialog_open(&setup, cx));
        cx.simulate_input("User");
        cx.run_until_parked();
        confirm(cx);
        let expected = "<?php\n\nnamespace App\\Models;\n\nclass User\n{\n    \n}\n";
        assert_eq!(
            read(&setup, path!("/root/src/Models/User.php"))
                .await
                .as_deref(),
            Some(expected)
        );
        assert!(!dialog_open(&setup, cx));
        assert_eq!(cursor(&setup, cx), Point::new(6, 4));
    }

    #[gpui::test]
    async fn test_new_php_class_refuses_bad_and_existing_names(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        select(&setup, "src", cx);
        cx.dispatch_action(NewPhpClass);
        cx.run_until_parked();
        cx.simulate_input("Foo");
        cx.run_until_parked();
        confirm(cx);
        assert!(dialog_open(&setup, cx), "refused: stays open");
        assert_eq!(
            read(&setup, path!("/root/src/Foo.php")).await.as_deref(),
            Some("keep me")
        );
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(!dialog_open(&setup, cx));
        cx.dispatch_action(NewPhpClass);
        cx.run_until_parked();
        cx.simulate_input("class");
        cx.run_until_parked();
        confirm(cx);
        assert!(read(&setup, path!("/root/src/class.php")).await.is_none());
    }

    #[gpui::test]
    async fn test_php_class_row_opens_the_dialog(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        open(&setup, cx);
        cx.simulate_input("PHP Class");
        cx.run_until_parked();
        confirm(cx);
        assert!(dialog_open(&setup, cx));
    }

    #[gpui::test]
    async fn test_user_and_project_templates_with_a_prompt(cx: &mut TestAppContext) {
        let (setup, cx) = setup(cx).await;
        setup
            .fs
            .as_fake()
            .insert_tree(
                template::user_dir(),
                json!({
                    "wp-plugin": {
                        "manifest.json": r#"{"name":"WP Plugin Main File","extension":"php",
                            "variables":[{"name":"TEXT_DOMAIN","label":"Text domain","default":"my-plugin"}]}"#,
                        "template.php": "<?php\n// ${NAME} ${TEXT_DOMAIN}\n${CURSOR}",
                    }
                }),
            )
            .await;
        setup
            .fs
            .as_fake()
            .insert_tree(
                path!("/root/.zenith/templates"),
                json!({
                    "json": {
                        "manifest.json": r#"{"name":"Project JSON","extension":"json"}"#,
                        "template.json": "[]\n",
                    }
                }),
            )
            .await;
        let picker = open(&setup, cx);
        picker.read_with(cx, |picker, _| {
            let labels = picker
                .delegate
                .entries
                .iter()
                .map(|entry| entry.label.to_string())
                .collect::<Vec<_>>();
            assert!(labels.contains(&"Project JSON".to_string()), "{labels:?}");
            assert!(!labels.contains(&"JSON File".to_string()), "{labels:?}");
            assert_eq!(
                labels.last().map(String::as_str),
                Some("WP Plugin Main File")
            );
        });
        cx.simulate_input("WP Plugin");
        cx.run_until_parked();
        confirm(cx);
        cx.simulate_input("plugin");
        cx.run_until_parked();
        confirm(cx);
        // The prompt starts with its default; accept it.
        confirm(cx);
        assert_eq!(
            read(&setup, path!("/root/plugin.php")).await.as_deref(),
            Some("<?php\n// plugin my-plugin\n")
        );
        assert_eq!(cursor(&setup, cx), Point::new(2, 0));
    }

    #[test]
    fn test_name_stem_and_point_at() {
        assert_eq!(name_stem("Foo.php", "php"), "Foo");
        assert_eq!(name_stem("Foo.PHP", "php"), "Foo");
        assert_eq!(name_stem("view.blade.php", "blade.php"), "view");
        assert_eq!(name_stem(".php", "php"), ".php");
        assert_eq!(name_stem("é.md", "md"), "é");
        assert_eq!(point_at("ab\ncd", 4), Point::new(1, 1));
        assert_eq!(point_at("ab", 0), Point::new(0, 0));
    }
}
