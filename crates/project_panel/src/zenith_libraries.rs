//! Glue between the project panel and `zenith_external_libraries`: the "External Libraries"
//! node, its keyboard handling, row decoration and context menu. The upstream file only calls
//! into this module; all panel-state access for the feature lives here.

use super::*;
use zenith_external_libraries::{
    RowDecoration, RowKind, create_external_libraries_root_entry, decorate_row, detect_libraries,
    is_enabled, is_library_worktree, is_section_expanded, is_under_library, may_be_library_path,
    row_kind, section_worktree, set_section_expanded, take_libraries,
};

impl ProjectPanel {
    /// Expands (`Some(true)`), collapses (`Some(false)`) or toggles (`None`) an "External
    /// Libraries" row: the row `entry` names (mouse) or, without one, the selected row (keyboard).
    /// Does nothing for any other row.
    pub(super) fn zenith_section(
        &mut self,
        entry: Option<ProjectEntryId>,
        expand: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match entry {
            Some(entry_id) => section_worktree(entry_id).map(|worktree_id| (worktree_id, entry_id)),
            None => self.selection.and_then(|selection| {
                (section_worktree(selection.entry_id) == Some(selection.worktree_id))
                    .then_some((selection.worktree_id, selection.entry_id))
            }),
        };
        let Some((worktree_id, entry_id)) = target else {
            return;
        };
        // The ids exist once the worktree's first update marked its root expanded.
        let Some(expanded_dir_ids) = self.state.expanded_dir_ids.get_mut(&worktree_id) else {
            return;
        };
        if set_section_expanded(worktree_id, expanded_dir_ids, expand) {
            self.update_visible_entries(Some((worktree_id, entry_id)), false, false, window, cx);
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// Moves every library row below all project roots, under one "External Libraries" node:
    /// each worktree's library folders and every user-added library worktree. The rows keep
    /// their worktree's group (several groups may share a worktree id), the node row heads the
    /// first one and its expansion is that worktree's. Runs on the freshly derived state, before
    /// the panel repaints.
    pub(super) fn zenith_libraries(&mut self, cx: &mut Context<Self>) {
        if !is_enabled(cx) {
            return;
        }
        let project = self.project.read(cx);
        let mut libraries: Vec<(WorktreeId, Vec<GitEntry>)> = Vec::new();
        for visible in self.state.visible_entries.iter_mut() {
            let Some(worktree) = project.worktree_for_id(visible.worktree_id, cx) else {
                continue;
            };
            let worktree = worktree.read(cx);
            let rows = if is_library_worktree(&worktree.abs_path(), cx) {
                Some(std::mem::take(&mut visible.entries))
            } else {
                let expanded_dir_ids = self
                    .state
                    .expanded_dir_ids
                    .get(&visible.worktree_id)
                    .map_or(&[][..], Vec::as_slice);
                take_libraries(worktree, expanded_dir_ids, &mut visible.entries)
            };
            if let Some(rows) = rows {
                visible.index = OnceCell::new();
                libraries.push((visible.worktree_id, rows));
            }
        }
        let Some(&(owner, _)) = libraries.first() else {
            return;
        };
        self.state
            .visible_entries
            .retain(|visible| !visible.entries.is_empty());
        let expanded = self
            .state
            .expanded_dir_ids
            .get(&owner)
            .is_some_and(|ids| is_section_expanded(owner, ids));
        if !expanded {
            libraries = vec![(owner, Vec::new())];
        }
        for (ix, (worktree_id, mut entries)) in libraries.into_iter().enumerate() {
            if ix == 0 {
                entries.insert(0, create_external_libraries_root_entry(owner));
            }
            if !entries.is_empty() {
                self.state.visible_entries.push(VisibleEntriesForWorktree {
                    worktree_id,
                    entries,
                    index: OnceCell::new(),
                });
            }
        }
        // The widest-item index was computed before rows were moved.
        self.state.max_width_item_index = None;
    }

    /// The project name a library root of `worktree_id` is labelled with: only when more than
    /// one project worktree has libraries, so the node tells them apart.
    fn zenith_project_label(&self, worktree_id: WorktreeId, cx: &App) -> Option<String> {
        let project = self.project.read(cx);
        let with_libraries = project
            .visible_worktrees(cx)
            .filter(|worktree| {
                let worktree = worktree.read(cx);
                !is_library_worktree(&worktree.abs_path(), cx)
                    && !detect_libraries(worktree).is_empty()
            })
            .count();
        if with_libraries < 2 {
            return None;
        }
        let worktree = project.worktree_for_id(worktree_id, cx)?;
        Some(worktree.read(cx).root_name_str().to_string())
    }

    /// Whether `worktree_id` is a user-added library worktree (read-only, no project root).
    fn zenith_library_worktree(&self, worktree_id: WorktreeId, cx: &App) -> bool {
        is_enabled(cx)
            && self
                .project
                .read(cx)
                .worktree_for_id(worktree_id, cx)
                .is_some_and(|worktree| is_library_worktree(&worktree.read(cx).abs_path(), cx))
    }

    /// The depth the row is drawn at, for the indent guides and sticky rows: they are computed
    /// from the raw entries and would otherwise draw (and capture clicks) over the chevrons of
    /// rows this module moved or indented.
    pub(super) fn zenith_depth(&self, entry: &Entry, depth: usize, cx: &App) -> usize {
        let worktree_id = section_worktree(entry.id)
            .or_else(|| self.project.read(cx).worktree_id_for_entry(entry.id, cx));
        worktree_id.map_or(depth, |worktree_id| {
            self.zenith_decorate(entry, worktree_id, String::new(), None, depth, cx)
                .2
        })
    }

    /// Context menu of the "External Libraries" row (a synthetic entry, so the regular menu has
    /// nothing for it): Add Library Path…. Does nothing for any other row.
    pub(super) fn zenith_section_menu(
        &mut self,
        placement: ContextMenuPlacement,
        entry_id: ProjectEntryId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree_id) = section_worktree(entry_id) else {
            return;
        };
        self.selection = Some(SelectedEntry {
            worktree_id,
            entry_id,
        });
        let focus = self.focus_handle.clone();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(focus).action(
                "Add Library Path…",
                Box::new(zenith_external_libraries::AddLibraryPath),
            )
        });
        let (position, anchor) = match placement {
            ContextMenuPlacement::AtMouse(mouse_position) => (mouse_position, None),
            ContextMenuPlacement::AtEntry => match self.scroll_selection_into_view() {
                Some(entry_bounds) => {
                    let (position, anchor) =
                        Self::context_menu_placement_for_entry(entry_bounds, window, cx);
                    let offset = if anchor.is_bottom() {
                        -CONTEXT_MENU_KEYBOARD_OFFSET
                    } else {
                        CONTEXT_MENU_KEYBOARD_OFFSET
                    };
                    (position + offset, Some(anchor))
                }
                None => (window.mouse_position(), None),
            },
        };
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some(DeployedContextMenu {
            menu: context_menu,
            position,
            anchor,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Library rows indent one level under the node and carry its labels. Runs for every visible
    /// row, so it bails out before touching the worktree unless the row can be a library row.
    pub(super) fn zenith_decorate(
        &self,
        entry: &Entry,
        worktree_id: WorktreeId,
        filename: String,
        icon: Option<SharedString>,
        depth: usize,
        cx: &App,
    ) -> (String, Option<SharedString>, usize) {
        // A user-added library worktree sits one level under its node, root row labelled.
        if section_worktree(entry.id).is_none() && self.zenith_library_worktree(worktree_id, cx) {
            return if entry.path.is_empty() {
                (
                    format!("{filename} (read-only)"),
                    Some(SharedString::from(IconName::BookCopy.path())),
                    depth + 1,
                )
            } else {
                (filename, icon, depth + 1)
            };
        }
        let project = self.project.read(cx);
        let worktree = match section_worktree(entry.id) {
            Some(worktree_id) => project.worktree_for_id(worktree_id, cx),
            None if may_be_library_path(&entry.path) => project.worktree_for_entry(entry.id, cx),
            None => None,
        };
        let kind = worktree
            .filter(|_| is_enabled(cx))
            .and_then(|worktree| row_kind(worktree.read(cx), entry.id, &entry.path));
        let Some(kind) = kind else {
            return (filename, icon, depth);
        };
        let project_label = match kind {
            RowKind::Library { .. } => self.zenith_project_label(worktree_id, cx),
            _ => None,
        };
        let row = decorate_row(
            kind,
            project_label.as_deref(),
            RowDecoration {
                filename,
                icon,
                depth,
            },
        );
        (row.filename, row.icon, row.depth)
    }

    /// `settings` with drag-and-drop off for the "External Libraries" node and every row inside
    /// a library: they are read-only, so nothing is dragged from them or dropped onto them. The
    /// row render reads `drag_and_drop` for both the row handlers and the folded-path separators.
    /// Runs for every visible row every frame, so ordinary rows stop at the path check.
    pub(super) fn zenith_dnd(
        &self,
        mut settings: ProjectPanelSettings,
        entry_id: ProjectEntryId,
        details: &EntryDetails,
        cx: &App,
    ) -> ProjectPanelSettings {
        if settings.drag_and_drop && is_enabled(cx) {
            let is_library_row = self.zenith_library_worktree(details.worktree_id, cx)
                || ((section_worktree(entry_id).is_some() || may_be_library_path(&details.path))
                    && self
                        .project
                        .read(cx)
                        .worktree_for_id(details.worktree_id, cx)
                        .is_some_and(|worktree| {
                            row_kind(worktree.read(cx), entry_id, &details.path).is_some()
                        }));
            settings.drag_and_drop &= !is_library_row;
        }
        settings
    }

    /// For a file or folder inside a library: the read-only menu plus Local History, Reveal,
    /// Copy Path and Copy Relative Path, in place of `menu`. Other entries keep `menu`.
    pub(super) fn zenith_menu(
        &self,
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
        menu: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        if !is_enabled(cx) {
            return menu;
        }
        let project = self.project.read(cx);
        let Some(worktree) = project.worktree_for_id(worktree_id, cx) else {
            return menu;
        };
        let worktree = worktree.read(cx);
        let Some(entry) = worktree.entry_for_id(entry_id) else {
            return menu;
        };
        if !is_library_worktree(&worktree.abs_path(), cx)
            && !is_under_library(&detect_libraries(&worktree.snapshot()), &entry.path)
        {
            return menu;
        }
        let is_dir = entry.is_dir();
        let file_name = Path::new(entry_file_name(entry, worktree.root_name()));
        let is_markdown =
            !is_dir && MarkdownPreviewView::is_markdown_path(file_name, project.languages());
        let is_svg = !is_dir && SvgPreviewView::is_svg_path(file_name);
        let is_remote = project.is_remote();
        let is_local = project.is_local() || project.is_via_wsl_with_host_interop(cx);
        let focus = self.focus_handle.clone();
        ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(focus)
                .when(is_markdown, |menu| {
                    menu.action("Open Markdown Preview", Box::new(OpenMarkdownPreview))
                })
                .when(is_svg, |menu| {
                    menu.action("Open SVG Preview", Box::new(OpenSvgPreview))
                })
                .when(is_dir, |menu| {
                    menu.action("Search Inside", Box::new(NewSearchInDirectory))
                })
                .separator()
                .when(!is_dir && is_local, |menu| {
                    menu.action(
                        "Local History…",
                        Box::new(zed_actions::zenith_local_history::ShowFileHistory),
                    )
                })
                .when(is_local, |menu| {
                    menu.action(
                        ui::utils::reveal_in_file_manager_label(is_remote),
                        Box::new(RevealInFileManager),
                    )
                })
                .action("Copy Path", Box::new(zed_actions::workspace::CopyPath))
                .action(
                    "Copy Relative Path",
                    Box::new(zed_actions::workspace::CopyRelativePath),
                )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_panel_tests::init_test;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::settings_content::{
        ZenithExternalLibrariesSettingsContent, ZenithSettingsContent,
    };
    use workspace::MultiWorkspace;
    use zenith_external_libraries::section_id;

    async fn open_panel(
        cx: &mut TestAppContext,
        fs: Arc<FakeFs>,
        roots: &[&str],
    ) -> (Entity<ProjectPanel>, VisualTestContext) {
        init_test(cx);
        open_panel_inited(cx, fs, roots).await
    }

    async fn open_panel_inited(
        cx: &mut TestAppContext,
        fs: Arc<FakeFs>,
        roots: &[&str],
    ) -> (Entity<ProjectPanel>, VisualTestContext) {
        let roots: Vec<&Path> = roots.iter().map(Path::new).collect();
        let project = Project::test(fs, roots, cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(cx, ProjectPanel::new);
        cx.run_until_parked();
        (panel, cx.clone())
    }

    fn rows(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) -> Vec<(String, usize)> {
        panel.update_in(cx, |panel, window, cx| {
            let mut rows = Vec::new();
            panel.for_each_visible_entry(0..200, window, cx, &mut |_, details, _, _| {
                rows.push((details.filename, details.depth));
            });
            rows
        })
    }

    fn names(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) -> Vec<String> {
        rows(panel, cx).into_iter().map(|(name, _)| name).collect()
    }

    fn composer_tree() -> serde_json::Value {
        json!({
            "composer.json": "{}",
            "vendor": { "autoload.php": "", "laravel": { "framework": { "a.php": "" } } },
            "src": { "main.php": "" }
        })
    }

    fn first_worktree_id(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) -> WorktreeId {
        panel.read_with(cx, |panel, cx| {
            panel
                .project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .id()
        })
    }

    fn select_section_of(panel: &Entity<ProjectPanel>, id: WorktreeId, cx: &mut VisualTestContext) {
        panel.update(cx, |panel, _| {
            panel.selection = Some(SelectedEntry {
                worktree_id: id,
                entry_id: section_id(id),
            });
        });
    }

    #[gpui::test]
    async fn test_section_replaces_vendor_and_keyboard_toggles(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/site"]).await;
        let cx = &mut cx;

        let before = names(&panel, cx);
        assert!(
            !before
                .iter()
                .any(|name| name.contains("vendor") || name.contains("Composer"))
        );
        assert_eq!(
            before.last().map(String::as_str),
            Some("External Libraries")
        );

        let id = first_worktree_id(&panel, cx);
        select_section_of(&panel, id, cx);

        // Right expands.
        panel.update_in(cx, |panel, window, cx| {
            panel.expand_selected_entry(&ExpandSelectedEntry, window, cx)
        });
        cx.run_until_parked();
        let expanded = rows(&panel, cx);
        let section_ix = expanded
            .iter()
            .position(|(name, _)| name == "External Libraries")
            .unwrap();
        let (label, depth) = &expanded[section_ix + 1];
        assert_eq!(label, "Composer Packages (read-only)");
        assert_eq!(*depth, 1, "libraries are indented under the node");

        // Left collapses.
        panel.update_in(cx, |panel, window, cx| {
            panel.collapse_selected_entry(&CollapseSelectedEntry, window, cx)
        });
        cx.run_until_parked();
        assert!(
            !names(&panel, cx)
                .iter()
                .any(|name| name.starts_with("Composer"))
        );

        // Enter toggles open, and a second Enter closes.
        panel.update_in(cx, |panel, window, cx| panel.open(&Open, window, cx));
        cx.run_until_parked();
        assert!(
            names(&panel, cx)
                .iter()
                .any(|name| name.starts_with("Composer"))
        );
        panel.update_in(cx, |panel, window, cx| panel.open(&Open, window, cx));
        cx.run_until_parked();
        assert!(
            !names(&panel, cx)
                .iter()
                .any(|name| name.starts_with("Composer"))
        );
    }

    #[gpui::test]
    async fn test_mouse_toggle_goes_through_toggle_expanded(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/site"]).await;
        let cx = &mut cx;
        let id = first_worktree_id(&panel, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_expanded(section_id(id), window, cx)
        });
        cx.run_until_parked();
        assert!(
            names(&panel, cx)
                .iter()
                .any(|name| name.starts_with("Composer"))
        );
    }

    #[gpui::test]
    async fn test_nested_library_children_are_indented(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/site"]).await;
        let cx = &mut cx;
        let id = first_worktree_id(&panel, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_expanded(section_id(id), window, cx);
        });
        cx.run_until_parked();
        // Expand the vendor folder itself.
        panel.update_in(cx, |panel, window, cx| {
            let vendor = panel
                .project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .entry_for_path(RelPath::from_unix_str("vendor").unwrap())
                .unwrap()
                .id;
            panel.toggle_expanded(vendor, window, cx);
        });
        cx.run_until_parked();
        let rows = rows(&panel, cx);
        let depth_of = |name: &str| rows.iter().find(|(n, _)| n == name).map(|(_, d)| *d);
        assert_eq!(depth_of("External Libraries"), Some(0));
        assert_eq!(depth_of("Composer Packages (read-only)"), Some(1));
        assert_eq!(depth_of("autoload.php"), Some(2));
    }

    #[gpui::test]
    async fn test_go_vendor_stays_in_the_tree(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/go",
            json!({ "go.mod": "", "vendor": { "modules.txt": "" }, "main.go": "" }),
        )
        .await;
        let (panel, mut cx) = open_panel(cx, fs, &["/go"]).await;
        let names = names(&panel, &mut cx);
        assert!(names.contains(&"vendor".to_string()), "{names:?}");
        assert!(!names.contains(&"External Libraries".to_string()));
    }

    #[gpui::test]
    async fn test_disabled_setting_keeps_the_plain_tree(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(ZenithSettingsContent {
                        external_libraries: Some(ZenithExternalLibrariesSettingsContent {
                            enabled: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                });
            });
        });
        let (panel, mut cx) = open_panel_inited(cx, fs, &["/site"]).await;
        let names = names(&panel, &mut cx);
        assert!(names.contains(&"vendor".to_string()), "{names:?}");
        assert!(!names.contains(&"External Libraries".to_string()));
    }

    #[gpui::test]
    async fn test_one_node_for_all_projects(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/one", composer_tree()).await;
        fs.insert_tree("/two", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/one", "/two"]).await;
        let cx = &mut cx;

        let collapsed = names(&panel, cx);
        assert_eq!(count(&collapsed, "External Libraries"), 1, "{collapsed:?}");
        assert_eq!(
            collapsed.last().map(String::as_str),
            Some("External Libraries"),
            "the node follows every project root: {collapsed:?}"
        );

        toggle_node(&panel, cx);
        let expanded = rows(&panel, cx);
        assert_eq!(
            expanded[expanded.len() - 3..].to_vec(),
            vec![
                ("External Libraries".to_string(), 0),
                ("Composer Packages (one)".to_string(), 1),
                ("Composer Packages (two)".to_string(), 1),
            ],
            "{expanded:?}"
        );

        // Keyboard navigation walks from the node into the moved library rows.
        let owner = first_worktree_id(&panel, cx);
        select_section_of(&panel, owner, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.select_next(&SelectNext, window, cx);
            panel.select_next(&SelectNext, window, cx);
        });
        let selected = panel.update_in(cx, |panel, window, cx| {
            let mut selected = Vec::new();
            panel.for_each_visible_entry(0..200, window, cx, &mut |_, details, _, _| {
                if details.is_selected {
                    selected.push(details.filename);
                }
            });
            selected
        });
        assert_eq!(selected, vec!["Composer Packages (two)".to_string()]);
    }

    #[gpui::test]
    async fn test_library_menu_only_for_library_entries(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/site"]).await;
        let cx = &mut cx;
        let (library, ordinary) = panel.update_in(cx, |panel, window, cx| {
            let worktree = panel
                .project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx);
            let worktree_id = worktree.id();
            let find = |path: &str| {
                worktree
                    .entry_for_path(RelPath::from_unix_str(path).unwrap())
                    .unwrap()
                    .id
            };
            let (vendor_file, source) = (find("vendor/autoload.php"), find("src/main.php"));
            let original = ContextMenu::build(window, cx, |menu, _, _| menu);
            let replaced =
                panel.zenith_menu(worktree_id, vendor_file, original.clone(), window, cx);
            let kept = panel.zenith_menu(worktree_id, source, original.clone(), window, cx);
            (replaced != original, kept == original)
        });
        assert!(library, "library files get the library menu");
        assert!(ordinary, "ordinary files keep the regular menu");
    }

    #[gpui::test]
    async fn test_library_rows_have_no_drag_and_drop(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/site"]).await;
        let cx = &mut cx;
        let id = first_worktree_id(&panel, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_expanded(section_id(id), window, cx);
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| {
            let vendor = panel
                .project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .entry_for_path(RelPath::from_unix_str("vendor").unwrap())
                .unwrap()
                .id;
            panel.toggle_expanded(vendor, window, cx);
        });
        cx.run_until_parked();

        let drag_and_drop = panel.update_in(cx, |panel, window, cx| {
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
        let allowed = |name: &str| {
            drag_and_drop
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no row {name:?} in {drag_and_drop:?}"))
                .1
        };
        for library_row in [
            "External Libraries",
            "Composer Packages (read-only)",
            "autoload.php",
        ] {
            assert!(!allowed(library_row), "{library_row} must not drag or drop");
        }
        for ordinary_row in ["site", "src", "composer.json"] {
            assert!(allowed(ordinary_row), "{ordinary_row} keeps drag and drop");
        }
    }

    fn set_library_paths(cx: &mut TestAppContext, paths: &[&str]) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(ZenithSettingsContent {
                        external_libraries: Some(ZenithExternalLibrariesSettingsContent {
                            paths: Some(paths.iter().map(ToString::to_string).collect()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                });
            });
        });
    }

    /// Opens `roots` with `/libs/acme` (holding `lib/Acme.php`) added last as a library path.
    async fn open_with_library(
        cx: &mut TestAppContext,
        fs: Arc<FakeFs>,
        roots: &[&str],
    ) -> (Entity<ProjectPanel>, VisualTestContext) {
        fs.insert_tree("/libs/acme", json!({ "lib": { "Acme.php": "" } }))
            .await;
        init_test(cx);
        set_library_paths(cx, &["/libs/acme"]);
        let roots: Vec<&str> = roots.iter().copied().chain(["/libs/acme"]).collect();
        open_panel_inited(cx, fs, &roots).await
    }

    fn worktree_id_of(
        panel: &Entity<ProjectPanel>,
        abs_path: &str,
        cx: &mut VisualTestContext,
    ) -> WorktreeId {
        panel.read_with(cx, |panel, cx| {
            panel
                .project
                .read(cx)
                .worktrees(cx)
                .find(|worktree| &*worktree.read(cx).abs_path() == Path::new(abs_path))
                .unwrap_or_else(|| panic!("no worktree at {abs_path}"))
                .read(cx)
                .id()
        })
    }

    /// Toggles the "External Libraries" row, whichever worktree owns it.
    fn toggle_node(panel: &Entity<ProjectPanel>, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            let mut node = None;
            panel.for_each_visible_entry(0..200, window, cx, &mut |entry_id, details, _, _| {
                if details.filename == "External Libraries" {
                    node = Some(entry_id);
                }
            });
            panel.toggle_expanded(node.expect("an External Libraries row"), window, cx)
        });
        cx.run_until_parked();
    }

    fn count(names: &[String], name: &str) -> usize {
        names.iter().filter(|n| *n == name).count()
    }

    #[gpui::test]
    async fn test_library_worktree_joins_the_node(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_with_library(cx, fs, &["/site"]).await;
        let cx = &mut cx;

        let collapsed = names(&panel, cx);
        assert_eq!(count(&collapsed, "External Libraries"), 1, "{collapsed:?}");
        for hidden in ["acme", "acme (read-only)", "lib"] {
            assert!(
                !collapsed.iter().any(|n| n == hidden),
                "{hidden} stays hidden while the node is collapsed: {collapsed:?}"
            );
        }

        toggle_node(&panel, cx);
        let expanded = rows(&panel, cx);
        let position = |name: &str| {
            expanded
                .iter()
                .position(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no row {name:?} in {expanded:?}"))
        };
        let node = position("External Libraries");
        let composer = position("Composer Packages (read-only)");
        let library = position("acme (read-only)");
        assert!(node < composer && composer < library, "{expanded:?}");
        assert_eq!(expanded[node].1, 0);
        assert_eq!(expanded[library].1, 1, "a sibling of the Composer packages");
        assert_eq!(expanded[position("lib")].1, 2);
        assert_eq!(
            count(&names(&panel, cx), "External Libraries"),
            1,
            "still one node once expanded"
        );

        toggle_node(&panel, cx);
        let collapsed_again = names(&panel, cx);
        assert!(
            !collapsed_again.iter().any(|n| n == "acme (read-only)"),
            "collapsing the node hides the joined library: {collapsed_again:?}"
        );
    }

    #[gpui::test]
    async fn test_library_worktree_shares_the_node_with_every_project(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/one", composer_tree()).await;
        fs.insert_tree("/two", composer_tree()).await;
        fs.insert_tree("/libs/acme", json!({ "lib": { "Acme.php": "" } }))
            .await;
        init_test(cx);
        set_library_paths(cx, &["/libs/acme"]);
        // The library path is opened first, yet its tree goes below every project root.
        let (panel, mut cx) = open_panel_inited(cx, fs, &["/libs/acme", "/one", "/two"]).await;
        let cx = &mut cx;

        let collapsed = names(&panel, cx);
        assert_eq!(count(&collapsed, "External Libraries"), 1, "{collapsed:?}");
        assert_eq!(collapsed.first().map(String::as_str), Some("one"));
        assert_eq!(
            collapsed.last().map(String::as_str),
            Some("External Libraries")
        );
        assert_eq!(count(&collapsed, "acme (read-only)"), 0, "{collapsed:?}");

        toggle_node(&panel, cx);
        let expanded = names(&panel, cx);
        let position = |name: &str| {
            expanded
                .iter()
                .position(|n| n == name)
                .unwrap_or_else(|| panic!("no row {name:?} in {expanded:?}"))
        };
        assert!(position("two") < position("External Libraries"));
        assert_eq!(count(&expanded, "External Libraries"), 1);
        for library in [
            "Composer Packages (one)",
            "Composer Packages (two)",
            "acme (read-only)",
        ] {
            assert!(
                position("External Libraries") < position(library),
                "{library} sits under the one node: {expanded:?}"
            );
        }

        toggle_node(&panel, cx);
        let collapsed_again = names(&panel, cx);
        assert_eq!(
            collapsed_again, collapsed,
            "one toggle collapses every library"
        );
    }

    #[gpui::test]
    async fn test_library_worktree_rows_are_read_only(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/site", composer_tree()).await;
        let (panel, mut cx) = open_with_library(cx, fs, &["/site"]).await;
        let cx = &mut cx;
        let site = worktree_id_of(&panel, "/site", cx);
        let acme = worktree_id_of(&panel, "/libs/acme", cx);
        toggle_node(&panel, cx);

        let drag_and_drop = panel.update_in(cx, |panel, window, cx| {
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
        let allowed = |name: &str| {
            drag_and_drop
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no row {name:?} in {drag_and_drop:?}"))
                .1
        };
        for library_row in ["acme (read-only)", "lib"] {
            assert!(!allowed(library_row), "{library_row} must not drag or drop");
        }
        for ordinary_row in ["site", "src"] {
            assert!(allowed(ordinary_row), "{ordinary_row} keeps drag and drop");
        }

        let (library_file, source) = panel.read_with(cx, |panel, cx| {
            let find = |worktree_id: WorktreeId, path: &str| {
                panel
                    .project
                    .read(cx)
                    .worktree_for_id(worktree_id, cx)
                    .unwrap()
                    .read(cx)
                    .entry_for_path(RelPath::from_unix_str(path).unwrap())
                    .unwrap()
                    .id
            };
            (find(acme, "lib/Acme.php"), find(site, "src/main.php"))
        });
        let (library, ordinary) = panel.update_in(cx, |panel, window, cx| {
            let original = ContextMenu::build(window, cx, |menu, _, _| menu);
            let replaced = panel.zenith_menu(acme, library_file, original.clone(), window, cx);
            let kept = panel.zenith_menu(site, source, original.clone(), window, cx);
            (replaced != original, kept == original)
        });
        assert!(
            library,
            "library worktree files get the read-only library menu"
        );
        assert!(ordinary, "project files keep the regular menu");
    }
}
