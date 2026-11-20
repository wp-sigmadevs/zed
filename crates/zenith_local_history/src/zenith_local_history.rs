mod capture;
mod history_view;
pub mod store;
#[cfg(test)]
mod tests;

use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent};

pub use history_view::LocalHistoryView;
pub use store::{Revision, Store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct LocalHistorySettings {
    pub enabled: bool,
    pub retention_days: u32,
    pub max_db_size_mb: u64,
    pub max_file_size_kb: u64,
}

impl Settings for LocalHistorySettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let history = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.local_history.as_ref());
        Self {
            enabled: history
                .and_then(|settings| settings.enabled)
                .unwrap_or(true),
            retention_days: history
                .and_then(|settings| settings.retention_days)
                .unwrap_or(14)
                .max(1),
            max_db_size_mb: history
                .and_then(|settings| settings.max_db_size_mb)
                .unwrap_or(500)
                // 0 would make every prune delete the whole history.
                .max(1),
            max_file_size_kb: history
                .and_then(|settings| settings.max_file_size_kb)
                .unwrap_or(2048)
                // A larger file could be recorded but never read back.
                .min(store::MAX_CONTENT_BYTES / 1024),
        }
    }
}

/// Records a revision of every local file when it is opened and saved, and
/// prunes old revisions at start and every 24 hours.
pub fn init(cx: &mut App) {
    history_view::init(cx);
    capture::start(cx, |create| {
        if create {
            Store::open().map(Some)
        } else {
            Store::open_existing()
        }
    });
}
