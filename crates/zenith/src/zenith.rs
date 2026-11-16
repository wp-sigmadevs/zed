use gpui::App;

/// Must run after `onboarding::init` so Zenith's global action listeners
/// take precedence over upstream ones.
pub fn init(cx: &mut App) {
    #[cfg(feature = "welcome")]
    zenith_welcome::init(cx);
}
