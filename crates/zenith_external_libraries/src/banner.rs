//! The soft "this file is from an external library" strip under the tab bar. Like PhpStorm it is
//! informational only: the file stays editable. It is a secondary toolbar item on every centre
//! pane, shown while the active item's path lies in a library folder or a user library root.

use gpui::{App, Context, Entity, EventEmitter, IntoElement, Render, Role, WeakEntity, Window};
use project::Project;
use ui::{Banner, Label, LabelSize, Severity, prelude::*};
use workspace::{
    ItemHandle, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
};

use crate::is_library_file;

const TEXT: &str = "This file is from an external library and should not be edited directly.";

/// Adds the banner to every pane's toolbar, current and future. Same wiring as
/// `zenith_markdown::init` (`observe_new` + `PaneAdded`).
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(crate::add_path::add_library_path);
        let Some(window) = window else {
            return;
        };
        let project = workspace.project().downgrade();
        for pane in workspace.panes().to_vec() {
            attach(&pane, project.clone(), window, cx);
        }
        cx.subscribe_in(&cx.entity(), window, move |_, _, event, window, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                attach(pane, project.clone(), window, cx);
            }
        })
        .detach();
    })
    .detach();
}

fn attach(
    pane: &Entity<Pane>,
    project: WeakEntity<Project>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            toolbar.add_item(cx.new(|_| LibraryFileBanner { project }), window, cx);
        });
    });
}

pub struct LibraryFileBanner {
    project: WeakEntity<Project>,
}

impl EventEmitter<ToolbarItemEvent> for LibraryFileBanner {}

impl ToolbarItemView for LibraryFileBanner {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let is_library = active_pane_item
            .and_then(|item| item.project_path(cx))
            .zip(self.project.upgrade())
            .is_some_and(|(path, project)| is_library_file(project.read(cx), &path, cx));
        if is_library {
            ToolbarItemLocation::Secondary
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for LibraryFileBanner {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("zenith-library-file-banner")
            .role(Role::Status)
            .aria_label(TEXT)
            .w_full()
            .child(
                Banner::new()
                    .severity(Severity::Info)
                    .wrap_content(true)
                    .child(Label::new(TEXT).size(LabelSize::Small)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::ProjectPath;
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use workspace::{
        AppState, MultiWorkspace,
        item::test::{TestItem, TestProjectItem},
    };

    async fn project_with(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            AppState::test(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "composer.json": "{}",
                "src": { "a.php": "" },
                "vendor": { "acme": { "x.php": "" } },
            }),
        )
        .await;
        fs.insert_tree(
            path!("/bare"),
            json!({ "vendor": { "acme": { "x.php": "" } } }),
        )
        .await;
        Project::test(fs, [path!("/root").as_ref(), path!("/bare").as_ref()], cx).await
    }

    fn path_in(project: &Entity<Project>, root: usize, rel: &str, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: project.read(cx).worktrees(cx).nth(root).unwrap().read(cx).id(),
            path: rel_path(rel).into(),
        }
    }

    #[gpui::test]
    async fn is_library_file_needs_folder_and_manifest(cx: &mut TestAppContext) {
        let project = project_with(cx).await;
        cx.update(|cx| {
            let check = |root, rel| {
                is_library_file(project.read(cx), &path_in(&project, root, rel, cx), cx)
            };
            assert!(check(0, "vendor/acme/x.php"));
            assert!(!check(0, "src/a.php"));
            // vendor/ without composer.json is an ordinary folder.
            assert!(!check(1, "vendor/acme/x.php"));
        });
    }

    #[gpui::test]
    async fn banner_is_secondary_for_library_files_only(cx: &mut TestAppContext) {
        let project = project_with(cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .update(cx, |multi, _, _| multi.workspace().clone())
            .unwrap();
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let banner = pane
            .read_with(cx, |pane, cx| pane.toolbar().read(cx).item_of_type::<LibraryFileBanner>())
            .expect("init() adds the banner to every pane's toolbar");
        // The toolbar applies exactly this return value as the item's location.
        let location = |cx: &mut TestAppContext| {
            let item = pane.read_with(cx, |pane, _| pane.active_item());
            window
                .update(cx, |_, window, cx| {
                    banner.update(cx, |banner, cx| {
                        banner.set_active_pane_item(item.as_deref(), window, cx)
                    })
                })
                .unwrap()
        };

        let open = |rel: &'static str, cx: &mut TestAppContext| {
            let path = cx.update(|cx| path_in(&project, 0, rel, cx));
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let item = cx.new(|cx| {
                            TestItem::new(cx).with_project_items(&[cx.new(|_| TestProjectItem {
                                entry_id: None,
                                project_path: Some(path),
                                is_dirty: false,
                            })])
                        });
                        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
                    })
                })
                .unwrap();
        };

        open("src/a.php", cx);
        assert_eq!(location(cx), ToolbarItemLocation::Hidden);
        open("vendor/acme/x.php", cx);
        assert_eq!(location(cx), ToolbarItemLocation::Secondary);
        open("src/a.php", cx);
        assert_eq!(location(cx), ToolbarItemLocation::Hidden);
    }
}
