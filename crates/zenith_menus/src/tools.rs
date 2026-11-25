//! The Tools menu: one submenu per Zenith feature (its actions plus an "Enabled" check mark),
//! the "IDE Features" submenu of the check-marked switches, and the command line launcher.

use gpui::{Action, App, Menu, MenuItem, actions};
use settings::{SettingsContent, ZenithSettingsContent};

use crate::{app_menu::tidy, bar_items::action, toggle_key};

actions!(
    zenith_features,
    [
        /// Switches the Agent Review feature on or off in the user settings.
        ToggleAgentReview,
        /// Switches the Local History feature on or off in the user settings.
        ToggleLocalHistory,
        /// Switches the Scratches feature on or off in the user settings.
        ToggleScratches,
        /// Switches the TODO feature on or off in the user settings.
        ToggleTodo,
        /// Switches the Inspections feature on or off in the user settings.
        ToggleInspections,
        /// Switches the Deployment feature on or off in the user settings.
        ToggleDeployment,
        /// Switches the WordPress feature on or off in the user settings.
        ToggleWordPress,
        /// Switches the Laravel feature on or off in the user settings.
        ToggleLaravel,
        /// Switches the Next.js feature on or off in the user settings.
        ToggleNextJs,
        /// Switches Search Everywhere on or off in the user settings.
        ToggleSearchEverywhere,
        /// Switches the Navigation Bar on or off in the user settings.
        ToggleNavbar,
        /// Switches the Tool Window Rails on or off in the user settings.
        ToggleToolWindowRails,
        /// Switches the Zenith Look on or off in the user settings.
        ToggleZenithLook,
        /// Switches the Islands layout on or off in the user settings.
        ToggleIslands,
        /// Switches File Templates on or off in the user settings.
        ToggleFileTemplates,
        /// Switches External Libraries on or off in the user settings.
        ToggleExternalLibraries,
        /// Switches the Title Bar Additions on or off in the user settings.
        ToggleTitleBarAdditions,
        /// Switches the Hover Card on or off in the user settings.
        ToggleHoverCard,
        /// Switches the Welcome screen on or off in the user settings.
        ToggleWelcome,
        /// Switches the Editor Context Menu on or off in the user settings.
        ToggleEditorContextMenu,
        /// Switches the Tab Context Menu on or off in the user settings.
        ToggleTabContextMenu,
        /// Switches the Inspection Widget on or off in the user settings.
        ToggleInspectionWidget,
    ]
);

/// One feature the Tools menu can switch, backed by a `zenith.*` boolean setting.
pub(crate) struct Feature {
    pub key: &'static str,
    pub label: &'static str,
    /// Listed under "IDE Features" instead of in a submenu of its own.
    pub ide: bool,
    /// Needs a restart to apply; the toggle offers one.
    pub restart: bool,
    pub get: fn(&ZenithSettingsContent) -> Option<bool>,
    pub set: fn(&mut ZenithSettingsContent, bool),
    /// The unit action (command palette, keymaps) that switches this feature.
    pub toggle: fn() -> Box<dyn Action>,
    /// Registers that action's handler.
    pub register: fn(&mut App),
}

/// `row!(key, Action, "Label", ide, restart)`: the setting is `zenith.<key>.enabled`.
macro_rules! row {
    ($key:ident, $action:ident, $label:literal, $ide:literal, $restart:literal) => {
        Feature {
            key: stringify!($key),
            label: $label,
            ide: $ide,
            restart: $restart,
            get: |zenith| zenith.$key.as_ref().and_then(|setting| setting.enabled),
            set: |zenith, on| zenith.$key.get_or_insert_default().enabled = Some(on),
            toggle: || Box::new($action),
            register: |cx| {
                cx.on_action(|_: &$action, cx| toggle_key(stringify!($key), cx));
            },
        }
    };
}

