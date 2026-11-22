//! The run widget (plan 14): `[configuration ▾] ▶ 🐞 ■ ↻`. Configurations are the
//! project's and the global tasks.json tasks plus debug.json scenarios; the selected
//! one is kept per workspace (its worktree roots) in the key-value store.

use std::collections::HashMap;

use db::kvp::KeyValueStore;
use gpui::{
    Action, App, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity, Window, actions,
};
use project::{
    GIT_COMMAND_TASK_TAG, Project, TaskContexts, TaskSourceKind, debugger::dap_store::DapStoreEvent,
};
use task::{DebugScenario, TaskContext, TaskId, TaskTemplate};
use ui::{ContextMenu, IconButton, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    zenith_run,
    [
        /// Runs the selected run configuration.
        RunSelected,
        /// Debugs the selected run configuration.
        DebugSelected,
        /// Stops the tasks and debug sessions started from the run widget.
        StopRun,
        /// Runs the last run configuration again.
        RerunLast,
    ]
);

const NAMESPACE: &str = "zenith_run_widget";
const NO_CONFIGURATION: &str = "Add Configuration…";

/// One entry of the configuration dropdown.
#[derive(Clone, Debug)]
pub enum RunConfig {
    Task(TaskSourceKind, TaskTemplate),
    Debug(TaskSourceKind, DebugScenario),
}

impl RunConfig {
    pub fn label(&self) -> &str {
        match self {
            Self::Task(_, template) => &template.label,
            Self::Debug(_, scenario) => &scenario.label,
        }
    }

    pub fn is_debug(&self) -> bool {
        matches!(self, Self::Debug(..))
    }

    /// What the selection is saved as; the kind keeps a task and a scenario of the same
    /// name apart.
    pub fn id(&self) -> String {
        let kind = if self.is_debug() { "debug" } else { "task" };
        format!("{kind}:{}", self.label())
    }
}

/// Tasks first, then debug scenarios, each in inventory order, without repeats (global
/// files show up once per worktree) and without git-command tasks.
pub fn merge_configs(
    tasks: impl IntoIterator<Item = (TaskSourceKind, TaskTemplate)>,
    scenarios: impl IntoIterator<Item = (TaskSourceKind, DebugScenario)>,
) -> Vec<RunConfig> {
    let mut seen = std::collections::HashSet::new();
    tasks
        .into_iter()
        .filter(|(_, template)| !template.tags.iter().any(|tag| tag == GIT_COMMAND_TASK_TAG))
        .map(|(source, template)| RunConfig::Task(source, template))
        .chain(
            scenarios
                .into_iter()
                .map(|(source, scenario)| RunConfig::Debug(source, scenario)),
        )
        .filter(|config| seen.insert(config.id()))
        .collect()
}

/// Run configurations and run state of one project.
pub struct RunModel {
    project: WeakEntity<Project>,
    configs: Vec<RunConfig>,
    selected: Option<String>,
    /// The worktree roots the selection is saved under; `None` until the project has one.
    store_key: Option<String>,
    running_tasks: Vec<TaskId>,
    debugging: bool,
    /// Configuration id and whether it was debugged.
    last_run: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
}

