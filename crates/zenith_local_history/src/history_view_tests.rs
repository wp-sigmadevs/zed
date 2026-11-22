use super::{LocalHistoryView, State, row_title, size_delta};
use crate::{Store, capture};
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use language::Buffer;
use project::{Project, ProjectPath};
use project_panel::ProjectPanel;
use serde_json::json;
use settings::{KeymapFile, KeymapFileLoadResult, SettingsStore};
use std::{path::Path, time::Duration};
use util::rel_path::RelPath;
use workspace::{AppState, MultiWorkspace, Workspace};

/// Binds a keymap file. Entries for actions of crates this test does not link are
/// skipped, but none of this crate's own may be.
fn bind_keymap(json: &str, cx: &mut gpui::App) {
    let key_bindings = match KeymapFile::load(json, cx) {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings,
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => {
            assert!(
                !error_message.0.contains("zenith_local_history"),
                "{}",
                error_message.0
            );
            key_bindings
        }
        KeymapFileLoadResult::JsonParseFailure { error } => panic!("{error:#}"),
    };
    cx.bind_keys(key_bindings);
}

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
        // The shipped keymaps: Tab, Escape and the history keys are decided by both of them.
        bind_keymap(
            include_str!("../../../assets/keymaps/default-macos.json"),
            cx,
        );
        bind_keymap(include_str!("../../../assets/keymaps/zenith.json"), cx);
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

#[gpui::test]
async fn test_history_view_still_lists_revisions_after_recording_is_turned_off(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1", "2"], cx).await;
    cx.update(|_, cx| {
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
    });

    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();

    let view = history_view(&fixture, cx);
    assert_eq!(view.read_with(cx, |view, _| view.entries.len()), 3);
}

/// Opens a.txt in an editor and its history, then types in the history's right pane.
async fn edit_in_the_history_pane(
    fixture: &Fixture,
    cx: &mut VisualTestContext,
) -> (Entity<LocalHistoryView>, Entity<Buffer>) {
    let buffer = open_and_save(fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(fixture, cx);
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Z");
    cx.run_until_parked();
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    (view, buffer)
}

fn close_active_tab(
    fixture: &Fixture,
    cx: &mut VisualTestContext,
) -> gpui::Task<anyhow::Result<()>> {
    fixture.workspace.update_in(cx, |workspace, window, cx| {
        workspace.active_pane().update(cx, |pane, cx| {
            pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
        })
    })
}

// Closing prompts only when no other tab holds the dirty buffer, so with an
// editor tab open the history tab closes silently and nothing is lost.
#[gpui::test]
async fn test_history_view_closes_without_a_prompt_while_an_editor_tab_holds_the_buffer(
    cx: &mut TestAppContext,
) {
    use workspace::Item as _;
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = edit_in_the_history_pane(&fixture, cx).await;
    assert!(view.read_with(cx, |view, cx| view.is_dirty(cx)));

    let closing = close_active_tab(&fixture, cx);
    cx.run_until_parked();

    assert!(!cx.has_pending_prompt());
    closing.await.expect("close");
    assert_eq!(view_count(&fixture, cx), 0);
    assert!(buffer.read_with(cx, |buffer, _| buffer.text().contains('Z')));
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    let editors = fixture.workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<editor::Editor>(cx).count()
    });
    assert_eq!(editors, 1);
}

// With no other tab on the file, closing the history tab prompts exactly once.
#[gpui::test]
async fn test_history_view_prompts_once_when_it_is_the_only_tab_holding_the_edit(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let buffer = open_and_save(&fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);
    // Close the editor tab (clean, so silent), leaving only the history tab.
    let editor_id = fixture.workspace.read_with(cx, |workspace, cx| {
        workspace
            .items_of_type::<editor::Editor>(cx)
            .next()
            .expect("editor tab")
            .entity_id()
    });
    let closing_editor = fixture.workspace.update_in(cx, |workspace, window, cx| {
        workspace.active_pane().update(cx, |pane, cx| {
            pane.close_item_by_id(editor_id, workspace::SaveIntent::Close, window, cx)
        })
    });
    cx.run_until_parked();
    closing_editor.await.expect("close editor");
    assert!(!cx.has_pending_prompt());
    assert!(list_is_focused(&view, cx));
    cx.simulate_keystrokes("enter");
    cx.simulate_input("Z");
    cx.run_until_parked();
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));

    let closing = close_active_tab(&fixture, cx);
    cx.run_until_parked();
    let (message, detail) = cx.pending_prompt().expect("one prompt");
    assert!(
        message.contains("a.txt") || detail.contains("a.txt"),
        "{message:?} {detail:?}"
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    // Cancelled: the tab stays, the edit is kept, and there is no second prompt.
    closing.await.ok();
    assert!(!cx.has_pending_prompt());
    assert_eq!(view_count(&fixture, cx), 1);
    assert!(buffer.read_with(cx, |buffer, _| buffer.text().contains('Z')));
}

