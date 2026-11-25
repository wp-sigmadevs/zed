//! The right end of the tab bar, as in PhpStorm: the editor toolbar's right-hand items (the
//! ⋮ menu, the Markdown view buttons) are drawn there instead of in a toolbar row of their own.
//!
//! Contributors register a view per toolbar; every workspace pane's tab bar draws the views of
//! its own toolbar after the stock buttons.

use std::{collections::HashMap, rc::Rc};

use editor::{Editor, EditorSettings};
use gpui::{AnyElement, AnyView, App, Context, EntityId, Global, Window};
use settings::Settings as _;
use ui::prelude::*;
use workspace::{
    Pane, TabBarSettings, Toolbar, dock::RailsActive, pane::default_render_tab_bar_buttons,
};

#[derive(Default)]
pub(crate) struct Views(pub(crate) HashMap<EntityId, Vec<Entry>>);

#[derive(Clone)]
pub(crate) struct Entry {
    order: i32,
    view: AnyView,
    visible: Rc<dyn Fn(&Pane, &App) -> bool>,
    /// While visible, the stock Split Pane button is not drawn beside it.
    hides_split: bool,
}

/// Whether the rails draw the toolbar's right-hand items at the tab bar's end: the rails are on
/// and the tab bar and its buttons are shown. Otherwise those items keep their toolbar place.
pub fn draws_tab_bar_end(cx: &App) -> bool {
    let tab_bar = TabBarSettings::get_global(cx);
    cx.has_global::<RailsActive>() && tab_bar.show && tab_bar.show_tab_bar_buttons
}

impl Global for Views {}

/// The editor's quick-action menu at the tab bar's end, while the `quick_actions` setting is on
/// and the active tab is an editor.
pub fn quick_actions_tab_bar_end(view: impl Into<AnyView>, cx: &mut Context<Toolbar>) {
    tab_bar_end(
        view,
        1,
        |pane, cx| {
            EditorSettings::get_global(cx).toolbar.quick_actions
                && pane
                    .active_item()
                    .is_some_and(|item| item.act_as::<Editor>(cx).is_some())
        },
        cx,
    );
}

/// Draws `view` at the right end of the tab bar of the pane that owns this toolbar; a lower
/// `order` sits further left. `visible` is asked at every draw; a pane where no view is visible
/// gets the stock buttons and no end slot of ours. Only the rails (`init`) install the tab bar
/// hook, so without them nothing is drawn.
pub fn tab_bar_end(
    view: impl Into<AnyView>,
    order: i32,
    visible: impl Fn(&Pane, &App) -> bool + 'static,
    cx: &mut Context<Toolbar>,
) {
    register(view.into(), order, visible, false, cx);
}

/// Like [`tab_bar_end`], and while `view` is visible the stock Split Pane button is not drawn
/// beside it: the view has a split button of its own that would look the same. Splitting stays
/// in the tab menu and on the keyboard, and the button stays on a tab that cannot split.
pub fn tab_bar_end_hiding_split(
    view: impl Into<AnyView>,
    order: i32,
    visible: impl Fn(&Pane, &App) -> bool + 'static,
    cx: &mut Context<Toolbar>,
) {
    register(view.into(), order, visible, true, cx);
}

fn register(
    view: AnyView,
    order: i32,
    visible: impl Fn(&Pane, &App) -> bool + 'static,
    hides_split: bool,
    cx: &mut Context<Toolbar>,
) {
    let toolbar = cx.entity_id();
    let views = cx.default_global::<Views>().0.entry(toolbar).or_default();
    let first = views.is_empty();
    views.push(Entry {
        order,
        view,
        visible: Rc::new(visible),
        hides_split,
    });
    // A pane that already drew must draw again to show it.
    cx.notify();
    if first {
        cx.on_release(move |_, cx| {
            cx.global_mut::<Views>().0.remove(&toolbar);
        })
        .detach();
    }
}

/// Whether the stock Split Pane button is left out: a visible view replaces it, the tab bar
/// end is drawn at all (otherwise that view sits in its toolbar row and the button stays), and
/// the active tab can be split. A tab that cannot split (the markdown split view) offers only
/// moving the pane there, and every other route (tab menu, keys) clones, so the button stays.
pub(crate) fn hides_split(visible_views: &[Entry], pane: &Pane, cx: &App) -> bool {
    draws_tab_bar_end(cx)
        && visible_views.iter().any(|entry| entry.hides_split)
        && pane.active_item().is_some_and(|item| item.can_split(cx))
}

/// Installed on every workspace pane. Resolved at draw time, so it follows the views the
/// toolbar holds then; a pane without registered views keeps exactly the stock buttons.
pub(crate) fn render(
    pane: &mut Pane,
    window: &mut Window,
    cx: &mut Context<Pane>,
) -> (Option<AnyElement>, Option<AnyElement>) {
    let mut views = cx
        .try_global::<Views>()
        .and_then(|views| views.0.get(&pane.toolbar().entity_id()))
        .cloned()
        .unwrap_or_default();
    views.retain(|entry| (entry.visible)(pane, cx));
    // Decided from the views drawn below, so the split button is back whenever the view that
    // replaces it is not drawn.
    let hide_split = hides_split(&views, pane, cx);
    let (left, right) = default_render_tab_bar_buttons(pane, hide_split, window, cx);
    if views.is_empty() {
        return (left, right);
    }
    views.sort_by_key(|entry| entry.order);
    let right = h_flex()
        .debug_selector(|| "zenith-tab-bar-end".into())
        .children(right)
        .children(views.into_iter().map(|entry| entry.view))
        .into_any_element();
    (left, Some(right))
}

pub(crate) fn install(pane: &gpui::Entity<Pane>, cx: &mut App) {
    // The last caller wins: nothing else may replace a workspace pane's hook after this.
    pane.update(cx, |pane, cx| pane.set_render_tab_bar_buttons(cx, render));
}
