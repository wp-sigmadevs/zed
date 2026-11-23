//! The "PHP" modal (plan 39, owner-approved additions (b), M2): the project's language level,
//! the CLI interpreter (user settings only) and the global include paths with the project's
//! read-only `vendor/*/*` packages. The PHP Runtime, Analysis and Composer Files tabs are not
//! built, so they are not shown.

use crate::php_config::{self, PhpInterpreter, PhpToolsSettings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ReadGlobal as _,
    Subscription, Task, Window, uniform_list,
};
use project::Project;
use settings::{Settings as _, SettingsStore, update_settings_file};
use std::{ops::Range, path::PathBuf};
use ui::{ContextMenu, DropdownMenu, IconPosition, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zenith_external_libraries::{AddLibraryPath, ConfigurePhpIncludePaths};

const AUTO_LEVEL: &str = "Auto (composer.json)";
const AUTO_INTERPRETER: &str = "Auto (first PHP found)";

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ConfigurePhpIncludePaths, window, cx| {
            let project = workspace.project().clone();
            let interpreters = php_config::list_interpreters(cx);
            workspace.toggle_modal(window, cx, |window, cx| {
                PhpModal::new(project, interpreters, window, cx)
            });
        });
    })
    .detach();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhpTab {
    Php,
    IncludePath,
}

/// The built tabs. PhpStorm's PHP Runtime, Analysis and Composer Files tabs come with M3.
const TABS: [(&str, &str, PhpTab); 2] = [
    ("zenith-php-tab-php", "PHP", PhpTab::Php),
    (
        "zenith-php-tab-include",
        "Include Path",
        PhpTab::IncludePath,
    ),
];

pub struct PhpModal {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    /// The folder whose `.phpantom.toml` holds the language level: the first visible worktree of
    /// a local project.
    root: Option<PathBuf>,
    tab: PhpTab,
    /// `None` while the interpreters are being probed.
    interpreters: Option<Vec<PhpInterpreter>>,
    level: Option<String>,
    project_include: Option<Vec<String>>,
    packages: Vec<String>,
    error: Option<SharedString>,
    _load: Task<()>,
    _write: Task<()>,
    _settings: Subscription,
}