pub(crate) static FEATURES: &[Feature] = &[
    row!(
        agent_review,
        ToggleAgentReview,
        "Agent Review",
        false,
        false
    ),
    row!(
        local_history,
        ToggleLocalHistory,
        "Local History",
        false,
        false
    ),
    row!(scratches, ToggleScratches, "Scratches", false, true),
    row!(todo, ToggleTodo, "TODO", false, true),
    row!(inspections, ToggleInspections, "Inspections", false, true),
    row!(deploy, ToggleDeployment, "Deployment", false, true),
    row!(wordpress, ToggleWordPress, "WordPress", false, true),
    row!(laravel, ToggleLaravel, "Laravel", false, true),
    row!(nextjs, ToggleNextJs, "Next.js", false, true),
    row!(
        search,
        ToggleSearchEverywhere,
        "Search Everywhere",
        true,
        true
    ),
    row!(navbar, ToggleNavbar, "Navigation Bar", true, true),
    row!(
        rails,
        ToggleToolWindowRails,
        "Tool Window Rails",
        true,
        true
    ),
    row!(look, ToggleZenithLook, "Zenith Look", true, true),
    Feature {
        key: "islands",
        label: "Islands",
        ide: true,
        restart: true,
        get: |zenith| zenith.ui.as_ref().and_then(|ui| ui.islands),
        set: |zenith, on| zenith.ui.get_or_insert_default().islands = Some(on),
        toggle: || Box::new(ToggleIslands),
        register: |cx| {
            cx.on_action(|_: &ToggleIslands, cx| toggle_key("islands", cx));
        },
    },
    row!(templates, ToggleFileTemplates, "File Templates", true, true),
    row!(
        external_libraries,
        ToggleExternalLibraries,
        "External Libraries",
        true,
        true
    ),
    row!(
        title_bar,
        ToggleTitleBarAdditions,
        "Title Bar Additions",
        true,
        false
    ),
    row!(hover_card, ToggleHoverCard, "Hover Card", true, false),
    row!(welcome, ToggleWelcome, "Welcome", true, false),
    row!(
        editor_menu,
        ToggleEditorContextMenu,
        "Editor Context Menu",
        true,
        true
    ),
    row!(
        tab_menu,
        ToggleTabContextMenu,
        "Tab Context Menu",
        true,
        false
    ),
    row!(
        inspection_widget,
        ToggleInspectionWidget,
        "Inspection Widget",
        true,
        false
    ),
];

/// A Tools submenu: the actions it lists (label, action name) and, when the feature is not an
/// IDE one, the feature whose "Enabled" check mark closes it.
pub(crate) struct Group {
    pub name: &'static str,
    pub feature: Option<&'static str>,
    pub items: &'static [(&'static str, &'static str)],
}

/// Not built, so not listed: Local History's revision actions (they work inside its tab) and
/// Scratches' rename / delete / reveal / run (they act on the scratch file in the project panel).
pub(crate) static GROUPS: &[Group] = &[
    Group {
        name: "AI",
        feature: Some("agent_review"),
        items: &[
            ("Agent Review Panel", "zenith_agent_review::ToggleFocus"),
            ("Keep All Agent Edits", "zenith_agent_review::KeepAll"),
            ("Revert Agent Turn", "zenith_agent_review::RevertTurn"),
        ],
    },
    Group {
        name: "Templates",
        feature: None,
        items: &[
            ("New from Template…", "zenith_templates::NewFromTemplate"),
            ("New PHP Class…", "zenith_templates::NewPhpClass"),
        ],
    },
    Group {
        name: "Local History",
        feature: Some("local_history"),
        items: &[
            ("Show History", "zenith_local_history::ShowFileHistory"),
            (
                "Recover Deleted Files…",
                "zenith_local_history::RecoverDeletedFiles",
            ),
        ],
    },
    Group {
        name: "Scratches",
        feature: Some("scratches"),
        items: &[
            ("New Scratch File", "zenith_scratches::NewScratchFile"),
            ("Scratches Panel", "zenith_scratches::ToggleFocus"),
        ],
    },
    Group {
        name: "TODO",
        feature: Some("todo"),
        items: &[
            ("TODO Panel", "zenith_todo::ToggleFocus"),
            ("Refresh", "zenith_todo::Refresh"),
            ("Next TODO", "zenith_todo::NextTodo"),
            ("Previous TODO", "zenith_todo::PreviousTodo"),
        ],
    },
    Group {
        name: "Inspections",
        feature: Some("inspections"),
        items: &[
            (
                "Inspect Current File",
                "zenith_inspections::InspectCurrentFile",
            ),
            (
                "Inspect Changed Files",
                "zenith_inspections::InspectChangedFiles",
            ),
            ("Inspect Project", "zenith_inspections::InspectProject"),
            (
                "Fix File with PHPCBF",
                "zenith_inspections::FixFileWithPhpcbf",
            ),
        ],
    },
    Group {
        name: "Deployment",
        feature: Some("deploy"),
        items: &[
            ("Configure Deployment…", "zenith_deploy::OpenDeployment"),
            ("Remote Host Panel", "zenith_deploy::ToggleFocus"),
        ],
    },
    Group {
        name: "PHP",
        feature: None,
        items: &[
            ("PHP Tools Status", "zenith_php_tools::Status"),
            ("Update PHP Tools", "zenith_php_tools::Update"),
            (
                "Show PHP Tools Install Log",
                "zenith_php_tools::ShowInstallLog",
            ),
            (
                "Configure PHP Include Paths…",
                "zenith_external_libraries::ConfigurePhpIncludePaths",
            ),
            (
                "Add Library Path…",
                "zenith_external_libraries::AddLibraryPath",
            ),
        ],
    },
    Group {
        name: "WordPress",
        feature: Some("wordpress"),
        items: &[(
            "WordPress Hooks Panel",
            "zenith_wordpress::ToggleWpHooksPanel",
        )],
    },
    Group {
        name: "Laravel",
        feature: Some("laravel"),
        items: &[
            ("Laravel Panel", "zenith_laravel::ToggleLaravelPanel"),
            ("Show Routes", "zenith_laravel::ShowRoutes"),
            ("Show Views", "zenith_laravel::ShowViews"),
        ],
    },
    Group {
        name: "Next.js",
        feature: Some("nextjs"),
        items: &[
            ("Next.js Panel", "zenith_nextjs::ToggleNextJsPanel"),
            ("Show All Routes", "zenith_nextjs::ShowAllRoutes"),
            ("Show Pages", "zenith_nextjs::ShowPages"),
            ("Show Route Handlers", "zenith_nextjs::ShowRouteHandlers"),
        ],
    },
    Group {
        name: "Postfix",
        feature: None,
        items: &[
            ("Surround With…", "zenith_postfix::SurroundWith"),
            ("Insert Live Template…", "zenith_postfix::LiveTemplates"),
        ],
    },
    Group {
        name: "HTTP Client",
        feature: None,
        items: &[
            ("Run Request", "zenith_http::RunRequest"),
            ("Run All Requests", "zenith_http::RunAllRequests"),
            ("Copy as cURL", "zenith_http::CopyAsCurl"),
            ("Select Environment…", "zenith_http::SelectEnvironment"),
            ("Import from cURL…", "zenith_http::ImportFromCurl"),
        ],
    },
    Group {
        name: "Markdown",
        feature: None,
        items: &[
            ("Editor Only", "zenith_markdown::ShowEditorOnly"),
            (
                "Editor and Preview",
                "zenith_markdown::ShowEditorAndPreview",
            ),
            ("Preview Only", "zenith_markdown::ShowPreviewOnly"),
        ],
    },
];

