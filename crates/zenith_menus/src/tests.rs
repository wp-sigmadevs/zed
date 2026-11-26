use gpui::{
    Action, App, BorrowAppContext as _, Menu, MenuItem, OwnedMenu, OwnedMenuItem, TestAppContext,
    VisualTestContext,
};
use project::Project;
use settings::{Settings as _, SettingsContent, SettingsStore};
use workspace::{AppState, MultiWorkspace};

use crate::{
    ToggleFeature,
    app_menu::arrange as arrange_with,
    bar_items::{self, Extras},
    flatten::for_platform,
    restart_notice_id, restart_notice_text, toggle_feature,
    tools::{
        FEATURES, GROUPS, changed_restart_features, feature, feature_states, set_feature,
        tools_action_names, tools_menu,
    },
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

fn arrange(upstream: Vec<Menu>, tools: Menu) -> Vec<Menu> {
    arrange_with(upstream, tools, Vec::new())
}

fn fake(name: &str) -> Box<dyn Action> {
    // Leaked: a handful of short names per test run.
    Box::new(Fake(Box::leak(name.to_string().into_boxed_str())))
}

fn extras_where(keep: impl Fn(&str) -> bool, ai_disabled: bool) -> Extras {
    bar_items::build(|name| keep(name).then(|| fake(name)), ai_disabled)
}

fn tools() -> Menu {
    Menu::new("Tools").items([item("T", "tools::T")])
}

fn with_extras(extras: Extras) -> Vec<Menu> {
    arrange_with(upstream(), tools(), extras)
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
                item("Install CLI", "install_cli::InstallCliBinary"),
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

const DROPPED: [&str; 8] = [
    "install_cli::InstallCliBinary",
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

/// The feature a unit toggle action belongs to.
fn key_of(action: &dyn Action) -> Option<&'static str> {
    FEATURES
        .iter()
        .find(|feature| (feature.toggle)().name() == action.name())
        .map(|feature| feature.key)
}

/// (key, checked) of every feature toggle in the menu, submenus included; panics when a
/// submenu nests deeper than one level.
fn checked_flags(menu: &Menu) -> Vec<(String, bool)> {
    let mut flags = Vec::new();
    for item in &menu.items {
        match item {
            MenuItem::Submenu(submenu) => {
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
                if let Some(key) = key_of(action.as_ref()) {
                    flags.push((key.to_string(), *checked));
                }
            }
            _ => {}
        }
    }
    flags
}

fn tools_with(on: &[bool]) -> Menu {
    tools_menu(on, |name| Some(fake(name)))
}

#[test]
fn tools_menu_checked() {
    let on: Vec<bool> = (0..FEATURES.len()).map(|index| index % 3 != 0).collect();
    let flags = checked_flags(&tools_with(&on));
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
        checked_flags(&tools_with(&[]))
            .iter()
            .all(|(_, checked)| *checked)
    );
}

#[test]
fn tools_menu_structure_and_order() {
    let tools = tools_with(&[]);
    assert_eq!(
        shape(&tools),
        [
            "[AI]",
            "[Templates]",
            "[Local History]",
            "[Scratches]",
            "[TODO]",
            "[Inspections]",
            "[Deployment]",
            "[PHP]",
            "[WordPress]",
            "[Laravel]",
            "[Next.js]",
            "[Postfix]",
            "[HTTP Client]",
            "[Markdown]",
            "-",
            "[IDE Features]",
            "-",
            "zenith_open_project::ConfirmWindow",
            "install_cli::InstallCliBinary",
        ]
    );
    let Some(MenuItem::Submenu(ai)) = tools.items.first() else {
        panic!("AI first");
    };
    assert_eq!(
        shape(ai),
        [
            "zenith_agent_review::ToggleFocus",
            "zenith_agent_review::KeepAll",
            "zenith_agent_review::RevertTurn",
            "-",
            "zenith_features::ToggleAgentReview",
        ]
    );
    assert_eq!(labels(ai).last().map(String::as_str), Some("Enabled"));
    assert_eq!(
        labels(&tools),
        [
            "Confirm Window to Open Project In",
            "Create Command Line Launcher…"
        ],
        "the Open Project reset and the launcher are the only top-level actions"
    );
}

#[test]
fn tools_menu_without_resolvable_actions_keeps_the_switches() {
    let tools = tools_menu(&[], |_| None);
    let submenus: Vec<&str> = tools
        .items
        .iter()
        .filter_map(|item| match item {
            MenuItem::Submenu(menu) => Some(menu.name.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        submenus,
        [
            "AI",
            "Local History",
            "Scratches",
            "TODO",
            "Inspections",
            "Deployment",
            "WordPress",
            "Laravel",
            "Next.js",
            "IDE Features"
        ]
    );
    assert_eq!(checked_flags(&tools).len(), FEATURES.len());
    assert_no_stray_separators(&tools);
}

fn assert_no_stray_separators(menu: &Menu) {
    let shown = shape(menu);
    assert_ne!(
        shown.first().map(String::as_str),
        Some("-"),
        "{}",
        menu.name
    );
    assert_ne!(shown.last().map(String::as_str), Some("-"), "{}", menu.name);
    assert!(
        shown.windows(2).all(|w| w[0] != "-" || w[1] != "-"),
        "{}: {shown:?}",
        menu.name
    );
    for item in &menu.items {
        if let MenuItem::Submenu(sub) = item {
            assert!(!sub.items.is_empty(), "{} is empty", sub.name);
            assert_no_stray_separators(sub);
        }
    }
}

#[test]
fn every_feature_has_one_toggle_in_tools() {
    let flags = checked_flags(&tools_with(&[]));
    for feature in FEATURES {
        let count = flags.iter().filter(|(key, _)| key == feature.key).count();
        assert_eq!(count, 1, "{} needs exactly one Tools toggle", feature.key);
        let name = (feature.toggle)().name();
        assert!(name.starts_with("zenith_features::Toggle"), "{name}");
    }
    let mut names: Vec<_> = FEATURES.iter().map(|f| (f.toggle)().name()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), FEATURES.len(), "one unit action per feature");
    // A group's switch is its own feature's, and IDE features keep theirs in IDE Features.
    for group in GROUPS {
        if let Some(key) = group.feature {
            assert!(!feature(key).expect("feature").1.ide, "{key}");
        }
    }
}

#[gpui::test]
fn unit_actions_are_listed_for_the_palette(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let listed = cx
            .all_action_names()
            .iter()
            .filter(|name| name.starts_with("zenith_features::"))
            .count();
        assert_eq!(listed, FEATURES.len(), "no unit action without a row");
        for feature in FEATURES {
            let name = (feature.toggle)().name();
            assert!(cx.build_action(name, None).is_ok(), "{name}");
        }
    });
}