#[gpui::test]
async fn test_history_view_shows_why_the_diff_could_not_be_built(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    open_and_save(&fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);

    view.update(cx, |view, cx| {
        view.diff = None;
        view.diff_failed(&anyhow::anyhow!("boom"), cx);
    });
    cx.run_until_parked();

    assert_eq!(
        view.read_with(cx, |view, _| view.note.clone()).as_deref(),
        Some("Could not compare: boom")
    );
}

/// Opens the history of a.txt (three saved revisions, newest "21base") and selects
/// the oldest, "base", waiting until its text is loaded.
async fn history_on_the_oldest_revision(
    fixture: &Fixture,
    cx: &mut VisualTestContext,
) -> (Entity<LocalHistoryView>, Entity<Buffer>) {
    let buffer = open_and_save(fixture, "a.txt", &["1", "2"], cx).await;
    // Tests group nothing by default; real editing groups edits typed close together.
    buffer.update(cx, |buffer, _| {
        buffer.set_group_interval(Duration::from_secs(300))
    });
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(fixture, cx);
    cx.simulate_keystrokes("down down");
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.selected), 2);
    assert_eq!(
        view.read_with(cx, |view, _| view.revision_text.clone())
            .as_deref(),
        Some("base")
    );
    (view, buffer)
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

fn status(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> Option<String> {
    view.read_with(cx, |view, _| view.status.as_ref().map(|s| s.to_string()))
}

fn revert_is_focused(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| view.read(cx).revert_focus.is_focused(window))
}

fn copy_is_focused(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| view.read(cx).copy_focus.is_focused(window))
}

#[gpui::test]
async fn test_history_view_revert_replaces_the_text_in_one_undoable_edit(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    // An unsaved edit: reverting must not lose it.
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "Z")], None, cx));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "Z21base");

    cx.dispatch_action(super::RevertToRevision);
    cx.run_until_parked();

    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
    assert!(
        buffer.read_with(cx, |b, _| b.is_dirty()),
        "saved by the user, not by revert"
    );
    assert!(
        status(&view, cx).unwrap().starts_with("Reverted to the "),
        "{:?}",
        status(&view, cx)
    );
    let on_disk = project::Fs::load(
        fixture
            .project
            .read_with(cx, |p, _| p.fs().clone())
            .as_ref(),
        Path::new("/dir/a.txt"),
    )
    .await
    .expect("load");
    assert_eq!(on_disk, "21base", "revert must not write the file");

    // The unsaved text is kept as a labelled revision, with exactly that text.
    let entries = cx
        .update(|_, cx| capture::list(cx, "/dir".into(), "a.txt".into()))
        .await
        .expect("list");
    let newest = &entries[0].revision;
    assert_eq!(newest.source, "before-revert");
    assert_eq!(super::source_name(&newest.source), "Before revert");
    let kept = cx
        .update(|_, cx| capture::content(cx, newest.blob_hash.clone()))
        .await
        .expect("content")
        .expect("blob");
    assert_eq!(kept, b"Z21base");

    // One undo brings the unsaved text back.
    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "Z21base");
}

#[gpui::test]
async fn test_history_view_revert_works_from_the_diff_pane_and_keeps_later_typing_separate(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.simulate_keystrokes("enter"); // focus moves into the diff pane
    assert!(diff_is_focused(&view, cx));

    cx.dispatch_action(super::RevertToRevision);
    cx.run_until_parked();
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");

    // Typing afterwards is its own undo step: undo removes it, not the revert.
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "Q")], None, cx));
    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
}

#[gpui::test]
async fn test_history_view_revert_keeps_cursors_outside_the_changed_lines_and_the_line_endings(
    cx: &mut TestAppContext,
) {
    use language::LineEnding;
    let (fixture, cx) = fixture(true, cx).await;
    // Saved as "x\nbase"; the oldest revision is "base". Only the first line changes.
    let buffer = open_and_save(&fixture, "a.txt", &["x\n"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "x\nbase");
    let anchor = buffer.update(cx, |buffer, cx| {
        buffer.set_line_ending(LineEnding::Windows, cx);
        buffer.anchor_before(4) // before the 's'
    });

    cx.dispatch_action(super::RevertToRevision);
    cx.run_until_parked();

    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
    let offset = buffer.read_with(cx, |b, _| b.snapshot().summary_for_anchor::<usize>(&anchor));
    assert_eq!(
        offset, 2,
        "a whole-text replacement would lose the position"
    );
    assert_eq!(
        buffer.read_with(cx, |b, _| b.line_ending()),
        LineEnding::Windows,
        "the revision's line ending must not replace the file's"
    );
    drop(view);
}

#[gpui::test]
async fn test_history_view_revert_still_matches_the_revision_when_typing_races_the_diff(
    cx: &mut TestAppContext,
) {
    use gpui::Action as _;
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;

    // The diff is computed in the background; a keystroke lands before it is applied
    // and clashes with the hunk that deletes "21".
    view.update_in(cx, |_, window, cx| {
        window.dispatch_action(super::RevertToRevision.boxed_clone(), cx);
    });
    buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "Q")], None, cx));
    cx.run_until_parked();

    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(
        buffer.read_with(cx, |b, _| b.text()),
        "Q21base",
        "the revert is one undo step"
    );
}

