// SPDX-License-Identifier: GPL-3.0-or-later
//! The hover card's looks (plan 34): one card for an editor hover, the
//! diagnostic first and the documentation under it. The editor builds the parts
//! and the wrapper behaviour (`editor::hover_popover::zenith_card`); this only
//! draws them. Language-server text never goes through markdown unescaped: the
//! diagnostic is plain label text, docs are the editor's parsed markdown.

use editor::{
    Editor, SelectionEffects,
    actions::ToggleCodeActions,
    hover_popover::{
        hide_hover, hover_markdown_style,
        zenith_card::{
            HoverCardDiagnostic, HoverCardParts, HoverCardSeverity, open_doc_link,
            set_hover_card_renderer,
        },
    },
};
use gpui::{AnyElement, App, Focusable as _, Pixels, Role, SharedString, Size, WeakEntity, Window};
use markdown::{CodeBlockRenderer, CopyButtonVisibility, MarkdownElement, WrapButtonVisibility};
use theme::ActiveTheme as _;
use ui::{CopyButton, Divider, ElevationIndex, KeyBinding, Tooltip, prelude::*};

pub fn init(cx: &mut App) {
    set_hover_card_renderer(render, cx);
}

/// `phpcs: Sniff.Code: message`; the source and the code are left out when the
/// language server sent none.
pub(crate) fn diagnostic_text(diagnostic: &HoverCardDiagnostic) -> SharedString {
    let mut text = String::new();
    for part in [&diagnostic.source, &diagnostic.code].into_iter().flatten() {
        text.push_str(part);
        text.push_str(": ");
    }
    text.push_str(&diagnostic.message);
    text.into()
}

fn severity_name(severity: HoverCardSeverity) -> &'static str {
    match severity {
        HoverCardSeverity::Error => "Error",
        HoverCardSeverity::Warning => "Warning",
        HoverCardSeverity::Info => "Info",
        HoverCardSeverity::Hint => "Hint",
    }
}

fn severity_icon(severity: HoverCardSeverity) -> (IconName, Color) {
    match severity {
        HoverCardSeverity::Error => (IconName::XCircle, Color::Error),
        HoverCardSeverity::Warning => (IconName::Warning, Color::Warning),
        HoverCardSeverity::Info => (IconName::Info, Color::Info),
        HoverCardSeverity::Hint => (IconName::Circle, Color::Hint),
    }
}

fn render(
    parts: HoverCardParts,
    max_size: Size<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let colors = cx.theme().colors();
    let (surface, border, code_background) = (
        colors.surface_background,
        colors.border_variant,
        colors.editor_background,
    );
    let has_docs = !parts.docs.is_empty();
    let HoverCardParts {
        diagnostic,
        docs,
        scroll_handle,
        editor,
    } = parts;

    let card = v_flex()
        .max_w(max_size.width.min(window.rem_size() * 40.))
        .max_h(max_size.height)
        .overflow_hidden()
        .bg(surface)
        .border_1()
        .border_color(border)
        .rounded_lg()
        .shadow(ElevationIndex::ElevatedSurface.shadow(cx))
        .when_some(diagnostic, |card, diagnostic| {
            card.child(render_diagnostic(&diagnostic, &editor, max_size, cx))
                .when(has_docs, |card| card.child(Divider::horizontal()))
        });

    let mut style = hover_markdown_style(window, cx);
    style.code_block = style.code_block.bg(code_background);
    let docs_handle = scroll_handle.clone();
    let docs = docs.into_iter().map(|doc| {
        let editor = editor.clone();
        MarkdownElement::new(doc.markdown, style.clone())
            // The docs container scrolls, so every doc follows its handle.
            .when_some(docs_handle.clone(), |element, handle| {
                element.scroll_handle(handle)
            })
            .code_block_renderer(CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::Hidden,
                wrap_button_visibility: WrapButtonVisibility::Hidden,
                border: false,
            })
            .on_url_click(move |link, window, cx| open_doc_link(&editor, link, window, cx))
    });

    Some(
        card.when_some(scroll_handle, |card, handle| {
            card.child(
                div()
                    .id("zenith-hover-docs")
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .child(v_flex().gap_2().p_2().children(docs)),
            )
        })
        .into_any_element(),
    )
}

fn render_diagnostic(
    diagnostic: &HoverCardDiagnostic,
    editor: &WeakEntity<Editor>,
    max_size: Size<Pixels>,
    cx: &App,
) -> impl IntoElement {
    let (icon, color) = severity_icon(diagnostic.severity);
    let text = diagnostic_text(diagnostic);
    let range_start = diagnostic.range_start;
    let editor = editor.clone();

    v_flex()
        .flex_none()
        .child(
            h_flex()
                .items_stretch()
                // The bar and the icon's shape both carry the severity.
                .child(div().w(px(2.)).flex_none().bg(color.color(cx)))
                .child(
                    h_flex()
                        .min_w_0()
                        .items_start()
                        .gap_2()
                        .p_2()
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(
                            div()
                                .id("zenith-hover-diagnostic")
                                .role(Role::Group)
                                .aria_label(format!(
                                    "{}: {text}",
                                    severity_name(diagnostic.severity)
                                ))
                                .min_w_0()
                                .max_h(max_size.height * 0.5)
                                .overflow_y_scroll()
                                .track_scroll(&diagnostic.scroll_handle)
                                .child(Label::new(text.clone()).size(LabelSize::Small)),
                        ),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .px_2()
                .pb_2()
                .child(
                    Button::new("zenith-hover-quick-fixes", "Show Quick Fixes")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .key_binding(KeyBinding::for_action(&ToggleCodeActions::default(), cx))
                        .tooltip(Tooltip::for_action_title(
                            "Show Quick Fixes",
                            &ToggleCodeActions::default(),
                        ))
                        .on_click(move |_, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    hide_hover(editor, cx);
                                    editor.change_selections(
                                        SelectionEffects::default(),
                                        window,
                                        cx,
                                        |selections| {
                                            selections
                                                .select_anchor_ranges([range_start..range_start])
                                        },
                                    );
                                    window.focus(&editor.focus_handle(cx), cx);
                                    editor.toggle_code_actions(
                                        &ToggleCodeActions::default(),
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }),
                )
                .child(CopyButton::new("zenith-hover-copy-diagnostic", text)),
        )
}
