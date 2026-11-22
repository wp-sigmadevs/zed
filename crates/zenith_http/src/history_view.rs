//! Response history for the app session: every response a run gets is recorded (credential
//! headers masked, see [`HttpHistoryStore::add`]) and `ShowHistory` lists them, newest first;
//! picking one shows it again in the response view.

use crate::choice_picker::ChoicePicker;
use crate::{HttpHistoryStore, HttpRequest, HttpResponse, HttpResponseView};
use anyhow::Result;
use gpui::{App, Context, Global, Window};
use workspace::Workspace;

gpui::actions!(
    zenith_http,
    [
        /// Lists the HTTP responses received in this session; pick one to show it again.
        ShowHistory,
    ]
);

/// Most responses kept (plan 08's `history_limit` default).
/// shortcut: in memory for the app session, no `history_limit` setting yet; move to the
/// on-disk per-project store of plan 08 with a setting when history must survive a restart.
pub const HISTORY_LIMIT: usize = 200;

struct SessionHistory(HttpHistoryStore);

impl Global for SessionHistory {}

impl Default for SessionHistory {
    fn default() -> Self {
        Self(HttpHistoryStore::new(HISTORY_LIMIT))
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ShowHistory, window, cx| {
            show_history(workspace, window, cx);
        });
    })
    .detach();
}

/// Adds every response in `results` to the session history; failed requests are skipped.
pub(crate) fn record(results: &[(HttpRequest, Result<HttpResponse>)], cx: &mut App) {
    let history = &mut cx.default_global::<SessionHistory>().0;
    for (request, response) in results {
        if let Ok(response) = response {
            history.add(request.clone(), response.clone());
        }
    }
}

fn show_history(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let (ids, items): (Vec<String>, Vec<String>) = cx
        .try_global::<SessionHistory>()
        .map(|history| {
            history
                .0
                .entries
                .iter()
                .map(|entry| {
                    let label = format!(
                        "{} {} - {} {} ({} ms)",
                        entry.request.method,
                        entry.request.url,
                        entry.response.status,
                        entry.response.status_text,
                        entry.response.duration_ms
                    );
                    (entry.id.clone(), label)
                })
                .unzip()
        })
        .unwrap_or_default();
    let workspace_handle = cx.weak_entity();
    ChoicePicker::show(
        workspace,
        window,
        cx,
        "Show HTTP response...",
        items,
        move |ix, window, cx| {
            let Some(id) = ids.get(ix).cloned() else {
                return;
            };
            // The picker is still closing: open the response once it is gone.
            window.defer(cx, move |window, cx| {
                let Some(entry) = cx.try_global::<SessionHistory>().and_then(|history| {
                    history
                        .0
                        .entries
                        .iter()
                        .find(|entry| entry.id == id)
                        .cloned()
                }) else {
                    return;
                };
                workspace_handle
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |_, cx| {
                            HttpResponseView::new(entry.response, entry.request, cx)
                        });
                    })
                    .ok();
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MASKED_VALUE;
    use crate::gutter::tests::{TWO_REQUESTS, open};
    use anyhow::anyhow;
    use gpui::TestAppContext;
    use serde_json::json;

    fn entries(cx: &App) -> Vec<(String, u16)> {
        cx.try_global::<SessionHistory>()
            .map(|history| {
                history
                    .0
                    .entries
                    .iter()
                    .map(|entry| (entry.request.url.clone(), entry.response.status))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[gpui::test]
    fn test_record_keeps_responses_masked_and_capped(cx: &mut App) {
        let mut request = HttpRequest::new("GET", "https://a.test/0", 1);
        request.headers = vec![("Authorization".into(), "Bearer t".into())];
        record(
            &[
                (request, Ok(HttpResponse::new(200, "OK", vec![], "", 1))),
                (
                    HttpRequest::new("GET", "https://a.test/failed", 3),
                    Err(anyhow!("offline")),
                ),
            ],
            cx,
        );
        assert_eq!(entries(cx), vec![("https://a.test/0".to_string(), 200)]);
        let history = &cx.global::<SessionHistory>().0;
        assert_eq!(
            history.entries[0].request.headers[0].1, MASKED_VALUE,
            "recorded through the masking store"
        );

        for n in 1..=HISTORY_LIMIT {
            record(
                &[(
                    HttpRequest::new("GET", &format!("https://a.test/{n}"), 1),
                    Ok(HttpResponse::new(201, "Created", vec![], "", 1)),
                )],
                cx,
            );
        }
        let kept = entries(cx);
        assert_eq!(kept.len(), HISTORY_LIMIT);
        assert_eq!(kept[0].0, format!("https://a.test/{HISTORY_LIMIT}"));
        assert!(
            !kept.iter().any(|(url, _)| url == "https://a.test/0"),
            "the oldest is dropped"
        );
    }

    #[gpui::test]
    async fn test_show_history_lists_responses_in_a_picker(cx: &mut TestAppContext) {
        let (workspace, _, cx) = open(json!({ "api.http": TWO_REQUESTS }), "api.http", cx).await;
        cx.update(|_, cx| {
            record(
                &[(
                    HttpRequest::new("GET", "https://a.test/one", 2),
                    Ok(HttpResponse::new(200, "OK", vec![], "{}", 3)),
                )],
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            show_history(workspace, window, cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal::<ChoicePicker>(cx).is_some());
        });
    }
}
