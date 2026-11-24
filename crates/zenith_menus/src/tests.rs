use gpui::{
    Action, App, BorrowAppContext as _, Menu, MenuItem, OwnedMenu, OwnedMenuItem, TestAppContext,
};
use settings::{SettingsContent, SettingsStore};

use crate::{
    ToggleFeature,
    app_menu::arrange,
    tools::{FEATURES, feature_states, set_feature, tools_menu},
};

/// Not registered anywhere, so a name here never clashes with a real action.
struct Fake(&'static str);

impl Action for Fake {
    fn boxed_clone(&self) -> Box<dyn Action> {
        Box::new(Fake(self.0))
    }
    fn partial_eq(&self, other: &dyn Action) -> bool {
        other
            .as_any()
            .downcast_ref::<Fake>()
            .is_some_and(|other| other.0 == self.0)
    }
    fn name(&self) -> &'static str {
        self.0
    }
    fn name_for_type() -> &'static str {
        "Fake"
    }
    fn build(_: serde_json::Value) -> anyhow::Result<Box<dyn Action>> {
        anyhow::bail!("Fake is test-only")
    }
}

fn item(label: &'static str, name: &'static str) -> MenuItem {
    MenuItem::action(label, Fake(name))
}

fn menu(name: &'static str, items: Vec<MenuItem>) -> Menu {
    Menu::new(name).items(items)
}

/// A small upstream bar: every menu upstream has, with the items the arrange step acts on.
fn upstream() -> Vec<Menu> {
    vec![
        menu(
            "Zed",
            vec![
                item("About Zed", "zed::About"),
                item("Check for Updates", "auto_update::Check"),
                MenuItem::separator(),
                MenuItem::submenu(menu(
                    "Settings",
                    vec![item("Open Settings", "zed::OpenSettings")],
                )),
                MenuItem::separator(),
                item("Quit Zed", "zed::Quit"),
            ],
        ),
        menu("File", vec![item("New", "workspace::NewFile")]),
        menu(
            "Edit",
            vec![
                item("Undo", "editor::Undo"),
                MenuItem::separator(),
                item("Toggle Line Comment", "editor::ToggleComments"),
            ],
        ),
        menu(
            "Selection",
            vec![
                item("Select All", "editor::SelectAll"),
                MenuItem::separator(),
                item("Move Line Up", "editor::MoveLineUp"),
                item("Move Line Down", "editor::MoveLineDown"),
                item("Duplicate Selection", "editor::DuplicateLineDown"),
            ],
        ),
        menu(
            "View",
            vec![
                item("Project Panel", "project_panel::ToggleFocus"),
                item("Collab Panel", "collab_panel::ToggleFocus"),
                item("Terminal Panel", "terminal_panel::Toggle"),
            ],
        ),
        menu(
            "Go",
            vec![
                item("Back", "workspace::GoBack"),
                MenuItem::separator(),
                item("Command Palette...", "command_palette::Toggle"),
                MenuItem::separator(),
                item("Go to File...", "file_finder::Toggle"),
            ],
        ),
        menu("Run", vec![item("Spawn Task", "task::Spawn")]),
        menu("Plugins", vec![item("Plugin Thing", "plugin::Thing")]),
        menu(
            "Window",
            vec![item("Minimize", "zed::Minimize"), MenuItem::separator()],
        ),
        menu(
            "Help",
            vec![
                item(
                    "View Release Notes Locally",
                    "auto_update::ViewReleaseNotesLocally",
                ),
                item("View Telemetry", "zed::OpenTelemetryLog"),
                MenuItem::separator(),
                item("File Bug Report...", "feedback::FileBugReport"),
                item("Request Feature...", "feedback::RequestFeature"),
                item("Email Us...", "feedback::EmailZed"),
                MenuItem::separator(),
                item("Documentation", "zed::OpenBrowser"),
                item("Zed Repository", "zed::OpenZedRepo"),
            ],
        ),
    ]
}

fn names(menus: &[Menu]) -> Vec<String> {
    menus.iter().map(|menu| menu.name.to_string()).collect()
}

