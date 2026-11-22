//! Hooks the HTTP client into the workspace: `RunRequest` sends the request under the cursor
//! of the active `.http` file and shows the response in a modal, `RunAllRequests` sends every
//! request of the file in order and shows a summary, `SelectEnvironment` picks the environment
//! (from `http-client.env.json`) whose variables later runs use, and `CopyAsCurl` copies the
//! request as a curl command.

use crate::choice_picker::ChoicePicker;
use crate::{
    CopyAsCurl, EnvironmentStore, HttpRequest, HttpResponse, HttpResponseView, ImportFromCurl,
    RunAllRequests, RunRequest, SelectEnvironment, execute_request_with_body, parse_curl,
    parse_http_file, upload,
};
use anyhow::{Result, anyhow, bail};
use editor::Editor;
use fs::Fs;
use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, EventEmitter, Global, Window,
};
use language::Point;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

impl EventEmitter<DismissEvent> for HttpResponseView {}
impl ModalView for HttpResponseView {}

/// The environment picked with `SelectEnvironment`, kept until the app closes.
struct SessionEnvironment(Option<String>);
impl Global for SessionEnvironment {}

const PUBLIC_ENV_FILE: &str = "http-client.env.json";
const PRIVATE_ENV_FILE: &str = "http-client.private.env.json";

pub fn init(cx: &mut App) {
    crate::gutter::init(cx);
    crate::history_view::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunRequest, window, cx| {
            let Some(file) = active_http_file(workspace, cx) else {
                return;
            };
            let requests = parse_http_file(&file.text);
            if let Some(request) = request_for_line(&file.text, &requests, file.line).cloned() {
                run_requests(workspace, vec![request], file.dir, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &RunAllRequests, window, cx| {
            let Some(file) = active_http_file(workspace, cx) else {
                return;
            };
            let requests = parse_http_file(&file.text);
            if !requests.is_empty() {
                run_requests(workspace, requests, file.dir, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &CopyAsCurl, _, cx| {
            let Some(file) = active_http_file(workspace, cx) else {
                return;
            };
            let requests = parse_http_file(&file.text);
            if let Some(request) = request_for_line(&file.text, &requests, file.line) {
                cx.write_to_clipboard(ClipboardItem::new_string(request.to_curl()));
            }
        });
        workspace.register_action(|workspace, _: &SelectEnvironment, window, cx| {
            select_environment(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ImportFromCurl, window, cx| {
            import_from_curl(workspace, window, cx);
        });
    })
    .detach();
}

/// The active editor's `.http` file: its text, the 1-based cursor line, its directory.
struct HttpFile {
    text: String,
    line: usize,
    dir: Option<PathBuf>,
}

fn active_http_file(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<HttpFile> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let line = editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
            .row as usize
            + 1;
        let buffer = buffer.read(cx);
        let file = buffer.file()?;
        if !file
            .full_path(cx)
            .extension()
            .is_some_and(|ext| ext == "http" || ext == "rest")
        {
            return None;
        }
        let dir = file
            .as_local()
            .and_then(|file| file.abs_path(cx).parent().map(Path::to_path_buf));
        Some(HttpFile {
            text: buffer.text(),
            line,
            dir,
        })
    })
}

fn is_comment_or_blank(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with('#') || line.starts_with("//")
}

/// The request the cursor line belongs to: the last one whose request line is at or above it.
/// On a `###` or comment line above a request (nothing but comments and blanks between) that
/// request is meant instead.
fn request_for_line<'a>(
    text: &str,
    requests: &'a [HttpRequest],
    line: usize,
) -> Option<&'a HttpRequest> {
    let current = requests.iter().rev().find(|r| r.line_number <= line);
    let next = requests.iter().find(|r| r.line_number > line);
    let lines: Vec<&str> = text.lines().collect();
    let on_comment = lines
        .get(line - 1)
        .is_some_and(|text| text.trim().starts_with('#') || text.trim().starts_with("//"));
    match next {
        Some(next)
            if on_comment
                && lines
                    .get(line..next.line_number - 1)
                    .is_some_and(|between| between.iter().all(|l| is_comment_or_blank(l))) =>
        {
            Some(next)
        }
        _ => current,
    }
}

/// Where the environment files are looked for: the `.http` file's directory, then the
/// project's folders (local projects only).
fn env_dirs(workspace: &Workspace, file_dir: Option<PathBuf>, cx: &App) -> Vec<PathBuf> {
    let project = workspace.project().read(cx);
    let mut dirs: Vec<PathBuf> = file_dir.into_iter().collect();
    if project.is_local() {
        dirs.extend(
            project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf()),
        );
    }
    dirs
}

