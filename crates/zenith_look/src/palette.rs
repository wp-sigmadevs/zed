//! Hides upstream-branded actions (merch store, docs site, status page, the
//! upstream repo, upstream support email) from the command palette.

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;
use std::any::TypeId;

pub(crate) fn init(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&[
            TypeId::of::<zed_actions::GetMerch>(),
            TypeId::of::<zed_actions::OpenDocs>(),
            TypeId::of::<zed_actions::OpenStatusPage>(),
            TypeId::of::<feedback::OpenZedRepo>(),
            TypeId::of::<zed_actions::feedback::EmailZed>(),
        ]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn test_branded_actions_are_hidden_from_the_palette(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            command_palette_hooks::init(cx);
            init(cx);
            let filter = CommandPaletteFilter::try_global(cx).unwrap();
            assert!(filter.is_hidden(&zed_actions::GetMerch));
            assert!(filter.is_hidden(&zed_actions::OpenDocs));
            assert!(filter.is_hidden(&zed_actions::OpenStatusPage));
            assert!(filter.is_hidden(&feedback::OpenZedRepo));
            assert!(filter.is_hidden(&zed_actions::feedback::EmailZed));
            assert!(!filter.is_hidden(&zed_actions::feedback::FileBugReport));
        });
    }
}