fn find<'a>(menus: &'a [Menu], name: &str) -> &'a Menu {
    menus
        .iter()
        .find(|menu| menu.name == name)
        .unwrap_or_else(|| panic!("no {name} menu in {:?}", names(menus)))
}

/// Action names of a menu's own items, top level only; "-" stands for a separator.
fn shape(menu: &Menu) -> Vec<String> {
    menu.items
        .iter()
        .map(|item| match item {
            MenuItem::Action { action, .. } => action.name().to_string(),
            MenuItem::Separator => "-".to_string(),
            MenuItem::Submenu(submenu) => format!("[{}]", submenu.name),
            MenuItem::SystemMenu(_) => "system".to_string(),
        })
        .collect()
}

fn labels(menu: &Menu) -> Vec<String> {
    menu.items
        .iter()
        .filter_map(|item| match item {
            MenuItem::Action { name, .. } => Some(name.to_string()),
            _ => None,
        })
        .collect()
}

fn all_action_names(menus: &[Menu], out: &mut Vec<String>) {
    for menu in menus {
        for item in &menu.items {
            match item {
                MenuItem::Action { action, .. } => out.push(action.name().to_string()),
                MenuItem::Submenu(submenu) => all_action_names(std::slice::from_ref(submenu), out),
                _ => {}
            }
        }
    }
}

const DROPPED: [&str; 7] = [
    "auto_update::Check",
    "auto_update::ViewReleaseNotesLocally",
    "feedback::EmailZed",
    "feedback::FileBugReport",
    "feedback::RequestFeature",
    "zed::OpenBrowser",
    "zed::OpenZedRepo",
];

#[test]
fn arrange_keeps_every_action() {
    let mut before = Vec::new();
    all_action_names(&upstream(), &mut before);
    let mut expected: Vec<String> = before
        .into_iter()
        .filter(|name| !DROPPED.contains(&name.as_str()) && name != "collab_panel::ToggleFocus")
        .collect();
    let mut after = Vec::new();
    all_action_names(
        &arrange(
            upstream(),
            Menu::new("Tools").items([item("T", "tools::T")]),
        ),
        &mut after,
    );
    after.retain(|name| name != "tools::T");
    expected.sort();
    after.sort();
    assert_eq!(after, expected);
}

#[test]
fn arrange_bar_order() {
    let arranged = arrange(
        upstream(),
        Menu::new("Tools").items([item("T", "tools::T")]),
    );
    // Refactor and Git have no items yet, so they are left out; the unknown menu sits before Window.
    assert_eq!(
        names(&arranged),
        [
            "Zenith", "File", "Edit", "View", "Navigate", "Code", "Run", "Tools", "Plugins",
            "Window", "Help"
        ]
    );
    assert_eq!(
        shape(find(&arranged, "Edit")),
        [
            "editor::Undo",
            "-",
            "editor::SelectAll",
            "-",
            "editor::DuplicateLineDown"
        ]
    );
}

#[test]
fn arrange_moves_and_relabels() {
    let arranged = arrange(
        upstream(),
        Menu::new("Tools").items([item("T", "tools::T")]),
    );
    assert_eq!(
        shape(find(&arranged, "Code")),
        [
            "editor::ToggleComments",
            "editor::MoveLineUp",
            "editor::MoveLineDown"
        ]
    );
    assert_eq!(
        labels(find(&arranged, "Help")),
        ["Find Action…", "View Telemetry"]
    );
    assert_eq!(
        labels(find(&arranged, "Navigate")),
        ["Back", "Go to File..."]
    );
    let app = find(&arranged, "Zenith");
    assert_eq!(labels(app), ["About Zenith", "Quit Zenith"]);
    // Check for Updates, the feedback forms and the Zed links are gone.
    let mut kept = Vec::new();
    all_action_names(&arranged, &mut kept);
    assert!(
        kept.iter().all(|name| !DROPPED.contains(&name.as_str())),
        "{kept:?}"
    );
}

#[test]
fn tidy_separators() {
    let arranged = arrange(
        vec![
            menu("Zed", vec![item("About Zed", "zed::About")]),
            menu(
                "File",
                vec![
                    MenuItem::separator(),
                    item("New", "workspace::NewFile"),
                    MenuItem::separator(),
                    MenuItem::separator(),
                    item("Save", "workspace::Save"),
                    MenuItem::separator(),
                ],
            ),
        ],
        Menu::new("Tools"),
    );
    assert_eq!(
        shape(find(&arranged, "File")),
        ["workspace::NewFile", "-", "workspace::Save"]
    );
}