/// Environment files are small JSON; anything bigger is not one.
const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;

/// A regular environment file at `path`. Symlinks are refused: a cloned project could
/// otherwise point the private file at any JSON secret on disk and send it in a request.
async fn read_env_file(fs: &Arc<dyn Fs>, path: &Path) -> Result<Option<String>> {
    let Some(metadata) = fs.metadata(path).await? else {
        return Ok(None);
    };
    if metadata.is_dir || metadata.is_fifo {
        return Ok(None);
    }
    if metadata.is_symlink {
        bail!(
            "{} is a symlink; environment files must be regular files",
            path.display()
        );
    }
    if metadata.len > MAX_ENV_FILE_BYTES {
        bail!("{} is larger than 1 MB", path.display());
    }
    Ok(Some(fs.load(path).await?))
}

/// Public and private files come from the same folder: the first one that has either, so
/// one folder's requests never pick up another folder's secrets.
async fn load_environments(fs: &Arc<dyn Fs>, dirs: &[PathBuf]) -> Result<EnvironmentStore> {
    let (mut public, mut private) = (None, None);
    for dir in dirs {
        public = read_env_file(fs, &dir.join(PUBLIC_ENV_FILE)).await?;
        private = read_env_file(fs, &dir.join(PRIVATE_ENV_FILE)).await?;
        if public.is_some() || private.is_some() {
            break;
        }
    }
    let mut store = EnvironmentStore::new();
    store
        .load_from_json(public.as_deref().unwrap_or_default(), private.as_deref())
        .map_err(|error| anyhow!(error))?;
    Ok(store)
}

fn selected_environment(cx: &App) -> Option<String> {
    cx.try_global::<SessionEnvironment>()
        .and_then(|session| session.0.clone())
}

fn select_environment(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let file_dir = active_http_file(workspace, cx).and_then(|file| file.dir);
    let dirs = env_dirs(workspace, file_dir, cx);
    let fs = workspace.project().read(cx).fs().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let store = cx
            .background_spawn(async move { load_environments(&fs, &dirs).await })
            .await;
        workspace
            .update_in(cx, |workspace, window, cx| match store {
                Ok(store) if !store.environments.is_empty() => {
                    let current = selected_environment(cx);
                    let names = store.names();
                    let mut items = vec!["No environment".to_string()];
                    items.extend(names.iter().map(|name| {
                        if current.as_ref() == Some(name) {
                            format!("{name} (active)")
                        } else {
                            name.clone()
                        }
                    }));
                    ChoicePicker::show(
                        workspace,
                        window,
                        cx,
                        "Select environment...",
                        items,
                        move |ix, _, cx| {
                            let name = ix.checked_sub(1).and_then(|ix| names.get(ix)).cloned();
                            cx.set_global(SessionEnvironment(name));
                        },
                    );
                }
                Ok(_) => workspace.show_error(
                    anyhow!("No environments found: add {PUBLIC_ENV_FILE} next to the .http file or in the project root"),
                    cx,
                ),
                Err(error) => workspace.show_error(error, cx),
            })
            .log_err();
    })
    .detach();
}

/// Inserts the curl command on the clipboard, as a request, at the cursor of the active
/// `.http` editor.
fn import_from_curl(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if active_http_file(workspace, cx).is_none() {
        workspace.show_error(
            anyhow!("Open a .http file to import a cURL command into"),
            cx,
        );
        return;
    }
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
    match clipboard.as_deref().map(parse_curl) {
        Some(Ok(request)) => {
            let text = request.to_http_text();
            editor.update(cx, |editor, cx| editor.insert(&text, window, cx));
        }
        Some(Err(error)) => {
            workspace.show_error(anyhow!("The clipboard is not a cURL command: {error}"), cx)
        }
        None => workspace.show_error(anyhow!("Copy a cURL command to the clipboard first"), cx),
    }
}

