//! Menus without nested submenus, for the in-window menu off macOS.
//!
//! The Linux and Windows in-window menu drops nested submenus, so there each submenu becomes a
//! run of items labelled "Submenu: item" between separators. This can go once an upstream PR
//! for real nested client-menu submenus lands.

use gpui::{Menu, MenuItem};

use crate::app_menu::tidy;

/// `menus` as they are when `nested` (macOS has native submenus), flattened otherwise.
pub(crate) fn for_platform(menus: Vec<Menu>, nested: bool) -> Vec<Menu> {
    if nested {
        return menus;
    }
    menus
        .into_iter()
        .map(|mut menu| {
            menu.items = tidy(flatten_items(menu.items, "", menu.disabled));
            menu
        })
        .collect()
}

fn flatten_items(items: Vec<MenuItem>, prefix: &str, disabled: bool) -> Vec<MenuItem> {
    let mut out = Vec::new();
    for item in items {
        match item {
            MenuItem::Submenu(submenu) => {
                let prefix = format!("{prefix}{}: ", submenu.name);
                out.push(MenuItem::separator());
                out.extend(flatten_items(
                    submenu.items,
                    &prefix,
                    disabled || submenu.disabled,
                ));
                out.push(MenuItem::separator());
            }
            MenuItem::Action {
                name,
                action,
                os_action,
                checked,
                disabled: item_disabled,
            } => out.push(MenuItem::Action {
                name: format!("{prefix}{name}").into(),
                action,
                os_action,
                checked,
                disabled: item_disabled || disabled,
            }),
            other => out.push(other),
        }
    }
    out
}
