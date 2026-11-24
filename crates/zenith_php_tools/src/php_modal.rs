//! The "PHP" modal (plan 39 additions (b), plan 44 #7): the project's language level, the CLI
//! interpreter (user settings only) and the global include paths with the project's read-only
//! `vendor/*/*` packages. Every edit is staged: nothing is written before Apply or OK. The PHP
//! Runtime, Analysis and Composer Files tabs are not built, so they are not shown.

use crate::php_config::{self, PhpInterpreter, PhpToolsSettings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PathPromptOptions,
    ReadGlobal as _, ScrollStrategy, Subscription, Task, UniformListScrollHandle, Window,
    uniform_list,
};
use project::Project;
use settings::{Settings as _, SettingsStore, update_settings_file_with_completion};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use ui::{ContextMenu, DropdownMenu, IconPosition, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zenith_external_libraries::{ConfigurePhpIncludePaths, open_as_libraries};

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

/// The three things the modal edits. `base` is what is saved, `staged` what the dialog shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Staged {
    /// `[php] version` of `.phpantom.toml`; `None` is Auto.
    level: Option<String>,
    /// `zenith.php_tools.php_binary`; `None` is Auto.
    interpreter: Option<PathBuf>,
    /// `zenith.external_libraries.paths`, in order.
    include: Vec<String>,
}

/// A row of the include list. Keyed by value, not by index: the list grows and shrinks under a
/// selection.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// A user include path (editable).
    User(String),
    /// A `vendor/<vendor>/<package>` folder of the project (read-only).
    Vendor(String),
}

pub struct PhpModal {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    /// The folder whose `.phpantom.toml` holds the language level: the first visible worktree of
    /// a local project.
    root: Option<PathBuf>,
    /// `None` while the interpreters are being probed.
    interpreters: Option<Vec<PhpInterpreter>>,
    base: Staged,
    staged: Staged,
    /// `.phpantom.toml` has been read: until then the level (and so Apply) is not offered.
    loaded: bool,
    selected: Option<Row>,
    applying: bool,
    /// OK was pressed while Apply was running: close when it has finished.
    close_after: bool,
    list_scroll: UniformListScrollHandle,
    /// The last probe error, so a good probe clears that one and not an Apply error.
    probe_error: Option<SharedString>,
    packages: Vec<String>,
    /// This project's `.phpantom.toml` has `[indexing] include`, which replaces the global paths.
    project_include: Option<Vec<String>>,
    error: Option<SharedString>,
    _load: Task<()>,
    _apply: Task<()>,
    _probe: Task<()>,
    _prompt: Task<()>,
    _settings: Subscription,
}