#[test]
fn hover_card_is_disabled_while_look_is_off() {
    let hover_disabled = |on: &[bool]| {
        let tools = tools_with(on);
        let ide = tools
            .items
            .iter()
            .find_map(|item| match item {
                MenuItem::Submenu(menu) if menu.name == "IDE Features" => Some(menu),
                _ => None,
            })
            .expect("IDE Features");
        ide.items
            .iter()
            .filter(|item| matches!(item, MenuItem::Action { name, .. } if name == "Hover Card"))
            .map(MenuItem::is_disabled)
            .collect::<Vec<_>>()
    };
    let (look, _) = feature("look").expect("look");
    let mut on = vec![true; FEATURES.len()];
    assert_eq!(hover_disabled(&on), [false]);
    on[look] = false;
    assert_eq!(hover_disabled(&on), [true]);
    // Nothing else goes grey with Look.
    let disabled_items = |on: &[bool]| {
        let mut count = 0;
        for item in &tools_with(on).items {
            if let MenuItem::Submenu(menu) = item {
                count += menu.items.iter().filter(|item| item.is_disabled()).count();
            }
        }
        count
    };
    assert_eq!(disabled_items(&on), 1);
}

/// Names whose crate this one does not link, so `build_action` cannot see them here; the
/// umbrella test resolves them (`zenith_menus::tools_action_names`).
const UNLINKED_TOOLS: &[&str] = &[
    "zenith_agent_review::",
    "zenith_templates::",
    "zenith_scratches::",
    "zenith_todo::",
    "zenith_inspections::",
    "zenith_deploy::",
    "zenith_php_tools::",
    "zenith_external_libraries::",
    "zenith_wordpress::",
    "zenith_laravel::",
    "zenith_nextjs::",
    "zenith_postfix::",
    "zenith_http::",
    "zenith_markdown::",
    "zenith_open_project::",
    "install_cli::",
];

#[gpui::test]
fn linked_tools_actions_exist(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let missing: Vec<&str> = tools_action_names()
            .into_iter()
            .filter(|name| !UNLINKED_TOOLS.iter().any(|prefix| name.starts_with(prefix)))
            .filter(|name| cx.build_action(name, None).is_err())
            .collect();
        assert!(missing.is_empty(), "no such action: {missing:?}");
    });
}