/// Sends `requests` one after the other with the selected environment's variables. One
/// request shows its response, several show a summary of them all.
fn run_requests(
    workspace: &Workspace,
    requests: Vec<HttpRequest>,
    file_dir: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let client = cx.http_client();
    let fs = workspace.project().read(cx).fs().clone();
    let dirs = env_dirs(workspace, file_dir.clone(), cx);
    // `< file` bodies stay inside the worktree holding the .http file (its folder when none does).
    let root = file_dir.as_ref().map(|dir| {
        workspace
            .project()
            .read(cx)
            .find_worktree(dir, cx)
            .map_or_else(
                || dir.clone(),
                |(worktree, _)| worktree.read(cx).abs_path().to_path_buf(),
            )
    });
    let selected = selected_environment(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let results = cx
            .background_spawn(async move {
                let variables = match selected {
                    Some(name) => load_environments(&fs, &dirs)
                        .await?
                        .environments
                        .get(&name)
                        .cloned()
                        .unwrap_or_default(),
                    None => HashMap::new(),
                };
                let mut results = Vec::new();
                for request in requests {
                    let body = upload::request_body(
                        &request,
                        &variables,
                        &fs,
                        file_dir.as_deref(),
                        root.as_deref(),
                    )
                    .await;
                    let response = match body {
                        Ok(body) => {
                            execute_request_with_body(
                                &request,
                                client.clone(),
                                Some(&variables),
                                body,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    results.push((request, response));
                }
                anyhow::Ok(results)
            })
            .await;
        workspace
            .update_in(cx, |workspace, window, cx| match results {
                Ok(results) => {
                    crate::history_view::record(workspace, &results, cx);
                    show_results(workspace, results, window, cx)
                }
                Err(error) => workspace.show_error(error, cx),
            })
            .log_err();
    })
    .detach();
}

fn show_results(
    workspace: &mut Workspace,
    mut results: Vec<(HttpRequest, Result<HttpResponse>)>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if results.len() == 1
        && let Some((request, response)) = results.pop()
    {
        match response {
            Ok(response) => {
                workspace.toggle_modal(window, cx, |_, cx| {
                    HttpResponseView::new(response, request, cx)
                });
            }
            Err(error) => workspace.show_error(error, cx),
        }
        return;
    }
    let total = results.len();
    let mut failed = 0;
    let mut duration_ms = 0;
    let mut lines = Vec::new();
    for (index, (request, response)) in results.iter().enumerate() {
        let outcome = match response {
            Ok(response) => {
                duration_ms += response.duration_ms;
                if !response.is_success() {
                    failed += 1;
                }
                format!(
                    "{} {} ({} ms)",
                    response.status, response.status_text, response.duration_ms
                )
            }
            Err(error) => {
                failed += 1;
                format!("failed: {error:#}")
            }
        };
        lines.push(format!(
            "{}. {} {} -> {outcome}",
            index + 1,
            request.method,
            request.url
        ));
    }
    let status = if failed == 0 { 200 } else { 500 };
    let text = format!("{} of {total} requests succeeded", total - failed);
    let summary = HttpResponse::new(status, &text, Vec::new(), &lines.join("\n"), duration_ms);
    let line_number = results
        .first()
        .map_or(0, |(request, _)| request.line_number);
    let request = HttpRequest::new("RUN ALL", &format!("{total} requests"), line_number);
    workspace.toggle_modal(window, cx, |_, cx| {
        HttpResponseView::new(summary, request, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::request_for_line;
    use crate::parser::parse_http_file;

    #[test]
    fn test_request_for_line_picks_the_request_the_cursor_belongs_to() {
        let text = "### First\nGET https://a.test/one\n\n### Second\n# note\nPOST https://a.test/two\nContent-Type: application/json\n";
        let requests = parse_http_file(text);
        assert_eq!(requests.len(), 2);
        let url = |line| request_for_line(text, &requests, line).map(|r| r.url.as_str());
        assert_eq!(url(1), Some("https://a.test/one"), "### above the first");
        assert_eq!(url(2), Some("https://a.test/one"));
        assert_eq!(url(3), Some("https://a.test/one"), "blank line after it");
        assert_eq!(url(4), Some("https://a.test/two"), "### above the second");
        assert_eq!(
            url(5),
            Some("https://a.test/two"),
            "comment above the second"
        );
        assert_eq!(url(7), Some("https://a.test/two"), "header line");
        assert_eq!(url(99), Some("https://a.test/two"), "past the end");
    }
}
