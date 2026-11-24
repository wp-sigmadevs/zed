//! Hides upstream-branded actions (merch store, docs site, status page, the
//! upstream repo, upstream support email, the hosted edit-prediction
//! onboarding) from the command palette.

use command_palette_hooks::CommandPaletteFilter;
use gpui::App;
use settings::SettingsStore;
use std::any::TypeId;

fn hide(cx: &mut App) {
    CommandPaletteFilter::update_global(cx, |filter, _| {
        filter.hide_action_types(&[
            TypeId::of::<zed_actions::GetMerch>(),
            TypeId::of::<zed_actions::OpenDocs>(),
            TypeId::of::<zed_actions::OpenStatusPage>(),
            TypeId::of::<feedback::OpenZedRepo>(),
            TypeId::of::<zed_actions::feedback::EmailZed>(),
            TypeId::of::<zed_actions::OpenZedPredictOnboarding>(),
        ]);
    });
}

pub(crate) fn init(cx: &mut App) {
    debug_assert!(
        CommandPaletteFilter::try_global(cx).is_some(),
        "command_palette_hooks::init must run before zenith_look::init"
    );
    hide(cx);
    // agent_ui re-shows the predict onboarding action on every settings
    // change; observers run in registration order, so this one runs after it.
    cx.observe_global::<SettingsStore>(hide).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::BorrowAppContext as _;

    #[gpui::test]
    fn test_branded_actions_are_hidden_from_the_palette(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            command_palette_hooks::init(cx);
            init(cx);
        });
        let assert_hidden = |cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let filter = CommandPaletteFilter::try_global(cx).unwrap();
                assert!(filter.is_hidden(&zed_actions::GetMerch));
                assert!(filter.is_hidden(&zed_actions::OpenDocs));
                assert!(filter.is_hidden(&zed_actions::OpenStatusPage));
                assert!(filter.is_hidden(&feedback::OpenZedRepo));
                assert!(filter.is_hidden(&zed_actions::feedback::EmailZed));
                assert!(filter.is_hidden(&zed_actions::OpenZedPredictOnboarding));
                assert!(!filter.is_hidden(&zed_actions::feedback::FileBugReport));
            });
        };
        assert_hidden(cx);

        // Another observer shows the onboarding action, then settings change.
        cx.update(|cx| {
            CommandPaletteFilter::update_global(cx, |filter, _| {
                filter.show_action_types(&[TypeId::of::<zed_actions::OpenZedPredictOnboarding>()]);
            });
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |_| {});
            });
        });
        assert_hidden(cx);
    }
}