fn all_menus() -> Vec<Menu> {
    arrange_with(upstream(), tools_with(&[]), extras_where(|_| true, false))
}

#[test]
fn flattening_leaves_no_submenus_and_labels_each_item() {
    let flat = for_platform(all_menus(), false);
    for menu in &flat {
        assert!(
            menu.items.iter().all(|item| depth(item) == 0),
            "{} still nests",
            menu.name
        );
        assert!(!menu.items.is_empty(), "{} is empty", menu.name);
        assert_no_stray_separators(menu);
    }
    let tools = find(&flat, "Tools");
    let tools_labels = labels(tools);
    for expected in [
        "AI: Agent Review Panel",
        "AI: Enabled",
        "Markdown: Preview Only",
        "IDE Features: Navigation Bar",
        "Create Command Line Launcher…",
    ] {
        assert!(tools_labels.iter().any(|l| l == expected), "{expected}");
    }
    // A submenu sits between separators.
    let shown = shape(tools);
    let at = shown
        .iter()
        .position(|name| name == "zenith_agent_review::ToggleFocus")
        .expect("AI item");
    assert!(at == 0 || shown[at - 1] == "-" || shown[at - 1].starts_with("zenith"));
    // Every action survives, and the check marks with it.
    let mut before = Vec::new();
    all_action_names(&all_menus(), &mut before);
    let mut after = Vec::new();
    all_action_names(&flat, &mut after);
    before.sort();
    after.sort();
    assert_eq!(before, after);
    assert_eq!(checked_flags(tools).len(), FEATURES.len());
}

#[test]
fn flattening_is_off_where_menus_nest() {
    let nested = for_platform(all_menus(), true);
    assert_eq!(
        nested.iter().map(shape).collect::<Vec<_>>(),
        all_menus().iter().map(shape).collect::<Vec<_>>()
    );
    assert!(
        nested
            .iter()
            .any(|menu| menu.items.iter().any(|i| depth(i) == 1))
    );
}

#[test]
fn flattening_edge_cases() {
    let menus = vec![
        menu(
            "A",
            vec![
                MenuItem::submenu(menu("Empty", vec![])),
                MenuItem::submenu(menu("One", vec![item("x", "a::X")]).disabled(true)),
                MenuItem::submenu(menu("Two", vec![item("y", "a::Y")])),
                item("z", "a::Z"),
            ],
        ),
        menu(
            "B",
            vec![MenuItem::submenu(menu("Only", vec![item("w", "b::W")]))],
        ),
    ];
    let flat = for_platform(menus, false);
    assert_eq!(shape(&flat[0]), ["a::X", "-", "a::Y", "-", "a::Z"]);
    assert_eq!(labels(&flat[0]), ["One: x", "Two: y", "z"]);
    let disabled: Vec<bool> = flat[0].items.iter().map(MenuItem::is_disabled).collect();
    assert_eq!(disabled, [true, false, false, false, false]);
    assert_eq!(shape(&flat[1]), ["b::W"]);
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

#[test]
fn restart_features_follow_flips() {
    let all_on = vec![true; FEATURES.len()];
    assert!(changed_restart_features(&all_on, &all_on).is_empty());
    let mut flipped = all_on.clone();
    for key in ["navbar", "agent_review", "islands"] {
        let (index, _) = feature(key).expect("feature");
        flipped[index] = false;
    }
    let keys: Vec<&str> = changed_restart_features(&all_on, &flipped)
        .iter()
        .map(|feature| feature.key)
        .collect();
    // agent_review applies live, so it never asks for a restart.
    assert_eq!(keys, ["navbar", "islands"]);
}

#[test]
fn restart_notice_names_the_palette_command() {
    let (_, navbar) = feature("navbar").expect("navbar");
    let text = restart_notice_text(navbar);
    assert_eq!(
        text,
        "Restart Zenith to apply Navigation Bar. Command palette: \"workspace: reload\"."
    );
    // The palette titles `workspace::Reload` as "workspace: reload".
    assert_eq!(workspace::Reload.name(), "workspace::Reload");
}

fn has_notice(
    key: &str,
    workspace: &gpui::Entity<workspace::Workspace>,
    cx: &mut VisualTestContext,
) -> bool {
    let (_, feature) = feature(key).expect("feature");
    let id = restart_notice_id(feature);
    workspace.read_with(cx, |workspace, _| workspace.has_notification(&id))
}

// Uses a feature no other test flips: app notifications live in a process-wide store.
#[gpui::test]
async fn restart_notice_follows_the_setting(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();

    assert!(!has_notice("templates", &workspace, cx));

    // The toggle only asks for a write; the notice comes when the setting has changed.
    cx.update(|_, cx| {
        toggle_feature(
            &ToggleFeature {
                feature: "templates".into(),
            },
            cx,
        )
    });
    cx.run_until_parked();
    let templates_off = cx.update(|_, cx| {
        !crate::FeatureToggles::get_global(cx).0[feature("templates").expect("templates").0]
    });
    assert!(templates_off, "the write reached the settings");
    assert!(has_notice("templates", &workspace, cx));

    // A live feature changing shows nothing.
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                set_feature(content, "agent_review", false);
            });
        });
    });
    cx.run_until_parked();
    assert!(!has_notice("agent_review", &workspace, cx));
}