impl RunModel {
    pub fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let dap_store = project.read(cx).dap_store();
        let subscriptions = vec![
            cx.subscribe(&dap_store, |this, dap_store, event: &DapStoreEvent, cx| {
                if matches!(
                    event,
                    DapStoreEvent::DebugClientStarted(_) | DapStoreEvent::DebugClientShutdown(_)
                ) {
                    this.debugging = dap_store.read(cx).sessions().next().is_some();
                    cx.notify();
                }
            }),
            cx.subscribe(project, |this, _, event: &project::Event, cx| {
                if matches!(
                    event,
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
                ) {
                    this.load_selection(cx);
                    this.refresh(cx).detach();
                }
            }),
        ];
        let mut this = Self {
            project: project.downgrade(),
            configs: Vec::new(),
            selected: None,
            store_key: None,
            running_tasks: Vec::new(),
            debugging: false,
            last_run: None,
            _subscriptions: subscriptions,
        };
        this.load_selection(cx);
        this.refresh(cx).detach();
        this
    }

    /// The model of `project`'s workspace, once [`crate::init`] made one.
    pub fn for_project(project: &Entity<Project>, cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<RunModels>()?
            .0
            .get(&project.entity_id())
            .cloned()
    }

    pub fn configs(&self) -> &[RunConfig] {
        &self.configs
    }

    /// The selected configuration, or the first one while none is selected or the
    /// selected one is gone.
    pub fn selected_config(&self) -> Option<&RunConfig> {
        self.selected
            .as_deref()
            .and_then(|id| self.configs.iter().find(|config| config.id() == id))
            .or_else(|| self.configs.first())
    }

    pub fn label(&self) -> SharedString {
        if let Some(config) = self.selected_config() {
            return config.label().to_string().into();
        }
        // Before the first refresh the saved selection is all there is.
        self.selected
            .as_deref()
            .and_then(|id| id.split_once(':'))
            .map(|(_, label)| label.to_string().into())
            .unwrap_or_else(|| NO_CONFIGURATION.into())
    }

    /// Whether a task or debug session started here is still running; ■ is enabled then.
    pub fn is_running(&self) -> bool {
        !self.running_tasks.is_empty() || self.debugging
    }

    pub fn select(&mut self, id: String, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        cx.notify();
        if let Some(key) = self.store_key.clone() {
            let store = KeyValueStore::global(cx);
            db::write_and_log(cx, move || async move {
                store.scoped(NAMESPACE).write(key, id).await
            });
        }
    }

    fn load_selection(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let mut roots = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        roots.sort();
        let key = (!roots.is_empty()).then(|| roots.join("\n"));
        if key == self.store_key {
            return;
        }
        self.selected = key.as_deref().and_then(|key| {
            KeyValueStore::global(cx)
                .scoped(NAMESPACE)
                .read(key)
                .log_err()
                .flatten()
        });
        self.store_key = key;
        cx.notify();
    }

    /// Re-reads the tasks and debug scenarios; the inventory sends no change events.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(project) = self.project.upgrade() else {
            return Task::ready(());
        };
        let (inventory, worktrees) = {
            let project = project.read(cx);
            let Some(inventory) = project.task_store().read(cx).task_inventory().cloned() else {
                return Task::ready(());
            };
            let worktrees = project
                .visible_worktrees(cx)
                .map(|worktree| Some(worktree.read(cx).id()))
                .collect::<Vec<_>>();
            (inventory, worktrees)
        };
        // No worktree still lists the global files.
        let worktrees = if worktrees.is_empty() {
            vec![None]
        } else {
            worktrees
        };
        let task_lists = worktrees
            .iter()
            .map(|worktree| inventory.read(cx).list_tasks(None, None, *worktree, cx))
            .collect::<Vec<_>>();
        let scenario_lists = worktrees
            .iter()
            .map(|worktree| {
                let contexts = TaskContexts {
                    active_worktree_context: worktree.map(|id| (id, TaskContext::default())),
                    ..TaskContexts::default()
                };
                inventory.update(cx, |inventory, cx| {
                    inventory.list_debug_scenarios(&contexts, Vec::new(), Vec::new(), false, cx)
                })
            })
            .collect::<Vec<_>>();
        cx.spawn(async move |this, cx| {
            let mut tasks = Vec::new();
            for list in task_lists {
                tasks.extend(list.await);
            }
            let mut scenarios = Vec::new();
            for list in scenario_lists {
                scenarios.extend(list.await.1);
            }
            this.update(cx, |this, cx| {
                this.configs = merge_configs(tasks, scenarios);
                cx.notify();
            })
            .ok();
        })
    }

    fn started(
        &mut self,
        config_id: String,
        debug: bool,
        task: Option<TaskId>,
        cx: &mut Context<Self>,
    ) {
        self.last_run = Some((config_id, debug));
        self.running_tasks.extend(task);
        cx.notify();
    }

    fn finished(&mut self, task: &TaskId, cx: &mut Context<Self>) {
        self.running_tasks.retain(|running| running != task);
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let running = std::mem::take(&mut self.running_tasks);
        cx.notify();
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let terminals = project.read(cx).local_terminal_handles().clone();
        for terminal in terminals {
            terminal
                .update(cx, |terminal, _| {
                    if terminal
                        .task()
                        .is_some_and(|task| running.contains(&task.spawned_task.id))
                    {
                        terminal.kill_active_task();
                    }
                })
                .ok();
        }
        if self.debugging {
            project
                .read(cx)
                .dap_store()
                .update(cx, |dap_store, cx| dap_store.shutdown_sessions(cx))
                .detach();
        }
    }

    /// What ▶/🐞 (`rerun` false) or ↻ (`rerun` true) should start.
    fn target(&self, debug: bool, rerun: bool) -> Option<(RunConfig, bool)> {
        if rerun {
            let (id, debug) = self.last_run.as_ref()?;
            let config = self.configs.iter().find(|config| &config.id() == id)?;
            return Some((config.clone(), *debug));
        }
        Some((self.selected_config()?.clone(), debug))
    }
}

