//! Rearranges upstream's menu bar into PhpStorm's order.

use gpui::{Menu, MenuItem, SharedString};

const BAR: [&str; 12] = [
    "Zenith", "File", "Edit", "View", "Navigate", "Code", "Refactor", "Run", "Tools", "Git",
    "Window", "Help",
];

/// Upstream items with no use here: update checks, release notes and links to zed.dev, the
/// Zed repository and the feedback forms, and the collab panel.
const DROP: &[&str] = &[
    "auto_update::Check",
    "auto_update::ViewReleaseNotesLocally",
    "feedback::EmailZed",
    "feedback::FileBugReport",
    "feedback::RequestFeature",
    "zed::OpenBrowser",
    "zed::OpenZedRepo",
    "collab_panel::ToggleFocus",
];

/// Edit and Selection items that PhpStorm keeps in its Code menu.
const CODE: &[&str] = &[
    "editor::ToggleComments",
    "editor::MoveLineUp",
    "editor::MoveLineDown",
];

const COMMAND_PALETTE: &str = "command_palette::Toggle";

/// Upstream menu bar plus the Tools menu, in PhpStorm order. Empty menus are left out, and no
/// upstream action survives except through the DROP list.
pub(crate) fn arrange(upstream: Vec<Menu>, tools: Menu) -> Vec<Menu> {
    let mut bar: Vec<Menu> = BAR.iter().map(|name| Menu::new(*name)).collect();
    let mut extra = Vec::new();
    let mut code = Vec::new();
    let mut help = Vec::new();
    for (index, menu) in upstream.into_iter().enumerate() {
        let is_app_menu = index == 0;
        let target = match (is_app_menu, &*menu.name) {
            (true, _) => "Zenith",
            (_, "Selection") => "Edit",
            (_, "Go") => "Navigate",
            (_, name) => name,
        };
        let mut items = Vec::new();
        for item in menu.items {
            let name = match &item {
                MenuItem::Action { action, .. } => action.name(),
                _ => "",
            };
            if DROP.contains(&name) {
                continue;
            } else if CODE.contains(&name) {
                code.push(item);
            } else if name == COMMAND_PALETTE {
                help.push(relabel(item, "Find Action…"));
            } else if is_app_menu {
                items.push(rename_zed(item));
            } else {
                items.push(item);
            }
        }
        match bar.iter_mut().find(|slot| slot.name == target) {
            Some(slot) => {
                if !slot.items.is_empty() {
                    slot.items.push(MenuItem::separator());
                }
                slot.items.extend(items);
                slot.disabled |= menu.disabled;
            }
            None => extra.push(Menu {
                name: menu.name,
                items,
                disabled: menu.disabled,
            }),
        }
    }
    for (name, moved) in [("Code", code), ("Help", help)] {
        if let Some(slot) = bar.iter_mut().find(|slot| slot.name == name) {
            // Moved items lead; a separator keeps them apart from what was already there.
            let rest = std::mem::take(&mut slot.items);
            slot.items = moved;
            slot.items.push(MenuItem::separator());
            slot.items.extend(rest);
        }
    }
    if let Some(slot) = bar.iter_mut().find(|slot| slot.name == "Tools") {
        *slot = tools;
    }

    let mut arranged = Vec::new();
    for mut menu in bar {
        if menu.name == "Window" {
            arranged.append(&mut extra);
        }
        menu.items = tidy(std::mem::take(&mut menu.items));
        if !menu.items.is_empty() {
            arranged.push(menu);
        }
    }
    // Only when Window is empty and so was left out above.
    arranged.append(&mut extra);
    arranged
}

/// Drops leading, trailing and repeated separators.
fn tidy(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut tidy: Vec<MenuItem> = Vec::new();
    for item in items {
        let is_separator = matches!(item, MenuItem::Separator);
        let after_separator = matches!(tidy.last(), None | Some(MenuItem::Separator));
        if !(is_separator && after_separator) {
            tidy.push(item);
        }
    }
    if matches!(tidy.last(), Some(MenuItem::Separator)) {
        tidy.pop();
    }
    tidy
}

fn relabel(item: MenuItem, label: &str) -> MenuItem {
    match item {
        MenuItem::Action {
            action,
            os_action,
            checked,
            disabled,
            ..
        } => MenuItem::Action {
            name: SharedString::from(label.to_string()),
            action,
            os_action,
            checked,
            disabled,
        },
        other => other,
    }
}

/// The app menu's "Zed" labels become "Zenith".
fn rename_zed(item: MenuItem) -> MenuItem {
    match item {
        MenuItem::Action {
            name,
            action,
            os_action,
            checked,
            disabled,
        } => MenuItem::Action {
            name: SharedString::from(name.replace("Zed", "Zenith")),
            action,
            os_action,
            checked,
            disabled,
        },
        other => other,
    }
}
