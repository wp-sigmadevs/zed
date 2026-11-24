use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

/// Settings for Zenith-only features.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithSettingsContent {
    /// Settings for the Zenith welcome screen.
    pub welcome: Option<ZenithWelcomeSettingsContent>,
    /// Settings for PHP inspections (PHPCS and PHPStan).
    pub inspections: Option<ZenithInspectionsSettingsContent>,
    /// Settings for the TODO panel.
    pub todo: Option<ZenithTodoSettingsContent>,
    /// Settings for Local History.
    pub local_history: Option<ZenithLocalHistorySettingsContent>,
    /// Settings for the External Libraries project panel section.
    pub external_libraries: Option<ZenithExternalLibrariesSettingsContent>,
    /// Settings for the New-from-template popup.
    pub templates: Option<ZenithTemplatesSettingsContent>,

    /// Settings for the tool window rails.
    pub rails: Option<ZenithRailsSettingsContent>,

    /// Settings for Zenith's window chrome.
    pub ui: Option<ZenithUiSettingsContent>,

    /// Settings for Zenith's title bar additions.
    pub title_bar: Option<ZenithTitleBarSettingsContent>,

    /// Settings for Zenith's editor look.
    pub look: Option<ZenithLookSettingsContent>,

    /// Settings for Search Everywhere.
    pub search: Option<ZenithSearchSettingsContent>,
    /// Settings for the WordPress Hooks tool window.
    pub wordpress: Option<ZenithWordPressSettingsContent>,
    /// Settings for the Laravel tool window.
    pub laravel: Option<ZenithLaravelSettingsContent>,
    /// Settings for the Next.js tool window.
    pub nextjs: Option<ZenithNextJsSettingsContent>,
    /// Settings for the hover card.
    pub hover_card: Option<ZenithHoverCardSettingsContent>,
    /// Settings for agent review.
    pub agent_review: Option<ZenithAgentReviewSettingsContent>,
    /// Settings for scratch files.
    pub scratches: Option<ZenithScratchesSettingsContent>,
    /// Settings for Deployment (Remote Host tool window).
    pub deploy: Option<ZenithDeploySettingsContent>,
    /// Settings for PHP tools.
    pub php_tools: Option<ZenithPhpToolsSettingsContent>,
    /// Settings for the navigation bar.
    pub navbar: Option<ZenithNavbarSettingsContent>,
}

/// Settings for the navigation bar.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithNavbarSettingsContent {
    /// Whether the navigation bar (project › folder › file › symbol) sits at the left of the
    /// status bar. Takes effect after a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for Deployment. Upload on save is a per-server option, never a setting.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithDeploySettingsContent {
    /// Whether the Remote Host tool window and Deployment actions are offered. Read at startup.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Where the Remote Host tool window docks.
    ///
    /// Default: right
    pub dock: Option<crate::DockPosition>,
}

/// Settings for PHP tools.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithPhpToolsSettingsContent {
    /// Absolute path of the PHP command line interpreter that tools and inspections run,
    /// tried before every PHP found on this machine. Read from user settings only: a
    /// binary chosen by a project would run its code.
    ///
    /// Default: null
    pub php_binary: Option<String>,
}

/// Settings for agent review.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithAgentReviewSettingsContent {
    /// Whether each AI agent turn records local-history checkpoints of the
    /// files it edits and its changes are offered for review.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the New-from-template popup.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithTemplatesSettingsContent {
    /// Whether the New popup (file templates) is offered. Takes effect after a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the tool window rails.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithRailsSettingsContent {
    /// Whether tool window buttons sit in rails down each side of the window instead of
    /// the status bar. Takes effect after a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for Zenith's window chrome.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithUiSettingsContent {
    /// Whether the editor, docks and bottom panel are drawn as separate rounded islands.
    /// Takes effect after a restart.
    ///
    /// Default: true
    pub islands: Option<bool>,
}

