//! Hooks the postfix expander into the workspace: `ExpandPostfix` rewrites the
//! `expr.trigger` before the cursor in the active editor.

use crate::{ExpandPostfix, LanguageKind, expand_postfix, extract_expression_and_trigger};
use editor::Editor;
use gpui::{App, Context, Window};
use language::Point;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ExpandPostfix, window, cx| {
            if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                editor.update(cx, |editor, cx| expand_at_cursor(editor, window, cx));
            }
        });
    })
    .detach();
}

fn expand_at_cursor(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let Some(lang) = buffer
        .read(cx)
        .file()
        .and_then(|file| LanguageKind::from_path(&file.full_path(cx)))
    else {
        return;
    };
    let selection = editor.selections.newest::<Point>(&editor.display_snapshot(cx));
    if !selection.is_empty() {
        return;
    }
    let cursor = selection.head();
    let snapshot = buffer.read(cx).snapshot();
    let line_start = Point::new(cursor.row, 0);
    let prefix: String = snapshot.text_for_range(line_start..cursor).collect();
    let Some((expr, _)) = extract_expression_and_trigger(&prefix) else {
        return;
    };
    let Some(result) = expand_postfix(&prefix, lang) else {
        return;
    };
    // The text to replace is the expression, the dot and the trigger.
    let Some(dot) = prefix.trim_end().rfind('.') else {
        return;
    };
    let Some(expr_start) = prefix.get(..dot).and_then(|before| before.rfind(&expr)) else {
        return;
    };
    let indent: String = prefix.chars().take_while(|c| c.is_whitespace()).collect();
    let text = result.replacement.replace('\n', &format!("\n{indent}"));
    let before_cursor = result
        .cursor_offset
        .and_then(|offset| result.replacement.get(..offset))
        .map(|head| head.replace('\n', &format!("\n{indent}")));
    let start = Point::new(cursor.row, expr_start as u32);
    editor.edit([(start..cursor, text.as_str())], cx);
    if let Some(head) = before_cursor {
        let lines = head.matches('\n').count() as u32;
        let column = match head.rfind('\n') {
            Some(newline) => head.len() - newline - 1,
            None => expr_start + head.len(),
        };
        let target = Point::new(cursor.row + lines, column as u32);
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([target..target])
        });
    }
}
