use std::{collections::HashMap, path::PathBuf, sync::Arc};

use extension_host::ExtensionStore;
use fs::Fs;
use gpui::{
    Action as _, AnyElement, App, AsyncWindowContext, Context, Entity, EntityId, FocusHandle,
    Focusable as _, Global, Render, Subscription, Task, WeakEntity, Window, actions,
};
use picker::Picker;
use ui::{Vector, VectorName, prelude::*};
use util::ResultExt as _;
use workspace::{
    Open, RecentWorkspace, SerializedWorkspaceLocation, Workspace, WorkspaceDb,
    notifications::NotifyTaskExt as _,
};

use crate::detect::{self, ProjectInfo};
use crate::projects::{self, ProjectsDelegate};

actions!(
    zenith_welcome,
    [
        /// Asks for a name and location, creates that folder and opens it as a project.
        NewProject,
        /// Leaves the setup page for the projects list.
        FinishSetup,
    ]
);

/// Detection results for this app session, keyed by project root.
#[derive(Default)]
pub struct DetectCache(pub HashMap<PathBuf, ProjectInfo>);
impl Global for DetectCache {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Projects,
    Customize,
    Plugins,
    Learn,
}

impl Page {
    const ALL: [Page; 4] = [Page::Projects, Page::Customize, Page::Plugins, Page::Learn];

    fn label(self) -> &'static str {
        match self {
            Page::Projects => "Projects",
            Page::Customize => "Customize",
            Page::Plugins => "Plugins",
            Page::Learn => "Learn",
        }
    }
}

/// Asks the welcome screen of one workspace to switch to a page.
/// Consumed by the first matching `ZenithWelcome`.
pub(crate) struct PageRequest(pub Option<(EntityId, Page)>);
impl Global for PageRequest {}

pub struct ZenithWelcome {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    page: Page,
    projects: Entity<Picker<ProjectsDelegate>>,
    /// Extension id → newer version on the server; fetched when Plugins first shows.
    plugin_updates: HashMap<Arc<str>, Arc<str>>,
    plugin_updates_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ZenithWelcome {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        focus_handle: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let projects = cx.new(|cx| {
            Picker::list(ProjectsDelegate::new(workspace.clone()), window, cx)
                .embedded()
                .show_scrollbar(true)
        });
        Self::load_projects(&workspace, &projects, window, cx);
        let mut _subscriptions = vec![
            cx.observe_global::<PageRequest>(Self::take_page_request),
            // The item focuses our root; hand keys to the picker on Projects.
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                if this.page == Page::Projects {
                    this.projects.focus_handle(cx).focus(window, cx);
                }
            }),
        ];
        // Plugins shows install state; re-render when it changes.
        if let Some(store) = ExtensionStore::try_global(cx) {
            _subscriptions.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            workspace,
            focus_handle,
            page: Page::Projects,
            projects,
            plugin_updates: HashMap::default(),
            plugin_updates_task: None,
            _subscriptions,
        };
        this.take_page_request(cx);
        this
    }

    pub fn page(&self) -> Page {
        self.page
    }

    #[cfg(test)]
    pub(crate) fn projects(&self) -> &Entity<Picker<ProjectsDelegate>> {
        &self.projects
    }

    fn load_projects(
        workspace: &WeakEntity<Workspace>,
        picker: &Entity<Picker<ProjectsDelegate>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let db = WorkspaceDb::global(cx);
        let workspace = workspace.clone();
        let picker = picker.downgrade();
        // Runs after the caller's update, so reading the workspace is safe here.
        cx.spawn_in(window, async move |_, cx| {
            let Ok(fs) = workspace.read_with(cx, |ws, _| ws.app_state().fs.clone()) else {
                return;
            };
            let projects = db
                .recent_project_workspaces(fs.as_ref())
                .await
                .log_err()
                .unwrap_or_default();
            show_projects(fs, projects, picker, cx).await;
        })
        .detach();
    }

    fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Plugins {
            self.fetch_plugin_updates(cx);
        }
        cx.notify();
    }

    /// One server query per welcome page, like extensions_ui's update check.
    fn fetch_plugin_updates(&mut self, cx: &mut Context<Self>) {
        if self.plugin_updates_task.is_some() {
            return;
        }
        let Some(store) = ExtensionStore::try_global(cx) else {
            return;
        };
        let fetch = store.update(cx, |store, cx| {
            store.fetch_extensions_with_update_available(cx)
        });
        self.plugin_updates_task = Some(cx.spawn(async move |this, cx| {
            let Some(updates) = fetch.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.plugin_updates = updates
                    .into_iter()
                    .map(|extension| (extension.id, extension.manifest.version))
                    .collect();
                cx.notify();
            })
            .log_err();
        }));
    }

    fn finish_setup(&mut self, _: &FinishSetup, window: &mut Window, cx: &mut Context<Self>) {
        self.set_page(Page::Projects, cx);
        self.projects.focus_handle(cx).focus(window, cx);
    }

    fn new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let prompt = cx.prompt_for_new_path(util::paths::home_dir(), Some("untitled"));
        let task = cx.spawn_in(window, async move |_, cx| {
            // Dropped sender or Cancel: nothing to do.
            let Ok(response) = prompt.await else {
                return anyhow::Ok(());
            };
            let Some(path) = response? else {
                return Ok(());
            };
            let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone())?;
            fs.create_dir(&path).await?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let open_mode = projects::open_mode(cx);
                    workspace.open_workspace_for_paths(open_mode, vec![path], window, cx)
                })?
                .await?;
            Ok(())
        });
        task.detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn take_page_request(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace.entity_id();
        let Some(&PageRequest(Some((id, page)))) = cx.try_global::<PageRequest>() else {
            return;
        };
        if id != workspace_id {
            return;
        }
        self.set_page(page, cx);
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tab_index: isize = 0;
        v_flex()
            .w(rems(12.))
            .h_full()
            .p_4()
            .gap_1()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .mb_4()
                    .child(Vector::square(VectorName::ZedLogo, rems_from_px(28_f32)))
                    .child(Headline::new("Zenith").size(HeadlineSize::Small)),
            )
            .children(Page::ALL.into_iter().map(|page| {
                tab_index += 1;
                // Not full_width: that centers the label; the column stretches it instead.
                Button::new(page.label(), page.label())
                    .tab_index(tab_index)
                    .toggle_state(self.page == page)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_page(page, cx);
                        if page == Page::Projects {
                            this.projects.focus_handle(cx).focus(window, cx);
                        }
                    }))
            }))
    }

    fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Projects => v_flex()
                .size_full()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("new-project", "New Project").on_click(
                            |_, window, cx| window.dispatch_action(NewProject.boxed_clone(), cx),
                        ))
                        .child(Button::new("open", "Open").on_click(|_, window, cx| {
                            window.dispatch_action(Open::DEFAULT.boxed_clone(), cx)
                        }))
                        .child(Button::new("clone", "Clone Repository").on_click(
                            |_, window, cx| window.dispatch_action(git::Clone.boxed_clone(), cx),
                        )),
                )
                .child(self.projects.clone())
                .into_any_element(),
            Page::Customize => v_flex()
                .gap_6()
                .max_w(rems(48.))
                .child(crate::customize::render_customize(cx))
                // Upstream onboarding's Finish only leaves setup; both buttons do that here.
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("customize-skip", "Skip").on_click(|_, window, cx| {
                                window.dispatch_action(FinishSetup.boxed_clone(), cx)
                            }),
                        )
                        .child(
                            Button::new("customize-finish", "Finish Setup")
                                .style(ButtonStyle::Filled)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(FinishSetup.boxed_clone(), cx)
                                }),
                        ),
                )
                .into_any_element(),
            Page::Plugins => {
                crate::plugins::render_plugins(&self.plugin_updates, cx).into_any_element()
            }
            Page::Learn => crate::learn::render_learn(cx).into_any_element(),
        }
    }
}

