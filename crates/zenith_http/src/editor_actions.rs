//! Hooks the HTTP client into the workspace: `RunRequest` sends the request under the cursor
//! of the active `.http` file and shows the response in a modal; `CopyAsCurl` copies it as a
//! curl command.

use crate::{
    CopyAsCurl, HttpRequest, HttpResponseView, RunRequest, execute_request, parse_http_file,
};
use editor::Editor;
use gpui::{App, AppContext as _, ClipboardItem, Context, DismissEvent, EventEmitter, Window};
use language::Point;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

impl EventEmitter<DismissEvent> for HttpResponseView {}
impl ModalView for HttpResponseView {}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RunRequest, window, cx| {
            if let Some(request) = request_at_cursor(workspace, cx) {
                run_request(request, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &CopyAsCurl, _, cx| {
            if let Some(request) = request_at_cursor(workspace, cx) {
                cx.write_to_clipboard(ClipboardItem::new_string(request.to_curl()));
            }
        });
    })
    .detach();
}

/// The request whose block holds the cursor of the active editor, if that is a `.http` file.
fn request_at_cursor(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<HttpRequest> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let is_http = buffer
            .read(cx)
            .file()
            .is_some_and(|file| file.full_path(cx).extension().is_some_and(|ext| ext == "http"));
        if !is_http {
            return None;
        }
        // Line numbers in a parsed request are 1-based.
        let line = editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
            .row as usize
            + 1;
        let text = buffer.read(cx).text();
        parse_http_file(&text)
            .into_iter()
            .rev()
            .find(|request| request.line_number <= line)
    })
}

fn run_request(request: HttpRequest, window: &mut Window, cx: &mut Context<Workspace>) {
    let client = cx.http_client();
    cx.spawn_in(window, async move |workspace, cx| {
        let response = cx
            .background_spawn({
                let request = request.clone();
                async move { execute_request(&request, client, None).await }
            })
            .await;
        workspace
            .update_in(cx, |workspace, window, cx| match response {
                Ok(response) => {
                    workspace.toggle_modal(window, cx, |_, cx| {
                        HttpResponseView::new(response, request, cx)
                    });
                }
                Err(error) => workspace.show_error(error, cx),
            })
            .log_err();
    })
    .detach();
}
