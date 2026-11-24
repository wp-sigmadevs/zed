//! Sizes, spacing and colours the popups share, so a look change is one edit here.
//! Theme colours come from `cx.theme()` at the call sites; only PhpStorm's two
//! popup bands, which no theme colour matches, are fixed values.

use gpui::{Pixels, Rems, px, rems};

/// PhpStorm's popup palette: a lighter title band and footer over the list.
pub(crate) const POPUP_BAND: u32 = 0x393b40;
pub(crate) const POPUP_FOOTER: u32 = 0x303236;

/// Header and footer heights.
pub(crate) const HEADER_HEIGHT: Rems = rems(2.75);
pub(crate) const FOOTER_HEIGHT: Rems = rems(3.);
/// The side padding of every band and row, and the gap between header controls.
pub(crate) const PADDING: Rems = rems(1.);
pub(crate) const HEADER_GAP: Rems = rems(0.75);

/// A result row, and how many the list shows before it scrolls.
pub(crate) const ROW_HEIGHT: f32 = 26.;
pub(crate) const VISIBLE_ROWS: f32 = 7.;
/// About nine lines of the preview's code. The splitter changes it, within the
/// least the preview keeps and the result rows the list keeps.
pub(crate) const PREVIEW_HEIGHT: f32 = 210.;
pub(crate) const MIN_PREVIEW_HEIGHT: f32 = 80.;
pub(crate) const MIN_LIST_ROWS: f32 = 3.;
/// The splitter's hit area, a thin line between the list and the preview.
pub(crate) const SPLITTER_HEIGHT: f32 = 5.;

/// The small boxes (mask, directory, replace) and the search box.
pub(crate) const FIELD_HEIGHT: Pixels = px(30.);
pub(crate) const SEARCH_BOX_HEIGHT: Pixels = px(36.);
pub(crate) const MASK_WIDTH: Rems = rems(10.);
