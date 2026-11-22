//! Response history per project, kept on disk (see [`HistoryDir`]): every response a run gets
//! is recorded with credential headers masked, `ShowHistory` lists them newest first (read
//! from disk when the picker opens) and picking one shows it again in the response view.
//! `ClearHistory` deletes the project's history.

use crate::choice_picker::ChoicePicker;
use crate::{HistoryDir, HistoryIndexEntry, HttpRequest, HttpResponse, HttpResponseView};
use anyhow::Result;
use gpui::{App, AppContext as _, Context, Global, TaskExt as _, Window};
use std::path::PathBuf;
use workspace::Workspace;

gpui::actions!(
    zenith_http,
    [
        /// Lists the HTTP responses received in this project; pick one to show it again.
        ShowHistory,
        /// Deletes the HTTP response history of this project.
        ClearHistory,
    ]
);

/// Most responses kept per project (plan 08's `history_limit` default).
pub const HISTORY_LIMIT: usize = 200;
/// Most bytes of history kept per project; the oldest responses go first.
pub const HISTORY_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Where the per-project history folders live, in place of `<data dir>/http-history`.
pub(crate) struct HistoryRoot(pub PathBuf);

impl Global for HistoryRoot {}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowHistory, window, cx| {
            show_history(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ClearHistory, _, cx| {
            clear_history(workspace, cx);
        });
    })
    .detach();
}

fn project_history(workspace: &Workspace, cx: &App) -> Option<HistoryDir> {
    let base = match cx.try_global::<HistoryRoot>() {
        Some(root) => root.0.clone(),
        // Tests that run requests must never write into the real data dir.
        None if cfg!(test) => return None,
        None => paths::data_dir().join("http-history"),
    };
    Some(HistoryDir::for_project(
        &base,
        &workspace.root_paths(cx),
        HISTORY_LIMIT,
        HISTORY_MAX_BYTES,
    ))
}

/// Writes every response in `results` to the project's history in the background; failed
/// requests are skipped.
pub(crate) fn record(
    workspace: &Workspace,
    results: &[(HttpRequest, Result<HttpResponse>)],
    cx: &mut App,
) {
    let Some(history) = project_history(workspace, cx) else {
        return;
    };
    let responses: Vec<(HttpRequest, HttpResponse)> = results
        .iter()
        .filter_map(|(request, response)| Some((request.clone(), response.as_ref().ok()?.clone())))
        .collect();
    if responses.is_empty() {
        return;
    }
    cx.background_spawn(async move {
        for (request, response) in responses {
            history.append(request, response)?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn show_history(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let history = project_history(workspace, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let entries = match history.clone() {
            Some(history) => cx.background_spawn(async move { history.list() }).await,
            None => Vec::new(),
        };
        workspace.update_in(cx, |workspace, window, cx| {
            show_picker(workspace, history, entries, window, cx)
        })
    })
    .detach_and_log_err(cx);
}

fn show_picker(
    workspace: &mut Workspace,
    history: Option<HistoryDir>,
    entries: Vec<HistoryIndexEntry>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (ids, items): (Vec<u64>, Vec<String>) = entries
        .into_iter()
        .map(|entry| {
            let label = format!(
                "{} {} - {} {} ({} ms)",
                entry.method, entry.url, entry.status, entry.status_text, entry.duration_ms
            );
            (entry.id, label)
        })
        .unzip();
    let workspace_handle = cx.weak_entity();
    ChoicePicker::show(
        workspace,
        window,
        cx,
        "Show HTTP response...",
        items,
        move |ix, window, cx| {
            let (Some(id), Some(history)) = (ids.get(ix).copied(), history) else {
                return;
            };
            let load = cx.background_spawn(async move { history.load(id) });
            // Opened once loaded, after the closing picker is gone.
            window
                .spawn(cx, async move |cx| {
                    let entry = load.await;
                    workspace_handle.update_in(cx, |workspace, window, cx| match entry {
                        Ok(entry) => workspace.toggle_modal(window, cx, |_, cx| {
                            HttpResponseView::new(entry.response, entry.request, cx)
                        }),
                        Err(error) => workspace.show_error(error, cx),
                    })
                })
                .detach_and_log_err(cx);
        },
    );
}

fn clear_history(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(history) = project_history(workspace, cx) else {
        return;
    };
    cx.spawn(async move |workspace, cx| {
        if let Err(error) = cx.background_spawn(async move { history.clear() }).await {
            workspace.update(cx, |workspace, cx| workspace.show_error(error, cx))?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MASKED_VALUE;
    use crate::gutter::tests::{TWO_REQUESTS, open};
    use anyhow::anyhow;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;

    async fn open_with_history(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<Workspace>, &mut VisualTestContext) {
        let temp = tempfile::tempdir().expect("temp dir");
        let (workspace, _, cx) = open(json!({ "api.http": TWO_REQUESTS }), "api.http", cx).await;
        let root = temp.path().to_path_buf();
        cx.update(|_, cx| cx.set_global(HistoryRoot(root)));
        (temp, workspace, cx)
    }

    fn stored(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> HistoryDir {
        workspace
            .read_with(cx, |workspace, cx| project_history(workspace, cx))
            .expect("the history root is set")
    }

    #[gpui::test]
    async fn test_record_writes_masked_responses_to_disk(cx: &mut TestAppContext) {
        let (_temp, workspace, cx) = open_with_history(cx).await;
        let mut request = HttpRequest::new("GET", "https://a.test/0", 1);
        request.headers = vec![("Authorization".into(), "Bearer t".into())];
        workspace.update_in(cx, |workspace, _, cx| {
            record(
                workspace,
                &[
                    (request, Ok(HttpResponse::new(200, "OK", vec![], "", 1))),
                    (
                        HttpRequest::new("GET", "https://a.test/failed", 3),
                        Err(anyhow!("offline")),
                    ),
                ],
                cx,
            )
        });
        cx.run_until_parked();

        let history = stored(&workspace, cx);
        let entries = history.list();
        let urls: Vec<&str> = entries.iter().map(|entry| entry.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.test/0"], "failed requests skipped");
        let item = history.load(entries[0].id).expect("entry file");
        assert_eq!(item.request.headers[0].1, MASKED_VALUE);
    }

    #[gpui::test]
    async fn test_show_history_lists_responses_in_a_picker(cx: &mut TestAppContext) {
        let (_temp, workspace, cx) = open_with_history(cx).await;
        workspace.update_in(cx, |workspace, _, cx| {
            record(
                workspace,
                &[(
                    HttpRequest::new("GET", "https://a.test/one", 2),
                    Ok(HttpResponse::new(200, "OK", vec![], "{}", 3)),
                )],
                cx,
            )
        });
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            show_history(workspace, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ChoicePicker>(cx).is_some());
        });
    }

    #[gpui::test]
    async fn test_clear_history_deletes_the_project_history(cx: &mut TestAppContext) {
        let (_temp, workspace, cx) = open_with_history(cx).await;
        workspace.update_in(cx, |workspace, _, cx| {
            record(
                workspace,
                &[(
                    HttpRequest::new("GET", "https://a.test/one", 2),
                    Ok(HttpResponse::new(200, "OK", vec![], "{}", 3)),
                )],
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(stored(&workspace, cx).list().len(), 1);

        workspace.update_in(cx, |workspace, _, cx| clear_history(workspace, cx));
        cx.run_until_parked();
        assert!(stored(&workspace, cx).list().is_empty());
    }
}
