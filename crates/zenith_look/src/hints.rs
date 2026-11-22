//! PhpStorm-style parameter hints: names only before literal arguments in
//! PHP/JS/TS, drawn as small rounded pills in the UI font.

use editor::ChunkRendererContext;
use language::{BufferSnapshot, language_settings::InlayHintKind};
use project::InlayHint;
use settings::Settings as _;
use text::ToOffset as _;
use theme_settings::ThemeSettings;
use ui::prelude::*;

/// Languages whose parameter hints follow PhpStorm's literal-only rule.
const LITERAL_ONLY: [&str; 4] = ["PHP", "JavaScript", "TypeScript", "TSX"];

pub fn init(cx: &mut App) {
    editor::set_inlay_hint_filter(keep_hint, cx);
    editor::set_inlay_hint_renderer(render_pill);
}

fn keep_hint(hint: &InlayHint, buffer: &BufferSnapshot) -> bool {
    if hint.kind != Some(InlayHintKind::Parameter) {
        return true;
    }
    let Some(language) = buffer.language() else {
        return true;
    };
    if !LITERAL_ONLY.contains(&language.name().as_ref()) {
        return true;
    }
    // Parameter hints sit at the start of their argument.
    let argument: String = buffer
        .chars_at(hint.position.to_offset(buffer))
        .take(16)
        .collect();
    starts_with_literal(&argument)
}

/// Whether the argument text starts with a literal: a string, number,
/// true/false/null or an array (`[…]`, `array(…)`). Variables, calls,
/// property accesses and named constants are not literals.
pub fn starts_with_literal(argument: &str) -> bool {
    let text = argument.trim_start();
    let text = text.strip_prefix(['-', '+']).map_or(text, str::trim_start);
    let signed = text.len() != argument.trim_start().len();
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('.') => chars.next().is_some_and(|c| c.is_ascii_digit()),
        _ if signed => false,
        Some('\'' | '"' | '`' | '[') => true,
        _ if text.starts_with("<<<") => true,
        _ => {
            let word_len = text
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(text.len());
            let (word, rest) = text.split_at(word_len);
            let word = word.to_ascii_lowercase();
            matches!(word.as_str(), "true" | "false" | "null")
                || (word == "array" && rest.trim_start().starts_with('('))
        }
    }
}

/// A hint as a small rounded pill: UI font at 0.85× the editor size.
fn render_pill(text: SharedString, cx: &mut ChunkRendererContext) -> AnyElement {
    let font_size = cx
        .window
        .text_style()
        .font_size
        .to_pixels(cx.window.rem_size())
        * 0.85;
    let gap = font_size * 0.3;
    let label = text.trim().to_string();
    let ui_font = ThemeSettings::get_global(cx.context).ui_font.clone();
    let colors = cx.context.theme().status();
    div()
        .when(text.starts_with(' '), |this| this.ml(gap))
        .when(text.ends_with(' '), |this| this.mr(gap))
        .when(!label.is_empty(), |this| {
            this.px(font_size * 0.3)
                .rounded(px(3.))
                .bg(colors.hint_background)
                .text_color(colors.hint)
                .font(ui_font)
                .text_size(font_size)
                .line_height(font_size * 1.3)
                .child(label)
        })
        .into_any_element()
}
