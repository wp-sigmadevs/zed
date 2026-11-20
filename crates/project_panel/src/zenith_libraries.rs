//! Glue between the project panel and `zenith_external_libraries`: the "External Libraries"
//! node, its keyboard handling, row decoration and context menu. The upstream file only calls
//! into this module; all panel-state access for the feature lives here.

use super::*;
use zenith_external_libraries::{
    RowDecoration, classify, decorate, detect_libraries, is_enabled, is_section_expanded,
    is_under_library, may_be_library_path, section_id, section_worktree, set_section_expanded,
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

    /// Moves library folders out of each worktree's entries and adds the "External Libraries"
    /// node. Runs on the freshly derived state, before the panel repaints.
    pub(super) fn zenith_libraries(&mut self, cx: &mut Context<Self>) {
        if !is_enabled(cx) {
            return;
        }
        let project = self.project.read(cx);
        let mut changed = false;
        for visible in &mut self.state.visible_entries {
            let Some(worktree) = project.worktree_for_id(visible.worktree_id, cx) else {
                continue;
            };
            let snapshot = worktree.read(cx).snapshot();
            let expanded_dir_ids = self
                .state
                .expanded_dir_ids
                .get(&visible.worktree_id)
                .map_or(&[][..], Vec::as_slice);
            if zenith_external_libraries::apply(&snapshot, expanded_dir_ids, &mut visible.entries) {
                visible.index = OnceCell::new();
                changed = true;
            }
        }
        if changed {
            // The widest-item index was computed before rows were moved.
            self.state.max_width_item_index = None;
        }
    }

    /// Library rows indent one level under the node and carry its labels.
    pub(super) fn zenith_decorate(
        &self,
        entry: &Entry,
        worktree_id: WorktreeId,
        row: (String, Option<SharedString>, Option<SharedString>, usize),
        cx: &App,
    ) -> (String, Option<SharedString>, Option<SharedString>, usize) {
        let (filename, icon, chevron, depth) = row;
        let unchanged = RowDecoration {
            filename,
            icon,
            chevron,
            depth,
        };
        // Mirrors the upstream rule that hides the root row.
        let root_visible = !(ProjectPanelSettings::get_global(cx).hide_root
            && self.project.read(cx).visible_worktrees(cx).count() == 1);
        let decorated = self
            .zenith_row_kind(entry, worktree_id, cx)
            .map(|kind| {
                let expanded = self
                    .state
                    .expanded_dir_ids
                    .get(&worktree_id)
                    .is_some_and(|ids| is_section_expanded(worktree_id, ids));
                decorate(&kind, expanded, root_visible, unchanged.clone())
            })
            .unwrap_or(unchanged);
        (
            decorated.filename,
            decorated.icon,
            decorated.chevron,
            decorated.depth,
        )
    }

    fn zenith_row_kind(
        &self,
        entry: &Entry,
        worktree_id: WorktreeId,
        cx: &App,
    ) -> Option<zenith_external_libraries::RowKind> {
        if !is_enabled(cx) {
            return None;
        }
        let is_section = entry.id == section_id(worktree_id);
        if !is_section && !may_be_library_path(&entry.path) {
            return None;
        }
        let worktree = self.project.read(cx).worktree_for_id(worktree_id, cx)?;
        let libraries = detect_libraries(&worktree.read(cx).snapshot());
        classify(&libraries, worktree_id, entry.id, &entry.path)
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
        if !is_under_library(&detect_libraries(&worktree.snapshot()), &entry.path) {
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
    async fn test_expansion_is_per_worktree(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/one", composer_tree()).await;
        fs.insert_tree("/two", composer_tree()).await;
        let (panel, mut cx) = open_panel(cx, fs, &["/one", "/two"]).await;
        let cx = &mut cx;
        let id = first_worktree_id(&panel, cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.toggle_expanded(section_id(id), window, cx)
        });
        cx.run_until_parked();
        let libraries = names(&panel, cx)
            .iter()
            .filter(|name| name.starts_with("Composer"))
            .count();
        assert_eq!(libraries, 1, "only the toggled worktree expands");
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
}
