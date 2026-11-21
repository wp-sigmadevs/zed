// SPDX-License-Identifier: GPL-3.0-or-later
//! Zenith hover card: the editor-side adapter. When a renderer is registered
//! (by `zenith_look`), the hover popovers are replaced by ONE card built from
//! the diagnostic and documentation the editor already holds, wrapped with the
//! behaviours upstream's popovers have (occlusion, mouse handling, bounds
//! tracking for the hide logic). The card's looks live in the renderer, not here.

use super::*;
use gpui::{Global, WeakEntity};
use settings::{RegisterSetting, SettingsContent};

/// `zenith.hover_card.enabled` (default true). Read it with
/// `ZenithHoverCardSettings::get_global(cx).enabled`.
#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct ZenithHoverCardSettings {
    pub enabled: bool,
}

impl Settings for ZenithHoverCardSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let enabled = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.hover_card.as_ref())
            .and_then(|hover_card| hover_card.enabled)
            .unwrap_or(true);
        Self { enabled }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoverCardSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// The diagnostic under the pointer.
pub struct HoverCardDiagnostic {
    pub severity: HoverCardSeverity,
    /// The tool that reported it, e.g. `phpstan`.
    pub source: Option<SharedString>,
    pub code: Option<SharedString>,
    pub message: SharedString,
    /// Where the diagnostic starts; the card is anchored here.
    pub range_start: Anchor,
    /// Upstream's rendering of the diagnostic (links, code description), for a
    /// renderer that prefers it to `message`. Links: `open_diagnostic_link`.
    pub markdown: Entity<Markdown>,
    pub scroll_handle: ScrollHandle,
}

/// One documentation block (an LSP hover) under the pointer. Links:
/// `open_doc_link`; styling: `hover_markdown_style`.
pub struct HoverCardDoc {
    pub markdown: Entity<Markdown>,
    pub scroll_handle: ScrollHandle,
}

pub struct HoverCardParts {
    pub diagnostic: Option<HoverCardDiagnostic>,
    pub docs: Vec<HoverCardDoc>,
    /// The first doc's scroll handle. The card must track it
    /// (`.track_scroll(&handle)`) so scrolling the hover from the keyboard works.
    pub scroll_handle: Option<ScrollHandle>,
    /// For click handlers; never read it while the editor is updating.
    pub editor: WeakEntity<Editor>,
}

/// Builds the card element from its parts, or returns `None` to fall back to
/// upstream's popovers. Runs during the editor's layout.
pub type HoverCardRenderFn =
    fn(HoverCardParts, Size<Pixels>, &mut Window, &mut App) -> Option<AnyElement>;

struct HoverCardRenderer(HoverCardRenderFn);

impl Global for HoverCardRenderer {}

pub fn set_hover_card_renderer(render: HoverCardRenderFn, cx: &mut App) {
    cx.set_global(HoverCardRenderer(render));
}

/// Opens a link clicked in a doc's markdown, as upstream's info popover does.
pub fn open_doc_link(
    editor: &WeakEntity<Editor>,
    link: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = editor
        .read_with(cx, |editor, _| editor.workspace())
        .ok()
        .flatten();
    open_markdown_url(workspace, link, window, cx);
}

/// Opens a link clicked in a diagnostic's markdown, through the installed
/// diagnostic renderer as upstream's diagnostic popover does.
pub fn open_diagnostic_link(
    editor: &WeakEntity<Editor>,
    link: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(renderer) = GlobalDiagnosticRenderer::global(cx) {
        editor
            .update(cx, |editor, cx| {
                renderer.as_ref().open_link(editor, link, window, cx);
            })
            .ok();
    }
}

fn diagnostic_parts(popover: &DiagnosticPopover) -> HoverCardDiagnostic {
    let diagnostic = &popover.local_diagnostic.diagnostic;
    HoverCardDiagnostic {
        severity: match diagnostic.severity {
            DiagnosticSeverity::ERROR => HoverCardSeverity::Error,
            DiagnosticSeverity::WARNING => HoverCardSeverity::Warning,
            DiagnosticSeverity::INFORMATION => HoverCardSeverity::Info,
            _ => HoverCardSeverity::Hint,
        },
        source: diagnostic.source.clone().map(SharedString::from),
        code: diagnostic.code.as_ref().map(|code| match code {
            lsp::NumberOrString::Number(number) => number.to_string().into(),
            lsp::NumberOrString::String(string) => string.clone().into(),
        }),
        message: diagnostic.message.as_shared_string().clone(),
        range_start: popover.local_diagnostic.range.start,
        markdown: popover.markdown.clone(),
        scroll_handle: popover.scroll_handle.clone(),
    }
}

/// The card for `state`, or `None` when the setting is off, no renderer is
/// registered, a debugger hover is showing, there is nothing to show, or the
/// renderer declines.
pub(crate) fn render(
    state: &HoverState,
    max_size: Size<Pixels>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<AnyElement> {
    if !ZenithHoverCardSettings::get_global(cx).enabled {
        return None;
    }
    let render_card = cx.try_global::<HoverCardRenderer>()?.0;
    if state
        .info_popovers
        .iter()
        .any(|popover| popover.debugger_hover.is_some())
    {
        return None;
    }

    let diagnostic = state.diagnostic_popover.as_ref().map(diagnostic_parts);
    let docs: Vec<HoverCardDoc> = state
        .info_popovers
        .iter()
        .filter_map(|popover| {
            Some(HoverCardDoc {
                markdown: popover.parsed_content.clone()?,
                scroll_handle: popover.scroll_handle.clone(),
            })
        })
        .collect();
    if diagnostic.is_none() && docs.is_empty() {
        return None;
    }

    // Every popover's bounds and keyboard grace are driven by the one card, so
    // the hover hit-testing and hide logic keep working.
    let bounds_cells: Vec<_> = state
        .diagnostic_popover
        .iter()
        .map(|popover| popover.last_bounds.clone())
        .chain(
            state
                .info_popovers
                .iter()
                .map(|popover| popover.last_bounds.clone()),
        )
        .collect();
    let keyboard_graces: Vec<_> = state
        .diagnostic_popover
        .iter()
        .map(|popover| popover.keyboard_grace.clone())
        .chain(
            state
                .info_popovers
                .iter()
                .map(|popover| popover.keyboard_grace.clone()),
        )
        .collect();

    let editor = cx.entity().downgrade();
    let parts = HoverCardParts {
        scroll_handle: docs.first().map(|doc| doc.scroll_handle.clone()),
        diagnostic,
        docs,
        editor: editor.clone(),
    };
    let card = render_card(parts, max_size, window, cx)?;

    Some(
        div()
            .id("zenith_hover_card")
            .occlude()
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        for cell in &bounds_cells {
                            cell.set(Some(bounds));
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            // Prevent a mouse move on the card from reaching the editor, which
            // would dismiss it.
            .on_mouse_move(move |_, _, cx: &mut App| {
                editor
                    .update(cx, |editor, _| {
                        editor.hover_state.closest_mouse_distance = Some(px(0.0));
                        editor.hover_state.hiding_delay_task = None;
                        editor.hover_state.hint_hover_task = None;
                    })
                    .ok();
                cx.stop_propagation()
            })
            // Prevent a mouse down on the card from moving the cursor.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                for keyboard_grace in &keyboard_graces {
                    *keyboard_grace.borrow_mut() = false;
                }
                cx.stop_propagation();
            })
            .child(card)
            .into_any_element(),
    )
}
