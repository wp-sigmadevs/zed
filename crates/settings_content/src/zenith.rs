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