impl PhpModal {
    pub fn new(
        project: Entity<Project>,
        interpreters: Task<Vec<PhpInterpreter>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let root = project
            .read(cx)
            .is_local()
            .then(|| crate::first_root(&project, cx))
            .flatten();
        let fs = project.read(cx).fs().clone();
        let load = cx.spawn({
            let root = root.clone();
            async move |this, cx| {
                let (config, packages) = cx
                    .background_spawn(async move {
                        match &root {
                            Some(root) => (
                                php_config::read_project_config(fs.as_ref(), root).await,
                                php_config::vendor_packages(fs.as_ref(), root).await,
                            ),
                            None => (Ok(Default::default()), Vec::new()),
                        }
                    })
                    .await;
                this.update(cx, |this, cx| {
                    match config {
                        Ok(config) => {
                            this.level = config.version;
                            this.project_include = config.include;
                        }
                        Err(error) => this.error = Some(error.to_string().into()),
                    }
                    this.packages = packages;
                    cx.notify();
                })
                .log_err();
                let interpreters = interpreters.await;
                this.update(cx, |this, cx| {
                    this.interpreters = Some(interpreters);
                    cx.notify();
                })
                .log_err();
            }
        });
        Self {
            focus_handle,
            project,
            root,
            tab: PhpTab::Php,
            interpreters: None,
            level: None,
            project_include: None,
            packages: Vec::new(),
            error: None,
            _load: load,
            _write: Task::ready(()),
            _settings: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    pub fn set_tab(&mut self, tab: PhpTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    /// Writes `[php] version` (`None` = Auto) into the project's `.phpantom.toml`, then restarts
    /// PHPantom so it reads it. A failed write keeps the old level and shows the error.
    pub fn set_language_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let previous = std::mem::replace(&mut self.level, level.clone());
        let fs = self.project.read(cx).fs().clone();
        self._write = cx.spawn(async move |this, cx| {
            let result =
                php_config::write_language_level(fs.as_ref(), &root, level.as_deref()).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.error = None;
                        php_config::restart_phpantom(&this.project, cx);
                    }
                    Err(error) => {
                        this.level = previous;
                        this.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// Stores the interpreter in the user settings file (`None` = Auto). Only a detected
    /// interpreter is offered: a project never chooses the binary.
    pub fn set_interpreter(&mut self, binary: Option<PathBuf>, cx: &mut Context<Self>) {
        let fs = self.project.read(cx).fs().clone();
        update_settings_file(fs, cx, move |content, _| {
            content
                .zenith
                .get_or_insert_default()
                .php_tools
                .get_or_insert_default()
                .php_binary = binary.map(|binary| binary.to_string_lossy().into_owned());
        });
    }

    /// Takes `path` out of `zenith.external_libraries.paths`. PHPantom's config follows the
    /// setting and PHPantom restarts when that file changes (`phpantom::sync`).
    pub fn remove_include_path(&mut self, path: String, cx: &mut Context<Self>) {
        let fs = self.project.read(cx).fs().clone();
        update_settings_file(fs, cx, move |content, _| {
            if let Some(paths) = content
                .zenith
                .as_mut()
                .and_then(|zenith| zenith.external_libraries.as_mut())
                .and_then(|libraries| libraries.paths.as_mut())
            {
                paths.retain(|entry| *entry != path);
            }
        });
    }

    fn add_include_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        window.dispatch_action(Box::new(AddLibraryPath), cx);
    }

    fn render_tab(
        &self,
        (id, label, tab): (&'static str, &'static str, PhpTab),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .debug_selector(move || id.into())
            .child(
                Button::new(id, label)
                    .toggle_state(self.tab == tab)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx))),
            )
            .into_any_element()
    }

    fn render_php_tab(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let modal = cx.weak_entity();
        let selected_level = self.level.clone();
        let level_menu = ContextMenu::build(window, cx, {
            let modal = modal.clone();
            move |mut menu, _, _| {
                let levels = std::iter::once(None).chain(
                    php_config::LANGUAGE_LEVELS
                        .iter()
                        .map(|level| Some(level.to_string())),
                );
                for level in levels {
                    let label = level.clone().unwrap_or_else(|| AUTO_LEVEL.to_string());
                    let modal = modal.clone();
                    let toggled = level == selected_level;
                    menu = menu.toggleable_entry(
                        label,
                        toggled,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            modal
                                .update(cx, |modal, cx| modal.set_language_level(level.clone(), cx))
                                .log_err();
                        },
                    );
                }
                menu
            }
        });
        let level_label = self.level.clone().unwrap_or_else(|| AUTO_LEVEL.to_string());

        let selected_binary = PhpToolsSettings::get_global(cx).php_binary.clone();
        let interpreters = self.interpreters.clone().unwrap_or_default();
        let interpreter_label = match (&selected_binary, &self.interpreters) {
            (None, _) => AUTO_INTERPRETER.to_string(),
            (Some(binary), _) => match interpreters.iter().find(|found| &found.path == binary) {
                Some(found) => interpreter_entry(found),
                None => format!("{} (not found)", binary.display()),
            },
        };
        let interpreter_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let options = std::iter::once(None).chain(interpreters.iter().cloned().map(Some));
            for option in options {
                let label = option
                    .as_ref()
                    .map(interpreter_entry)
                    .unwrap_or_else(|| AUTO_INTERPRETER.to_string());
                let binary = option.map(|found| found.path);
                let toggled = binary == selected_binary;
                let modal = modal.clone();
                menu = menu.toggleable_entry(
                    label,
                    toggled,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        modal
                            .update(cx, |modal, cx| modal.set_interpreter(binary.clone(), cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .debug_selector(|| "zenith-php-level-row".into())
                    .justify_between()
                    .child(Label::new("PHP language level"))
                    .child(
                        DropdownMenu::new("zenith-php-level", level_label, level_menu)
                            .aria_label("PHP language level")
                            .disabled(self.root.is_none()),
                    ),
            )
            .when(self.root.is_none(), |this| {
                this.child(
                    Label::new("Open a local project folder to set its language level.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("CLI interpreter"))
                    .child(
                        DropdownMenu::new(
                            "zenith-php-interpreter",
                            if self.interpreters.is_none() {
                                "Looking for PHP…".to_string()
                            } else {
                                interpreter_label
                            },
                            interpreter_menu,
                        )
                        .aria_label("CLI interpreter"),
                    ),
            )
            .child(
                Label::new("The interpreter is kept in your user settings, for every project.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_include_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let paths = user_include_paths(cx);
        v_flex()
            .gap_2()
            .when(self.project_include.is_some(), |this| {
                this.child(
                    h_flex()
                        .debug_selector(|| "zenith-php-include-warning".into())
                        .gap_1()
                        .child(Icon::new(IconName::Warning).color(Color::Warning))
                        .child(
                            Label::new(
                                "Warning: this project's .phpantom.toml sets [indexing] include, \
                                 which replaces these paths in this project.",
                            )
                            .size(LabelSize::Small),
                        ),
                )
            })
            .child(
                h_flex()
                    .debug_selector(|| "zenith-php-include-header".into())
                    .justify_between()
                    .child(Label::new("Include paths (all projects)"))
                    .child(Button::new("zenith-php-add-include", "Add…").on_click(
                        cx.listener(|this, _, window, cx| this.add_include_path(window, cx)),
                    )),
            )
            .when(paths.is_empty(), |this| {
                this.child(Label::new("No include paths.").color(Color::Muted))
            })
            .children(paths.into_iter().enumerate().map(|(index, path)| {
                h_flex()
                    .justify_between()
                    .child(Label::new(path.clone()).size(LabelSize::Small))
                    .child(
                        IconButton::new(("zenith-php-remove-include", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Remove Include Path"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_include_path(path.clone(), cx)
                            })),
                    )
            }))
            .child(Label::new("Vendor packages (read-only)"))
            .child(if self.packages.is_empty() {
                Label::new("No vendor packages.")
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                uniform_list(
                    "zenith-php-vendor-packages",
                    self.packages.len(),
                    cx.processor(|this, range: Range<usize>, _, _| {
                        range
                            .filter_map(|index| {
                                let package = this.packages.get(index)?;
                                Some(
                                    Label::new(package.clone())
                                        .size(LabelSize::Small)
                                        .into_any_element(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .h(px(200.))
                .into_any_element()
            })
            .into_any_element()
    }
}

fn interpreter_entry(interpreter: &PhpInterpreter) -> String {
    format!(
        "PHP {} ({})",
        interpreter.version,
        interpreter.path.display()
    )
}

/// `zenith.external_libraries.paths` from the user settings file only, as `phpantom::sync`
/// reads them.
fn user_include_paths(cx: &App) -> Vec<String> {
    SettingsStore::global(cx)
        .raw_user_settings()
        .and_then(|user| user.content.zenith.as_ref())
        .and_then(|zenith| zenith.external_libraries.as_ref())
        .and_then(|libraries| libraries.paths.clone())
        .unwrap_or_default()
}

impl ModalView for PhpModal {}

impl EventEmitter<DismissEvent> for PhpModal {}

impl Focusable for PhpModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PhpModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            PhpTab::Php => self.render_php_tab(window, cx),
            PhpTab::IncludePath => self.render_include_tab(cx),
        };
        let tabs: Vec<_> = TABS
            .into_iter()
            .map(|tab| self.render_tab(tab, cx))
            .collect();
        v_flex()
            .id("zenith-php-modal")
            .key_context("ZenithPhpModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .elevation_3(cx)
            .w(rems(36.))
            .p_3()
            .gap_3()
            .child(Headline::new("PHP").size(HeadlineSize::Small))
            .child(h_flex().gap_1().children(tabs))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::XCircle).color(Color::Error))
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(body)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, UpdateGlobal as _, VisualTestContext};
    use project::{FakeFs, Fs as _};
    use serde_json::json;
    use std::{path::Path, sync::Arc};
    use workspace::MultiWorkspace;

    struct Fixture {
        workspace: Entity<Workspace>,
        fs: Arc<FakeFs>,
    }

    async fn setup(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
    ) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::init(app_state.clone(), cx);
            php_config::init(cx);
            init(cx);
            app_state
        });
        let fs = app_state.fs.as_fake();
        fs.insert_tree("/p", tree).await;
        let project = Project::test(app_state.fs.clone(), [Path::new("/p")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();
        (Fixture { workspace, fs }, cx)
    }

    /// Opens the modal with a fixed interpreter list (the action probes the real machine).
    fn open(
        fixture: &Fixture,
        interpreters: Vec<PhpInterpreter>,
        cx: &mut VisualTestContext,
    ) -> Entity<PhpModal> {
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            workspace.toggle_modal(window, cx, |window, cx| {
                PhpModal::new(project, Task::ready(interpreters), window, cx)
            });
        });
        cx.run_until_parked();
        fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<PhpModal>(cx))
            .expect("the PHP modal is open")
    }

    fn user_settings_text(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
        cx.run_until_parked();
        futures::executor::block_on(fixture.fs.load(paths::settings_file())).unwrap_or_default()
    }

    #[gpui::test]
    async fn test_action_opens_modal_with_only_the_built_tabs(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        // The action probes the PHPs on this machine; that runs on a real thread.
        cx.executor().allow_parking();
        cx.dispatch_action(ConfigurePhpIncludePaths);
        cx.run_until_parked();
        let modal = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<PhpModal>(cx));
        assert!(modal.is_some(), "the action opens the PHP modal");

        assert_eq!(
            TABS.map(|(_, label, _)| label),
            ["PHP", "Include Path"],
            "PHP Runtime, Analysis and Composer Files stay hidden until built"
        );
        for (id, _, _) in TABS {
            assert!(cx.debug_bounds(id).is_some(), "{id} is shown");
        }
        assert!(cx.debug_bounds("zenith-php-level-row").is_some());
        assert!(cx.debug_bounds("zenith-php-include-header").is_none());

        let tab = cx.debug_bounds("zenith-php-tab-include").unwrap();
        cx.simulate_click(tab.center(), Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("zenith-php-include-header").is_some());
        assert!(cx.debug_bounds("zenith-php-level-row").is_none());

        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(
            fixture
                .workspace
                .read_with(cx, |workspace, cx| workspace.active_modal::<PhpModal>(cx))
                .is_none(),
            "Escape closes it"
        );
    }

    #[gpui::test]
    async fn test_language_level_is_written_to_phpantom_toml(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({
                "composer.json": "{}",
                ".phpantom.toml": "[indexing]\nexclude = [\"build\"]\n",
                "vendor": { "acme": { "one": {} }, "autoload.php": "" },
            }),
        )
        .await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.level, None, "no version is Auto");
            assert_eq!(modal.packages, ["acme/one"]);
        });
        let file = Path::new("/p/.phpantom.toml");

        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.2".into()), cx)
        });
        cx.run_until_parked();
        let text = fixture.fs.load(file).await.unwrap();
        assert!(text.contains("[php]\nversion = \"8.2\""), "{text}");
        assert!(text.contains("exclude = [\"build\"]"), "{text}");
        assert!(
            !text.contains("include"),
            "a project `include` would replace the global include paths: {text}"
        );