#[gpui::test]
async fn test_history_view_revert_of_the_current_text_changes_nothing_and_says_so(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.simulate_keystrokes("up up"); // the newest revision is the current text
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.selected), 0);

    cx.dispatch_action(super::RevertToRevision);
    cx.run_until_parked();

    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
    assert!(!buffer.read_with(cx, |b, _| b.is_dirty()));
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("The file already matches this revision.")
    );
}

#[gpui::test]
async fn test_history_view_revert_is_refused_for_a_read_only_buffer_but_copy_still_works(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(language::Capability::ReadOnly, cx)
    });

    assert_eq!(
        view.read_with(cx, |view, cx| view.revert_blocker(cx)),
        Some("The file is read-only.")
    );
    cx.dispatch_action(super::RevertToRevision);
    cx.run_until_parked();
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
    assert_eq!(status(&view, cx).as_deref(), Some("The file is read-only."));
    // The disabled Revert button is skipped by Tab.
    cx.simulate_keystrokes("tab");
    assert!(copy_is_focused(&view, cx));

    cx.dispatch_action(super::CopyRevisionText);
    assert_eq!(clipboard_text(cx).as_deref(), Some("base"));
}

#[gpui::test]
async fn test_history_view_copy_puts_the_revision_text_on_the_clipboard(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));

    cx.dispatch_action(super::CopyRevisionText);
    cx.run_until_parked();

    assert_eq!(clipboard_text(cx).as_deref(), Some("base"));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
    assert_eq!(status(&view, cx).as_deref(), Some("Copied revision text."));
}

#[gpui::test]
async fn test_history_view_revert_and_copy_do_nothing_when_the_file_is_no_longer_viewable(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    assert!(view.read_with(cx, |view, cx| view.revision_to_use(cx).is_ok()));
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.project.worktree.private_files = Some(vec!["**/a.txt".to_string()].into());
            });
        });
    });

    // The text is loaded, the gate alone refuses.
    assert!(view.read_with(cx, |view, _| view.revision_text.is_some()));
    assert!(view.read_with(cx, |view, cx| view.revision_to_use(cx).is_err()));
    cx.dispatch_action(super::RevertToRevision);
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("This file's history can no longer be shown.")
    );
    status_cleared_then(&view, cx);
    cx.dispatch_action(super::CopyRevisionText);
    cx.run_until_parked();

    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
    assert!(!buffer.read_with(cx, |b, _| b.is_dirty()));
    assert_eq!(clipboard_text(cx).as_deref(), Some("sentinel"));
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("This file's history can no longer be shown.")
    );
}

/// Forgets the status, so the next assertion proves the next action set it.
fn status_cleared_then(view: &Entity<LocalHistoryView>, cx: &mut VisualTestContext) {
    view.update(cx, |view, _| view.status = None);
}

#[gpui::test]
async fn test_history_view_revert_and_copy_do_nothing_without_a_selected_loaded_revision(
    cx: &mut TestAppContext,
) {
    // No history at all: nothing is selected.
    let (fixture, cx) = fixture(false, cx).await;
    let buffer = open_and_save(&fixture, "a.txt", &["1"], cx).await;
    cx.simulate_keystrokes("alt-shift-h");
    cx.run_until_parked();
    let view = history_view(&fixture, cx);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));
    cx.dispatch_action(super::RevertToRevision);
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("No revision is selected.")
    );
    status_cleared_then(&view, cx);
    cx.dispatch_action(super::CopyRevisionText);
    cx.run_until_parked();
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("No revision is selected.")
    );
    assert!(view.read_with(cx, |view, cx| view.revision_to_use(cx).is_err()));
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "1base");
    assert!(!buffer.read_with(cx, |b, _| b.is_dirty()));
    assert_eq!(clipboard_text(cx).as_deref(), Some("sentinel"));
}