#[test]
fn set_feature_round_trip() {
    for (index, feature) in FEATURES.iter().enumerate() {
        let mut content = SettingsContent::default();
        assert!(
            feature_states(&content).iter().all(|on| *on),
            "unset means on"
        );
        assert!(set_feature(&mut content, feature.key, false));
        let states = feature_states(&content);
        assert!(!states[index], "{} should be off", feature.key);
        assert_eq!(
            states.iter().filter(|on| !**on).count(),
            1,
            "{}",
            feature.key
        );
        assert!(set_feature(&mut content, feature.key, true));
        assert!(
            feature_states(&content)[index],
            "{} should be on",
            feature.key
        );
    }
    let mut content = SettingsContent::default();
    assert!(!set_feature(&mut content, "nope", false));
    set_feature(&mut content, "islands", false);
    let islands = content
        .zenith
        .and_then(|zenith| zenith.ui)
        .and_then(|ui| ui.islands);
    assert_eq!(islands, Some(false));
}

/// (key, checked) of every feature item; panics when a submenu nests deeper than one level.
fn checked_flags(menu: &Menu) -> Vec<(String, bool)> {
    let mut flags = Vec::new();
    for item in &menu.items {
        match item {
            MenuItem::Submenu(submenu) => {
                assert_eq!(submenu.name, "IDE Features");
                for inner in &submenu.items {
                    assert!(
                        !matches!(inner, MenuItem::Submenu(_)),
                        "menus nest one level at most"
                    );
                }
                flags.extend(checked_flags(submenu));
            }
            MenuItem::Action {
                action, checked, ..
            } => {
                let toggle = action
                    .as_any()
                    .downcast_ref::<ToggleFeature>()
                    .expect("ToggleFeature");
                flags.push((toggle.feature.clone(), *checked));
            }
            _ => {}
        }
    }
    flags
}

#[test]
fn tools_menu_checked() {
    let on: Vec<bool> = (0..FEATURES.len()).map(|index| index % 3 != 0).collect();
    let flags = checked_flags(&tools_menu(&on));
    assert_eq!(flags.len(), FEATURES.len());
    for (index, feature) in FEATURES.iter().enumerate() {
        let (_, checked) = flags
            .iter()
            .find(|(key, _)| key == feature.key)
            .unwrap_or_else(|| panic!("{} missing", feature.key));
        assert_eq!(*checked, on[index], "{}", feature.key);
    }
    // A short list counts as on.
    assert!(
        checked_flags(&tools_menu(&[]))
            .iter()
            .all(|(_, checked)| *checked)
    );
}

fn fake_upstream(_: &mut App) -> Vec<Menu> {
    vec![menu("Zed", vec![item("About Zed", "zed::About")])]
}

fn navbar_checked(menus: &[OwnedMenu]) -> bool {
    let tools = menus
        .iter()
        .find(|menu| menu.name == "Tools")
        .expect("Tools menu");
    let ide = tools
        .items
        .iter()
        .find_map(|item| match item {
            OwnedMenuItem::Submenu(menu) if menu.name == "IDE Features" => Some(menu),
            _ => None,
        })
        .expect("IDE Features");
    ide.items
        .iter()
        .find_map(|item| match item {
            OwnedMenuItem::Action { name, checked, .. } if name == "Navigation Bar" => {
                Some(*checked)
            }
            _ => None,
        })
        .expect("Navigation Bar item")
}

#[gpui::test]
fn toggle_rebuilds_menus(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        crate::init(cx);
        let menus = crate::app_menus(fake_upstream, cx);
        cx.set_menus(menus);
        assert!(navbar_checked(&cx.get_menus().expect("menus")));
    });
    // A separate update: the observer is only active once the first one has flushed.
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                set_feature(content, "navbar", false);
            });
        });
    });
    cx.update(|cx| {
        assert!(!navbar_checked(&cx.get_menus().expect("menus")));
    });
}
