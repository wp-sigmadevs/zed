use gpui::App;

#[cfg(not(feature = "welcome"))]
pub use onboarding::show_onboarding_view;
#[cfg(feature = "welcome")]
pub use zenith_welcome::show_onboarding_view;

/// Shows the startup card; call right after `release_channel::init`.
pub fn show_splash(cx: &mut App) {
    #[cfg(feature = "splash")]
    zenith_splash::show(cx);
    #[cfg(not(feature = "splash"))]
    let _ = cx;
}

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
    #[cfg(feature = "markdown")]
    zenith_markdown::init(cx);
    #[cfg(feature = "problems")]
    zenith_problems::init(cx);
    #[cfg(feature = "folds")]
    zenith_folds::init(cx);
    #[cfg(feature = "templates")]
    zenith_templates::init(cx);
    #[cfg(feature = "scratches")]
    zenith_scratches::init(cx);
    #[cfg(feature = "local_history")]
    zenith_local_history::init(cx);
    #[cfg(feature = "wordpress")]
    zenith_wordpress::init(cx);
    #[cfg(feature = "laravel")]
    zenith_laravel::init(cx);
    #[cfg(feature = "nextjs")]
    zenith_nextjs::init(cx);
    #[cfg(feature = "postfix")]
    zenith_postfix::init(cx);
    #[cfg(feature = "http")]
    zenith_http::init(cx);
    #[cfg(feature = "refactor")]
    zenith_refactor::init(cx);
    #[cfg(feature = "external_libraries")]
    zenith_external_libraries::init(cx);
    #[cfg(feature = "agent_review")]
    zenith_agent_review::init(cx);
    #[cfg(feature = "navbar")]
    zenith_navbar::init(cx);
}
