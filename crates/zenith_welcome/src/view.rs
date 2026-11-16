use gpui::{
    Action as _, AnyElement, App, Context, Entity, EntityId, FocusHandle, Focusable as _, Global,
    Render, Subscription, WeakEntity, Window,
};
use picker::Picker;
use ui::{Vector, VectorName, prelude::*};
use util::ResultExt as _;
use workspace::{NewFile, Open, Workspace, WorkspaceDb};

use crate::projects::ProjectsDelegate;

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
    _subscriptions: [Subscription; 2],
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
        let _subscriptions = [
            cx.observe_global::<PageRequest>(Self::take_page_request),
            // The item focuses our root; hand keys to the picker on Projects.
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                if this.page == Page::Projects {
                    this.projects.focus_handle(cx).focus(window, cx);
                }
            }),
        ];
        let mut this = Self {
            workspace,
            focus_handle,
            page: Page::Projects,
            projects,
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
            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.set_projects(projects);
                    picker.refresh(window, cx);
                })
                .log_err();
        })
        .detach();
    }

    fn take_page_request(&mut self, cx: &mut Context<Self>) {
        let workspace_id = self.workspace.entity_id();
        let Some(PageRequest(Some((id, page)))) = cx.try_global::<PageRequest>() else {
            return;
        };
        if *id != workspace_id {
            return;
        }
        self.page = *page;
        cx.set_global(PageRequest(None));
        cx.notify();
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
                Button::new(page.label(), page.label())
                    .full_width()
                    .tab_index(tab_index)
                    .toggle_state(self.page == page)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.page = page;
                        if page == Page::Projects {
                            this.projects.focus_handle(cx).focus(window, cx);
                        }
                        cx.notify();
                    }))
            }))
    }

    fn render_page(&self) -> AnyElement {
        match self.page {
            Page::Projects => v_flex()
                .size_full()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("new-file", "New File").on_click(|_, window, cx| {
                                window.dispatch_action(NewFile.boxed_clone(), cx)
                            }),
                        )
                        .child(Button::new("open", "Open").on_click(|_, window, cx| {
                            window.dispatch_action(Open::DEFAULT.boxed_clone(), cx)
                        }))
                        .child(Button::new("clone", "Clone Repository").on_click(
                            |_, window, cx| window.dispatch_action(git::Clone.boxed_clone(), cx),
                        )),
                )
                .child(self.projects.clone())
                .into_any_element(),
            page => Headline::new(page.label()).into_any_element(),
        }
    }
}

impl Render for ZenithWelcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("ZenithWelcome")
            .track_focus(&self.focus_handle)
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
                    .child(self.render_page()),
            )
    }
}

pub(crate) fn request_page(workspace: EntityId, page: Page, cx: &mut App) {
    cx.set_global(PageRequest(Some((workspace, page))));
}
