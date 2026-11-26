//! Where a folder picked with Open goes (plan 44): this window, a new window, or attached to
//! this window's project. One seam for File > Open, the palette, the keymap and the recent
//! project rows. Not built: the dialog on drag and drop (a drop keeps attaching), the dialog in
//! upstream's Open Recent modal, and a close-and-replace choice.

mod dialog;

use std::{path::PathBuf, sync::Arc};

use futures::future::join_all;
use gpui::{App, AsyncWindowContext, Context, PathPromptOptions, WeakEntity, Window, actions};
use settings::{RegisterSetting, Settings, SettingsContent, ZenithOpenProjectWindow};
use util::ResultExt as _;
use workspace::{
    AppState, MultiWorkspace, OpenMode, OpenOptions, OpenPathPromptMode, OpenPathPromptResult,
    OpenVisible, Workspace, WorkspaceMatching, open_paths,
};

pub use dialog::OpenProjectDialog;

actions!(
    zenith_open_project,
    [
        /// Asks again which window a folder opens in, forgetting "Don't ask again".
        ConfirmWindow,
        /// Moves focus to the next control of the Open Project dialog (Tab).
        FocusNextControl,
        /// Moves focus to the previous control of the Open Project dialog (Shift-Tab).
        FocusPreviousControl
    ]
);

/// Where a folder opens when this window already has a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Add the folder and switch to it (a replace when the project sidebar is off).
    ThisWindow,
    NewWindow,
    /// Add the folder to this window's project.
    Attach,
}

impl Choice {
    fn content(self) -> ZenithOpenProjectWindow {
        match self {
            Choice::ThisWindow => ZenithOpenProjectWindow::ThisWindow,
            Choice::NewWindow => ZenithOpenProjectWindow::NewWindow,
            Choice::Attach => ZenithOpenProjectWindow::Attach,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Files only: upstream's behaviour.
    Upstream,
    Do(Choice),
    Ask,
}

/// `zenith.open_project.window`: the remembered choice; unset (or "ask") asks every time.
#[derive(Clone, Copy, RegisterSetting)]
pub struct ZenithOpenProjectSettings {
    pub window: Option<Choice>,
}

impl Settings for ZenithOpenProjectSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let window = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.open_project.as_ref())
            .and_then(|open| open.window);
        Self {
            window: match window {
                None | Some(ZenithOpenProjectWindow::Ask) => None,
                Some(ZenithOpenProjectWindow::ThisWindow) => Some(Choice::ThisWindow),
                Some(ZenithOpenProjectWindow::NewWindow) => Some(Choice::NewWindow),
                Some(ZenithOpenProjectWindow::Attach) => Some(Choice::Attach),
            },
        }
    }
}

pub fn init(cx: &mut App) {
    ZenithOpenProjectSettings::register(cx);
    cx.on_action(|_: &ConfirmWindow, cx| {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, |content, _| {
            if let Some(zenith) = content.zenith.as_mut() {
                zenith.open_project = None;
            }
        });
    });
}

/// What to do with `any_dir`-containing picks. `has_project`: this window is a local project
/// with a folder in it. A caller that already asked for a new window is never asked again.
pub fn decide(
    has_project: bool,
    any_dir: bool,
    mode: OpenPathPromptMode,
    remembered: Option<Choice>,
) -> Decision {
    if matches!(mode, OpenPathPromptMode::NewWindow) {
        return Decision::Do(Choice::NewWindow);
    }
    if !any_dir {
        return Decision::Upstream;
    }
    if !has_project {
        return Decision::Do(Choice::ThisWindow);
    }
    remembered.map_or(Decision::Ask, Decision::Do)
}

/// Same signature as `workspace::prompt_for_open_path_and_open`, which it replaces.
pub fn prompt_for_open_path_and_open(
    workspace: &mut Workspace,
    app_state: Arc<AppState>,
    mut options: PathPromptOptions,
    mode: OpenPathPromptMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if options.prompt.is_none() && matches!(mode, OpenPathPromptMode::CurrentWindow) {
        // The system dialog's default button says "Open in This Window", which this dialog may
        // contradict.
        options.prompt = Some("Open".into());
    }
    let paths = workspace.prompt_for_open_path(
        options,
        project::DirectoryLister::Local(workspace.project().clone(), app_state.fs.clone()),
        mode,
        window,
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(OpenPathPromptResult {
            paths,
            open_in_new_window,
        }) = paths.await.log_err().flatten()
        else {
            return;
        };
        let mode = if open_in_new_window {
            OpenPathPromptMode::NewWindow
        } else {
            mode
        };
        route(paths, mode, workspace, app_state, cx).await;
    })
    .detach();
}

