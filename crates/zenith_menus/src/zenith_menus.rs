//! The menu bar in PhpStorm order, plus a Tools menu whose check marks switch Zenith features
//! on and off (plan 44 item 14).

mod app_menu;
#[cfg(test)]
mod tests;
mod tools;

use gpui::{App, AppContext as _, DismissEvent, Global, Menu};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{Settings as _, SettingsStore};
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

use tools::{feature, feature_states, set_feature, tools_menu};

/// Switches one Zenith feature on or off in the user settings. `feature` is a row key of the
/// Tools menu, such as `navbar`.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = zenith_menus)]
#[serde(deny_unknown_fields)]
pub struct ToggleFeature {
    pub feature: String,
}

/// Every Tools-menu feature's state, in `FEATURES` order.
#[derive(Clone, PartialEq, settings::RegisterSetting)]
struct FeatureToggles(Vec<bool>);

impl settings::Settings for FeatureToggles {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(feature_states(content))
    }
}

/// The builder of upstream's menus, kept so a settings change can rebuild the bar.
#[derive(Clone, Copy)]
struct UpstreamMenus(fn(&mut App) -> Vec<Menu>);

impl Global for UpstreamMenus {}

pub fn init(cx: &mut App) {
    cx.on_action(toggle_feature);
    let mut shown = FeatureToggles::get_global(cx).clone();
    cx.observe_global::<SettingsStore>(move |cx| {
        let now = FeatureToggles::get_global(cx).clone();
        if now == shown {
            return;
        }
        shown = now;
        if let Some(&UpstreamMenus(upstream)) = cx.try_global::<UpstreamMenus>() {
            let menus = app_menu::arrange(upstream(cx), tools_menu(&shown.0));
            cx.set_menus(menus);
        }
    })
    .detach();
}

/// Upstream's menus rearranged into PhpStorm's order, with the Tools menu added.
pub fn app_menus(upstream: fn(&mut App) -> Vec<Menu>, cx: &mut App) -> Vec<Menu> {
    cx.set_global(UpstreamMenus(upstream));
    let tools = tools_menu(&FeatureToggles::get_global(cx).0);
    app_menu::arrange(upstream(cx), tools)
}

fn toggle_feature(action: &ToggleFeature, cx: &mut App) {
    let Some((index, feature)) = feature(&action.feature) else {
        log::warn!("zenith_menus: unknown feature {:?}", action.feature);
        return;
    };
    let on = !FeatureToggles::get_global(cx)
        .0
        .get(index)
        .copied()
        .unwrap_or(true);
    let key = action.feature.clone();
    settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |content, _| {
        set_feature(content, &key, on);
    });
    if feature.restart {
        let label = feature.label;
        show_app_notification(
            NotificationId::named(format!("zenith-menus-restart-{}", feature.key).into()),
            cx,
            move |cx| {
                cx.new(|cx| {
                    MessageNotification::new(format!("Restart Zenith to apply {label}"), cx)
                        .primary_message("Restart")
                        .primary_on_click(|_, cx| {
                            cx.emit(DismissEvent);
                            workspace::reload(cx);
                        })
                })
            },
        );
    }
}
