use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use acp_thread::{AcpThread, AgentConnection as _, StubAgentConnection};
use action_log::ActionLog;
use agent_client_protocol::{v1::schema as acp_v1, v2::schema as acp_v2};
use editor::Editor;
use gpui::{
    AppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
};
use language::Buffer;
use project::{FakeFs, Fs as _, Project};
use serde_json::json;
use settings::SettingsStore;
use util::path_list::PathList;
use workspace::{AppState, MultiWorkspace, Workspace};
use zenith_title_bar::{AgentChip, AgentCounts, agent_chip_label};

use crate::{
    AgentReview, AgentReviewPanel, ReviewStatus, Summary, ToggleFocus, keep_all, revert_turn,
    status::status_text,
};

struct Setup {
    // Dropping it deletes the local-history database.
    _history: tempfile::TempDir,
    fs: std::sync::Arc<FakeFs>,
    project: Entity<Project>,
    connection: Rc<StubAgentConnection>,
    thread: Entity<AcpThread>,
}

async fn setup(cx: &mut TestAppContext) -> Setup {
    let history = tempfile::tempdir().expect("temp dir");
    let directory = history.path().to_path_buf();
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        zenith_local_history::start_in(directory, cx);
        crate::init(cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/dir", json!({ "a.txt": "base\n" })).await;
    let project = Project::test(fs.clone(), [Path::new("/dir")], cx).await;
    let connection = Rc::new(StubAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&["/dir"]), cx)
        })
        .await
        .expect("stub session");
    cx.run_until_parked();
    Setup {
        _history: history,
        fs,
        project,
        connection,
        thread,
    }
}

/// Starts a turn and leaves it running.
fn start_turn(setup: &Setup, cx: &mut TestAppContext) -> impl Future<Output = ()> + use<> {
    let send = setup
        .thread
        .update(cx, |thread, cx| thread.send_raw("go", cx));
    cx.run_until_parked();
    async move {
        send.await.expect("turn");
    }
}

/// Writes `/dir/a.txt` through the client fs, as an ACP agent does.
async fn agent_writes(setup: &Setup, text: &str, cx: &mut TestAppContext) {
    setup
        .thread
        .update(cx, |thread, cx| {
            thread.write_text_file(PathBuf::from("/dir/a.txt"), text.to_owned(), cx)
        })
        .await
        .expect("agent write");
    cx.run_until_parked();
}

async fn end_turn(setup: &Setup, turn: impl Future<Output = ()>, cx: &mut TestAppContext) {
    let session_id = setup
        .thread
        .read_with(cx, |thread, _| thread.session_id().clone());
    setup
        .connection
        .end_turn(session_id, acp_v1::StopReason::EndTurn);
    turn.await;
    cx.run_until_parked();
}

async fn run_turn(setup: &Setup, text: &str, cx: &mut TestAppContext) {
    let turn = start_turn(setup, cx);
    agent_writes(setup, text, cx).await;
    end_turn(setup, turn, cx).await;
}

async fn buffer(setup: &Setup, cx: &mut TestAppContext) -> Entity<Buffer> {
    setup
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer("/dir/a.txt", cx)
        })
        .await
        .expect("open buffer")
}

async fn labeled(buffer: &Entity<Buffer>, label: &str, cx: &mut TestAppContext) -> Option<String> {
    cx.update(|cx| zenith_local_history::labeled_text(buffer, label, cx))
        .await
        .expect("labeled text")
}

fn label(setup: &Setup, turn: u32, cx: &mut TestAppContext) -> String {
    let id = setup.thread.entity_id();
    cx.update(|cx| {
        AgentReview::global(cx)
            .expect("review")
            .read(cx)
            .thread(id)
            .expect("tracked")
            .label(turn)
    })
}

fn summary(setup: &Setup, cx: &mut TestAppContext) -> Summary {
    let project_id = setup.project.entity_id();
    cx.update(|cx| {
        AgentReview::global(cx)
            .expect("review")
            .read(cx)
            .summary(project_id)
    })
}

