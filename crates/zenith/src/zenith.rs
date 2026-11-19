use gpui::App;

#[cfg(not(feature = "welcome"))]
pub use onboarding::show_onboarding_view;
#[cfg(feature = "welcome")]
pub use zenith_welcome::show_onboarding_view;

/// Must run after `onboarding::init` so Zenith's global action listeners
/// take precedence over upstream ones.
pub fn init(cx: &mut App) {
    #[cfg(feature = "welcome")]
    zenith_welcome::init(cx);
    #[cfg(feature = "search")]
    zenith_search::init(cx);
    #[cfg(feature = "rails")]
    zenith_rails::init(cx);
    #[cfg(feature = "php")]
    zenith_php_tools::init(cx);
    #[cfg(feature = "inspections")]
    zenith_inspections::init(cx);
    #[cfg(feature = "look")]
    zenith_look::init(cx);
    #[cfg(feature = "todo")]
    zenith_todo::init(cx);
}
