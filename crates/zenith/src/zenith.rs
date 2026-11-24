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
    zenith_title_bar::init(cx);
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
    #[cfg(feature = "deploy")]
    zenith_deploy::init(cx);
    #[cfg(feature = "menus")]
    zenith_menus::init(cx);
}

/// The menu bar: upstream's menus in PhpStorm order with a Tools menu. Call instead of
/// `upstream` wherever the bar is (re)built.
pub fn app_menus(upstream: fn(&mut App) -> Vec<gpui::Menu>, cx: &mut App) -> Vec<gpui::Menu> {
    #[cfg(feature = "menus")]
    {
        zenith_menus::app_menus(upstream, cx)
    }
    #[cfg(not(feature = "menus"))]
    {
        upstream(cx)
    }
}

/// Whether the editor toolbar's right-hand items are drawn at the tab bar's end (the rails are
/// on and the tab bar and its buttons are shown) instead of in the toolbar.
pub fn draws_tab_bar_end(cx: &App) -> bool {
    #[cfg(feature = "rails")]
    {
        zenith_rails::draws_tab_bar_end(cx)
    }
    #[cfg(not(feature = "rails"))]
    {
        let _ = cx;
        false
    }
}

/// Draws the editor's quick-action menu at the right end of its pane's tab bar, while the
/// `quick_actions` setting is on and the active tab is an editor.
pub fn quick_actions_tab_bar_end(
    view: impl Into<gpui::AnyView>,
    cx: &mut gpui::Context<workspace::Toolbar>,
) {
    #[cfg(feature = "rails")]
    zenith_rails::quick_actions_tab_bar_end(view, cx);
    #[cfg(not(feature = "rails"))]
    let _ = (view, cx);
}
