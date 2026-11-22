//! Hooks the postfix expander into the workspace: `ExpandPostfix` rewrites the
//! `expr.trigger` before the cursor in the active editor, `SurroundWith` wraps the
//! selected lines in a block and `LiveTemplates` expands the abbreviation before the
//! cursor (or lists the templates when there is none).

use crate::choice_picker::ChoicePicker;
use crate::live_templates::{LiveTemplate, live_templates};
use crate::{
    ExpandPostfix, LanguageKind, LiveTemplateContext, LiveTemplates, SurroundKind, SurroundWith,
    expand_postfix, extract_expression_and_trigger, surround_with, template_to_snippet,
};
use editor::Editor;
use gpui::{App, Context, Window};
use language::{Buffer, BufferSnapshot, Point};
use snippet::Snippet;
use time::{OffsetDateTime, UtcOffset};
use util::ResultExt as _;
use workspace::Workspace;
use zenith_refactor::editor_actions::namespace_of;

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
    let target = cursor_target(range.start, replacement, cursor_offset, indent);
    editor.edit([(range, text.as_str())], cx);
    if let Some(target) = target {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([target..target])
        });
    }
}

/// Where the cursor lands after `replacement` (re-indented with `indent`) is inserted at
/// `start`, `cursor_offset` bytes into the unindented replacement.
fn cursor_target(
    start: Point,
    replacement: &str,
    cursor_offset: Option<usize>,
    indent: &str,
) -> Option<Point> {
    let head = replacement
        .get(..cursor_offset?)?
        .replace('\n', &format!("\n{indent}"));
    let lines = head.matches('\n').count() as u32;
    let column = match head.rfind('\n') {
        Some(newline) => head.len() - newline - 1,
        None => start.column as usize + head.len(),
    };
    Some(Point::new(start.row + lines, column as u32))
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

/// Inserts `template` at the cursor as a snippet (tab stops, defaults, `$END$`), replacing
/// the abbreviation before it when `replace_word`.
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
    let start = if replace_word {
        let word = abbreviation_before_cursor(editor, cx).map_or(0, |word| word.len());
        Point::new(cursor.row, cursor.column.saturating_sub(word as u32))
    } else {
        cursor
    };
    let ctx = template_context(buffer.read(cx), start, cx);
    let Some(snippet) = Snippet::parse(&template_to_snippet(template.body, &ctx)).log_err() else {
        return;
    };
    let multi_buffer = editor.buffer().read(cx).snapshot(cx);
    let range = multi_buffer.point_to_offset(start)..multi_buffer.point_to_offset(cursor);
    editor
        .insert_snippet(&[range], snippet, window, cx)
        .log_err();
}

/// The built-in template variables at `position` in `buffer`.
fn template_context(buffer: &Buffer, position: Point, cx: &App) -> LiveTemplateContext {
    let path = buffer.file().map(|file| file.full_path(cx));
    let snapshot = buffer.snapshot();
    let (class_name, method_name) = enclosing_names(&snapshot, snapshot.point_to_offset(position));
    let is_php = path
        .as_deref()
        .and_then(LanguageKind::from_path)
        .is_some_and(|lang| lang.is_php());
    let now = OffsetDateTime::now_utc()
        .to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    let (date, year) = date_and_year(now);
    LiveTemplateContext {
        file_path: path
            .as_deref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned()),
        class_name: class_name.or_else(|| {
            path.as_deref()
                .and_then(|path| path.file_stem())
                .map(|stem| stem.to_string_lossy().into_owned())
        }),
        method_name,
        namespace: if is_php {
            namespace_of(&snapshot.text())
        } else {
            None
        },
        date: Some(date),
        year: Some(year),
    }
}

/// Names of the innermost class-like declaration and function or method around `offset`,
/// from the syntax tree (PHP and JS/TS node kinds).
fn enclosing_names(snapshot: &BufferSnapshot, offset: usize) -> (Option<String>, Option<String>) {
    let (mut class_name, mut method_name) = (None, None);
    let mut node = snapshot.syntax_ancestor(offset..offset);
    while let Some(current) = node {
        let slot = match current.kind() {
            "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => Some(&mut class_name),
            "method_declaration"
            | "method_definition"
            | "function_definition"
            | "function_declaration"
            | "generator_function_declaration" => Some(&mut method_name),
            _ => None,
        };
        if let Some(slot) = slot
            && slot.is_none()
        {
            *slot = current.child_by_field_name("name").map(|name| {
                snapshot
                    .text_for_range(name.byte_range())
                    .collect::<String>()
            });
        }
        node = current.parent();
    }
    (class_name, method_name)
}

/// `$DATE$` (ISO `YYYY-MM-DD`) and `$YEAR$` for `now`.
fn date_and_year(now: OffsetDateTime) -> (String, String) {
    let date = format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    (date, now.year().to_string())
}

#[cfg(test)]
mod tests {
    use super::{cursor_target, date_and_year, enclosing_names};
    use gpui::{App, AppContext as _};
    use language::{Buffer, Language, LanguageConfig, Point};
    use std::sync::Arc;
    use time::macros::datetime;

    #[gpui::test]
    fn test_enclosing_names_reads_class_and_method(cx: &mut App) {
        let typescript = Arc::new(Language::new(
            LanguageConfig {
                name: "TypeScript".into(),
                ..Default::default()
            },
            Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        ));
        let text = "class Invoice {\n  total() {\n    return 1;\n  }\n}\nfunction helper() {}\n";
        cx.new(|cx| {
            let buffer = Buffer::local(text, cx).with_language(typescript, cx);
            let snapshot = buffer.snapshot();
            let in_method = text.find("return").expect("fixture has a return");
            assert_eq!(
                enclosing_names(&snapshot, in_method),
                (Some("Invoice".to_string()), Some("total".to_string()))
            );
            let in_function = text.find("{}").expect("fixture has an empty body") + 1;
            assert_eq!(
                enclosing_names(&snapshot, in_function),
                (None, Some("helper".to_string()))
            );
            buffer
        });
    }

    #[test]
    fn test_date_and_year_are_zero_padded() {
        assert_eq!(
            date_and_year(datetime!(2026-03-07 23:59 UTC)),
            ("2026-03-07".to_string(), "2026".to_string())
        );
    }

    #[test]
    fn test_cursor_target_follows_indented_lines() {
        let start = Point::new(3, 4);
        assert_eq!(
            cursor_target(start, "if ($x) {\n    \n}", Some(14), "    "),
            Some(Point::new(4, 8))
        );
        assert_eq!(
            cursor_target(start, "echo $x;", Some(5), "    "),
            Some(Point::new(3, 9))
        );
        assert_eq!(cursor_target(start, "abc", None, ""), None);
        assert_eq!(
            cursor_target(start, "é", Some(1), ""),
            None,
            "not a char boundary"
        );
        assert_eq!(
            cursor_target(start, "abc", Some(9), ""),
            None,
            "past the end"
        );
    }
}
