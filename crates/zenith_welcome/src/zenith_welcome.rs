mod customize;
pub mod detect;
mod learn;
pub mod open_choice;
mod plugins;
mod projects;
mod view;

use std::sync::Arc;

use db::kvp::KeyValueStore;
use gpui::{
    AnyView, App, AppContext as _, Context, FocusHandle, Task, TaskExt as _, WeakEntity, Window,
};
use onboarding::FIRST_OPEN;
use settings::{RegisterSetting, Settings, SettingsContent};
use workspace::{
    AppState, Workspace,
    dock::DockPosition,
    open_new,
    welcome::{WelcomeOverride, WelcomePage},
    with_active_or_new_workspace,
};
use zed_actions::OpenOnboarding;

pub use projects::open_mode;
pub use view::{DetectCache, FinishSetup, NewProject, Page, ZenithWelcome};

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
    open_choice::init(cx);
    cx.set_global(WelcomeOverride(welcome_override));

    // Registered after `onboarding::init`, so this listener runs first.
    cx.on_action(|_: &OpenOnboarding, cx| {
        if !ZenithWelcomeSettings::get_global(cx).enabled {
            cx.propagate();
            return;
        }
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            workspace
                .with_local_workspace(window, cx, |workspace, window, cx| {
                    show_page(workspace, Page::Customize, window, cx);
                })
                .detach_and_log_err(cx);
        });
    });
}

fn welcome_override(
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyView> {
    ZenithWelcomeSettings::get_global(cx).enabled.then(|| {
        let view = cx.new(|cx| ZenithWelcome::new(workspace, focus_handle, window, cx));
        // WelcomePage keeps the view private; tests reach it through this.
        #[cfg(test)]
        cx.set_global(tests::LastWelcome(view.downgrade()));
        view.into()
    })
}

/// Activates (or adds) the welcome page in `workspace` and switches it to `page`.
pub fn show_page(
    workspace: &mut Workspace,
    page: Page,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    view::request_page(cx.entity_id(), page, cx);
    let existing = workspace
        .active_pane()
        .read(cx)
        .items()
        .find_map(|item| item.downcast::<WelcomePage>());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
    } else {
        let welcome = cx.new(|cx| WelcomePage::new(workspace.weak_handle(), false, window, cx));
        workspace.add_item_to_active_pane(Box::new(welcome), None, true, window, cx);
    }
}

