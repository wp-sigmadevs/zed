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
    /// Settings for the hover card.
    pub hover_card: Option<ZenithHoverCardSettingsContent>,
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
