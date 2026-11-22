use std::path::Path;

use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use util::rel_path::RelPath;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::{JumpToNavigationBar, NavBar, SelectNextSegment, SelectPreviousSegment, children};

async fn setup(
    cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<Workspace>, &mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        crate::init(cx);
        app_state
    });
    app_state
        .fs
        .as_fake()
        .insert_tree(
            "/project",
            json!({
                ".github": { "workflows": { "unit-tests.yml": "on: push\n", "lint.yml": "" } },
                "src": { "b.txt": "", "Zeta": {}, "A.txt": "", "alpha": {} },
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [Path::new("/project")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();
    (project, workspace, cx)
}

fn bar(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<NavBar> {
    workspace
        .read_with(cx, |workspace, cx| {
            workspace.status_bar().read(cx).item_of_type::<NavBar>()
        })
        .expect("navigation bar is in the status bar")
}

fn labels(bar: &Entity<NavBar>, cx: &mut VisualTestContext) -> Vec<String> {
    bar.read_with(cx, |bar, cx| {
        bar.segments(cx)
            .into_iter()
            .map(|segment| segment.label)
            .collect()
    })
}

#[gpui::test]
async fn test_bar_follows_the_active_file_and_keyboard_moves_between_popups(
    cx: &mut TestAppContext,
) {
    let (_, workspace, cx) = setup(cx).await;
    let bar = bar(&workspace, cx);
    assert!(labels(&bar, cx).is_empty(), "hidden without an item");

    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                "/project/.github/workflows/unit-tests.yml".into(),
                workspace::OpenOptions::default(),
                window,
                cx,
            )
        })
        .await
        .expect("file opens")
        .downcast::<Editor>()
        .expect("an editor");
    cx.run_until_parked();
    assert_eq!(
        labels(&bar, cx),
        ["project", ".github", "workflows", "unit-tests.yml"]
    );
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("zenith-navbar").is_some());

    cx.dispatch_action(JumpToNavigationBar);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(bar.read_with(cx, |bar, _| bar.open_segment()), Some(3));

    cx.dispatch_action(SelectPreviousSegment);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(bar.read_with(cx, |bar, _| bar.open_segment()), Some(2));

    for _ in 0..2 {
        cx.dispatch_action(SelectNextSegment);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    assert_eq!(
        bar.read_with(cx, |bar, _| bar.open_segment()),
        Some(3),
        "stops at the last segment"
    );

    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert_eq!(bar.read_with(cx, |bar, _| bar.open_segment()), None);
}

#[gpui::test]
async fn test_folder_popup_lists_folders_first_then_files(cx: &mut TestAppContext) {
    let (project, _, cx) = setup(cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("a worktree")
            .read(cx)
            .id()
    });
    let names = |files_only: bool, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            children(
                &project,
                worktree_id,
                RelPath::from_unix_str("src").unwrap(),
                files_only,
                cx,
            )
            .into_iter()
            .map(|(path, _)| path.file_name().unwrap().to_string())
            .collect::<Vec<_>>()
        })
    };
    assert_eq!(names(false, cx), ["alpha", "Zeta", "A.txt", "b.txt"]);
    assert_eq!(names(true, cx), ["A.txt", "b.txt"]);
}
