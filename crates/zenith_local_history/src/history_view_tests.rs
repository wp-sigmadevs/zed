use super::{LocalHistoryView, State, row_title, size_delta};
use crate::{Store, capture};
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use language::Buffer;
use project::{Project, ProjectPath};
use project_panel::ProjectPanel;
use serde_json::json;
use settings::{KeymapFile, SettingsStore};
use std::{path::Path, time::Duration};
use util::rel_path::RelPath;
use workspace::{AppState, MultiWorkspace, Workspace};

struct Fixture {
    workspace: Entity<Workspace>,
    project: Entity<Project>,
    _dir: tempfile::TempDir,
}

/// A workspace over `/dir` with `a.txt` and `b.txt`, recording into a temp database.
/// With `enabled == false` no database is ever created.
async fn fixture(enabled: bool, cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let history_dir = dir.path().to_path_buf();
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        menu::init();
        workspace::init(app_state.clone(), cx);
        project_panel::init(cx);
        super::init(cx);
        capture::start(cx, move |create| {
            if create {
                Store::open_in(&history_dir).map(Some)
            } else {
                Store::open_existing_in(&history_dir)
            }
        });
        if !enabled {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(settings::ZenithSettingsContent {
                        local_history: Some(settings::ZenithLocalHistorySettingsContent {
                            enabled: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                });
            });
        }
        // Zenith's keymap, reduced to this view; the loader rejects unknown action names.
        cx.bind_keys(KeymapFile::load_panic_on_failure(
            r#"[
                { "context": "Workspace", "bindings": { "alt-shift-h": "zenith_local_history::ShowFileHistory" } },
                { "context": "LocalHistoryList", "bindings": {
                    "down": "menu::SelectNext", "up": "menu::SelectPrevious",
                    "enter": "menu::Confirm", "escape": "menu::Cancel" } }
            ]"#,
            cx,
        ));
        app_state
    });
    app_state
        .fs
        .as_fake()
        .insert_tree("/dir", json!({ "a.txt": "base", "b.txt": "other" }))
        .await;
    let project = Project::test(app_state.fs.clone(), [Path::new("/dir")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    (
        Fixture {
            workspace,
            project,
            _dir: dir,
        },
        cx,
    )
}

/// Opens `name` in the workspace (so it is the active tab) and saves it once
/// per entry of `edits`, each typed at the start.
async fn open_and_save(
    fixture: &Fixture,
    name: &str,
    edits: &[&str],
    cx: &mut VisualTestContext,
) -> Entity<Buffer> {
    let path = Path::new("/dir").join(name);
    fixture
        .workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(path.clone(), Default::default(), window, cx)
        })
        .await
        .expect("open file");
    let buffer = fixture
        .project
        .update(cx, |project, cx| project.open_local_buffer(&path, cx))
        .await
        .expect("open buffer");
    for edit in edits {
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, *edit)], None, cx));
        fixture
            .project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .expect("save buffer");
    }
    cx.run_until_parked();
    buffer
}

fn history_view(fixture: &Fixture, cx: &mut VisualTestContext) -> Entity<LocalHistoryView> {
    fixture
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_item_as::<LocalHistoryView>(cx)
        })
        .expect("the history tab is active")
}

fn view_count(fixture: &Fixture, cx: &mut VisualTestContext) -> usize {
    fixture.workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<LocalHistoryView>(cx).count()
    })
}

fn list_is_focused(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| view.read(cx).focus_handle.contains_focused(window, cx))
}

fn diff_is_focused(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        let pane = view.read(cx).diff.as_ref().expect("diff is mounted");
        gpui::Focusable::focus_handle(pane.view.read(cx), cx).contains_focused(window, cx)
    })
}

fn hunk_count(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> usize {
    view.read_with(cx, |view, cx| {
        let current = view.current.as_ref().expect("buffer").read(cx).snapshot();
        let diff = view.diff.as_ref().expect("diff is mounted").diff.read(cx);
        diff.snapshot(cx).hunks(&current.text).count()
    })
}

#[gpui::test]
async fn test_history_view_lists_revisions_and_diffs_the_selected_one(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1", "2"], cx).await;

    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();

    let view = history_view(&fixture, cx);
    assert_eq!(view_count(&fixture, cx), 1);
    let (titles, deltas, selected) = view.read_with(cx, |view, _| {
        let deltas = (0..view.entries.len())
            .map(|ix| size_delta(&view.entries[ix], view.entries.get(ix + 1)))
            .collect::<Vec<_>>();
        (
            view.entries.iter().map(row_title).collect::<Vec<_>>(),
            deltas,
            view.selected,
        )
    });
    assert_eq!(titles, ["Saved", "Saved", "Opened"]);
    assert_eq!(deltas, ["+1B", "+1B", "4B"]);
    assert_eq!(selected, 0);
    // The newest revision is the current text; the diff is empty.
    assert_eq!(hunk_count(&view, cx), 0);

    // Down twice (list focus comes from opening the tab) selects the oldest.
    assert!(list_is_focused(&view, cx));
    cx.simulate_keystrokes("down down");
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.selected), 2);
    assert_eq!(
        view.read_with(cx, |view, cx| view
            .diff
            .as_ref()
            .unwrap()
            .old_buffer
            .read(cx)
            .text()),
        "base"
    );
    assert!(hunk_count(&view, cx) >= 1);

    // Down stops at the last row; up goes back.
    cx.simulate_keystrokes("down");
    assert_eq!(view.read_with(cx, |view, _| view.selected), 2);
    cx.simulate_keystrokes("up");
    assert_eq!(view.read_with(cx, |view, _| view.selected), 1);
}