#[gpui::test]
async fn test_history_view_revert_and_copy_do_nothing_until_the_revision_is_loaded(
    cx: &mut TestAppContext,
) {
    use gpui::Action as _;
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));

    // Selecting another row while the old row's text is still in the view: both actions
    // arrive before the new text has been read, so they must not use the old one.
    view.update_in(cx, |view, window, cx| {
        view.select(1, window, cx);
        window.dispatch_action(super::RevertToRevision.boxed_clone(), cx);
        window.dispatch_action(super::CopyRevisionText.boxed_clone(), cx);
    });
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("This revision's text is not loaded yet.")
    );
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "21base");
    assert_eq!(clipboard_text(cx).as_deref(), Some("sentinel"));

    // Once it has been read the actions work on the new row.
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |view, _| view.revision_text.clone())
            .as_deref(),
        Some("1base")
    );
    cx.dispatch_action(super::CopyRevisionText);
    assert_eq!(clipboard_text(cx).as_deref(), Some("1base"));
}

#[gpui::test]
async fn test_history_view_loading_again_makes_the_old_text_unusable_until_it_is_checked(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, _buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    view.update(cx, |view, _| view.status = Some("old".into()));
    view.update_in(cx, |view, window, cx| view.load(window, cx));
    assert!(view.read_with(cx, |view, _| view.revision_text.is_none()));
    assert_eq!(status(&view, cx), None);
}

#[gpui::test]
async fn test_history_view_keys_revert_and_copy_and_report_what_they_did(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("sentinel".into()));

    cx.simulate_keystrokes("alt-shift-c");
    assert_eq!(clipboard_text(cx).as_deref(), Some("base"));
    assert_eq!(status(&view, cx).as_deref(), Some("Copied revision text."));

    cx.simulate_keystrokes("alt-shift-r");
    cx.run_until_parked();
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
    let status = status(&view, cx).expect("a status");
    assert!(
        status.starts_with("Reverted to the ") && status.ends_with("save to keep."),
        "{status:?}"
    );
}

#[gpui::test]
async fn test_history_view_tab_moves_between_the_list_and_the_buttons(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    assert!(list_is_focused(&view, cx));

    // Tab leaves the list for the buttons instead of stepping the selection.
    cx.simulate_keystrokes("tab");
    assert!(revert_is_focused(&view, cx));
    assert_eq!(view.read_with(cx, |view, _| view.selected), 2);
    cx.simulate_keystrokes("tab");
    assert!(copy_is_focused(&view, cx));
    cx.simulate_keystrokes("shift-tab");
    assert!(revert_is_focused(&view, cx));
    cx.simulate_keystrokes("shift-tab");
    assert!(list_is_focused(&view, cx));

    // A focused button works from the keyboard.
    cx.simulate_keystrokes("tab alt-shift-r");
    cx.run_until_parked();
    assert_eq!(buffer.read_with(cx, |b, _| b.text()), "base");
}

#[gpui::test]
async fn test_history_view_escape_steps_back_to_the_list_then_closes(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, _buffer) = history_on_the_oldest_revision(&fixture, cx).await;

    // From the diff pane.
    cx.simulate_keystrokes("enter");
    assert!(diff_is_focused(&view, cx));
    cx.simulate_keystrokes("escape");
    assert!(list_is_focused(&view, cx));
    assert_eq!(view_count(&fixture, cx), 1);

    // From a button.
    cx.simulate_keystrokes("tab");
    assert!(revert_is_focused(&view, cx));
    cx.simulate_keystrokes("escape");
    assert!(list_is_focused(&view, cx));
    assert_eq!(view_count(&fixture, cx), 1);

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(view_count(&fixture, cx), 0);
}

#[gpui::test]
async fn test_history_view_status_is_forgotten_when_another_revision_is_selected(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(true, cx).await;
    let (view, _buffer) = history_on_the_oldest_revision(&fixture, cx).await;
    cx.simulate_keystrokes("alt-shift-c");
    assert_eq!(status(&view, cx).as_deref(), Some("Copied revision text."));

    cx.simulate_keystrokes("up");
    assert_eq!(status(&view, cx), None);
}

#[test]
fn test_history_view_agent_rows_name_the_turn() {
    let entry = |label: Option<&str>, source: &str| capture::Entry {
        revision: crate::store::Revision {
            id: 1,
            worktree_root: "/r".into(),
            rel_path: "a.txt".into(),
            blob_hash: String::new(),
            ts: 0,
            label: label.map(str::to_owned),
            source: source.into(),
        },
        size: 0,
    };
    assert_eq!(
        row_title(&entry(Some("agent:0a1b-2c:1763800000000:3"), "agent")),
        "Agent · turn 3"
    );
    // Labels from before the run id was added.
    assert_eq!(
        row_title(&entry(Some("agent:0a1b-2c:3"), "agent")),
        "Agent · turn 3"
    );
    assert_eq!(row_title(&entry(None, "agent")), "Agent");
    assert_eq!(row_title(&entry(Some("Release"), "save")), "Release");
    assert_eq!(row_title(&entry(None, "save")), "Saved");
}
