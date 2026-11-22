//! The "Scratches and Consoles" node: the scratch store, an invisible worktree that
//! `zenith_scratches` keeps open, listed after "External Libraries" with its files under a
//! "Scratches" folder. It reuses the External Libraries machinery: a synthetic node row whose
//! expansion lives in the store worktree's expanded ids, and a row group of its own.

use super::*;
use zenith_external_libraries::{
    append_children_recursive, create_external_libraries_root_entry, is_section_expanded,
    section_worktree,
};

const NODE_LABEL: &str = "Scratches and Consoles";
const FOLDER_LABEL: &str = "Scratches";

/// What a scratch row's context menu offers, as `(label, action)`; `None` is a separator. The
/// store is not a project folder, so there is no "Remove from Project", and its root row
/// (the "Scratches" folder) can be neither renamed nor trashed.
pub(super) fn scratch_menu_items(is_root: bool) -> Vec<Option<(SharedString, Box<dyn Action>)>> {
    let entry = |label: &str, action: Box<dyn Action>| {
        Some((SharedString::from(label.to_string()), action))
    };
    let mut items = vec![
        entry(
            "New Scratch File…",
            Box::new(zenith_scratches::NewScratchFile),
        ),
        None,
    ];
    if !is_root {
        items.push(entry("Rename…", Box::new(Rename)));
        items.push(entry("Trash", Box::new(Trash { skip_prompt: false })));
        items.push(None);
    }
    items.push(entry(
        ui::utils::reveal_in_file_manager_label(false),
        Box::new(RevealInFileManager),
    ));
    items.push(entry(
        "Copy Path",
        Box::new(zed_actions::workspace::CopyPath),
    ));
    items
}

/// The node's context menu: it only creates scratches.
pub(super) fn scratch_node_menu(menu: ContextMenu) -> ContextMenu {
    menu.action(
        "New Scratch File…",
        Box::new(zenith_scratches::NewScratchFile),
    )
}

/// A scratch row's presentation: the node and the "Scratches" folder are labelled, every
/// row below the node sits one level in (the group has no project root above it).
fn decorate_scratch_row(
    entry: &Entry,
    filename: &str,
    icon: Option<SharedString>,
    depth: usize,
) -> (String, Option<SharedString>, usize) {
    if section_worktree(entry.id).is_some() {
        (
            NODE_LABEL.to_string(),
            Some(SharedString::from(IconName::FileCode.path())),
            0,
        )
    } else if entry.path.is_empty() {
        (FOLDER_LABEL.to_string(), icon, depth + 1)
    } else {
        (filename.to_string(), icon, depth + 1)
    }
}

impl ProjectPanel {
    fn zenith_scratch_worktree(&self, cx: &App) -> Option<Entity<Worktree>> {
        zenith_scratches::store_worktree(self.project.read(cx), cx)
    }

    /// Whether `worktree_id` is the scratch store shown under the node. Runs for every visible
    /// row every frame: visible worktrees (almost all rows) are rejected first.
    pub(super) fn zenith_is_scratch(&self, worktree_id: WorktreeId, cx: &App) -> bool {
        let project = self.project.read(cx);
        project.is_local()
            && project
                .worktree_for_id(worktree_id, cx)
                .is_some_and(|worktree| zenith_scratches::is_store_worktree(worktree.read(cx), cx))
    }

    /// Appends the node's rows after every project root and library row: the node, and while it
    /// is expanded the "Scratches" folder with its files. Runs on the freshly derived state.
    pub(super) fn zenith_scratch_rows(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.zenith_scratch_worktree(cx) else {
            return;
        };
        let worktree = worktree.read(cx);
        let worktree_id = worktree.id();
        // No root yet: the store is still being scanned, and its update repaints the panel.
        let Some(root) = worktree.root_entry() else {
            return;
        };
        let settings = ProjectPanelSettings::get_global(cx);
        let expanded_dir_ids = self
            .state
            .expanded_dir_ids
            .entry(worktree_id)
            .or_insert_with(|| vec![root.id]);
        let mut entries = vec![create_external_libraries_root_entry(worktree_id)];
        if is_section_expanded(worktree_id, expanded_dir_ids) {
            let mut files = Vec::new();
            if expanded_dir_ids.binary_search(&root.id).is_ok() {
                append_children_recursive(worktree, &root.path, expanded_dir_ids, &mut files);
            }
            files.retain(|entry| {
                is_entry_shown(entry, settings.hide_gitignore, settings.hide_hidden)
            });
            par_sort_worktree_entries(&mut files, settings.sort_mode, settings.sort_order);
            entries.push(GitEntry {
                entry: root.clone(),
                git_summary: GitSummary::default(),
            });
            entries.extend(files);
        }
        self.state.visible_entries.push(VisibleEntriesForWorktree {
            worktree_id,
            entries,
            index: OnceCell::new(),
        });
        self.state.max_width_item_index = None;
    }