#[derive(Default)]
struct RunModels(HashMap<EntityId, Entity<RunModel>>);

impl Global for RunModels {}

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _: Option<&mut Window>, cx: &mut Context<Workspace>| {
            let project = workspace.project().clone();
            if RunModel::for_project(&project, cx).is_none() {
                let model = cx.new(|cx| RunModel::new(&project, cx));
                let project_id = project.entity_id();
                cx.default_global::<RunModels>().0.insert(project_id, model);
                App::observe_release(cx, &project, move |_, cx| {
                    cx.default_global::<RunModels>().0.remove(&project_id);
                })
                .detach();
            }
            workspace
                .register_action(|workspace, _: &RunSelected, window, cx| {
                    launch(workspace, false, false, window, cx)
                })
                .register_action(|workspace, _: &DebugSelected, window, cx| {
                    launch(workspace, true, false, window, cx)
                })
                .register_action(|workspace, _: &RerunLast, window, cx| {
                    launch(workspace, false, true, window, cx)
                })
                .register_action(|workspace, _: &StopRun, _, cx| {
                    if let Some(model) = RunModel::for_project(workspace.project(), cx) {
                        model.update(cx, |model, cx| model.stop(cx));
                    }
                });
        },
    )
    .detach();
}

/// Refreshes the configurations first, so an edit to tasks.json is picked up.
fn launch(
    workspace: &mut Workspace,
    debug: bool,
    rerun: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(model) = RunModel::for_project(workspace.project(), cx) else {
        return;
    };
    let refresh = model.update(cx, |model, cx| model.refresh(cx));
    let contexts = tasks_ui::task_contexts(workspace, window, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        refresh.await;
        let contexts = contexts.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                match model.read(cx).target(debug, rerun) {
                    Some((config, debug)) => {
                        start(workspace, &model, config, debug, &contexts, window, cx)
                    }
                    // Nothing run from here yet: fall back to the task rerun.
                    None if rerun => {
                        window.dispatch_action(Box::new(zed_actions::Rerun::default()), cx)
                    }
                    None => window.dispatch_action(zed_actions::Spawn::modal().boxed_clone(), cx),
                }
            })
            .ok();
    })
    .detach();
}

fn start(
    workspace: &mut Workspace,
    model: &Entity<RunModel>,
    config: RunConfig,
    debug: bool,
    contexts: &TaskContexts,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let config_id = config.id();
    match config {
        RunConfig::Task(source, template) if !debug => {
            let context = contexts
                .context_for_task(&source, &template)
                .cloned()
                .unwrap_or_default();
            let Some(resolved) = template.resolve_task(&source.to_id_base(), &context) else {
                return;
            };
            let task_id = resolved.id.clone();
            model.update(cx, |model, cx| {
                model.started(config_id, false, Some(task_id.clone()), cx)
            });
            let model = model.downgrade();
            workspace.schedule_resolved_task_with_completion(
                source,
                resolved,
                false,
                move |_, cx| {
                    model
                        .update(cx, |model, cx| model.finished(&task_id, cx))
                        .ok();
                },
                window,
                cx,
            );
        }
        // A task has no debug scenario of its own: let the debugger's own dialog pick one.
        // Looked up by name so the title bar doesn't depend on debugger_ui.
        RunConfig::Task(..) => {
            if let Ok(action) = cx.build_action("debugger::Start", None) {
                window.dispatch_action(action, cx);
            }
        }
        RunConfig::Debug(source, scenario) => {
            let worktree_id = match &source {
                TaskSourceKind::Worktree { id, .. } => Some(*id),
                _ => contexts.worktree(),
            };
            let context = contexts.active_context().cloned().unwrap_or_default();
            model.update(cx, |model, cx| model.started(config_id, true, None, cx));
            workspace.start_debug_session(scenario, context.into(), None, worktree_id, window, cx);
        }
    }
}

/// The dropdown, rebuilt once a fresh read of the configurations lands.
fn configuration_menu(
    model: Entity<RunModel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let menu = ContextMenu::build_persistent(window, cx, {
        let model = model.clone();
        move |menu, _, cx| configuration_items(menu.keep_open_on_confirm(false), &model, cx)
    });
    let refresh = model.update(cx, |model, cx| model.refresh(cx));
    let weak_menu = menu.downgrade();
    window
        .spawn(cx, async move |cx| {
            refresh.await;
            weak_menu
                .update_in(cx, |menu, window, cx| menu.rebuild(window, cx))
                .ok();
        })
        .detach();
    menu
}

