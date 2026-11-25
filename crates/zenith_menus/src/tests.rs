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
    restart_notice_id, restart_notice_text, toggle_feature,
    tools::{FEATURES, changed_restart_features, feature, feature_states, set_feature, tools_menu},
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
