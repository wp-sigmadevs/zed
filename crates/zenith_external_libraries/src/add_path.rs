//! "Add Library Path…": pick folders, mark them as libraries (read-only banner, local settings
//! and tasks inside never loaded) and open them in the project.

use gpui::{App, Context, Entity, PathPromptOptions, TaskExt as _, Window, actions};
use project::Project;
use settings::update_settings_file;
use std::path::PathBuf;
use workspace::Workspace;

actions!(
    zenith_external_libraries,
    [
        /// Picks folders to treat as external libraries: they are added to
        /// `zenith.external_libraries.paths` and opened in the project.
        AddLibraryPath,
        /// Opens the PHP settings: language level, CLI interpreter and include paths.
        ConfigurePhpIncludePaths,
    ]
);

pub(crate) fn add_library_path(
    workspace: &mut Workspace,
    _: &AddLibraryPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: true,
        prompt: Some("Add Library Path".into()),
    });
    let fs = workspace.app_state().fs.clone();
    let project = workspace.project().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(Ok(Some(paths))) = picked.await else {
            return;
        };
        workspace
            .update(cx, |_, cx| {
                let strings: Vec<String> = paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                update_settings_file(fs, cx, move |content, _| {
                    let list = content
                        .zenith
                        .get_or_insert_default()
                        .external_libraries
                        .get_or_insert_default()
                        .paths
                        .get_or_insert_default();
                    for path in strings {
                        if !list.contains(&path) {
                            list.push(path);
                        }
                    }
                });
                open_as_libraries(&paths, &project, cx);
            })
            .ok();
    })
    .detach();
}

/// Marks `paths` as library roots and opens them in `project`. The roots go first, immediately
/// for this run: the settings file round trip is asynchronous and each worktree must already be
/// gated when its first scan lands. The caller records the paths in the settings.
pub fn open_as_libraries(paths: &[PathBuf], project: &Entity<Project>, cx: &mut App) {
    for path in paths {
        zenith_local_settings_trust::add_library_root(path.as_path().into(), cx);
    }
    for path in paths {
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
            .detach_and_log_err(cx);
    }
}