fn configuration_items(mut menu: ContextMenu, model: &Entity<RunModel>, cx: &App) -> ContextMenu {
    let run_model = model.read(cx);
    let selected = run_model.selected_config().map(RunConfig::id);
    for (header, debug) in [("Tasks", false), ("Debug", true)] {
        let group = run_model
            .configs
            .iter()
            .filter(|config| config.is_debug() == debug)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        menu = menu.header(header);
        for config in group {
            let id = config.id();
            let toggled = selected.as_deref() == Some(id.as_str());
            let model = model.clone();
            menu = menu.toggleable_entry(
                config.label().to_string(),
                toggled,
                IconPosition::Start,
                None,
                move |_, cx| model.update(cx, |model, cx| model.select(id.clone(), cx)),
            );
        }
    }
    menu.separator().action(
        "Edit Configurations…",
        Box::new(zed_actions::OpenProjectTasks),
    )
}

/// `[configuration ▾] ▶ 🐞 ■ ↻`. ▶ turns into an outlined, amber icon while something
/// started here runs, and ■ is enabled only then.
pub(crate) fn run_controls(project: &Entity<Project>, cx: &App) -> impl IntoElement + use<> {
    let model = RunModel::for_project(project, cx);
    let (label, running) = model.as_ref().map_or_else(
        || (SharedString::from(NO_CONFIGURATION), false),
        |model| {
            let model = model.read(cx);
            (model.label(), model.is_running())
        },
    );
    let (run_icon, run_color, run_title) = if running {
        (IconName::PlayOutlined, Color::Warning, "Run (running)")
    } else {
        (IconName::PlayFilled, Color::Success, "Run")
    };
    h_flex()
        .gap_0p5()
        .child(
            PopoverMenu::new("zenith-run-configuration-menu")
                .menu(move |window, cx| {
                    model
                        .clone()
                        .map(|model| configuration_menu(model, window, cx))
                })
                .trigger_with_tooltip(
                    Button::new("zenith-run-configuration", label)
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                    Tooltip::text("Run Configurations"),
                ),
        )
        .child(
            run_button("zenith-run", run_icon, run_title, Box::new(RunSelected))
                .icon_color(run_color),
        )
        .child(run_button(
            "zenith-debug",
            IconName::Debug,
            "Debug",
            Box::new(DebugSelected),
        ))
        .child(
            run_button("zenith-stop", IconName::Stop, "Stop", Box::new(StopRun))
                .icon_color(if running { Color::Error } else { Color::Muted })
                .disabled(!running),
        )
        .child(run_button(
            "zenith-rerun",
            IconName::RotateCw,
            "Rerun",
            Box::new(RerunLast),
        ))
}

fn run_button(
    id: &'static str,
    icon: IconName,
    title: &'static str,
    action: Box<dyn Action>,
) -> IconButton {
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .aria_label(title)
        .tooltip(crate::action_tooltip(title, action.boxed_clone()))
        .on_click(crate::dispatch(action))
}


