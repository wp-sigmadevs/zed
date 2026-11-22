use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use acp_thread::{AcpThread, AgentConnection as _, StubAgentConnection};
use action_log::ActionLog;
use agent_client_protocol::{v1::schema as acp_v1, v2::schema as acp_v2};
use gpui::{AppContext as _, Entity, TestAppContext};
use language::Buffer;
use project::{FakeFs, Fs as _, Project};
use serde_json::json;
use settings::SettingsStore;
use util::path_list::PathList;

use crate::{AgentReview, ReviewStatus, Summary, keep_all, revert_turn};

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
