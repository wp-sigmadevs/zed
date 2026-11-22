//! The gutter ▶ on every request line of a `.http` / `.rest` file: a click puts the cursor on
//! that line and runs `RunRequest`, exactly like `ctrl-enter` there.
//!
//! Zed's runnables gutter is not used: it needs a tree-sitter grammar with a `runnables.scm`
//! (no HTTP grammar ships) and its button spawns task templates (shell commands) through the
//! code-actions menu, never a GPUI action. Instead each request line gets an empty inline
//! crease whose custom toggle (drawn in the fold column) is our button; no upstream change.
//! Like every fold toggle it shows only while `gutter.folds` is on (the default).

use crate::{RunRequest, parse_http_file};
use editor::display_map::{Crease, CreaseId};
use editor::{Addon, Editor, EditorEvent, FoldPlaceholder};
use gpui::{Empty, Focusable as _, WeakEntity};
use language::Point;
use std::any::Any;
use std::path::Path;
use ui::{IconButtonShape, Tooltip, prelude::*};

/// The request rows (0-based) an editor shows a button on, and the creases that draw them.
struct RunButtons {
    rows: Vec<u32>,
    creases: Vec<CreaseId>,
}

impl Addon for RunButtons {
    fn to_any(&self) -> &dyn Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
            return;
        }
        // A save-as can make a file `.http` later; edits move the request lines.
        cx.subscribe(&cx.entity(), |editor, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited | EditorEvent::TitleChanged) {
                refresh(editor, cx);
            }
        })
        .detach();
        refresh(editor, cx);
    })
    .detach();
}

pub(crate) fn is_http_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "http" || ext == "rest")
}

/// The 0-based rows of the request lines (`GET https://…`) in `text`.
pub(crate) fn request_rows(text: &str) -> Vec<u32> {
    parse_http_file(text)
        .iter()
        .map(|request| request.line_number.saturating_sub(1) as u32)
        .collect()
}

fn refresh(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let rows = {
        let buffer = buffer.read(cx);
        if buffer
            .file()
            .is_some_and(|file| is_http_path(&file.full_path(cx)))
        {
            request_rows(&buffer.text())
        } else {
            Vec::new()
        }
    };
    let old = match editor.addon_mut::<RunButtons>() {
        Some(buttons) if buttons.rows == rows => return,
        Some(buttons) => std::mem::take(&mut buttons.creases),
        None if rows.is_empty() => return,
        None => Vec::new(),
    };
    editor.remove_creases(old, cx);

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let this = cx.weak_entity();
    let creases = rows.iter().map(|&row| {
        // Empty range: folding it is a no-op, so the crease only carries the button.
        let at = snapshot.anchor_before(Point::new(row, 0));
        let editor = this.clone();
        Crease::inline(
            at..at,
            FoldPlaceholder::default(),
            move |_, _, _, _: &mut Window, _: &mut App| run_button(&editor, row),
            |_, _, _: &mut Window, _: &mut App| Empty,
        )
    });
    let creases = editor.insert_creases(creases, cx);
    editor.register_addon(RunButtons { rows, creases });
    cx.notify();
}

fn run_button(editor: &WeakEntity<Editor>, row: u32) -> IconButton {
    let editor = editor.clone();
    IconButton::new(("http-run-request", row as usize), IconName::PlayOutlined)
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::XSmall)
        .icon_color(Color::Success)
        .aria_label("Run request")
        .tooltip(Tooltip::text("Run request"))
        .on_click(move |_, window, cx| {
            let Some(editor) = editor.upgrade() else {
                return;
            };
            editor.update(cx, |editor, cx| {
                window.focus(&editor.focus_handle(cx), cx);
                let at = Point::new(row, 0);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([at..at])
                });
            });
            window.dispatch_action(Box::new(RunRequest), cx);
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{Project, ProjectPath};
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace, Workspace};

    pub(crate) const TWO_REQUESTS: &str = "### First\nGET https://a.test/one\nAccept: */*\n\n### Second\n# note\nPOST https://a.test/two\nContent-Type: application/json\n\n{\"a\": 1}\n";

    #[test]
    fn test_a_row_per_request_line() {
        assert_eq!(request_rows(TWO_REQUESTS), vec![1, 6]);
        assert_eq!(request_rows("GET https://a.test/\n"), vec![0]);
        assert_eq!(request_rows("# only a comment\n\n"), Vec::<u32>::new());
        assert!(is_http_path(Path::new("api/requests.http")));
        assert!(is_http_path(Path::new("requests.rest")));
        assert!(!is_http_path(Path::new("notes.txt")));
        assert!(!is_http_path(Path::new("http")));
    }

    /// A workspace over `/p` holding `files`, with `name` open and active.
    pub(crate) async fn open<'a>(
        files: serde_json::Value,
        name: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<Editor>, &'a mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        app_state.fs.as_fake().insert_tree(path!("/p"), files).await;
        let project = Project::test(app_state.fs.clone(), [path!("/p").as_ref()], cx).await;
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path(name).into(),
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        (workspace, editor, cx)
    }

    fn buttons(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Option<(Vec<u32>, usize)> {
        editor.read_with(cx, |editor, _| {
            editor
                .addon::<RunButtons>()
                .map(|buttons| (buttons.rows.clone(), buttons.creases.len()))
        })
    }

    #[gpui::test]
    async fn test_button_on_each_request_line_follows_edits(cx: &mut TestAppContext) {
        let (_, editor, cx) = open(json!({ "api.http": TWO_REQUESTS }), "api.http", cx).await;
        assert_eq!(buttons(&editor, cx), Some((vec![1, 6], 2)));

        editor.update(cx, |editor, cx| {
            editor.edit([(Point::new(0, 0)..Point::new(0, 0), "\n")], cx)
        });
        cx.run_until_parked();
        assert_eq!(
            buttons(&editor, cx),
            Some((vec![2, 7], 2)),
            "old creases replaced, not added to"
        );

        editor.update(cx, |editor, cx| {
            editor.edit(
                [(
                    Point::new(0, 0)..Point::new(0, 0),
                    "GET https://a.test/zero\n###\n",
                )],
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(buttons(&editor, cx), Some((vec![0, 4, 9], 3)));
    }

    #[gpui::test]
    async fn test_no_button_outside_http_files(cx: &mut TestAppContext) {
        let (_, editor, cx) = open(json!({ "notes.txt": TWO_REQUESTS }), "notes.txt", cx).await;
        assert_eq!(buttons(&editor, cx), None);
    }
}
