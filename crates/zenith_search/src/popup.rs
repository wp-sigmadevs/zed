//! The frame Search Everywhere, Recent Files and Find in Files share: a card the
//! header drags around and a bottom-right grip resizes. Its width and height
//! are the one place the popups are sized, so the list, the footer and the
//! header always span the same width.

use std::{cell::Cell, collections::HashMap, rc::Rc};

use gpui::{
    Bounds, Context, DragMoveEvent, Global, MouseButton, MouseDownEvent, Pixels, Point, Rems, Size,
    Window, canvas, point, px, rems, size,
};
use ui::{Tooltip, prelude::*};

/// The card's border, both sides: its children are this much narrower than it.
pub(crate) const BORDER: Pixels = px(2.);

/// Which popup: each remembers its own size for the session.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum PopupKind {
    /// Search Everywhere and its tabs.
    Search,
    /// Recent Files, with the tool-window column.
    Recent,
    /// Find in Files and Replace in Files.
    Find,
}

impl PopupKind {
    fn default_width(self) -> Rems {
        match self {
            Self::Search => rems(38.),
            Self::Recent => rems(48.),
            Self::Find => rems(44.),
        }
    }

    fn min_width(self) -> Rems {
        match self {
            Self::Search => rems(28.),
            Self::Recent => rems(34.),
            Self::Find => rems(36.),
        }
    }

    /// The least height the content below the header can shrink to.
    fn min_body_height(self) -> Pixels {
        match self {
            // The search box and two rows.
            Self::Search | Self::Recent => px(120.),
            // The query fields, three result rows, the preview and the footer.
            Self::Find => px(440.),
        }
    }
}

/// What the user dragged each popup to this session: the card width and the
/// height of everything under the header.
#[derive(Default)]
struct PopupSizes(HashMap<PopupKind, Size<Pixels>>);

impl Global for PopupSizes {}

/// Marks a header drag, which moves the popup.
pub(crate) struct DragPopup;

/// Marks a grip drag, which resizes the popup.
struct ResizePopup;

pub(crate) struct NoDragPreview;

impl Render for NoDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Limits `wanted` to what fits in `max`. The kind's minimum applies unless the
/// window is smaller still: then the card fits the window, so its footer and
/// grip stay on screen.
fn clamp_size(
    kind: PopupKind,
    wanted: Size<Pixels>,
    max: Size<Pixels>,
    rem: Pixels,
) -> Size<Pixels> {
    let min_width = kind.min_width().to_pixels(rem).min(max.width);
    let min_height = kind.min_body_height().min(max.height);
    size(
        wanted.width.min(max.width).max(min_width),
        wanted.height.min(max.height).max(min_height),
    )
}