impl Render for ZenithWelcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("ZenithWelcome")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_project))
            .on_action(cx.listener(Self::finish_setup))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_nav(cx))
            .child(
                v_flex()
                    .id("zenith-welcome-page")
                    .size_full()
                    .p_8()
                    .gap_4()
                    .overflow_y_scroll()
                    .child(self.render_page(cx)),
            )
    }
}

/// Fills the picker, then detects frameworks for roots not yet in [`DetectCache`]
/// on the background executor.
pub(crate) async fn show_projects(
    fs: Arc<dyn Fs>,
    projects: Vec<RecentWorkspace>,
    picker: WeakEntity<Picker<ProjectsDelegate>>,
    cx: &mut AsyncWindowContext,
) {
    let roots: Vec<PathBuf> = projects
        .iter()
        .filter(|p| matches!(p.location, SerializedWorkspaceLocation::Local))
        .filter_map(|p| p.paths.paths().first().cloned())
        .collect();
    let Ok(missing) = picker.update_in(cx, |picker, window, cx| {
        let cache = cx.default_global::<DetectCache>();
        let missing: Vec<PathBuf> = roots
            .into_iter()
            .filter(|root| !cache.0.contains_key(root))
            .collect();
        picker.delegate.set_info(cache.0.clone());
        picker.delegate.set_projects(projects);
        picker.refresh(window, cx);
        missing
    }) else {
        return;
    };
    if missing.is_empty() {
        return;
    }
    let found = cx
        .background_spawn(async move {
            let mut found = Vec::with_capacity(missing.len());
            for root in missing {
                let info = detect::detect(fs.as_ref(), &root).await;
                found.push((root, info));
            }
            found
        })
        .await;
    picker
        .update_in(cx, |picker, window, cx| {
            let cache = cx.default_global::<DetectCache>();
            cache.0.extend(found);
            picker.delegate.set_info(cache.0.clone());
            picker.refresh(window, cx);
        })
        .log_err();
}

pub(crate) fn request_page(workspace: EntityId, page: Page, cx: &mut App) {
    cx.set_global(PageRequest(Some((workspace, page))));
    // A workspace can hold two welcome views (the tab and upstream's cached empty-pane
    // one): all of them apply the request, then it is cleared so later views ignore it.
    cx.defer(|cx| cx.set_global(PageRequest(None)));
}