/// Upstream's "Install CLI" (the app menu drops it), under the name PhpStorm users know.
const COMMAND_LINE_LAUNCHER: (&str, &str) = (
    "Create Command Line Launcher…",
    "install_cli::InstallCliBinary",
);

/// Every action name the Tools menu looks up, for tests that check they exist.
pub fn tools_action_names() -> Vec<&'static str> {
    GROUPS
        .iter()
        .flat_map(|group| group.items.iter().map(|(_, name)| *name))
        .chain([COMMAND_LINE_LAUNCHER.1])
        .collect()
}

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

/// `on` follows `FEATURES`; a missing entry counts as on. `resolve` turns an action name into
/// an action; names it cannot resolve are skipped, and so is a submenu left empty. Menus nest
/// one level at most: each feature is one submenu, the IDE switches are one more.
pub(crate) fn tools_menu(
    on: &[bool],
    mut resolve: impl FnMut(&str) -> Option<Box<dyn Action>>,
) -> Menu {
    let state = |feature: &Feature| {
        let index = feature_index(feature.key);
        on.get(index).copied().unwrap_or(true)
    };
    let mut items = Vec::new();
    for group in GROUPS {
        let mut inner: Vec<MenuItem> = group
            .items
            .iter()
            .filter_map(|(label, name)| Some(action(label, resolve(name)?)))
            .collect();
        if let Some((_, feature)) = group.feature.and_then(feature) {
            if !inner.is_empty() {
                inner.push(MenuItem::separator());
            }
            inner.push(action("Enabled", (feature.toggle)()).checked(state(feature)));
        }
        if !inner.is_empty() {
            items.push(MenuItem::submenu(Menu::new(group.name).items(inner)));
        }
    }
    // The hover card renderer is registered only while Look is on.
    let look_on = feature("look").is_none_or(|(_, look)| state(look));
    let ide = FEATURES
        .iter()
        .filter(|feature| feature.ide)
        .map(|feature| {
            action(feature.label, (feature.toggle)())
                .checked(state(feature))
                .disabled(feature.key == "hover_card" && !look_on)
        });
    items.push(MenuItem::separator());
    items.push(MenuItem::submenu(Menu::new("IDE Features").items(ide)));
    if let Some(launcher) = resolve(COMMAND_LINE_LAUNCHER.1) {
        items.push(MenuItem::separator());
        items.push(action(COMMAND_LINE_LAUNCHER.0, launcher));
    }
    Menu::new("Tools").items(tidy(items))
}

fn feature_index(key: &str) -> usize {
    feature(key).map_or(usize::MAX, |(index, _)| index)
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