impl Drop for PhpModal {
    /// Esc during Apply must not stop it halfway (a dropped task stops at its next await): let it
    /// finish. It holds no reference to the modal (same as the deploy dialog).
    fn drop(&mut self) {
        std::mem::replace(&mut self._apply, Task::ready(())).detach();
    }
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
        let base = Staged {
            level: None,
            interpreter: PhpToolsSettings::get_global(cx).php_binary.clone(),
            include: user_include_paths(cx),
        };
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
                            this.base.level = config.version.clone();
                            this.staged.level = config.version;
                            this.project_include = config.include;
                        }
                        Err(error) => this.error = Some(error.to_string().into()),
                    }
                    this.packages = packages;
                    this.loaded = true;
                    cx.notify();
                })
                .log_err();
                let interpreters = interpreters.await;
                this.update(cx, |this, cx| {
                    // One chosen before the probe finished stays in the list.
                    let mut list = interpreters;
                    for added in this.interpreters.take().unwrap_or_default() {
                        if !list.iter().any(|known| known.path == added.path) {
                            list.push(added);
                        }
                    }
                    this.interpreters = Some(list);
                    cx.notify();
                })
                .log_err();
            }
        });
        Self {
            focus_handle,
            project,
            root,
            interpreters: None,
            staged: base.clone(),
            base,
            loaded: false,
            selected: None,
            applying: false,
            close_after: false,
            list_scroll: UniformListScrollHandle::new(),
            probe_error: None,
            packages: Vec::new(),
            project_include: None,
            error: None,
            _load: load,
            _apply: Task::ready(()),
            _probe: Task::ready(()),
            _prompt: Task::ready(()),
            _settings: cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        }
    }

    fn is_dirty(&self) -> bool {
        self.staged != self.base
    }

    fn can_apply(&self) -> bool {
        self.loaded && !self.applying && self.is_dirty()
    }

    /// Stages the level (`None` = Auto). Nothing is written until Apply.
    pub fn set_language_level(&mut self, level: Option<String>, cx: &mut Context<Self>) {
        if self.applying || !self.loaded || self.root.is_none() {
            return;
        }
        self.staged.level = level;
        cx.notify();
    }

    /// Stages the interpreter (`None` = Auto). Only a detected or probed interpreter is offered:
    /// a project never chooses the binary.
    pub fn set_interpreter(&mut self, binary: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        self.staged.interpreter = binary;
        cx.notify();
    }

    /// Probes `path` off the main thread; a PHP that answers `-v` joins the list and is staged.
    pub fn add_interpreter(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        let executor = cx.background_executor().clone();
        self._probe = cx.spawn(async move |this, cx| {
            let version = cx
                .background_spawn({
                    let path = path.clone();
                    async move {
                        php_config::within_probe_timeout(
                            &executor,
                            php_config::PROBE_TIMEOUT,
                            php_config::probe_php(&path),
                        )
                        .await
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match version {
                    Some(version) => {
                        let list = this.interpreters.get_or_insert_default();
                        if !list.iter().any(|known| known.path == path) {
                            list.push(PhpInterpreter {
                                path: path.clone(),
                                version,
                            });
                        }
                        this.staged.interpreter = Some(path);
                        if this
                            .probe_error
                            .take()
                            .is_some_and(|probe| this.error.as_ref() == Some(&probe))
                        {
                            this.error = None;
                        }
                    }
                    None => {
                        let error: SharedString =
                            format!("{} is not a PHP binary that answers -v.", path.display())
                                .into();
                        this.probe_error = Some(error.clone());
                        this.error = Some(error);
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    /// Stages include paths, skipping those already listed.
    pub fn stage_include_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        for path in paths {
            let path = path.to_string_lossy().into_owned();
            if !self.staged.include.contains(&path) {
                self.staged.include.push(path);
            }
        }
        cx.notify();
    }

    /// Takes the selected user row out of the staged list; a vendor row is read-only.
    pub fn remove_selected(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        if let Some(Row::User(path)) = self.selected.clone() {
            self.staged.include.retain(|entry| *entry != path);
            self.selected = None;
            cx.notify();
        }
    }

    fn select(&mut self, row: Row, cx: &mut Context<Self>) {
        self.selected = Some(row);
        cx.notify();
    }

    /// Shows the selected folder in the file manager. The path is worked out now, not when the
    /// row was drawn.
    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        let path = match (&self.selected, &self.root) {
            (Some(Row::User(path)), _) => PathBuf::from(path),
            (Some(Row::Vendor(package)), Some(root)) => root.join("vendor").join(package),
            _ => return,
        };
        if !path.is_absolute() {
            // A hand-edited `~/lib` or relative entry: the file manager would not find it.
            self.error =
                Some(format!("Cannot show {} (not an absolute path).", path.display()).into());
            cx.notify();
            return;
        }
        cx.reveal_path(&path);
    }

    fn prompt(
        &mut self,
        options: PathPromptOptions,
        cx: &mut Context<Self>,
        accept: impl FnOnce(&mut Self, Vec<PathBuf>, &mut Context<Self>) + 'static,
    ) {
        let picked = cx.prompt_for_paths(options);
        self._prompt = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |this, cx| accept(this, paths, cx))
                .log_err();
        });
    }

    fn choose_interpreter(&mut self, cx: &mut Context<Self>) {
        self.prompt(
            PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Choose PHP Binary".into()),
            },
            cx,
            |this, mut paths, cx| {
                if let Some(path) = paths.pop() {
                    this.add_interpreter(path, cx);
                }
            },
        );
    }

    fn choose_include_paths(&mut self, cx: &mut Context<Self>) {
        self.prompt(
            PathPromptOptions {
                files: false,
                directories: true,
                multiple: true,
                prompt: Some("Add Include Path".into()),
            },
            cx,
            |this, paths, cx| this.stage_include_paths(paths, cx),
        );
    }

    /// Writes the staged edits. The level goes first: when `.phpantom.toml` cannot be written
    /// nothing else is, and the dialog stays open with the edits. The settings file is written
    /// next and its result awaited: only a written include list opens worktrees, and only a
    /// written edit becomes the new base. A dirty OK that fails stays open.
    pub fn apply(&mut self, close: bool, cx: &mut Context<Self>) {
        if !self.loaded {
            return;
        }
        if self.applying {
            // OK while Apply is running: close when it has finished.
            self.close_after |= close;
            return;
        }
        if !self.is_dirty() {
            if close {
                cx.emit(DismissEvent);
            }
            return;
        }
        self.applying = true;
        self.close_after = close;
        self.error = None;
        // Everything the task needs is taken now: it never reads or writes through the modal
        // before it is done, because the modal may be gone (Esc) by then.
        let base = self.base.clone();
        let staged = self.staged.clone();
        let level_root = (base.level != staged.level)
            .then(|| self.root.clone())
            .flatten();
        let fs = self.project.read(cx).fs().clone();
        let project = self.project.clone();
        self._apply = cx.spawn(async move |this, cx| {
            if let Some(root) = &level_root
                && let Err(error) =
                    php_config::write_language_level(fs.as_ref(), root, staged.level.as_deref())
                        .await
            {
                // The modal may be gone (Esc): the error must not vanish with it.
                log::error!("PHP level was not saved: {error:#}");
                this.update(cx, |this, cx| {
                    this.applying = false;
                    this.close_after = false;
                    this.error = Some(error.to_string().into());
                    cx.notify();
                })
                .ok();
                return;
            }
            let removed: Vec<String> = base
                .include
                .iter()
                .filter(|path| !staged.include.contains(path))
                .cloned()
                .collect();
            let added: Vec<String> = staged
                .include
                .iter()
                .filter(|path| !base.include.contains(path))
                .cloned()
                .collect();
            let interpreter =
                (base.interpreter != staged.interpreter).then(|| staged.interpreter.clone());
            let written =
                (interpreter.is_some() || !removed.is_empty() || !added.is_empty()).then(|| {
                    let added = added.clone();
                    cx.update(|cx| {
                        update_settings_file_with_completion(fs, cx, move |content, _| {
                            let zenith = content.zenith.get_or_insert_default();
                            if let Some(binary) = interpreter {
                                zenith.php_tools.get_or_insert_default().php_binary =
                                    binary.map(|binary| binary.to_string_lossy().into_owned());
                            }
                            if !removed.is_empty() || !added.is_empty() {
                                // A diff on the list as it is now: edits made outside while the
                                // dialog was open survive.
                                let list = zenith
                                    .external_libraries
                                    .get_or_insert_default()
                                    .paths
                                    .get_or_insert_default();
                                list.retain(|path| !removed.contains(path));
                                for path in added {
                                    if !list.contains(&path) {
                                        list.push(path);
                                    }
                                }
                            }
                        })
                    })
                });
            // `update_settings_file` only queues the write and logs a failure (an unparsable
            // settings file): wait for the result.
            let settings_result = match written {
                Some(written) => written
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("the settings write was cancelled"))),
                None => Ok(()),
            };
            if let Err(error) = settings_result {
                log::error!("PHP settings were not saved: {error:#}");
                cx.update(|cx| {
                    if level_root.is_some() {
                        // `.phpantom.toml` was written: PHPantom still has to read it.
                        php_config::restart_phpantom(&project, cx);
                    }
                });
                this.update(cx, |this, cx| {
                    // Only the level is saved; the interpreter and the include list stay staged.
                    this.base.level = staged.level.clone();
                    this.applying = false;
                    this.close_after = false;
                    this.error = Some(
                        format!(
                            "Settings were not saved: {error}. Fix the settings file and try again."
                        )
                        .into(),
                    );
                    cx.notify();
                })
                .ok();
                return;
            }
            cx.update(|cx| {
                let added: Vec<PathBuf> = added.into_iter().map(PathBuf::from).collect();
                open_as_libraries(&added, &project, cx);
                if level_root.is_some() {
                    // ponytail: a level and an include change together restart PHPantom twice
                    // (here and in `phpantom::sync`, which sees the settings file change);
                    // accepted, a restart is cheap and the second one reads the final state.
                    php_config::restart_phpantom(&project, cx);
                }
            });
            this.update(cx, |this, cx| {
                this.base = staged;
                this.applying = false;
                if std::mem::take(&mut this.close_after) {
                    cx.emit(DismissEvent);
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// Moves the selection `delta` rows down the include list.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.staged.include.len() + self.packages.len();
        if total == 0 {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|row| (0..total).find(|&index| self.row_at(index).as_ref() == Some(row)));
        let next = match current {
            None if delta > 0 => 0,
            None => total - 1,
            Some(index) => (index as isize + delta).clamp(0, total as isize - 1) as usize,
        };
        self.selected = self.row_at(next);
        self.list_scroll
            .scroll_to_item(next, ScrollStrategy::Center);
        cx.notify();
    }

    fn row_at(&self, index: usize) -> Option<Row> {
        let users = self.staged.include.len();
        if index < users {
            self.staged.include.get(index).cloned().map(Row::User)
        } else {
            self.packages.get(index - users).cloned().map(Row::Vendor)
        }
    }

    fn render_level_row(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let modal = cx.weak_entity();
        let selected_level = self.staged.level.clone();
        let level_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
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
        });
        let level = self.staged.level.clone();
        let features = level.as_deref().and_then(php_config::level_features);
        let label = h_flex()
            .gap_1()
            .child(Label::new(level.unwrap_or_else(|| AUTO_LEVEL.to_string())))
            .when_some(features, |this, features| {
                this.child(Label::new(format!("({features})")).color(Color::Muted))
            })
            .into_any_element();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .debug_selector(|| "zenith-php-level-row".into())
                    .gap_2()
                    .child(div().w(rems(9.)).child(Label::new("PHP language level:")))
                    .child(
                        div().flex_1().child(
                            DropdownMenu::new_with_element("zenith-php-level", label, level_menu)
                                .full_width(true)
                                .aria_label("PHP language level")
                                .tab_index(1isize)
                                .disabled(self.root.is_none() || !self.loaded),
                        ),
                    )
                    .child(
                        IconButton::new("zenith-php-level-help", IconName::CircleHelp)
                            .icon_size(IconSize::Small)
                            .aria_label("About the PHP language level")
                            .tooltip(Tooltip::text(
                                "What the editor assumes PHP supports: it decides which \
                                 syntax and functions are suggested and flagged.",
                            )),
                    ),
            )
            .when(self.root.is_none(), |this| {
                this.child(
                    Label::new("Open a local project folder to set its language level.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }

    fn render_interpreter_row(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let modal = cx.weak_entity();
        let selected_binary = self.staged.interpreter.clone();
        let interpreters = self.interpreters.clone().unwrap_or_default();
        let interpreter_label = match &selected_binary {
            None => AUTO_INTERPRETER.to_string(),
            Some(binary) => match interpreters.iter().find(|found| &found.path == binary) {
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
        h_flex()
            .debug_selector(|| "zenith-php-interpreter-row".into())
            .gap_2()
            .child(div().w(rems(9.)).child(Label::new("CLI Interpreter:")))
            .child(
                div().flex_1().child(
                    DropdownMenu::new(
                        "zenith-php-interpreter",
                        if self.interpreters.is_none() {
                            "Looking for PHP…".to_string()
                        } else {
                            interpreter_label
                        },
                        interpreter_menu,
                    )
                    .full_width(true)
                    .tab_index(2isize)
                    .aria_label("CLI interpreter"),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "zenith-php-choose-interpreter".into())
                    .child(
                        IconButton::new("zenith-php-choose-interpreter", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .aria_label("Choose PHP binary")
                            .tab_index(3isize)
                            .tooltip(Tooltip::text("Choose a PHP binary…"))
                            .on_click(cx.listener(|this, _, _, cx| this.choose_interpreter(cx))),
                    ),
            )
    }

    fn render_include_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.staged.include.len() + self.packages.len();
        v_flex()
            .gap_2()
            .child(
                // The only tab there is: a heading, not a button that does nothing.
                div()
                    .debug_selector(|| "zenith-php-tab-include".into())
                    .child(Label::new("Include Path").weight(gpui::FontWeight::SEMIBOLD)),
            )
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
                    .debug_selector(|| "zenith-php-toolbar".into())
                    .gap_0p5()
                    .child(
                        div()
                            .debug_selector(|| "zenith-php-add-include".into())
                            .child(
                                IconButton::new("zenith-php-add-include", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .aria_label("Add include path")
                                    .tab_index(4isize)
                                    .tooltip(Tooltip::text("Add Include Path…"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_include_paths(cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "zenith-php-remove-include".into())
                            .child(
                                IconButton::new("zenith-php-remove-include", IconName::Dash)
                                    .icon_size(IconSize::Small)
                                    .aria_label("Remove include path")
                                    .tab_index(5isize)
                                    .tooltip(Tooltip::text("Remove Include Path"))
                                    .disabled(!matches!(self.selected, Some(Row::User(_))))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.remove_selected(cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "zenith-php-reveal-include".into())
                            .child(
                                IconButton::new("zenith-php-reveal-include", IconName::Folder)
                                    .icon_size(IconSize::Small)
                                    .aria_label("Show in file manager")
                                    .tab_index(6isize)
                                    .tooltip(Tooltip::text("Show in File Manager"))
                                    .disabled(self.selected.is_none())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reveal_selected(cx)),
                                    ),
                            ),
                    ),
            )
            .child(if total == 0 {
                Label::new("No include paths and no vendor packages.")
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                uniform_list(
                    "zenith-php-paths",
                    total,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let row = this.row_at(index)?;
                                Some(this.render_row(index, row, cx))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .h(px(240.))
                .into_any_element()
            })
    }

    fn render_row(&self, index: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected.as_ref() == Some(&row);
        let (text, vendor) = match &row {
            Row::User(path) => (path.clone(), false),
            Row::Vendor(package) => (
                self.root
                    .as_deref()
                    .unwrap_or(Path::new(""))
                    .join("vendor")
                    .join(package)
                    .display()
                    .to_string(),
                true,
            ),
        };
        div()
            .debug_selector(move || format!("zenith-php-row-{index}"))
            .child(
                ListItem::new(("zenith-php-row", index))
                    .toggle_state(selected)
                    .when(vendor, |item| {
                        item.start_slot(Icon::new(IconName::Screen).color(Color::Success))
                    })
                    .child(Label::new(text).color(if vendor {
                        Color::Success
                    } else {
                        Color::Default
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(row.clone(), cx))),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .justify_between()
            .child(
                IconButton::new("zenith-php-storage-help", IconName::CircleHelp)
                    .icon_size(IconSize::Small)
                    .aria_label("Where these settings are stored")
                    .tooltip(Tooltip::text(
                        "Language level: .phpantom.toml in the project folder.\n\
                         CLI interpreter (php_binary) and include paths: your user settings, \
                         for every project.",
                    )),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div().debug_selector(|| "zenith-php-cancel".into()).child(
                            Button::new("zenith-php-cancel", "Cancel")
                                .tab_index(7isize)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "zenith-php-apply".into()).child(
                            Button::new("zenith-php-apply", "Apply")
                                .disabled(!self.can_apply())
                                .tab_index(8isize)
                                .on_click(cx.listener(|this, _, _, cx| this.apply(false, cx))),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "zenith-php-ok".into()).child(
                            Button::new("zenith-php-ok", "OK")
                                .style(ButtonStyle::Filled)
                                .disabled(!self.loaded)
                                .tab_index(9isize)
                                .on_click(cx.listener(|this, _, _, cx| this.apply(true, cx))),
                        ),
                    ),
            )
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
        v_flex()
            .id("zenith-php-modal")
            .key_context("ZenithPhpModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.apply(true, cx)))
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SelectPrevious, _, cx| this.move_selection(-1, cx)),
            )
            .elevation_3(cx)
            .w(rems(48.))
            .p_3()
            .gap_3()
            .child(Headline::new("PHP").size(HeadlineSize::Small))
            .child(self.render_level_row(window, cx))
            .child(self.render_interpreter_row(window, cx))
            .child(self.render_include_tab(cx))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .debug_selector(|| "zenith-php-error".into())
                        .gap_1()
                        .child(Icon::new(IconName::XCircle).color(Color::Error))
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(self.render_footer(cx))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt as _;
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
        // Folders the tests add as include paths: they are opened as worktrees on Apply.
        fs.insert_tree("/libs", json!({ "a": {}, "b": {}, "x": {}, "new": {} }))
            .await;
        let project = Project::test(app_state.fs.clone(), [Path::new("/p")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.run_until_parked();
        (Fixture { workspace, fs }, cx)
    }

    /// Opens the modal with a fixed interpreter list (the action would probe the real machine).
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

    fn is_open(fixture: &Fixture, cx: &mut VisualTestContext) -> bool {
        fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<PhpModal>(cx))
            .is_some()
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("{id} is not shown"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn user_settings_text(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
        cx.run_until_parked();
        futures::executor::block_on(fixture.fs.load(paths::settings_file())).unwrap_or_default()
    }

    /// Puts `settings` in the user settings, as the file and as the loaded store.
    async fn set_user_settings(fixture: &Fixture, settings: &str, cx: &mut VisualTestContext) {
        fixture
            .fs
            .create_dir(paths::settings_file().parent().unwrap())
            .await
            .unwrap();
        fixture
            .fs
            .insert_file(paths::settings_file(), settings.as_bytes().to_vec())
            .await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.set_user_settings(settings, cx).result().unwrap();
            })
        });
    }

    #[gpui::test]
    async fn test_action_opens_modal_with_the_whole_dialog(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({ "composer.json": "{}", "vendor": { "acme": { "one": {} } } }),
        )
        .await;
        // No candidates: the action must not run the `php` of the machine the tests run on.
        php_config::TEST_CANDIDATES.with(|candidates| *candidates.borrow_mut() = Some(Vec::new()));
        cx.dispatch_action(ConfigurePhpIncludePaths);
        cx.run_until_parked();
        assert!(is_open(&fixture, cx), "the action opens the PHP modal");

        // Level row, interpreter row with "…", the Include Path tab, its toolbar, the list and
        // the footer are all there at once.
        for id in [
            "zenith-php-level-row",
            "zenith-php-interpreter-row",
            "zenith-php-choose-interpreter",
            "zenith-php-tab-include",
            "zenith-php-toolbar",
            "zenith-php-add-include",
            "zenith-php-remove-include",
            "zenith-php-reveal-include",
            "zenith-php-row-0",
            "zenith-php-cancel",
            "zenith-php-apply",
            "zenith-php-ok",
        ] {
            assert!(cx.debug_bounds(id).is_some(), "{id} is shown");
        }
        // PHP Runtime, Analysis and Composer Files are not built.
        for id in [
            "zenith-php-tab-runtime",
            "zenith-php-tab-analysis",
            "zenith-php-tab-composer",
            "zenith-php-tab-php",
        ] {
            assert!(cx.debug_bounds(id).is_none(), "{id} is not built");
        }
        let modal = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<PhpModal>(cx))
            .unwrap();
        modal.read_with(cx, |modal, _| {
            assert!(modal.loaded);
            assert!(!modal.can_apply(), "nothing to apply yet");
            assert_eq!(modal.packages, ["acme/one"]);
        });

        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(!is_open(&fixture, cx), "Escape closes it");
    }

    #[gpui::test]
    async fn test_nothing_is_written_before_apply(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let php = PhpInterpreter {
            path: PathBuf::from("/opt/php84/bin/php"),
            version: "8.4.1".into(),
        };
        let modal = open(&fixture, vec![php.clone()], cx);
        let toml = Path::new("/p/.phpantom.toml");
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.2".into()), cx);
            modal.set_interpreter(Some(php.path.clone()), cx);
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx);
        });
        cx.run_until_parked();
        assert_eq!(fixture.fs.write_count_for_path(toml), 0);
        assert_eq!(fixture.fs.write_count_for_path(paths::settings_file()), 0);
        assert!(!fixture.fs.is_file(toml).await);
        modal.read_with(cx, |modal, _| assert!(modal.can_apply()));

        click(cx, "zenith-php-apply");
        let text = fixture.fs.load(toml).await.unwrap();
        assert!(text.contains("[php]\nversion = \"8.2\""), "{text}");
        let settings = user_settings_text(&fixture, cx);
        assert!(
            settings.contains("\"php_binary\": \"/opt/php84/bin/php\""),
            "{settings}"
        );
        assert!(settings.contains("/libs/a"), "{settings}");
        // The project gets only `.phpantom.toml` (the interpreter is never written there).
        let local = Path::new("/p").join(paths::local_settings_file_relative_path().as_std_path());
        assert!(!fixture.fs.is_file(&local).await, "{local:?}");
        assert!(is_open(&fixture, cx), "Apply keeps the dialog open");
        modal.read_with(cx, |modal, _| {
            assert!(!modal.can_apply(), "Apply is disabled again");
            assert_eq!(modal.base, modal.staged);
        });
    }

    #[gpui::test]
    async fn test_cancel_discards_the_staged_edits(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.2".into()), cx);
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx);
        });
        click(cx, "zenith-php-cancel");
        assert!(!is_open(&fixture, cx));
        assert_eq!(
            fixture
                .fs
                .write_count_for_path(Path::new("/p/.phpantom.toml")),
            0
        );
        assert_eq!(fixture.fs.write_count_for_path(paths::settings_file()), 0);
        assert!(!fixture.fs.is_file(Path::new("/p/.phpantom.toml")).await);
    }

    #[gpui::test]
    async fn test_a_failed_level_write_writes_nothing_else(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({ "composer.json": "{}", ".phpantom.toml": "[php]\nversion = \"8.1\"\n" }),
        )
        .await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.read_with(cx, |modal, _| assert!(modal.error.is_none()));
        // The file turns unparsable after the dialog read it.
        let toml = Path::new("/p/.phpantom.toml");
        fixture.fs.insert_file(toml, b"not = [toml".to_vec()).await;
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.2".into()), cx);
            modal.set_interpreter(Some(PathBuf::from("/opt/php84/bin/php")), cx);
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx);
        });
        click(cx, "zenith-php-apply");

        assert_eq!(fixture.fs.write_count_for_path(paths::settings_file()), 0);
        assert_eq!(fixture.fs.load(toml).await.unwrap(), "not = [toml");
        assert!(cx.debug_bounds("zenith-php-error").is_some(), "error shown");
        assert!(is_open(&fixture, cx), "the dialog stays open");
        modal.read_with(cx, |modal, _| {
            assert!(modal.error.is_some());
            assert!(!modal.applying);
            assert_eq!(modal.staged.level.as_deref(), Some("8.2"), "edits kept");
            assert_eq!(modal.staged.include, ["/libs/a"]);
            assert!(modal.can_apply(), "can be tried again");
        });
    }

    #[gpui::test]
    async fn test_apply_restarts_phpantom_once_for_a_changed_level(cx: &mut TestAppContext) {
        use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
        let (fixture, cx) = setup(
            cx,
            json!({
                "composer.json": "{}",
                "a.php": "<?php\n",
                ".phpantom.toml": "[php]\nversion = \"8.4\"\n",
            }),
        )
        .await;
        let project = fixture
            .workspace
            .read_with(cx, |workspace, _| workspace.project().clone());
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(Arc::new(Language::new(
            LanguageConfig {
                name: "PHP".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["php".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            None,
        )));
        let mut servers = registry.register_fake_lsp(
            "PHP",
            FakeLspAdapter {
                name: "phpantom",
                ..Default::default()
            },
        );
        let (_buffer, _handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(Path::new("/p/a.php"), cx)
            })
            .await
            .unwrap();
        let server = servers.next().await.expect("phpantom starts");
        let _shutdown =
            server.set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| async { Ok(()) });
        let modal = open(&fixture, Vec::new(), cx);
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.staged.level.as_deref(), Some("8.4"));
        });
        let restarts = |servers: &mut futures::channel::mpsc::UnboundedReceiver<_>| {
            let mut count = 0;
            while servers.try_recv().is_ok() {
                count += 1;
            }
            count
        };

        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.5".into()), cx)
        });
        assert_eq!(restarts(&mut servers), 0, "staging restarts nothing");
        click(cx, "zenith-php-apply");
        assert_eq!(
            restarts(&mut servers),
            1,
            "8.4 to 8.5 restarts PHPantom once"
        );

        // Another kind of edit (no level change) restarts nothing.
        modal.update(cx, |modal, cx| {
            modal.set_interpreter(Some(PathBuf::from("/opt/php84/bin/php")), cx)
        });
        click(cx, "zenith-php-apply");
        assert_eq!(restarts(&mut servers), 0, "the level did not change");
        assert!(user_settings_text(&fixture, cx).contains("php_binary"));

        // Nothing changed: Apply does nothing, OK just closes.
        modal.update(cx, |modal, cx| modal.apply(false, cx));
        cx.run_until_parked();
        assert_eq!(restarts(&mut servers), 0);
        click(cx, "zenith-php-ok");
        assert_eq!(restarts(&mut servers), 0);
        assert!(!is_open(&fixture, cx), "a clean OK closes the dialog");
    }

    #[gpui::test]
    async fn test_escape_during_apply_still_writes(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.4".into()), cx);
            modal.apply(false, cx);
        });
        fixture
            .workspace
            .update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
        drop(modal);
        // The modal is released here, with its apply task not yet run.
        cx.update(|_, _| ());
        cx.run_until_parked();
        let text = fixture
            .fs
            .load(Path::new("/p/.phpantom.toml"))
            .await
            .unwrap();
        assert!(text.contains("version = \"8.4\""), "{text}");
    }

    #[gpui::test]
    async fn test_include_paths_are_written_as_a_diff(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({
                "composer.json": "{}",
                ".phpantom.toml": "[indexing]\ninclude = [\"lib\"]\n",
            }),
        )
        .await;
        set_user_settings(
            &fixture,
            r#"{ "zenith": { "external_libraries": { "paths": ["/libs/a", "/libs/b"] } } }"#,
            cx,
        )
        .await;
        let modal = open(&fixture, Vec::new(), cx);
        assert!(
            cx.debug_bounds("zenith-php-include-warning").is_some(),
            "the project's own include replaces the global list, so the tab warns"
        );
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.staged.include, ["/libs/a", "/libs/b"]);
        });

        // Remove /libs/a through the toolbar; adding /libs/b again is skipped.
        click(cx, "zenith-php-row-0");
        click(cx, "zenith-php-remove-include");
        modal.update(cx, |modal, cx| {
            modal.stage_include_paths(vec![PathBuf::from("/libs/b")], cx)
        });
        modal.read_with(cx, |modal, _| assert_eq!(modal.staged.include, ["/libs/b"]));
        // /libs/x is added to the settings from outside while the dialog is open.
        set_user_settings(
            &fixture,
            r#"{ "zenith": { "external_libraries": { "paths": ["/libs/a", "/libs/b", "/libs/x"] } } }"#,
            cx,
        )
        .await;
        modal.update(cx, |modal, cx| {
            modal.stage_include_paths(vec![PathBuf::from("/libs/new")], cx)
        });
        click(cx, "zenith-php-apply");

        let text = user_settings_text(&fixture, cx);
        assert!(!text.contains("/libs/a"), "removed: {text}");
        let order: Vec<_> = ["/libs/b", "/libs/x", "/libs/new"]
            .iter()
            .map(|path| text.find(path).unwrap_or_else(|| panic!("{path}: {text}")))
            .collect();
        assert!(order.is_sorted(), "outside edit kept, new one last: {text}");
    }

    #[gpui::test]
    async fn test_a_failed_settings_write_is_not_treated_as_saved(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({ "composer.json": "{}", ".phpantom.toml": "[php]\nversion = \"8.1\"\n" }),
        )
        .await;
        fixture
            .fs
            .create_dir(paths::settings_file().parent().unwrap())
            .await
            .unwrap();
        fixture
            .fs
            .insert_file(paths::settings_file(), b"{ not json".to_vec())
            .await;
        let project = fixture
            .workspace
            .read_with(cx, |workspace, _| workspace.project().clone());
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.2".into()), cx);
            modal.set_interpreter(Some(PathBuf::from("/opt/php84/bin/php")), cx);
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx);
        });
        // OK, not Apply: a failed save must not close the dialog either.
        click(cx, "zenith-php-ok");

        assert!(is_open(&fixture, cx), "the dialog stays open");
        assert!(cx.debug_bounds("zenith-php-error").is_some(), "error shown");
        modal.read_with(cx, |modal, _| {
            assert!(!modal.applying);
            assert!(modal.error.is_some());
            // The level was written, so it is the base now; the rest is still unsaved.
            assert_eq!(modal.base.level.as_deref(), Some("8.2"));
            assert_ne!(modal.base.include, modal.staged.include);
            assert_ne!(modal.base.interpreter, modal.staged.interpreter);
            assert_eq!(modal.staged.include, ["/libs/a"]);
            assert!(modal.can_apply(), "can be tried again");
        });
        let toml = fixture
            .fs
            .load(Path::new("/p/.phpantom.toml"))
            .await
            .unwrap();
        assert!(toml.contains("version = \"8.2\""), "{toml}");
        let worktrees = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>()
        });
        assert_eq!(worktrees, [PathBuf::from("/p")], "no worktree for /libs/a");
    }

    #[gpui::test]
    async fn test_ok_applies_and_closes(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.3".into()), cx);
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx);
        });
        click(cx, "zenith-php-ok");
        assert!(!is_open(&fixture, cx), "a dirty OK closes after writing");
        let text = fixture
            .fs
            .load(Path::new("/p/.phpantom.toml"))
            .await
            .unwrap();
        assert!(text.contains("version = \"8.3\""), "{text}");
        assert!(user_settings_text(&fixture, cx).contains("/libs/a"));
    }

    #[gpui::test]
    async fn test_ok_while_apply_runs_closes_when_it_finishes(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.set_language_level(Some("8.3".into()), cx);
            modal.apply(false, cx);
            modal.apply(true, cx);
        });
        assert!(is_open(&fixture, cx), "still running");
        cx.run_until_parked();
        assert!(!is_open(&fixture, cx), "OK closed it once Apply was done");
        let text = fixture
            .fs
            .load(Path::new("/p/.phpantom.toml"))
            .await
            .unwrap();
        assert!(text.contains("version = \"8.3\""), "{text}");
    }

    #[gpui::test]
    async fn test_arrow_keys_move_the_selection_over_user_and_vendor_rows(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({ "composer.json": "{}", "vendor": { "acme": { "one": {} } } }),
        )
        .await;
        let modal = open(&fixture, Vec::new(), cx);
        modal.update(cx, |modal, cx| {
            modal.stage_include_paths(vec![PathBuf::from("/libs/a")], cx)
        });
        let selected = |cx: &mut VisualTestContext| modal.read_with(cx, |m, _| m.selected.clone());
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(selected(cx), Some(Row::User("/libs/a".into())));
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(selected(cx), Some(Row::Vendor("acme/one".into())));
        cx.dispatch_action(menu::SelectNext);
        assert_eq!(
            selected(cx),
            Some(Row::Vendor("acme/one".into())),
            "stops at the end"
        );
        cx.dispatch_action(menu::SelectPrevious);
        assert_eq!(selected(cx), Some(Row::User("/libs/a".into())));
    }

    #[gpui::test]
    async fn test_a_vendor_row_cannot_be_removed(cx: &mut TestAppContext) {
        let (fixture, cx) = setup(
            cx,
            json!({ "composer.json": "{}", "vendor": { "acme": { "one": {} } } }),
        )
        .await;
        let modal = open(&fixture, Vec::new(), cx);
        click(cx, "zenith-php-row-0");
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.selected, Some(Row::Vendor("acme/one".into())));
        });
        click(cx, "zenith-php-remove-include");
        modal.update(cx, |modal, cx| modal.remove_selected(cx));
        modal.read_with(cx, |modal, _| {
            assert!(modal.staged.include.is_empty());
            assert_eq!(modal.packages, ["acme/one"], "read-only");
            assert_eq!(modal.selected, Some(Row::Vendor("acme/one".into())));
            assert!(!modal.is_dirty());
        });
    }

    /// Runs the executor until `done` holds. A probe is a real process on a real thread, so the
    /// executor alone cannot know when it is finished.
    fn wait_for(
        modal: &Entity<PhpModal>,
        cx: &mut VisualTestContext,
        done: impl Fn(&PhpModal) -> bool,
    ) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if modal.read_with(cx, |modal, _| done(modal)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the probe did not finish");
    }

    #[gpui::test]
    async fn test_a_probed_interpreter_is_accepted_only_if_it_answers(cx: &mut TestAppContext) {
        // Real processes run on real threads.
        cx.executor().allow_parking();
        let (fixture, cx) = setup(cx, json!({ "composer.json": "{}" })).await;
        let dir = tempfile::tempdir().unwrap();
        let good = crate::php_stub(dir.path(), "php83", "8.3.1", 0o755);
        let not_php = dir.path().join("not-php");
        std::fs::write(&not_php, "#!/bin/sh\necho hello\n").unwrap();
        std::fs::set_permissions(
            &not_php,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let modal = open(&fixture, Vec::new(), cx);

        modal.update(cx, |modal, cx| modal.add_interpreter(good.clone(), cx));
        wait_for(&modal, cx, |modal| modal.staged.interpreter.is_some());
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.staged.interpreter.as_ref(), Some(&good));
            assert_eq!(
                modal.interpreters.as_deref(),
                Some(
                    &[PhpInterpreter {
                        path: good.clone(),
                        version: "8.3.1".into()
                    }][..]
                )
            );
            assert!(modal.error.is_none());
        });

        modal.update(cx, |modal, cx| modal.add_interpreter(not_php.clone(), cx));
        wait_for(&modal, cx, |modal| modal.error.is_some());
        modal.read_with(cx, |modal, _| {
            assert_eq!(modal.staged.interpreter.as_ref(), Some(&good), "unchanged");
            assert_eq!(modal.interpreters.as_ref().map(Vec::len), Some(1));
            let error = modal.error.as_ref().expect("an error is shown");
            assert!(
                error.contains("not a PHP binary that answers -v"),
                "{error}"
            );
        });
        assert_eq!(fixture.fs.write_count_for_path(paths::settings_file()), 0);
    }
}