    pub(super) fn zenith_scratch_decorate(
        &self,
        entry: &Entry,
        worktree_id: WorktreeId,
        filename: &str,
        icon: &Option<SharedString>,
        depth: usize,
        cx: &App,
    ) -> Option<(String, Option<SharedString>, usize)> {
        self.zenith_is_scratch(worktree_id, cx)
            .then(|| decorate_scratch_row(entry, filename, icon.clone(), depth))
    }

    /// The menu of a row inside the store, in place of the regular one.
    pub(super) fn zenith_scratch_menu(
        &self,
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ContextMenu>> {
        if !self.zenith_is_scratch(worktree_id, cx) {
            return None;
        }
        let is_root = self
            .project
            .read(cx)
            .worktree_for_id(worktree_id, cx)?
            .read(cx)
            .entry_for_id(entry_id)
            .is_some_and(|entry| entry.path.is_empty());
        let focus = self.focus_handle.clone();
        Some(ContextMenu::build(window, cx, move |menu, _, _| {
            scratch_menu_items(is_root)
                .into_iter()
                .fold(menu.context(focus), |menu, item| match item {
                    Some((label, action)) => menu.action(label, action),
                    None => menu.separator(),
                })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::settings_content::{ZenithScratchesSettingsContent, ZenithSettingsContent};
    use std::{cell::Cell, rc::Rc};
    use workspace::{AppState, MultiWorkspace};
    use zenith_external_libraries::section_id;

    struct Opened {
        panel: Entity<ProjectPanel>,
        workspace: Entity<Workspace>,
        cx: VisualTestContext,
        fs: Arc<FakeFs>,
        store: PathBuf,
        /// The project holds an invisible worktree weakly; `zenith_scratches` keeps this handle.
        _store_worktree: Entity<Worktree>,
    }

    fn composer_tree() -> serde_json::Value {
        json!({
            "composer.json": "{}",
            "vendor": { "autoload.php": "", "laravel": { "framework": { "a.php": "" } } },
            "src": { "main.php": "" }
        })
    }

    fn init_editor_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            workspace::init(app_state, cx);
        });
    }

    /// A project on `/site` with the scratch store (`rest-api.http`, `notes.md`) opened as an
    /// invisible worktree, as `zenith_scratches` does.
    async fn open(cx: &mut TestAppContext, site: serde_json::Value) -> Opened {
        init_editor_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", site).await;
        let store = cx.update(|cx| zenith_scratches::store_dir(cx));
        fs.insert_tree(
            &store,
            json!({ "rest-api.http": "GET /", "notes.md": "# notes" }),
        )
        .await;
        let project = Project::test(fs.clone(), [Path::new("/site")], cx).await;
        let (store_worktree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&store, false, cx)
            })
            .await
            .unwrap();
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, ProjectPanel::new);
        cx.run_until_parked();
        Opened {
            panel,
            workspace,
            cx,
            fs,
            store,
            _store_worktree: store_worktree,
        }
    }

    fn rows(opened: &mut Opened) -> Vec<(String, usize)> {
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            let mut rows = Vec::new();
            panel.for_each_visible_entry(0..200, window, cx, &mut |_, details, _, _| {
                rows.push((details.filename, details.depth));
            });
            rows
        })
    }

    fn names(opened: &mut Opened) -> Vec<String> {
        rows(opened).into_iter().map(|(name, _)| name).collect()
    }

    fn scratch_worktree_id(opened: &mut Opened) -> WorktreeId {
        opened.panel.read_with(&opened.cx, |panel, cx| {
            panel.zenith_scratch_worktree(cx).unwrap().read(cx).id()
        })
    }

    fn toggle(opened: &mut Opened, label: &str) {
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            let mut found = None;
            panel.for_each_visible_entry(0..200, window, cx, &mut |entry_id, details, _, _| {
                if details.filename == label {
                    found = Some(entry_id);
                }
            });
            panel.toggle_expanded(
                found.unwrap_or_else(|| panic!("no row {label:?}")),
                window,
                cx,
            )
        });
        opened.cx.run_until_parked();
    }

    fn select_scratch(opened: &mut Opened, name: &str) {
        let worktree_id = scratch_worktree_id(opened);
        opened.panel.update(&mut opened.cx, |panel, cx| {
            let entry = panel
                .project
                .read(cx)
                .worktree_for_id(worktree_id, cx)
                .unwrap()
                .read(cx)
                .entry_for_path(RelPath::from_unix_str(name).unwrap())
                .unwrap()
                .id;
            panel.selection = Some(SelectedEntry {
                worktree_id,
                entry_id: entry,
            });
        });
    }

    fn set_node_setting(opened: &mut Opened, enabled: bool) {
        opened.cx.update(|_, cx| set_node_setting_in(cx, enabled));
    }

    fn set_node_setting_in(cx: &mut App, enabled: bool) {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.zenith = Some(ZenithSettingsContent {
                    scratches: Some(ZenithScratchesSettingsContent {
                        project_panel_node: Some(enabled),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            });
        });
    }

    #[gpui::test]
    async fn test_node_comes_after_external_libraries(cx: &mut TestAppContext) {
        let mut opened = open(cx, composer_tree()).await;
        let collapsed = names(&mut opened);
        assert_eq!(
            collapsed[collapsed.len() - 2..],
            ["External Libraries", "Scratches and Consoles"],
            "{collapsed:?}"
        );

        toggle(&mut opened, "External Libraries");
        toggle(&mut opened, "Scratches and Consoles");
        let expanded = names(&mut opened);
        let position = |name: &str| {
            expanded
                .iter()
                .position(|n| n == name)
                .unwrap_or_else(|| panic!("no row {name:?} in {expanded:?}"))
        };
        assert!(position("Composer Packages (read-only)") < position("Scratches and Consoles"));
        assert_eq!(
            expanded[position("Scratches and Consoles")..],
            [
                "Scratches and Consoles",
                "Scratches",
                "notes.md",
                "rest-api.http"
            ],
            "the node is the last group and lists the scratches in the panel's sort order"
        );
    }

    #[gpui::test]
    async fn test_node_shows_without_libraries(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        let collapsed = rows(&mut opened);
        assert_eq!(
            collapsed.last(),
            Some(&("Scratches and Consoles".to_string(), 0)),
            "{collapsed:?}"
        );
        assert!(
            !collapsed
                .iter()
                .any(|(name, _)| name == "External Libraries"),
            "{collapsed:?}"
        );

        toggle(&mut opened, "Scratches and Consoles");
        let expanded = rows(&mut opened);
        assert_eq!(
            expanded[expanded.len() - 4..],
            [
                ("Scratches and Consoles".to_string(), 0),
                ("Scratches".to_string(), 1),
                ("notes.md".to_string(), 2),
                ("rest-api.http".to_string(), 2),
            ],
        );
    }

    #[gpui::test]
    async fn test_keyboard_expands_node_and_a_scratch_opens_in_the_editor(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        let worktree_id = scratch_worktree_id(&mut opened);
        opened.panel.update(&mut opened.cx, |panel, _| {
            panel.selection = Some(SelectedEntry {
                worktree_id,
                entry_id: section_id(worktree_id),
            });
        });
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.expand_selected_entry(&ExpandSelectedEntry, window, cx)
        });
        opened.cx.run_until_parked();
        assert!(names(&mut opened).contains(&"rest-api.http".to_string()));

        select_scratch(&mut opened, "rest-api.http");
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.open(&Open, window, cx)
        });
        opened.cx.run_until_parked();
        let opened_path = opened.workspace.read_with(&opened.cx, |workspace, cx| {
            let item = workspace.active_item(cx)?;
            let path = item.project_path(cx)?;
            Some((path.worktree_id, path.path.as_unix_str().to_string()))
        });
        assert_eq!(
            opened_path,
            Some((worktree_id, "rest-api.http".to_string()))
        );
    }

    #[gpui::test]
    async fn test_rename_and_trash_a_scratch(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        toggle(&mut opened, "Scratches and Consoles");
        select_scratch(&mut opened, "notes.md");
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.rename(&Rename, window, cx)
        });
        let rename = opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel
                .filename_editor
                .update(cx, |editor, cx| editor.set_text("todo.md", window, cx));
            panel
                .confirm_edit(true, window, cx)
                .expect("a rename starts")
        });
        rename.await.unwrap();
        opened.cx.run_until_parked();
        assert_eq!(
            names(&mut opened)[names(&mut opened).len() - 3..],
            ["Scratches", "rest-api.http", "todo.md"],
        );
        assert!(opened.fs.is_file(&opened.store.join("todo.md")).await);
        assert!(!opened.fs.is_file(&opened.store.join("notes.md")).await);

        select_scratch(&mut opened, "todo.md");
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.trash(&Trash { skip_prompt: true }, window, cx)
        });
        opened.cx.run_until_parked();
        assert!(!opened.fs.is_file(&opened.store.join("todo.md")).await);
        assert_eq!(
            names(&mut opened)[names(&mut opened).len() - 2..],
            ["Scratches", "rest-api.http"],
        );
    }

    #[gpui::test]
    async fn test_scratch_rows_have_no_drag_and_drop(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        toggle(&mut opened, "Scratches and Consoles");
        let allowed = opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            let mut rows = Vec::new();
            panel.for_each_visible_entry(0..200, window, cx, &mut |entry_id, details, _, _| {
                rows.push((entry_id, details));
            });
            let settings = *ProjectPanelSettings::get_global(cx);
            assert!(settings.drag_and_drop, "drag and drop is on by default");
            rows.into_iter()
                .map(|(entry_id, details)| {
                    let allowed = panel.zenith_dnd(settings, entry_id, &details, cx);
                    (details.filename, allowed.drag_and_drop)
                })
                .collect::<Vec<_>>()
        });
        for (name, drag_and_drop) in &allowed {
            let is_scratch_row = [
                "Scratches and Consoles",
                "Scratches",
                "notes.md",
                "rest-api.http",
            ]
            .contains(&name.as_str());
            assert_eq!(*drag_and_drop, !is_scratch_row, "{name}: {allowed:?}");
        }
        assert!(allowed.iter().any(|(name, _)| name == "src"));
    }

    #[gpui::test]
    async fn test_scratch_menu_has_no_remove_from_project(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        toggle(&mut opened, "Scratches and Consoles");
        let worktree_id = scratch_worktree_id(&mut opened);
        let (file, root) = opened.panel.read_with(&opened.cx, |panel, cx| {
            let worktree = panel
                .project
                .read(cx)
                .worktree_for_id(worktree_id, cx)
                .unwrap();
            let worktree = worktree.read(cx);
            (
                worktree
                    .entry_for_path(RelPath::from_unix_str("notes.md").unwrap())
                    .unwrap()
                    .id,
                worktree.root_entry().unwrap().id,
            )
        });
        let replaced = opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            let original = ContextMenu::build(window, cx, |menu, _, _| menu);
            [file, root].map(|entry_id| {
                panel.zenith_menu(worktree_id, entry_id, original.clone(), window, cx) != original
            })
        });
        assert_eq!(replaced, [true, true], "scratch rows get their own menu");

        let remove = RemoveFromProject.name();
        for is_root in [false, true] {
            let items: Vec<_> = scratch_menu_items(is_root).into_iter().flatten().collect();
            let labels: Vec<_> = items.iter().map(|(label, _)| label.to_string()).collect();
            assert!(
                labels.iter().all(|label| label != "Remove from Project")
                    && items.iter().all(|(_, action)| action.name() != remove),
                "{labels:?}"
            );
            assert_eq!(
                labels.first().map(String::as_str),
                Some("New Scratch File…")
            );
            assert!(labels.contains(&"Copy Path".to_string()));
            assert_eq!(labels.contains(&"Rename…".to_string()), !is_root);
            assert_eq!(labels.contains(&"Trash".to_string()), !is_root);
        }
    }

    #[gpui::test]
    async fn test_node_menu_creates_scratches(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        let worktree_id = scratch_worktree_id(&mut opened);
        let created = Rc::new(Cell::new(0));
        opened.cx.update(|_, cx| {
            let created = created.clone();
            cx.on_action(move |_: &zenith_scratches::NewScratchFile, _| {
                created.set(created.get() + 1)
            });
        });
        let menu = opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.deploy_context_menu(
                ContextMenuPlacement::AtMouse(Default::default()),
                section_id(worktree_id),
                window,
                cx,
            );
            panel
                .context_menu
                .as_ref()
                .expect("the node has a menu")
                .menu
                .clone()
        });
        menu.update_in(&mut opened.cx, |menu, window, cx| {
            menu.select_first(&menu::SelectFirst, window, cx);
            menu.confirm(&menu::Confirm, window, cx);
        });
        opened.cx.run_until_parked();
        assert_eq!(created.get(), 1, "the first entry is New Scratch File…");
    }

    #[gpui::test]
    async fn test_setting_off_hides_the_node(cx: &mut TestAppContext) {
        let mut opened = open(cx, json!({ "src": { "main.php": "" } })).await;
        assert!(names(&mut opened).contains(&"Scratches and Consoles".to_string()));

        set_node_setting(&mut opened, false);
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.update_visible_entries(None, false, false, window, cx)
        });
        opened.cx.run_until_parked();
        let hidden = names(&mut opened);
        assert!(
            !hidden.iter().any(|name| name.starts_with("Scratches")),
            "{hidden:?}"
        );

        set_node_setting(&mut opened, true);
        opened.panel.update_in(&mut opened.cx, |panel, window, cx| {
            panel.update_visible_entries(None, false, false, window, cx)
        });
        opened.cx.run_until_parked();
        assert!(names(&mut opened).contains(&"Scratches and Consoles".to_string()));
    }
}