#[gpui::test]
async fn test_each_turn_checkpoints_the_files_it_changes(cx: &mut TestAppContext) {
    let setup = setup(cx).await;
    run_turn(&setup, "one\n", cx).await;
    run_turn(&setup, "two\n", cx).await;

    let buffer = buffer(&setup, cx).await;
    // Turn 1: the diff base, i.e. the text on disk before the agent's edit.
    let first = label(&setup, 1, cx);
    assert_eq!(
        labeled(&buffer, &first, cx).await.as_deref(),
        Some("base\n")
    );
    // Turn 2: the file was still pending, so its text when the turn started.
    let second = label(&setup, 2, cx);
    assert_eq!(
        labeled(&buffer, &second, cx).await.as_deref(),
        Some("one\n")
    );
    assert!(
        first.starts_with("agent:") && first.ends_with(":1"),
        "{first}"
    );

    let review = cx.update(|cx| AgentReview::global(cx).expect("review"));
    review.read_with(cx, |review, _| {
        let thread = &review.threads()[0];
        assert_eq!(thread.turn(), 2);
        for turn in &thread.turns {
            assert_eq!(turn.files.len(), 1, "turn {}", turn.number);
            assert!(turn.files[0].checkpointed && !turn.files[0].created);
        }
    });
}

#[gpui::test]
async fn test_revert_turn_restores_the_previous_turn_and_undo_brings_it_back(
    cx: &mut TestAppContext,
) {
    let setup = setup(cx).await;
    run_turn(&setup, "one\n", cx).await;
    run_turn(&setup, "two\n", cx).await;
    let buffer = buffer(&setup, cx).await;

    let missing = cx
        .update(|cx| revert_turn(&setup.thread, 2, cx))
        .await
        .expect("revert");
    cx.run_until_parked();
    assert!(missing.is_empty());
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "one\n");
    let on_disk = setup
        .fs
        .load(Path::new("/dir/a.txt"))
        .await
        .expect("saved file");
    assert_eq!(on_disk, "one\n");

    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "two\n");

    let untracked = cx.update(|cx| revert_turn(&setup.thread, 9, cx)).await;
    assert!(untracked.is_err());
}

#[gpui::test]
async fn test_summary_counts_running_then_review_then_nothing(cx: &mut TestAppContext) {
    let setup = setup(cx).await;
    assert_eq!(summary(&setup, cx), Summary::default());

    let turn = start_turn(&setup, cx);
    assert_eq!(
        summary(&setup, cx),
        Summary {
            running: 1,
            needs_input: 0,
            review: 0,
        }
    );
    agent_writes(&setup, "one\n", cx).await;
    end_turn(&setup, turn, cx).await;
    assert_eq!(
        summary(&setup, cx),
        Summary {
            running: 0,
            needs_input: 0,
            review: 1,
        }
    );
    let id = setup.thread.entity_id();
    let status = cx.update(|cx| {
        AgentReview::global(cx)
            .expect("review")
            .read(cx)
            .thread(id)
            .expect("tracked")
            .status
    });
    assert_eq!(status, ReviewStatus::Review);

    cx.update(|cx| keep_all(&setup.thread, cx));
    cx.run_until_parked();
    assert_eq!(summary(&setup, cx), Summary::default());
}

#[gpui::test]
async fn test_subagent_threads_are_not_tracked(cx: &mut TestAppContext) {
    let setup = setup(cx).await;
    let parent = setup
        .thread
        .read_with(cx, |thread, _| thread.session_id().clone());
    let project = setup.project.clone();
    let connection = setup.connection.clone();
    let _subagent = cx.update(|cx| {
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        cx.new(|cx| {
            AcpThread::new(
                Some(parent),
                None,
                None,
                connection,
                project,
                action_log,
                acp_v2::SessionId::new("child"),
                watch::Receiver::constant(acp_v2::PromptCapabilities::new()),
                cx,
            )
        })
    });
    cx.run_until_parked();
    let tracked = cx.update(|cx| {
        AgentReview::global(cx)
            .expect("review")
            .read(cx)
            .threads()
            .len()
    });
    assert_eq!(tracked, 1, "only the parent thread");
}

