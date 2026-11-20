//! The project panel's "+" popup (plan 20): pick a file type, type a name, and the
//! file is created in the selected directory with that type's starter content.
//! File and Directory hand over to the project panel's own inline name editor.

use command_palette_hooks::CommandPaletteFilter;
use file_icons::FileIcons;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, StringMatchOptions, match_strings};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Task, WeakEntity, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use project::{Project, ProjectPath};
use project_panel::{NewDirectory, NewFile, ProjectPanel};
use std::{path::Path, sync::Arc};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::rel_path::RelPath;
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt as _};

actions!(
    zenith_templates,
    [
        /// Opens the New popup: pick a file type and name, and it is created in the
        /// directory selected in the project panel.
        NewFromTemplate
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &NewFromTemplate, window, cx| {
            NewPopup::open(workspace, window, cx)
        });
    })
    .detach();
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// `project_panel::NewFile`
    File,
    /// `project_panel::NewDirectory`
    Directory,
    /// `agent::NewThread`
    AgentSession,
    /// `zenith_scratches::NewScratchFile`, looked up by name: no crate dependency.
    Scratch,
    /// A named file with starter content.
    Template {
        extension: &'static str,
        content: &'static str,
    },
}

struct Entry {
    label: &'static str,
    kind: Kind,
    /// A file name the icon theme can match, for the row's icon.
    icon_sample: &'static str,
}

const HTML: &str = "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n    <meta charset=\"UTF-8\">\n    \
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n    \
<title>Document</title>\n</head>\n<body>\n\n</body>\n</html>\n";

const fn template(extension: &'static str, content: &'static str) -> Kind {
    Kind::Template { extension, content }
}

/// PhpStorm's order. Left out until they exist: PHP Class and its
/// siblings, HTTP Request, Data Source, PHP Test and the framework generators.
const ENTRIES: &[Entry] = &[
    Entry {
        label: "New Scratch File",
        kind: Kind::Scratch,
        icon_sample: "",
    },
    Entry {
        label: "PHP File",
        kind: template("php", "<?php\n\n"),
        icon_sample: "x.php",
    },
    Entry {
        label: "File",
        kind: Kind::File,
        icon_sample: "x",
    },
    Entry {
        label: "Directory",
        kind: Kind::Directory,
        icon_sample: "",
    },
    Entry {
        label: "Agent Session",
        kind: Kind::AgentSession,
        icon_sample: "",
    },
    Entry {
        label: "JavaScript File",
        kind: template("js", ""),
        icon_sample: "x.js",
    },
    Entry {
        label: "TypeScript File",
        kind: template("ts", ""),
        icon_sample: "x.ts",
    },
    Entry {
        label: "HTML File",
        kind: template("html", HTML),
        icon_sample: "x.html",
    },
    Entry {
        label: "Stylesheet",
        kind: template("css", ""),
        icon_sample: "x.css",
    },
    Entry {
        label: "SCSS File",
        kind: template("scss", ""),
        icon_sample: "x.scss",
    },
    Entry {
        label: "Less File",
        kind: template("less", ""),
        icon_sample: "x.less",
    },
    Entry {
        label: "JSON File",
        kind: template("json", "{}\n"),
        icon_sample: "x.json",
    },
    Entry {
        label: "Markdown File",
        kind: template("md", ""),
        icon_sample: "x.md",
    },
];

fn agent_session_action(cx: &App) -> Option<Box<dyn Action>> {
    cx.build_action("agent::NewThread", None).ok()
}

/// `None` when the scratches crate is not built in: the row is then hidden.
fn scratch_action(cx: &App) -> Option<Box<dyn Action>> {
    cx.build_action("zenith_scratches::NewScratchFile", None)
        .ok()
}

/// Entries whose action exists in this build.
fn visible_entries(agent: bool, scratch: bool) -> Vec<&'static Entry> {
    ENTRIES
        .iter()
        .filter(|entry| match entry.kind {
            Kind::AgentSession => agent,
            Kind::Scratch => scratch,
            _ => true,
        })
        .collect()
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

pub struct NewPopup {
    picker: Entity<Picker<NewDelegate>>,
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
        let selected = workspace
            .panel::<ProjectPanel>(cx)
            .and_then(|panel| panel.read(cx).selected_entry_project_path(cx));
        let Some(dir) = target_dir(project.read(cx), selected, cx) else {
            return;
        };
        let workspace_handle = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let ai = agent_session_action(cx).is_some_and(|action| {
                !CommandPaletteFilter::try_global(cx).is_some_and(|f| f.is_hidden(&*action))
            });
            let entries = visible_entries(ai, scratch_action(cx).is_some());
            let delegate = NewDelegate::new(
                cx.entity().downgrade(),
                workspace_handle,
                project,
                dir,
                entries,
            );
            let picker = cx.new(|cx| {
                Picker::uniform_list(delegate, window, cx)
                    .initial_width(rems(20.))
                    .show_scrollbar(false)
            });
            NewPopup { picker }
        });
    }
}