/// Opens already-picked `paths` (a recent project row) the same way, per the
/// `default_open_behavior` setting for the "already asked for a new window" case.
pub fn open_with_choice(
    paths: Vec<PathBuf>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let mode = if crate::projects::open_mode(cx) == OpenMode::NewWindow {
        OpenPathPromptMode::NewWindow
    } else {
        OpenPathPromptMode::CurrentWindow
    };
    let app_state = workspace.app_state().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        route(paths, mode, workspace, app_state, cx).await;
    })
    .detach();
}

async fn route(
    paths: Vec<PathBuf>,
    mode: OpenPathPromptMode,
    workspace: WeakEntity<Workspace>,
    app_state: Arc<AppState>,
    cx: &mut AsyncWindowContext,
) {
    let Ok((is_local, has_project)) = workspace.read_with(cx, |workspace, cx| {
        let project = workspace.project().read(cx);
        let is_local = project.is_local();
        (
            is_local,
            is_local && project.visible_worktrees(cx).next().is_some(),
        )
    }) else {
        return;
    };
    let any_dir = join_all(paths.iter().map(|path| app_state.fs.is_dir(path)))
        .await
        .into_iter()
        .any(|is_dir| is_dir);
    let remembered = cx.update(|_, cx| ZenithOpenProjectSettings::get_global(cx).window);
    let Ok(remembered) = remembered else {
        return;
    };
    match decide(has_project, any_dir, mode, remembered) {
        Decision::Upstream => open_files(paths, is_local, workspace, cx).await,
        Decision::Do(choice) => apply(choice, paths, workspace, cx).await,
        Decision::Ask => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let weak = cx.weak_entity();
                    let fs = app_state.fs.clone();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        OpenProjectDialog::new(paths, weak, fs, window, cx)
                    });
                })
                .log_err();
        }
    }
}

/// Upstream's branch for files only: open them in this window (a remote project adds them as
/// a project, like a folder).
async fn open_files(
    paths: Vec<PathBuf>,
    is_local: bool,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    if !is_local {
        return apply(Choice::ThisWindow, paths, workspace, cx).await;
    }
    let Ok(task) = workspace.update_in(cx, |workspace, window, cx| {
        let visible = if workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .is_none()
        {
            OpenVisible::All
        } else {
            OpenVisible::None
        };
        workspace.open_paths(
            paths,
            OpenOptions {
                visible: Some(visible),
                ..OpenOptions::default()
            },
            None,
            window,
            cx,
        )
    }) else {
        return;
    };
    show_errors(task.await, &workspace, cx);
}

fn show_errors(
    results: Vec<Option<anyhow::Result<Box<dyn workspace::item::ItemHandle>>>>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    for result in results.into_iter().flatten() {
        if let Err(error) = result {
            workspace
                .update(cx, |workspace, cx| workspace.show_error(error, cx))
                .ok();
        }
    }
}

pub(crate) async fn apply(
    choice: Choice,
    paths: Vec<PathBuf>,
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) {
    match choice {
        Choice::ThisWindow => {
            let Some(handle) = cx.window_handle().downcast::<MultiWorkspace>() else {
                return;
            };
            if let Ok(task) = handle.update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
            }) {
                task.await.log_err();
            }
        }
        Choice::NewWindow => {
            let Ok(app_state) = workspace.read_with(cx, |workspace, _| workspace.app_state().clone())
            else {
                return;
            };
            if let Ok(task) = workspace.update_in(cx, |_, _, cx| {
                open_paths(
                    &paths,
                    app_state,
                    OpenOptions {
                        open_mode: OpenMode::NewWindow,
                        workspace_matching: WorkspaceMatching::None,
                        ..OpenOptions::default()
                    },
                    cx,
                )
            }) {
                task.await.log_err();
            }
        }
        Choice::Attach => {
            let Ok(task) = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_paths(
                    paths,
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..OpenOptions::default()
                    },
                    None,
                    window,
                    cx,
                )
            }) else {
                return;
            };
            show_errors(task.await, &workspace, cx);
        }
    }
}

#[cfg(test)]
mod tests;