#[test]
fn bar_items_follow_phpstorm_order() {
    let arranged = with_extras(extras_where(|_| true, false));
    assert_eq!(
        names(&arranged),
        [
            "Zenith", "File", "Edit", "View", "Navigate", "Code", "Refactor", "Run", "Tools",
            "Git", "Plugins", "Window", "Help"
        ]
    );
    assert_eq!(
        shape(find(&arranged, "Code")),
        [
            "[Code Completion]",
            "editor::ToggleCodeActions",
            "-",
            "zenith_postfix::SurroundWith",
            "zenith_postfix::LiveTemplates",
            "[Folding]",
            "-",
            "editor::ToggleComments",
            "editor::ToggleBlockComments",
            "-",
            "editor::Format",
            "editor::FormatSelections",
            "editor::AutoIndent",
            "editor::OrganizeImports",
            "-",
            "editor::MoveLineUp",
            "editor::MoveLineDown",
            "-",
            "[Inspect Code]",
        ]
    );
    assert_eq!(
        shape(find(&arranged, "Refactor")),
        [
            "zenith_refactor::RefactorThis",
            "editor::Rename",
            "zenith_refactor::ChangeSignature",
            "zenith_refactor::MoveClass",
            "zenith_refactor::SafeDelete",
            "[Extract / Introduce]",
        ]
    );
    assert_eq!(
        shape(find(&arranged, "Git")),
        [
            "git::Commit",
            "git::Amend",
            "git::Push",
            "git::ForcePush",
            "git::Pull",
            "git::Fetch",
            "-",
            "[Branches]",
            "git_panel::ToggleFocus",
            "-",
            "[Uncommitted Changes]",
            "[Current File]",
            "-",
            "git::Init",
            "git::Clone",
            "git::GenerateCommitMessage",
        ]
    );
    // Ours lead, then upstream's own Run items.
    assert_eq!(
        shape(find(&arranged, "Run")),
        [
            "zenith_run::RunSelected",
            "zenith_run::DebugSelected",
            "zenith_run::StopRun",
            "zenith_run::RerunLast",
            "-",
            "zenith_scratches::RunScratch",
            "zenith_http::RunRequest",
            "zenith_http::RunAllRequests",
            "-",
            "task::Spawn",
        ]
    );
}

#[test]
fn upstream_items_the_bar_items_repeat_stay_single_and_keep_their_place() {
    let arranged = with_extras(extras_where(|_| true, false));
    let mut all = Vec::new();
    all_action_names(&arranged, &mut all);
    for name in [
        "editor::ToggleComments",
        "editor::MoveLineUp",
        "editor::MoveLineDown",
    ] {
        assert_eq!(all.iter().filter(|n| *n == name).count(), 1, "{name}");
    }
    // The upstream item carries on, under PhpStorm's label.
    assert_eq!(
        labels(find(&arranged, "Code"))[3..5],
        ["Comment with Line Comment", "Comment with Block Comment"]
    );
}

#[test]
fn unresolved_bar_items_are_skipped() {
    // Nothing from the Zenith crates is linked: Refactor keeps only Rename, and the
    // submenus that held only Zenith actions are gone.
    let arranged = with_extras(extras_where(|name| !name.starts_with("zenith_"), false));
    assert_eq!(shape(find(&arranged, "Refactor")), ["editor::Rename"]);
    assert_eq!(
        shape(find(&arranged, "Code")),
        [
            "[Code Completion]",
            "editor::ToggleCodeActions",
            "-",
            "[Folding]",
            "-",
            "editor::ToggleComments",
            "editor::ToggleBlockComments",
            "-",
            "editor::Format",
            "editor::FormatSelections",
            "editor::AutoIndent",
            "editor::OrganizeImports",
            "-",
            "editor::MoveLineUp",
            "editor::MoveLineDown",
        ]
    );
    assert_eq!(shape(find(&arranged, "Run")), ["task::Spawn"]);
    // Nothing resolves: Refactor and Git are left out, Code is just the moved items.
    let none = with_extras(extras_where(|_| false, false));
    assert_eq!(
        names(&none),
        [
            "Zenith", "File", "Edit", "View", "Navigate", "Code", "Run", "Tools", "Plugins",
            "Window", "Help"
        ]
    );
    assert_eq!(
        shape(find(&none, "Code")),
        [
            "editor::ToggleComments",
            "editor::MoveLineUp",
            "editor::MoveLineDown"
        ]
    );
}

