mod customize;
mod detect;
mod learn;
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

pub use view::{Page, ZenithWelcome};

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
        cx.new(|cx| ZenithWelcome::new(workspace, focus_handle, window, cx))
            .into()
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
    use fs::FakeFs;
    use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
    use project::Project;
    use settings::SettingsStore;
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

    async fn workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        let project = Project::test(FakeFs::new(cx.executor()), None, cx).await;
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
        use workspace::{PathList, RecentWorkspace, SerializedWorkspaceLocation, WorkspaceId};
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
        let recent = |id, path: &str| RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(id),
            location: SerializedWorkspaceLocation::Local,
            paths: PathList::new(&[path]),
            identity_paths: PathList::new(&[path]),
            path_style: None,
            timestamp: chrono::Utc::now(),
        };
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
                assert!(matches!(cx.global::<view::PageRequest>(), view::PageRequest(None)));
            });
        }
    }
}