/// The popup chrome's state, held by each modal.
#[derive(Default)]
pub(crate) struct Frame {
    /// How far the popup has been dragged from where the modal layer put it.
    pub(crate) offset: Point<Pixels>,
    /// Last pointer position of the drag in progress.
    drag_last: Option<Point<Pixels>>,
    /// Last pointer position of the resize in progress.
    resize_last: Option<Point<Pixels>>,
    /// Where the body (everything under the header) was last painted.
    body: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Frame {
    /// The largest card: most of the window, leaving the header and some air.
    fn max_size(viewport: Size<Pixels>) -> Size<Pixels> {
        size(
            viewport.width * 0.95,
            (viewport.height - px(180.)).max(Pixels::ZERO),
        )
    }

    /// The card width, and the body height once the user has set one.
    pub(crate) fn size(
        &self,
        kind: PopupKind,
        window: &Window,
        cx: &gpui::App,
    ) -> (Pixels, Option<Pixels>) {
        let rem = window.rem_size();
        let viewport = window.viewport_size();
        let max = Self::max_size(viewport);
        let stored = cx
            .try_global::<PopupSizes>()
            .and_then(|sizes| sizes.0.get(&kind))
            .copied();
        let wanted = size(
            stored.map_or(kind.default_width().to_pixels(rem), |size| size.width),
            stored.map_or(Pixels::ZERO, |size| size.height),
        );
        let clamped = clamp_size(kind, wanted, max, rem);
        (clamped.width, stored.map(|_| clamped.height))
    }

    /// Moves the popup with the pointer, keeping it within reach of the window.
    pub(crate) fn drag_to(&mut self, position: Point<Pixels>, window: &Window) {
        if let Some(last) = self.drag_last {
            let viewport = window.viewport_size();
            let offset = self.offset + (position - last);
            self.offset = point(
                offset.x.clamp(-viewport.width / 2., viewport.width / 2.),
                offset.y.clamp(-viewport.height / 4., viewport.height / 2.),
            );
        }
        self.drag_last = Some(position);
    }

    /// Grows or shrinks the popup with the pointer, from its top-left corner,
    /// and remembers the size. Stays inside the window and above the minimum.
    pub(crate) fn resize_to(
        &mut self,
        kind: PopupKind,
        position: Point<Pixels>,
        window: &Window,
        cx: &mut gpui::App,
    ) {
        if let Some(last) = self.resize_last {
            let rem = window.rem_size();
            let viewport = window.viewport_size();
            let (width, height) = self.size(kind, window, cx);
            let body = self.body.get();
            let height = height
                .or(body.map(|body| body.size.height))
                .unwrap_or(kind.min_body_height());
            let origin = body.map_or(Point::default(), |body| body.origin);
            let margin = px(8.);
            // The same cap `size` applies, or the card would drift when the two disagree.
            let cap = Self::max_size(viewport);
            let max = size(
                cap.width.min(viewport.width - origin.x - margin),
                cap.height.min(viewport.height - origin.y - margin),
            );
            let delta = position - last;
            let next = clamp_size(kind, size(width + delta.x, height + delta.y), max, rem);
            // The modal layer centres the card: keep its left edge still, so
            // the corner follows the pointer.
            self.offset.x += (next.width - width) / 2.;
            cx.default_global::<PopupSizes>().0.insert(kind, next);
        }
        self.resize_last = Some(position);
    }

    #[cfg(test)]
    pub(crate) fn moved(&self) -> bool {
        self.offset.x > px(0.) && self.offset.y > px(0.)
    }

    /// Wraps `header` and `body` in the card. `root` carries the caller's key
    /// context and actions; `frame` finds this state in the modal.
    pub(crate) fn render<T: 'static>(
        &self,
        kind: PopupKind,
        root: Div,
        header: impl IntoElement,
        body: impl IntoElement,
        frame: fn(&mut T) -> &mut Frame,
        window: &Window,
        cx: &mut Context<T>,
    ) -> Div {
        let (width, height) = self.size(kind, window, cx);
        let measured = self.body.clone();
        let header = div()
            .id("zenith-popup-drag")
            .debug_selector(|| "zenith-popup-drag".into())
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    frame(this).drag_last = Some(event.position)
                }),
            )
            .on_drag(DragPopup, |_, _, _, cx| cx.new(|_| NoDragPreview))
            .child(header);
        let body = v_flex()
            .relative()
            .when_some(height, |this, height| this.h(height))
            .child(
                canvas(
                    move |bounds, _, _| measured.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(body);
        let grip = div()
            .id("zenith-popup-resize")
            .debug_selector(|| "zenith-popup-resize".into())
            .absolute()
            .bottom_0()
            .right_0()
            .size_4()
            .flex()
            .items_end()
            .justify_end()
            .cursor_nwse_resize()
            .tooltip(Tooltip::text("Drag to resize"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    frame(this).resize_last = Some(event.position)
                }),
            )
            .on_drag(ResizePopup, |_, _, _, cx| cx.new(|_| NoDragPreview))
            .child(
                Icon::from_path("icons/zenith_resize_grip.svg")
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            );
        root.relative()
            .left(self.offset.x)
            .top(self.offset.y)
            .on_drag_move::<DragPopup>(cx.listener(
                move |this, event: &DragMoveEvent<DragPopup>, window, cx| {
                    frame(this).drag_to(event.event.position, window);
                    cx.notify();
                },
            ))
            .on_drag_move::<ResizePopup>(cx.listener(
                move |this, event: &DragMoveEvent<ResizePopup>, window, cx| {
                    frame(this).resize_to(kind, event.event.position, window, cx);
                    cx.notify();
                },
            ))
            .w(width)
            .elevation_3(cx)
            .overflow_hidden()
            .child(header)
            .child(body)
            .child(grip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_card_fits_a_window_smaller_than_its_minimum() {
        let rem = px(14.);
        let window = size(px(300.), px(100.));
        let max = Frame::max_size(window);
        let card = clamp_size(PopupKind::Find, size(px(900.), px(900.)), max, rem);
        assert!(card.width <= window.width, "{card:?}");
        assert!(card.height <= window.height, "{card:?}");
        // In a roomy window the minimum holds.
        let roomy_max = Frame::max_size(size(px(1900.), px(1000.)));
        let roomy = clamp_size(PopupKind::Find, size(px(1.), px(1.)), roomy_max, rem);
        assert_eq!(roomy, size(rems(36.).to_pixels(rem), px(440.)));
    }
}