#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::{Project, TaskSourceKind, task_store::TaskSettingsLocation};
    use serde_json::json;
    use task::{DebugScenario, TaskId, TaskTemplate};
    use ui::prelude::*;
    use workspace::{AppState, MultiWorkspace};

    use super::{RunConfig, RunModel, StopRun, merge_configs, run_controls};

    fn task(label: &str) -> (TaskSourceKind, TaskTemplate) {
        (
            TaskSourceKind::UserInput,
            TaskTemplate {
                label: label.into(),
                command: "make".into(),
                ..TaskTemplate::default()
            },
        )
    }

    fn scenario(label: &str) -> (TaskSourceKind, DebugScenario) {
        (
            TaskSourceKind::UserInput,
            DebugScenario {
                adapter: "Xdebug".into(),
                label: label.to_string().into(),
                build: None,
                config: json!({}),
                tcp_connection: None,
            },
        )
    }

    #[test]
    fn test_merge_puts_tasks_before_debug_and_drops_repeats() {
        let mut git = task("Git push").1;
        git.tags = vec![project::GIT_COMMAND_TASK_TAG.to_string()];
        let configs = merge_configs(
            [
                task("Build"),
                task("Test"),
                task("Build"),
                (TaskSourceKind::UserInput, git),
            ],
            [scenario("Build"), scenario("Listen"), scenario("Listen")],
        );
        let ids = configs.iter().map(RunConfig::id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            ["task:Build", "task:Test", "debug:Build", "debug:Listen"]
        );
        assert!(!configs[1].is_debug());
        assert!(configs[3].is_debug());
    }

    /// A workspace window on `root` (one per test: the test key-value store is shared), with `crate::init` run before it opens.
    async fn setup(
        root: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            workspace::init(app_state.clone(), cx);
            crate::init(cx);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(root, json!({ "index.php": "" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [Path::new(root)], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        (project, cx)
    }

    fn model(project: &Entity<Project>, cx: &mut VisualTestContext) -> Entity<RunModel> {
        cx.update(|_, cx| RunModel::for_project(project, cx))
            .expect("init made a run model for the workspace")
    }

    #[gpui::test]
    async fn test_model_lists_global_tasks_and_debug_scenarios(cx: &mut TestAppContext) {
        let (project, mut cx) = setup("/run-list", cx).await;
        let inventory = project.read_with(&cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("local projects have a task inventory")
        });
        inventory.update(&mut cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/config/tasks.json")),
                    Some(r#"[{ "label": "Serve", "command": "php artisan serve" }]"#),
                )
                .expect("tasks parse");
            inventory
                .update_file_based_scenarios(
                    TaskSettingsLocation::Global(Path::new("/config/debug.json")),
                    Some(
                        r#"[{ "label": "Listen for Xdebug", "adapter": "Xdebug", "request": "launch" }]"#,
                    ),
                )
                .expect("scenarios parse");
        });
        let model = model(&project, &mut cx);
        model
            .update(&mut cx, |model, cx| model.refresh(cx))
            .await;
        model.read_with(&cx, |model, _| {
            let ids = model.configs().iter().map(RunConfig::id).collect::<Vec<_>>();
            assert_eq!(ids, ["task:Serve", "debug:Listen for Xdebug"]);
            assert_eq!(model.label(), "Serve", "first configuration until one is picked");
        });
    }

    #[gpui::test]
    async fn test_selection_is_saved_per_workspace(cx: &mut TestAppContext) {
        let (project, mut cx) = setup("/run-select", cx).await;
        let model = model(&project, &mut cx);
        model.update(&mut cx, |model, cx| {
            model.select("debug:Listen for Xdebug".to_string(), cx)
        });
        cx.run_until_parked();
        let reloaded = cx.update(|_, cx| cx.new(|cx| RunModel::new(&project, cx)));
        reloaded.read_with(&cx, |model, _| {
            assert_eq!(model.selected.as_deref(), Some("debug:Listen for Xdebug"));
            assert_eq!(model.label(), "Listen for Xdebug");
        });
    }

    #[gpui::test]
    async fn test_stop_is_enabled_only_while_something_runs(cx: &mut TestAppContext) {
        let (project, mut cx) = setup("/run-stop", cx).await;
        let model = model(&project, &mut cx);
        let task_id = TaskId("build".into());
        model.read_with(&cx, |model, _| assert!(!model.is_running()));
        model.update(&mut cx, |model, cx| {
            model.started("task:Build".into(), false, Some(task_id.clone()), cx)
        });
        model.read_with(&cx, |model, _| assert!(model.is_running()));
        model.update(&mut cx, |model, cx| model.finished(&task_id, cx));
        model.read_with(&cx, |model, _| assert!(!model.is_running()));

        model.update(&mut cx, |model, cx| {
            model.started("task:Build".into(), false, Some(task_id.clone()), cx)
        });
        cx.dispatch_action(StopRun);
        cx.run_until_parked();
        model.read_with(&cx, |model, _| {
            assert!(!model.is_running(), "■ stops what ▶ started");
            assert_eq!(
                model.last_run,
                Some(("task:Build".to_string(), false)),
                "↻ still knows what to rerun"
            );
        });
    }

    struct Widget(Entity<Project>);

    impl Render for Widget {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            run_controls(&self.0, cx)
        }
    }

    #[gpui::test]
    async fn test_widget_shows_the_running_state(cx: &mut TestAppContext) {
        let (project, mut workspace_cx) = setup("/run-widget", cx).await;
        let model = model(&project, &mut workspace_cx);
        let window = cx.add_window({
            let project = project.clone();
            move |_, _| Widget(project)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        assert!(cx.debug_bounds("ICON-PlayFilled").is_some(), "idle ▶");
        assert!(cx.debug_bounds("ICON-PlayOutlined").is_none());
        assert!(cx.debug_bounds("ICON-Debug").is_some());
        assert!(cx.debug_bounds("ICON-Stop").is_some());
        assert!(cx.debug_bounds("ICON-RotateCw").is_some());

        model.update(&mut cx, |model, cx| {
            model.started("task:Build".into(), false, Some(TaskId("build".into())), cx)
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("ICON-PlayOutlined").is_some(),
            "▶ changes while running"
        );
        assert!(cx.debug_bounds("ICON-PlayFilled").is_none());
    }
}
