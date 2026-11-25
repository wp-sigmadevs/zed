//! The Tools menu: one check-marked item per Zenith feature.

use gpui::{Menu, MenuItem};
use settings::{SettingsContent, ZenithSettingsContent};

use crate::ToggleFeature;

/// One feature the Tools menu can switch, backed by a `zenith.*` boolean setting.
pub(crate) struct Feature {
    pub key: &'static str,
    pub label: &'static str,
    /// Listed under "IDE Features" instead of directly in Tools.
    pub ide: bool,
    /// Needs a restart to apply; the toggle offers one.
    pub restart: bool,
    pub get: fn(&ZenithSettingsContent) -> Option<bool>,
    pub set: fn(&mut ZenithSettingsContent, bool),
}

/// `row!(key, "Label", ide, restart)`: the setting is `zenith.<key>.enabled`.
macro_rules! row {
    ($key:ident, $label:literal, $ide:literal, $restart:literal) => {
        Feature {
            key: stringify!($key),
            label: $label,
            ide: $ide,
            restart: $restart,
            get: |zenith| zenith.$key.as_ref().and_then(|setting| setting.enabled),
            set: |zenith, on| zenith.$key.get_or_insert_default().enabled = Some(on),
        }
    };
}

pub(crate) static FEATURES: &[Feature] = &[
    row!(agent_review, "Agent Review", false, false),
    row!(local_history, "Local History", false, false),
    row!(scratches, "Scratches", false, true),
    row!(todo, "TODO", false, true),
    row!(inspections, "Inspections", false, true),
    row!(deploy, "Deployment", false, true),
    row!(wordpress, "WordPress", false, true),
    row!(laravel, "Laravel", false, true),
    row!(nextjs, "Next.js", false, true),
    row!(search, "Search Everywhere", true, true),
    row!(navbar, "Navigation Bar", true, true),
    row!(rails, "Tool Window Rails", true, true),
    row!(look, "Zenith Look", true, true),
    Feature {
        key: "islands",
        label: "Islands",
        ide: true,
        restart: true,
        get: |zenith| zenith.ui.as_ref().and_then(|ui| ui.islands),
        set: |zenith, on| zenith.ui.get_or_insert_default().islands = Some(on),
    },
    row!(templates, "File Templates", true, true),
    row!(external_libraries, "External Libraries", true, true),
    row!(title_bar, "Title Bar Additions", true, false),
    row!(hover_card, "Hover Card", true, false),
    row!(welcome, "Welcome", true, false),
    row!(editor_menu, "Editor Context Menu", true, true),
    row!(tab_menu, "Tab Context Menu", true, false),
    row!(inspection_widget, "Inspection Widget", true, false),
];

pub(crate) fn feature(key: &str) -> Option<(usize, &'static Feature)> {
    FEATURES
        .iter()
        .enumerate()
        .find(|(_, feature)| feature.key == key)
}

/// Every feature's state in `FEATURES` order; unset means on.
pub(crate) fn feature_states(content: &SettingsContent) -> Vec<bool> {
    FEATURES
        .iter()
        .map(|feature| {
            let state = content
                .zenith
                .as_ref()
                .and_then(|zenith| (feature.get)(zenith));
            state.unwrap_or(true)
        })
        .collect()
}

/// Writes `on` to the setting of the feature `key`; false when no such feature.
pub(crate) fn set_feature(content: &mut SettingsContent, key: &str, on: bool) -> bool {
    let Some((_, feature)) = feature(key) else {
        return false;
    };
    (feature.set)(content.zenith.get_or_insert_default(), on);
    true
}

/// `on` follows `FEATURES`; a missing entry counts as on. Each feature is one check-marked
/// item; the IDE ones sit in one "IDE Features" submenu (menus nest one level at most).
pub(crate) fn tools_menu(on: &[bool]) -> Menu {
    let items_where = |ide: bool| {
        FEATURES
            .iter()
            .enumerate()
            .filter(move |(_, feature)| feature.ide == ide)
            .map(|(index, feature)| {
                let enabled = on.get(index).copied().unwrap_or(true);
                MenuItem::action(
                    feature.label,
                    ToggleFeature {
                        feature: feature.key.to_string(),
                    },
                )
                .checked(enabled)
            })
            .collect::<Vec<_>>()
    };
    let mut items = items_where(false);
    items.push(MenuItem::separator());
    items.push(MenuItem::submenu(
        Menu::new("IDE Features").items(items_where(true)),
    ));
    Menu::new("Tools").items(items)
}

/// Restart-type features whose state differs between `old` and `new` (both in `FEATURES` order).
pub(crate) fn changed_restart_features(old: &[bool], new: &[bool]) -> Vec<&'static Feature> {
    let state = |states: &[bool], index: usize| states.get(index).copied().unwrap_or(true);
    FEATURES
        .iter()
        .enumerate()
        .filter(|(index, feature)| feature.restart && state(old, *index) != state(new, *index))
        .map(|(_, feature)| feature)
        .collect()
}