/// [`setup`] inside a real window and workspace, so the panel and status item exist.
async fn setup_in_workspace(
    cx: &mut TestAppContext,
) -> (Setup, Entity<Workspace>, &mut VisualTestContext) {
    let history = tempfile::tempdir().expect("temp dir");
    let directory = history.path().to_path_buf();
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        zenith_local_history::start_in(directory, cx);
        crate::init(cx);
        cx.bind_keys([KeyBinding::new(
            "down",
            menu::SelectNext,
            Some("AgentReviewPanel"),
        )]);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree("/dir", json!({ "a.txt": "base\n" })).await;
    let project = Project::test(app_state.fs.clone(), [Path::new("/dir")], cx).await;
    let connection = Rc::new(StubAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&["/dir"]), cx)
        })
        .await
        .expect("stub session");
    thread.update(cx, |thread, cx| {
        thread.set_provisional_title("Fix nonce".into(), cx)
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();
    let setup = Setup {
        _history: history,
        fs,
        project,
        connection,
        thread,
    };
    (setup, workspace, cx)
}

fn panel(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<AgentReviewPanel> {
    workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentReviewPanel>(cx))
        .expect("panel is registered")
}

fn rows(panel: &Entity<AgentReviewPanel>, cx: &mut VisualTestContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| panel.row_texts())
}

#[gpui::test]
async fn test_panel_lists_a_pending_file_and_keep_removes_it(cx: &mut TestAppContext) {
    let (setup, workspace, cx) = setup_in_workspace(cx).await;
    let panel = panel(&workspace, cx);
    assert!(rows(&panel, cx).is_empty(), "no turn yet");

    run_turn(&setup, "one\n", cx).await;
    assert_eq!(
        rows(&panel, cx),
        [
            "Fix nonce, Review, turn 1, 1 file to review",
            "a.txt, +1 −1",
            "Actions for turn 1",
        ]
    );

    // Keyboard path: focus the panel, select the file row, keep it.
    cx.dispatch_action(ToggleFocus);
    cx.run_until_parked();
    cx.simulate_keystrokes("down down");
    assert_eq!(panel.read_with(cx, |panel, _| panel.selected()), Some(1));
    cx.dispatch_action(agent_ui::Keep);
    cx.run_until_parked();
    assert_eq!(
        rows(&panel, cx),
        [
            "Fix nonce, Done, turn 1, 0 files to review",
            "Actions for turn 1"
        ]
    );
    assert_eq!(summary(&setup, cx), Summary::default());
}

#[gpui::test]
async fn test_chip_and_status_text_follow_the_thread(cx: &mut TestAppContext) {
    let (setup, _workspace, cx) = setup_in_workspace(cx).await;
    let project_id = setup.project.entity_id();
    let chip = |cx: &mut VisualTestContext| cx.update(|_, cx| AgentChip::counts(project_id, cx));
    let status = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let review = AgentReview::global(cx).expect("review");
            status_text(review.read(cx), project_id)
        })
    };
    assert_eq!(agent_chip_label(chip(cx)), ("Agents".to_string(), None));
    assert_eq!(status(cx), None);

    let turn = start_turn(&setup, cx);
    assert_eq!(
        chip(cx),
        AgentCounts {
            running: 1,
            review: 0
        }
    );
    assert_eq!(agent_chip_label(chip(cx)).0, "1 running");
    assert_eq!(status(cx).as_deref(), Some("Fix nonce running…"));

    agent_writes(&setup, "one\n", cx).await;
    end_turn(&setup, turn, cx).await;
    assert_eq!(agent_chip_label(chip(cx)), ("review".to_string(), Some(1)));
    assert_eq!(
        status(cx).as_deref(),
        Some("Fix nonce waiting for review (1 file)")
    );

    cx.update(|_, cx| keep_all(&setup.thread, cx));
    cx.run_until_parked();
    assert_eq!(agent_chip_label(chip(cx)), ("Agents".to_string(), None));
    assert_eq!(status(cx), None);
}

// Pre-bundle gate: the panel, its rail button and the status item render in a real
// workspace with an editor open and an agent running, without reading an entity mid-update.
#[gpui::test]
async fn test_opening_a_file_and_toggling_the_panel_renders(cx: &mut TestAppContext) {
    let (setup, workspace, cx) = setup_in_workspace(cx).await;
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                "/dir/a.txt".into(),
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
    let turn = start_turn(&setup, cx);
    agent_writes(&setup, "one\n", cx).await;

    cx.dispatch_action(ToggleFocus);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let panel = panel(&workspace, cx);
    assert!(cx.update(|window, cx| panel.read(cx).focus_handle(cx).contains_focused(window, cx)));
    assert!(cx.debug_bounds("zenith-agent-review-panel").is_some());
    assert!(cx.debug_bounds("zenith-agent-status").is_some());

    end_turn(&setup, turn, cx).await;
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.dispatch_action(ToggleFocus);
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(editor.read_with(cx, |editor, cx| editor.text(cx)), "one\n");
}