#[test]
fn commit_message_is_hidden_without_ai() {
    let has = |ai_disabled| {
        let arranged = with_extras(extras_where(|_| true, ai_disabled));
        shape(find(&arranged, "Git")).contains(&bar_items::COMMIT_MESSAGE.to_string())
    };
    assert!(has(false));
    assert!(!has(true));
}

fn depth(item: &MenuItem) -> usize {
    match item {
        MenuItem::Submenu(menu) => 1 + menu.items.iter().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

#[test]
fn bar_menus_nest_one_level_and_are_never_empty() {
    for keep_zenith in [true, false] {
        for ai_disabled in [true, false] {
            let arranged = with_extras(extras_where(
                |name| keep_zenith || !name.starts_with("zenith_"),
                ai_disabled,
            ));
            for menu in &arranged {
                assert!(!menu.items.is_empty(), "{} is empty", menu.name);
                assert!(
                    menu.items.iter().all(|item| depth(item) <= 1),
                    "{} nests deeper than one level",
                    menu.name
                );
                let shown = shape(menu);
                assert_ne!(
                    shown.first().map(String::as_str),
                    Some("-"),
                    "{}",
                    menu.name
                );
                assert_ne!(shown.last().map(String::as_str), Some("-"), "{}", menu.name);
                assert!(
                    shown.windows(2).all(|w| w[0] != "-" || w[1] != "-"),
                    "{}",
                    menu.name
                );
                for item in &menu.items {
                    if let MenuItem::Submenu(sub) = item {
                        assert!(!sub.items.is_empty(), "{} has an empty submenu", sub.name);
                    }
                }
            }
        }
    }
}

/// Every action name the bar items use.
fn bar_item_names() -> Vec<String> {
    let mut all = Vec::new();
    all_action_names(
        &bar_items::build(|name| Some(fake(name)), false)
            .into_iter()
            .map(|(name, items)| Menu::new(name).items(items))
            .collect::<Vec<_>>(),
        &mut all,
    );
    all
}

/// Names whose crate this one does not link, so `build_action` cannot see them here.
const UNLINKED: &[&str] = &[
    "zenith_postfix::",
    "zenith_inspections::",
    "zenith_problems::",
    "zenith_refactor::",
    "zenith_run::",
    "zenith_scratches::",
    "zenith_http::",
    "git::DiffBranch",
];

#[gpui::test]
fn linked_bar_item_actions_exist(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let missing: Vec<String> = bar_item_names()
            .into_iter()
            .filter(|name| !UNLINKED.iter().any(|prefix| name.starts_with(prefix)))
            .filter(|name| cx.build_action(name, None).is_err())
            .collect();
        assert!(missing.is_empty(), "no such action: {missing:?}");
    });
}

// Every row's unit action, dispatched like a keypress, flips that setting and nothing else, and
// asks for a restart exactly when the menu item would. templates and agent_review are left to
// `restart_notice_follows_the_setting`: app notifications live in a process-wide store.
#[gpui::test]
async fn unit_actions_toggle_like_the_menu(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        crate::init(cx);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();

    for (index, feature) in FEATURES.iter().enumerate() {
        if ["templates", "agent_review"].contains(&feature.key) {
            continue;
        }
        let before = cx.update(|_, cx| crate::FeatureToggles::get_global(cx).0.clone());
        assert!(before[index], "{} starts on", feature.key);
        cx.update(|window, cx| window.dispatch_action((feature.toggle)(), cx));
        cx.run_until_parked();
        let after = cx.update(|_, cx| crate::FeatureToggles::get_global(cx).0.clone());
        let flipped: Vec<usize> = (0..before.len())
            .filter(|i| before[*i] != after[*i])
            .collect();
        assert_eq!(
            flipped,
            [index],
            "{} flips its own setting only",
            feature.key
        );
        assert_eq!(
            has_notice(feature.key, &workspace, cx),
            feature.restart,
            "{} restart notice",
            feature.key
        );
    }
}
