//! The right end of the tab bar, as in PhpStorm: the editor toolbar's right-hand items (the
//! ⋮ menu, the Markdown view buttons) are drawn there instead of in a toolbar row of their own.
//!
//! Contributors register a view per toolbar; every workspace pane's tab bar draws the views of
//! its own toolbar after the stock buttons.

use std::collections::HashMap;

use gpui::{AnyElement, AnyView, App, Context, EntityId, Global, Window};
use ui::prelude::*;
use workspace::{Pane, Toolbar, pane::default_render_tab_bar_buttons};

#[derive(Default)]
pub(crate) struct Views(pub(crate) HashMap<EntityId, Vec<(i32, AnyView)>>);

impl Global for Views {}

/// Draws `view` at the right end of the tab bar of the pane that owns this toolbar; a lower
/// `order` sits further left. The view renders nothing while it has nothing to show. Only the
/// rails (`init`) install the tab bar hook, so without them nothing is drawn.
pub fn tab_bar_end(view: impl Into<AnyView>, order: i32, cx: &mut Context<Toolbar>) {
    let toolbar = cx.entity_id();
    let views = cx.default_global::<Views>().0.entry(toolbar).or_default();
    let first = views.is_empty();
    views.push((order, view.into()));
    // A pane that already drew must draw again to show it.
    cx.notify();
    if first {
        cx.on_release(move |_, cx| {
            cx.global_mut::<Views>().0.remove(&toolbar);
        })
        .detach();
    }
}

/// Installed on every workspace pane. Resolved at draw time, so it follows the views the
/// toolbar holds then; a pane without registered views keeps exactly the stock buttons.
pub(crate) fn render(
    pane: &mut Pane,
    window: &mut Window,
    cx: &mut Context<Pane>,
) -> (Option<AnyElement>, Option<AnyElement>) {
    let (left, right) = default_render_tab_bar_buttons(pane, window, cx);
    let mut views = cx
        .try_global::<Views>()
        .and_then(|views| views.0.get(&pane.toolbar().entity_id()))
        .cloned()
        .unwrap_or_default();
    if views.is_empty() {
        return (left, right);
    }
    views.sort_by_key(|(order, _)| *order);
    let right = h_flex()
        .debug_selector(|| "zenith-tab-bar-end".into())
        .children(right)
        .children(views.into_iter().map(|(_, view)| view))
        .into_any_element();
    (left, Some(right))
}

pub(crate) fn install(pane: &gpui::Entity<Pane>, cx: &mut App) {
    pane.update(cx, |pane, cx| pane.set_render_tab_bar_buttons(cx, render));
}
