//! Zenith: where the quick-action menu sits (toolbar row or tab bar end).

use super::*;
use gpui::TestAppContext;
use project::Project;
use workspace::{MultiWorkspace, NewFile, dock::RailsActive};

/// Where the quick-action menu sits for a new editor tab of a fresh window.
async fn location(
    cx: &mut TestAppContext,
) -> (
    gpui::WindowHandle<MultiWorkspace>,
    ToolbarItemLocation,
    Entity<QuickActionBar>,
) {
    let app_state = cx.update(|cx| workspace::AppState::global(cx));
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    cx.dispatch_action(window.into(), NewFile);
    let (pane, item) = window
        .read_with(cx, |multi, cx| {
            let pane = multi.workspace().read(cx).active_pane().clone();
            let item = pane.read(cx).active_item().unwrap();
            (pane, item)
        })
        .unwrap();
    let bar = pane.read_with(cx, |pane, cx| {
        pane.toolbar()
            .read(cx)
            .item_of_type::<QuickActionBar>()
            .unwrap()
    });
    let location = window
        .update(cx, |_, window, cx| {
            bar.update(cx, |bar, cx| {
                bar.set_active_pane_item(Some(item.as_ref()), window, cx)
            })
        })
        .unwrap();
    (window, location, bar)
}

fn set_tab_bar(cx: &mut TestAppContext, show_tab_bar_buttons: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.tab_bar.get_or_insert_default().show_tab_bar_buttons =
                    Some(show_tab_bar_buttons);
            });
        });
    });
}

#[gpui::test]
async fn test_quick_actions_leave_the_toolbar_only_where_the_tab_bar_draws_them(
    cx: &mut TestAppContext,
) {
    let _app_state = crate::zed::tests::init_test(cx);
    // Without the rails the menu keeps its toolbar row.
    assert_eq!(location(cx).await.1, ToolbarItemLocation::PrimaryRight);

    cx.update(|cx| cx.set_global(RailsActive));
    let (window, at, bar) = location(cx).await;
    assert_eq!(at, ToolbarItemLocation::Hidden, "the tab bar draws it");

    // The tab bar buttons are off: nothing would draw it there, so it stays in the toolbar.
    set_tab_bar(cx, false);
    cx.run_until_parked();
    let item = window
        .read_with(cx, |multi, cx| {
            multi
                .workspace()
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .unwrap()
        })
        .unwrap();
    let at = window
        .update(cx, |_, window, cx| {
            bar.update(cx, |bar, cx| {
                bar.set_active_pane_item(Some(item.as_ref()), window, cx)
            })
        })
        .unwrap();
    assert_eq!(at, ToolbarItemLocation::PrimaryRight);
    set_tab_bar(cx, true);
    cx.run_until_parked();
    let at = window
        .update(cx, |_, window, cx| {
            bar.update(cx, |bar, cx| {
                bar.set_active_pane_item(Some(item.as_ref()), window, cx)
            })
        })
        .unwrap();
    assert_eq!(at, ToolbarItemLocation::Hidden);
}