/// First-run entry point. Replaces `onboarding::show_onboarding_view`
/// unless the Zenith welcome screen is turned off.
pub fn show_onboarding_view(app_state: Arc<AppState>, cx: &mut App) -> Task<anyhow::Result<()>> {
    if !ZenithWelcomeSettings::get_global(cx).enabled {
        return onboarding::show_onboarding_view(app_state, cx);
    }
    open_new(
        Default::default(),
        app_state,
        cx,
        |workspace, window, cx| {
            workspace.toggle_dock(DockPosition::Left, window, cx);
            show_page(workspace, Page::Customize, window, cx);
            // Same flag as upstream onboarding, so returning users skip setup.
            let kvp = KeyValueStore::global(cx);
            db::write_and_log(cx, move || async move {
                kvp.write_kvp(FIRST_OPEN.to_string(), "false".to_string())
                    .await
            });
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::{FakeFs, Fs as _};
    use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            // Customize renders upstream theme tiles, which need the bundled themes.
            theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
            editor::init(cx);
            init(cx);
        });
    }

    // Plan 34 step 1: Driftwood is bundled and is the default dark theme.
    #[gpui::test]
    fn test_driftwood_is_bundled_default_dark(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let theme = theme::ThemeRegistry::global(cx)
                .get("Driftwood")
                .expect("Driftwood bundled");
            assert_eq!(theme.appearance, theme::Appearance::Dark);
        });
        let defaults = settings::default_settings();
        assert!(defaults.contains(r#""dark": "Driftwood""#));
        // Icons ship inside the app (zenith_look), not from the extension registry.
        assert!(defaults.contains(r#""icon_theme": "Archipelago Icons""#));
        assert!(!defaults.contains(r#""archipelago-icons": true"#));
    }

    // Plan 34 step 6: PhpStorm layout — project tools left, agent right.
    #[test]
    fn test_panels_dock_like_phpstorm() {
        let defaults: serde_json::Value =
            settings::parse_json_with_comments(&settings::default_settings())
                .expect("default settings parse");
        for (panel, side) in [
            ("project_panel", "left"),
            ("outline_panel", "left"),
            ("git_panel", "left"),
            ("agent", "right"),
            ("debugger", "bottom"),
            ("terminal", "right"),
        ] {
            assert_eq!(defaults[panel]["dock"], side, "{panel}");
        }
    }

    // Plan 34 step 3: types show in the editor, as in PhpStorm. Inline diagnostics are off
    // (`29ab1ddb12`): squiggle + hover, since noisy language servers made them unusable.
    #[gpui::test]
    fn test_inline_diagnostics_off_and_inlay_hints_on_by_default(cx: &mut TestAppContext) {
        use settings::Settings as _;
        init_test(cx);
        cx.update(|cx| {
            let project_settings = project::project_settings::ProjectSettings::get_global(cx);
            assert!(!project_settings.diagnostics.inline.enabled);
            let language_settings = language::language_settings::all_language_settings(None, cx);
            assert!(language_settings.defaults.inlay_hints.enabled);
        });
    }

    #[gpui::test]
    fn test_driftwood_warning_is_not_agent_orange(cx: &mut TestAppContext) {
        use theme::ActiveTheme as _;
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(r#"{"theme": "Driftwood"}"#, cx)
                    .result()
                    .expect("valid settings");
            });
        });
        // Theme reload runs in the settings observer, after the update above.
        cx.update(|cx| {
            assert_eq!(cx.theme().name, "Driftwood");
            let amber: gpui::Hsla = gpui::rgb(0xe2b13c).into();
            assert_eq!(cx.theme().status().warning, amber);
        });
    }

    fn set_enabled(enabled: bool, cx: &mut App) {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    &format!(r#"{{"zenith": {{"welcome": {{"enabled": {enabled}}}}}}}"#),
                    cx,
                )
                .result()
                .expect("valid settings");
        });
    }

    /// The welcome view most recently built by `welcome_override`.
    pub(crate) struct LastWelcome(pub(crate) gpui::WeakEntity<ZenithWelcome>);
    impl gpui::Global for LastWelcome {}

    fn last_welcome(cx: &mut VisualTestContext) -> Entity<ZenithWelcome> {
        cx.update(|_, cx| cx.global::<LastWelcome>().0.upgrade())
            .expect("welcome view alive")
    }

    fn recent(id: i64, path: &str) -> workspace::RecentWorkspace {
        workspace::RecentWorkspace {
            workspace_id: workspace::WorkspaceId::from_i64(id),
            location: workspace::SerializedWorkspaceLocation::Local,
            paths: workspace::PathList::new(&[path]),
            identity_paths: workspace::PathList::new(&[path]),
            path_style: None,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Whether any window has a project rooted at `root`. Through the app context, not a
    /// window update: that leases the current window, and `cx.windows()` skips it.
    fn project_open(root: &str, cx: &mut VisualTestContext) -> bool {
        cx.cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .any(|window| {
                    window.read(cx).is_ok_and(|multi| {
                        multi.workspaces().any(|workspace| {
                            workspace
                                .read(cx)
                                .project()
                                .read(cx)
                                .visible_worktrees(cx)
                                .any(|tree| tree.read(cx).abs_path().as_ref() == Path::new(root))
                        })
                    })
                })
        })
    }

    async fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        workspace_on(fs, cx).await
    }

    async fn workspace_on(
        fs: Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let project = Project::test(fs, None, cx).await;
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        // Upstream Customize sections read these app globals.
        cx.update(|_, cx| {
            let app_state = workspace.read(cx).app_state().clone();
            client::Client::set_global(app_state.client.clone(), cx);
            <dyn fs::Fs>::set_global(app_state.fs.clone(), cx);
        });
        (workspace, cx)
    }

    fn welcome_pages(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items()
                .filter(|item| item.downcast::<WelcomePage>().is_some())
                .count()
        })
    }

    #[gpui::test]
    fn welcome_enabled_by_default_and_can_be_disabled(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(ZenithWelcomeSettings::get_global(cx).enabled);
            set_enabled(false, cx);
            assert!(!ZenithWelcomeSettings::get_global(cx).enabled);
        });
    }

    #[gpui::test]
    async fn override_follows_setting(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = workspace(cx).await;
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            let view = {
                let focus = cx.focus_handle();
                welcome_override(weak.clone(), focus, window, cx)
            }
            .expect("override on by default");
            let view = view.downcast::<ZenithWelcome>().expect("zenith view");
            assert_eq!(view.read(cx).page(), Page::Projects);

            set_enabled(false, cx);
            assert!(
                {
                    let focus = cx.focus_handle();
                    welcome_override(weak, focus, window, cx)
                }
                .is_none()
            );
        });
    }

    #[gpui::test]
    async fn page_request_targets_one_workspace(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = workspace(cx).await;
        let weak = workspace.downgrade();
        cx.update(|window, cx| {
            view::request_page(gpui::EntityId::from(u64::MAX), Page::Learn, cx);
            let other = {
                let focus = cx.focus_handle();
                welcome_override(weak.clone(), focus, window, cx)
            }
            .and_then(|v| v.downcast::<ZenithWelcome>().ok())
            .expect("zenith view");
            assert_eq!(other.read(cx).page(), Page::Projects);

            view::request_page(workspace.entity_id(), Page::Customize, cx);
            let ours = {
                let focus = cx.focus_handle();
                welcome_override(weak, focus, window, cx)
            }
            .and_then(|v| v.downcast::<ZenithWelcome>().ok())
            .expect("zenith view");
            assert_eq!(ours.read(cx).page(), Page::Customize);
        });
        // Cleared after every view has applied it.
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(matches!(
                cx.global::<view::PageRequest>(),
                view::PageRequest(None)
            ));
        });
    }

    #[gpui::test]
    async fn open_onboarding_shows_customize(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = workspace(cx).await;
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        assert_eq!(welcome_pages(&workspace, cx), 1);
        cx.update(|_, cx| {
            assert!(matches!(
                cx.global::<view::PageRequest>(),
                view::PageRequest(None)
            ));
        });

        // A second request reuses the open page.
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        assert_eq!(welcome_pages(&workspace, cx), 1);
    }

    #[gpui::test]
    async fn open_onboarding_falls_through_when_disabled(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| set_enabled(false, cx));
        let (workspace, cx) = workspace(cx).await;
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        assert_eq!(welcome_pages(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn projects_picker_filters(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = workspace(cx).await;
        let weak = workspace.downgrade();
        let view = cx.update(|window, cx| {
            let focus = cx.focus_handle();
            welcome_override(weak, focus, window, cx)
                .and_then(|v| v.downcast::<ZenithWelcome>().ok())
                .expect("zenith view")
        });
        // Let the initial (empty) DB load finish before seeding.
        cx.run_until_parked();
        let picker = view.read_with(cx, |view, _| view.projects().clone());
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.set_projects(vec![
                recent(1, "/code/laravel-crm"),
                recent(2, "/sites/radius-theme"),
            ]);
            picker.set_query("crm", window, cx);
        });
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.matched_paths(), vec!["/code/laravel-crm"]);
        });
    }

    #[gpui::test]
    async fn every_page_renders(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, cx) = workspace(cx).await;
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        for page in [Page::Projects, Page::Customize, Page::Plugins, Page::Learn] {
            cx.update(|_, cx| view::request_page(workspace.entity_id(), page, cx));
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.update(|_, cx| {
                assert!(matches!(
                    cx.global::<view::PageRequest>(),
                    view::PageRequest(None)
                ));
            });
        }
    }

    #[gpui::test]
    async fn first_run_opens_customize_and_writes_flag(cx: &mut TestAppContext) {
        // `AppState::test` installs its own settings store and base themes.
        let app_state = cx.update(AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
            editor::init(cx);
            init(cx);
            client::Client::set_global(app_state.client.clone(), cx);
            <dyn fs::Fs>::set_global(app_state.fs.clone(), cx);
        });
        cx.update(|cx| show_onboarding_view(app_state, cx))
            .await
            .expect("first run opens a workspace");
        cx.run_until_parked();
        let flag = cx.update(|cx| KeyValueStore::global(cx).read_kvp(FIRST_OPEN));
        assert_eq!(flag.expect("kvp read").as_deref(), Some("false"));
        cx.update(|cx| {
            assert!(matches!(
                cx.global::<view::PageRequest>(),
                view::PageRequest(None)
            ));
        });
    }

    #[gpui::test]
    async fn enter_opens_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/code/crm", serde_json::json!({ "artisan": "" }))
            .await;
        let (workspace, cx) = workspace_on(fs, cx).await;
        cx.update(|_, cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "enter",
                menu::Confirm,
                Some("Picker"),
            )])
        });
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        let view = last_welcome(cx);
        cx.update(|_, cx| view::request_page(workspace.entity_id(), Page::Projects, cx));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.page()), Page::Projects);

        let picker = view.read_with(cx, |view, _| view.projects().clone());
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.set_projects(vec![recent(1, "/code/crm")]);
            picker.refresh(window, cx);
            picker.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!project_open("/code/crm", cx));

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(project_open("/code/crm", cx));
    }

    #[gpui::test]
    async fn new_project_creates_folder_and_opens_it(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/code", serde_json::json!({})).await;
        let (_workspace, cx) = workspace_on(fs.clone(), cx).await;
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // Through the welcome view's key context, as the button does.
        cx.dispatch_action(NewProject);
        cx.run_until_parked();
        assert!(cx.did_prompt_for_new_path());
        cx.simulate_new_path_selection(|_| Some(PathBuf::from("/code/new-app")));
        cx.run_until_parked();

        assert!(fs.is_dir(Path::new("/code/new-app")).await);
        assert!(project_open("/code/new-app", cx));
    }

    #[gpui::test]
    async fn finish_setup_returns_to_projects(cx: &mut TestAppContext) {
        init_test(cx);
        let (_workspace, cx) = workspace(cx).await;
        cx.dispatch_action(OpenOnboarding);
        cx.run_until_parked();
        let view = last_welcome(cx);
        assert_eq!(view.read_with(cx, |view, _| view.page()), Page::Customize);
        cx.update(|window, cx| window.draw(cx).clear(cx));

        cx.dispatch_action(FinishSetup);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.page()), Page::Projects);
    }

    #[gpui::test]
    async fn detect_cache_hit_skips_rescan(cx: &mut TestAppContext) {
        use detect::{Framework, ProjectInfo};
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/code",
            serde_json::json!({
                "crm": { "artisan": "" },
                "api": { "package.json": "{}" },
            }),
        )
        .await;
        let (workspace, cx) = workspace_on(fs.clone(), cx).await;
        // A stale entry that a rescan would replace with Laravel.
        let cached = ProjectInfo {
            framework: Some(Framework::WordPress),
            branch: None,
        };
        cx.update(|_, cx| {
            cx.set_global(view::DetectCache(
                [(PathBuf::from("/code/crm"), cached.clone())]
                    .into_iter()
                    .collect(),
            ))
        });
        let weak = workspace.downgrade();
        let view = cx.update(|window, cx| {
            let focus = cx.focus_handle();
            welcome_override(weak, focus, window, cx)
                .and_then(|v| v.downcast::<ZenithWelcome>().ok())
                .expect("zenith view")
        });
        cx.run_until_parked();
        let picker = view.read_with(cx, |view, _| view.projects().downgrade());
        let fs: Arc<dyn fs::Fs> = fs;
        cx.update(|window, cx| {
            window.spawn(cx, async move |cx| {
                view::show_projects(
                    fs,
                    vec![recent(1, "/code/crm"), recent(2, "/code/api")],
                    picker,
                    cx,
                )
                .await
            })
        })
        .await;
        cx.run_until_parked();

        let picker = view.read_with(cx, |view, _| view.projects().clone());
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.info(Path::new("/code/crm")), Some(&cached));
            assert_eq!(
                picker
                    .delegate
                    .info(Path::new("/code/api"))
                    .and_then(|info| info.framework),
                Some(Framework::Node),
            );
        });
        cx.update(|_, cx| {
            let cache = &cx.global::<view::DetectCache>().0;
            assert_eq!(cache.get(Path::new("/code/crm")), Some(&cached));
            assert!(cache.contains_key(Path::new("/code/api")));
        });
    }

    /// Plan 32 req. 4: none of upstream onboarding's telemetry toggles or events.
    #[test]
    fn no_telemetry_ids() {
        // Built with concat! so this test's own source doesn't match.
        let needles = [
            concat!("onboarding-", "telemetry"),
            concat!("render_", "telemetry_section"),
            concat!("Telemetry", "Settings"),
            concat!("telemetry", "::event"),
            concat!("Help improve ", "Z"),
            concat!("Help fix ", "Z"),
        ];
        let sources = [
            ("zenith_welcome.rs", include_str!("zenith_welcome.rs")),
            ("view.rs", include_str!("view.rs")),
            ("customize.rs", include_str!("customize.rs")),
            ("plugins.rs", include_str!("plugins.rs")),
            ("projects.rs", include_str!("projects.rs")),
            ("learn.rs", include_str!("learn.rs")),
            ("detect.rs", include_str!("detect.rs")),
        ];
        for (file, source) in sources {
            for needle in needles {
                assert!(!source.contains(needle), "{file} contains {needle:?}");
            }
        }
    }
}
