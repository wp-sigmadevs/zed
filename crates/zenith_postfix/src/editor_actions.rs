//! Hooks the postfix expander into the workspace: `ExpandPostfix` rewrites the
//! `expr.trigger` before the cursor in the active editor, `SurroundWith` wraps the
//! selected lines in a block and `LiveTemplates` expands the abbreviation before the
//! cursor (or lists the templates when there is none).

use crate::choice_picker::ChoicePicker;
use crate::live_templates::{LiveTemplate, live_templates};
use crate::{
    ExpandPostfix, LanguageKind, LiveTemplateContext, LiveTemplates, SurroundKind, SurroundWith,
    expand_live_template, expand_postfix, extract_expression_and_trigger, surround_with,
};
use editor::Editor;
use gpui::{App, Context, Window};
use language::Point;
use workspace::Workspace;

const SURROUNDS: [(SurroundKind, &str); 5] = [
    (SurroundKind::TryCatch, "try / catch"),
    (SurroundKind::If, "if"),
    (SurroundKind::Foreach, "foreach / for...of"),
    (SurroundKind::While, "while"),
    (SurroundKind::Function, "function"),
];

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ExpandPostfix, window, cx| {
            if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                editor.update(cx, |editor, cx| expand_at_cursor(editor, window, cx));
            }
        });
        workspace.register_action(|workspace, _: &SurroundWith, window, cx| {
            let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                return;
            };
            let Some(lang) = editor.update(cx, |editor, cx| buffer_language(editor, cx)) else {
                return;
            };
            let items = SURROUNDS
                .iter()
                .map(|(_, label)| label.to_string())
                .collect();
            ChoicePicker::show(
                workspace,
                window,
                cx,
                "Surround with...",
                items,
                move |ix, window, cx| {
                    let Some((kind, _)) = SURROUNDS.get(ix) else {
                        return;
                    };
                    editor.update(cx, |editor, cx| {
                        surround_selection(editor, *kind, lang, window, cx)
                    });
                },
            );
        });
        workspace.register_action(|workspace, _: &LiveTemplates, window, cx| {
            let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                return;
            };
            let Some(lang) = editor.update(cx, |editor, cx| buffer_language(editor, cx)) else {
                return;
            };
            let templates = live_templates(lang);
            // An abbreviation right before the cursor expands at once.
            if let Some(template) = editor.update(cx, |editor, cx| {
                abbreviation_before_cursor(editor, cx)
                    .and_then(|word| templates.iter().find(|t| t.abbreviation == word))
            }) {
                editor.update(cx, |editor, cx| {
                    expand_template(editor, template, true, window, cx)
                });
                return;
            }
            let items = templates
                .iter()
                .map(|t| format!("{}  {}", t.abbreviation, t.description))
                .collect();
            ChoicePicker::show(
                workspace,
                window,
                cx,
                "Insert live template...",
                items,
                move |ix, window, cx| {
                    let Some(template) = templates.get(ix) else {
                        return;
                    };
                    editor.update(cx, |editor, cx| {
                        expand_template(editor, template, false, window, cx)
                    });
                },
            );
        });
    })
    .detach();
}

fn buffer_language(editor: &Editor, cx: &App) -> Option<LanguageKind> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    LanguageKind::from_path(&file.full_path(cx))
}

fn expand_at_cursor(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let Some(lang) = buffer_language(editor, cx) else {
        return;
    };
    let selection = editor
        .selections
        .newest::<Point>(&editor.display_snapshot(cx));
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
    let start = Point::new(cursor.row, expr_start as u32);
    replace_with_cursor(
        editor,
        start..cursor,
        &result.replacement,
        result.cursor_offset,
        &indent,
        window,
        cx,
    );
}

/// Replaces `range` (on one line) with `replacement`, indenting its later lines like the
/// first, and puts the cursor `cursor_offset` bytes into the replacement when given.
fn replace_with_cursor(
    editor: &mut Editor,
    range: std::ops::Range<Point>,
    replacement: &str,
    cursor_offset: Option<usize>,
    indent: &str,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let text = replacement.replace('\n', &format!("\n{indent}"));
    let before_cursor = cursor_offset
        .and_then(|offset| replacement.get(..offset))
        .map(|head| head.replace('\n', &format!("\n{indent}")));
    editor.edit([(range.clone(), text.as_str())], cx);
    if let Some(head) = before_cursor {
        let lines = head.matches('\n').count() as u32;
        let column = match head.rfind('\n') {
            Some(newline) => head.len() - newline - 1,
            None => range.start.column as usize + head.len(),
        };
        let target = Point::new(range.start.row + lines, column as u32);
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([target..target])
        });
    }
}

/// Wraps the lines the selection touches in a block of `kind`.
fn surround_selection(
    editor: &mut Editor,
    kind: SurroundKind,
    lang: LanguageKind,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let selection = editor
        .selections
        .newest::<Point>(&editor.display_snapshot(cx));
    if selection.is_empty() {
        return;
    }
    let (start, end) = (selection.start, selection.end);
    // A selection ending at column 0 does not take the line it ends on.
    let last_row = if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    };
    let snapshot = buffer.read(cx).snapshot();
    let range = Point::new(start.row, 0)..Point::new(last_row, snapshot.line_len(last_row));
    let text: String = snapshot.text_for_range(range.clone()).collect();
    let indent: String = text
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let code = text
        .lines()
        .map(|line| line.strip_prefix(indent.as_str()).unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let replacement = surround_with(&code, kind, lang, &indent);
    editor.edit([(range.clone(), replacement.as_str())], cx);
    let end = Point::new(range.start.row, 0);
    editor.change_selections(Default::default(), window, cx, |selections| {
        selections.select_ranges([end..end])
    });
}

/// The identifier right before an empty selection's cursor, if any.
fn abbreviation_before_cursor(editor: &Editor, cx: &mut App) -> Option<String> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let selection = editor
        .selections
        .newest::<Point>(&editor.display_snapshot(cx));
    if !selection.is_empty() {
        return None;
    }
    let cursor = selection.head();
    let snapshot = buffer.read(cx).snapshot();
    let prefix: String = snapshot
        .text_for_range(Point::new(cursor.row, 0)..cursor)
        .collect();
    let word: String = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (!word.is_empty()).then_some(word)
}

/// Inserts `template` at the cursor, replacing the abbreviation before it when `replace_word`.
fn expand_template(
    editor: &mut Editor,
    template: &LiveTemplate,
    replace_word: bool,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let cursor = editor
        .selections
        .newest::<Point>(&editor.display_snapshot(cx))
        .head();
    let snapshot = buffer.read(cx).snapshot();
    let prefix: String = snapshot
        .text_for_range(Point::new(cursor.row, 0)..cursor)
        .collect();
    let indent: String = prefix.chars().take_while(|c| c.is_whitespace()).collect();
    let start = if replace_word {
        let word = abbreviation_before_cursor(editor, cx).map_or(0, |word| word.len());
        Point::new(cursor.row, cursor.column.saturating_sub(word as u32))
    } else {
        cursor
    };
    let class_name = buffer.read(cx).file().and_then(|file| {
        file.full_path(cx)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    });
    let ctx = LiveTemplateContext {
        class_name,
        ..Default::default()
    };
    let (head, tail) = template
        .body
        .split_once("$END$")
        .unwrap_or((template.body, ""));
    let head = expand_live_template(head, &ctx);
    let replacement = format!("{head}{}", expand_live_template(tail, &ctx));
    replace_with_cursor(
        editor,
        start..cursor,
        &replacement,
        Some(head.len()),
        &indent,
        window,
        cx,
    );
}
