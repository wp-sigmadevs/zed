use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Debug, Clone, Copy, RegisterSetting)]
pub struct ZenithWelcomeSettings {
    pub enabled: bool,
}

impl Settings for ZenithWelcomeSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let enabled = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.welcome.as_ref())
            .and_then(|welcome| welcome.enabled)
            .unwrap_or(true);
        Self { enabled }
    }
}

pub fn init(cx: &mut App) {
    ZenithWelcomeSettings::register(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use settings::SettingsStore;

    #[gpui::test]
    fn welcome_enabled_by_default_and_can_be_disabled(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            init(cx);
            assert!(ZenithWelcomeSettings::get_global(cx).enabled);

            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{"zenith": {"welcome": {"enabled": false}}}"#, cx)
                    .result()
                    .expect("valid settings");
            });
            assert!(!ZenithWelcomeSettings::get_global(cx).enabled);
        });
    }
}
