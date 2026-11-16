use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

/// Settings for Zenith-only features.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ZenithSettingsContent {
    /// Settings for the Zenith welcome screen.
    pub welcome: Option<ZenithWelcomeSettingsContent>,
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