#[gpui::test]
async fn test_history_view_focus_moves_between_list_and_diff_and_escape_closes(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);
    assert!(list_is_focused(&view, cx));

    // Enter moves into the diff, where Revert Hunk lives.
    cx.simulate_keystrokes("enter");
    assert!(diff_is_focused(&view, cx));
    assert!(!list_is_focused(&view, cx));

    // The same key again comes back to the list instead of opening a second tab.
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    assert_eq!(view_count(&fixture, cx), 1);
    assert!(list_is_focused(&view, cx));

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(view_count(&fixture, cx), 0);
}

#[gpui::test]
async fn test_history_view_without_history_shows_the_empty_state(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(false, cx).await;
    open_and_save(&fixture, "a.txt", &["1"], cx).await;

    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();

    let view = history_view(&fixture, cx);
    view.read_with(cx, |view, _| {
        assert!(matches!(view.state, State::Ready));
        assert!(view.entries.is_empty());
        assert!(view.diff.is_none());
        assert_eq!(view.name.as_ref(), "a.txt");
    });
    assert!(list_is_focused(&view, cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(view_count(&fixture, cx), 0);
}

#[gpui::test]
async fn test_history_view_uses_the_project_panel_selection_when_the_panel_has_focus(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1"], cx).await;
    open_and_save(&fixture, "b.txt", &["x"], cx).await;
    let weak = fixture.workspace.downgrade();
    let panel = fixture
        .workspace
        .update_in(cx, |_, window, cx| {
            window.spawn(cx, async move |cx| {
                ProjectPanel::load(weak, cx.clone()).await
            })
        })
        .await
        .expect("panel loads");
    fixture.workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx);
        workspace.focus_panel::<ProjectPanel>(window, cx);
    });
    cx.run_until_parked();
    // The active tab is b.txt, the panel selects a.txt.
    let a = fixture.project.read_with(cx, |project, cx| ProjectPath {
        worktree_id: project.visible_worktrees(cx).next().unwrap().read(cx).id(),
        path: RelPath::from_unix_str("a.txt").unwrap().into(),
    });
    panel.update(cx, |panel, cx| panel.select_path_for_test(a.clone(), cx));

    cx.dispatch_action(zed_actions::zenith_local_history::ShowFileHistory);
    cx.run_until_parked();

    let view = history_view(&fixture, cx);
    assert_eq!(view.read_with(cx, |view, _| view.project_path.clone()), a);
}

#[gpui::test]
async fn test_history_view_edits_in_the_diff_make_the_tab_dirty_and_can_be_saved(
    cx: &mut TestAppContext,
) {
    use workspace::{Item as _, item::SaveOptions};
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);
    assert!(view.read_with(cx, |view, cx| view.can_save(cx)));
    assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));

    // Typing in the right pane edits the project buffer.
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Z");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));

    let project = fixture.project.clone();
    view.update_in(cx, |view, window, cx| {
        view.save(SaveOptions::default(), project, window, cx)
    })
    .await
    .expect("save");
    cx.run_until_parked();
    assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
    let fs = fixture
        .project
        .read_with(cx, |project, _| project.fs().clone());
    let saved = project::Fs::load(fs.as_ref(), Path::new("/dir/a.txt"))
        .await
        .expect("load");
    assert!(saved.contains('Z'), "{saved:?}");
}

#[gpui::test]
async fn test_history_view_hides_old_revisions_of_a_file_that_is_now_private(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1", "2"], cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.worktree.private_files = Some(vec!["**/a.txt".to_string()].into());
            });
        });
    });

    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();

    let view = history_view(&fixture, cx);
    view.read_with(cx, |view, _| {
        assert!(matches!(view.state, State::Ready));
        assert!(view.entries.is_empty(), "old revisions must not be listed");
        assert!(view.diff.is_none());
    });
}
