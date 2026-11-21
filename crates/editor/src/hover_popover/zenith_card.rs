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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor_tests::init_test, test::editor_lsp_test_context::EditorLspTestContext};
    use futures::StreamExt as _;
    use indoc::indoc;
    use settings::SettingsStore;
    use text::Bias;

    thread_local! {
        /// (has diagnostic, doc count, has scroll handle) of the last render.
        static SEEN: Cell<Option<(bool, usize, bool)>> = const { Cell::new(None) };
    }

    fn record_parts(
        parts: HoverCardParts,
        _: Size<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        SEEN.set(Some((
            parts.diagnostic.is_some(),
            parts.docs.len(),
            parts.scroll_handle.is_some(),
        )));
        Some(div().size_full().into_any_element())
    }

    fn decline(
        _: HoverCardParts,
        _: Size<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<AnyElement> {
        None
    }

    /// An editor with a doc popover showing over `println!`.
    async fn hovered(cx: &mut gpui::TestAppContext) -> EditorLspTestContext {
        init_test(cx, |_| {});
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(indoc! {"
            fn ˇtest() { println!(); }
        "});
        let hover_point = cx.display_point(indoc! {"
            fn test() { printˇln!(); }
        "});
        let symbol_range = cx.lsp_range(indoc! {"
            fn test() { «println!»(); }
        "});
        let mut requests =
            cx.set_request_handler::<lsp::request::HoverRequest, _, _>(move |_, _, _| async move {
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: "some basic docs".to_string(),
                    }),
                    range: Some(symbol_range),
                }))
            });
        cx.update_editor(|editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            let anchor = snapshot
                .buffer_snapshot()
                .anchor_before(hover_point.to_offset(&snapshot, Bias::Left));
            crate::hover_popover::hover_at(editor, Some(anchor), None, window, cx)
        });
        let delay = cx.read(|cx: &App| EditorSettings::get_global(cx).hover_popover_delay.0);
        cx.background_executor
            .advance_clock(Duration::from_millis(delay + 100));
        requests.next().await;
        cx.run_until_parked();
        cx
    }

    fn render_card(cx: &mut EditorLspTestContext) -> bool {
        cx.update_editor(|editor, window, cx| {
            let max_size = gpui::size(px(400.), px(300.));
            render(&editor.hover_state, max_size, window, cx).is_some()
        })
    }

    #[gpui::test]
    async fn test_no_card_without_a_renderer(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered(cx).await;
        assert!(!render_card(&mut cx));
    }

    #[gpui::test]
    async fn test_card_gets_the_docs_and_the_scroll_handle(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered(cx).await;
        cx.update(|_, cx| set_hover_card_renderer(record_parts, cx));
        SEEN.set(None);
        assert!(render_card(&mut cx));
        assert_eq!(SEEN.get(), Some((false, 1, true)));
    }

    #[gpui::test]
    async fn test_declining_renderer_falls_back_to_upstream(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered(cx).await;
        cx.update(|_, cx| set_hover_card_renderer(decline, cx));
        assert!(!render_card(&mut cx));
    }

    #[gpui::test]
    async fn test_no_card_when_the_setting_is_off(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered(cx).await;
        cx.update(|_, cx| set_hover_card_renderer(record_parts, cx));
        assert!(render_card(&mut cx));
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(settings::ZenithSettingsContent {
                        hover_card: Some(settings::ZenithHoverCardSettingsContent {
                            enabled: Some(false),
                        }),
                        ..Default::default()
                    });
                });
            });
        });
        assert!(!render_card(&mut cx));
    }

    #[gpui::test]
    async fn test_painted_card_sets_the_bounds_of_every_popover(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered(cx).await;
        cx.update_editor(|editor, _, _| {
            for popover in &editor.hover_state.info_popovers {
                popover.last_bounds.set(None);
            }
        });
        cx.update(|_, cx| set_hover_card_renderer(record_parts, cx));
        cx.update_editor(|_, _, cx| cx.notify());
        cx.run_until_parked();
        cx.update_editor(|editor, _, _| {
            assert!(!editor.hover_state.info_popovers.is_empty());
            for popover in &editor.hover_state.info_popovers {
                assert!(popover.last_bounds.get().is_some());
            }
        });
    }

    #[gpui::test]
    async fn test_card_is_on_by_default(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});
        assert!(cx.update(|cx| ZenithHoverCardSettings::get_global(cx).enabled));
    }
}