        modal.update(cx, |modal, cx| modal.set_language_level(None, cx));
        cx.run_until_parked();
        assert_eq!(
            fixture.fs.load(file).await.unwrap(),
            "[indexing]\nexclude = [\"build\"]\n"
        );

        // An unparsable file is left alone; the old level stays and the error shows.
        fixture.fs.insert_file(file, b"not = [toml".to_vec()).await;
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.3".into()), cx)
        });
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.level, None);
            assert!(modal.error.is_some());
        });
        assert_eq!(fixture.fs.load(file).await.unwrap(), "not = [toml");
    }

    #[gpui::test]
    async fn test_interpreter_goes_to_user_settings_only(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let php = PhpInterpreter {
            path: PathBuf::from("/opt/php84/bin/php"),
            version: "8.4.1".into(),
        };
        let modal = open(&fixture, vec![php.clone()], cx);
        modal.update(cx, |modal, cx| {
            modal.set_interpreter(Some(php.path.clone()), cx)
        });
        let text = user_settings_text(&fixture, cx);
        assert!(
            text.contains("\"php_binary\": \"/opt/php84/bin/php\""),
            "{text}"
        );
        assert!(
            !fixture.fs.is_file(Path::new("/p/.zed/settings.json")).await
                && !fixture.fs.is_dir(Path::new("/p/.zed")).await,
            "nothing is written into the project"
        );

        modal.update(cx, |modal, cx| modal.set_interpreter(None, cx));
        let text = user_settings_text(&fixture, cx);
        assert!(!text.contains("/opt/php84/bin/php"), "{text}");
    }

    #[gpui::test]
    async fn test_include_paths_are_listed_and_removed(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({
                "composer.json": "{}",
                ".phpantom.toml": "[indexing]\ninclude = [\"lib\"]\n",
            }),
        )
        .await;
        let settings =
            r#"{ "zenith": { "external_libraries": { "paths": ["/libs/a", "/libs/b"] } } }"#;
        fixture
            .fs
            .insert_file(paths::settings_file(), settings.as_bytes().to_vec())
            .await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.set_user_settings(settings, cx).result().unwrap();
            })
        });
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| modal.set_tab(PhpTab::IncludePath, cx));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| user_include_paths(cx)),
            ["/libs/a", "/libs/b"]
        );
        assert!(
            cx.debug_bounds("ICON-Trash").is_some(),
            "each path has a remove button"
        );
        assert!(
            cx.debug_bounds("zenith-php-include-warning").is_some(),
            "the project's own include replaces the global list, so the tab warns"
        );

        modal.update(cx, |modal, cx| {
            modal.remove_include_path("/libs/a".into(), cx)
        });
        let text = user_settings_text(&fixture, cx);
        assert!(!text.contains("/libs/a"), "{text}");
        assert!(text.contains("/libs/b"), "{text}");
    }
}