/// Settings for Zenith's title bar additions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithTitleBarSettingsContent {
    /// Whether the title bar shows the project badge, search box, run controls and agent chip.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for Zenith's editor look.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithLookSettingsContent {
    /// Whether Zenith's icon theme, hint pills, AI bulb actions, hover card and indent
    /// status item are installed. Takes effect after a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for Search Everywhere.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithSearchSettingsContent {
    /// Whether Search Everywhere, Find in Files, Show Usages and the Switcher actions are
    /// registered. Takes effect after a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the WordPress Hooks tool window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithWordPressSettingsContent {
    /// Whether the WordPress Hooks tool window is offered for trusted WordPress projects.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the Laravel tool window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithLaravelSettingsContent {
    /// Whether the Laravel tool window (routes and views) is offered for trusted Laravel projects.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the Next.js tool window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithNextJsSettingsContent {
    /// Whether the Next.js tool window (App Router routes) is offered for trusted Next.js projects.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the hover card.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithHoverCardSettingsContent {
    /// Whether hovering shows one card holding the diagnostic and the
    /// documentation, instead of separate popovers.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for the External Libraries project panel section.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithExternalLibrariesSettingsContent {
    /// Whether vendor/, node_modules/ and WordPress core are moved under an
    /// "External Libraries" node in the project panel.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Absolute folders to treat as libraries: project-local settings, tasks, debug configs
    /// and editorconfig files inside them are never loaded. Read from user settings only.
    ///
    /// Default: []
    pub paths: Option<Vec<String>>,
}

/// Settings for the Zenith welcome screen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithWelcomeSettingsContent {
    /// Whether to show the Zenith welcome screen instead of the upstream
    /// onboarding and welcome pages.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Settings for PHP inspections.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithInspectionsSettingsContent {
    /// Whether PHP inspections run and their actions are registered. Takes effect after
    /// a restart.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Whether PHPCS runs.
    ///
    /// Default: true
    pub phpcs: Option<bool>,
    /// Whether PHPStan runs. It only runs in projects that have a PHPStan config.
    ///
    /// Default: true
    pub phpstan: Option<bool>,
    /// Whether saving a PHP file runs the analyzers on it.
    ///
    /// Default: true
    pub run_on_save: Option<bool>,
    /// How many files "Inspect changed files" analyzes at once.
    ///
    /// Default: 2
    pub max_parallel: Option<u32>,
}

/// Settings for the TODO panel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithTodoSettingsContent {
    /// Regex patterns, matched case-insensitively inside comments.
    ///
    /// Default: ["\\bTODO\\b", "\\bFIXME\\b", "\\bHACK\\b", "\\bXXX\\b", "@todo", "shortcut:"]
    pub patterns: Option<Vec<String>>,
    /// Whether TODO markers are coloured in the editor.
    ///
    /// Default: true
    pub highlight: Option<bool>,
    /// Which files the TODO panel lists when it opens.
    ///
    /// Default: project
    pub scope: Option<ZenithTodoScope>,
    /// Where the TODO panel docks.
    ///
    /// Default: bottom
    pub dock: Option<crate::DockPosition>,
    /// Whether the TODO panel, its editor colouring and the commit warning are on.
    ///
    /// Default: true
    pub enabled: Option<bool>,
}

/// Which files the TODO panel lists.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom)]
#[serde(rename_all = "snake_case")]
pub enum ZenithTodoScope {
    /// Every file in the project.
    Project,
    /// The file in the active editor.
    CurrentFile,
    /// Files git reports as changed.
    ChangedFiles,
}

/// Settings for Local History.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithLocalHistorySettingsContent {
    /// Whether file revisions are recorded when a file is opened or saved.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// How many days a revision is kept.
    ///
    /// Default: 14
    pub retention_days: Option<u32>,
    /// Size cap of the history database in megabytes; the oldest revisions go first.
    ///
    /// Default: 500
    pub max_db_size_mb: Option<u64>,
    /// Files larger than this many kilobytes are not recorded.
    ///
    /// Default: 2048
    pub max_file_size_kb: Option<u64>,
    /// Whether a change made to a file on disk by something else (a git
    /// checkout, a formatter, another tool) is recorded as a revision.
    ///
    /// Default: true
    pub snapshot_external_changes: Option<bool>,
}

/// Settings for scratch files.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithScratchesSettingsContent {
    /// Whether the Scratches tool window and scratch actions are offered.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Absolute folder that holds scratch files, instead of the default one in the data dir.
    ///
    /// Default: null
    pub directory: Option<String>,
    /// Run commands by file extension, as a program and its arguments; `$ZED_FILE` is the
    /// scratch file. Merged over the built-in runners; an empty list disables one.
    ///
    /// Default: {}
    pub runners: Option<collections::HashMap<String, Vec<String>>>,
    /// Whether a scratch runs in the project's root folder (e.g. so PHP's
    /// `vendor/autoload.php` resolves), instead of the scratches folder.
    ///
    /// Default: true
    pub use_project_cwd: Option<bool>,
    /// Whether the project panel ends with a "Scratches and Consoles" node listing the
    /// scratch files, as in PhpStorm.
    ///
    /// Default: true
    pub project_panel_node: Option<bool>,
}