pub struct NewDelegate {
    popup: WeakEntity<NewPopup>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    dir: ProjectPath,
    entries: Vec<&'static Entry>,
    matches: Vec<StringMatch>,
    selected: usize,
    /// Set once a typed entry is chosen: the query is then the file's name.
    naming: Option<&'static Entry>,
    name: String,
}

impl NewDelegate {
    fn new(
        popup: WeakEntity<NewPopup>,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        dir: ProjectPath,
        entries: Vec<&'static Entry>,
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

    fn create(
        &mut self,
        extension: &'static str,
        content: &'static str,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Ok(target) = resolve(self.project.read(cx), &self.dir, &self.name, extension, cx)
        else {
            return;
        };
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let abs_path = project
            .read(cx)
            .is_local()
            .then(|| project.read(cx).absolute_path(&target, cx))
            .flatten();
        let fs = project.read(cx).fs().clone();
        let Some(worktree) = project.read(cx).worktree_for_id(target.worktree_id, cx) else {
            return;
        };
        self.dismiss(cx);
        let task: Task<anyhow::Result<()>> = window.spawn(cx, async move |cx| {
            // The worktree snapshot misses excluded and unscanned ignored folders
            // (`vendor/`), and `create_entry` overwrites: ask the disk too.
            if let Some(abs_path) = abs_path
                && fs.metadata(&abs_path).await?.is_some()
            {
                anyhow::bail!("{} already exists", abs_path.display());
            }
            let path = target.path.clone();
            worktree
                .update(cx, |worktree, cx| {
                    worktree.create_entry(path, false, Some(content.as_bytes().to_vec()), cx)
                })
                .await?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(target, None, true, window, cx)
                })?
                .await?;
            Ok(())
        });
        task.detach_and_notify_err(self.workspace.clone(), window, cx);
    }
}

fn all_matches(entries: &[&Entry]) -> Vec<StringMatch> {
    entries
        .iter()
        .enumerate()
        .map(|(candidate_id, entry)| StringMatch {
            candidate_id,
            score: 0.,
            positions: Vec::new(),
            string: entry.label.into(),
        })
        .collect()
}

fn entry_icon(entry: &Entry, cx: &App) -> Icon {
    let (themed, fallback) = match entry.kind {
        Kind::File => (
            FileIcons::get_icon(Path::new(entry.icon_sample), cx),
            IconName::File,
        ),
        Kind::Directory => (None, IconName::Folder),
        Kind::AgentSession => (None, IconName::Thread),
        Kind::Scratch => (None, IconName::FileCode),
        Kind::Template { .. } => (
            FileIcons::get_icon(Path::new(entry.icon_sample), cx),
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
        match self.naming {
            Some(entry) => format!("New {} name", entry.label).into(),
            None => "Search file type".into(),
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
            self.name = query;
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
                .map(|(ix, entry)| StringMatchCandidate::new(ix, entry.label))
                .collect::<Vec<_>>();
            match_strings(&candidates, query, StringMatchOptions::default())
        };
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(entry) = self.naming {
            if let Kind::Template { extension, content } = entry.kind {
                self.create(extension, content, window, cx);
            }
            return;
        }
        let Some(entry) = self
            .matches
            .get(self.selected)
            .and_then(|found| self.entries.get(found.candidate_id))
            .copied()
        else {
            return;
        };
        match entry.kind {
            Kind::File => self.hand_over(Some(Box::new(NewFile)), true, window, cx),
            Kind::Directory => self.hand_over(Some(Box::new(NewDirectory)), true, window, cx),
            Kind::AgentSession => self.hand_over(agent_session_action(cx), true, window, cx),
            Kind::Scratch => self.hand_over(scratch_action(cx), false, window, cx),
            Kind::Template { .. } => {
                self.naming = Some(entry);
                self.name.clear();
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
        if let Some(entry) = self.naming {
            let Kind::Template { extension, .. } = entry.kind else {
                return None;
            };
            let project = self.project.read(cx);
            let (text, color) = match resolve(project, &self.dir, &self.name, extension, cx) {
                Ok(path) => (
                    path.path.display(project.path_style(cx)).into_owned(),
                    Color::Default,
                ),
                Err(message) => (message.to_string(), Color::Muted),
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
                .child(HighlightedLabel::new(entry.label, found.positions.clone())),
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
                    "src": { "a.txt": "a", "Foo.php": "keep me" },
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
            assert_eq!(picker.delegate.entries[0].label, "New Scratch File");
            assert_eq!(picker.delegate.entries[1].label, "PHP File");
            assert_eq!(picker.delegate.matches.len(), ENTRIES.len() - 1); // no agent crate here
        });
        cx.simulate_input("ts");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            let first = &picker.delegate.matches[0];
            assert_eq!(
                picker.delegate.entries[first.candidate_id].label,
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
            visible_entries(agent, scratch)
                .iter()
                .map(|entry| entry.label)
                .collect::<Vec<_>>()
        };
        assert!(labels(true, true).contains(&"New Scratch File"));
        assert!(!labels(true, false).contains(&"New Scratch File"));
        assert!(!labels(false, true).contains(&"Agent Session"));
        assert!(labels(false, true).contains(&"PHP File"));
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
}
