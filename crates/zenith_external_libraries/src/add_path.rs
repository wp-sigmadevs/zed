//! "Add Library Path…": pick folders, mark them as libraries (read-only banner, local settings
//! and tasks inside never loaded) and open them in the project.

use gpui::{Context, PathPromptOptions, TaskExt as _, Window, actions};
use settings::update_settings_file;
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
                for path in &paths {
                    // Immediate for this run: the settings file round trip is asynchronous and
                    // the worktree below must already be gated when its first scan lands.
                    zenith_local_settings_trust::add_library_root(path.as_path().into(), cx);
                }
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
                for path in paths {
                    project
                        .update(cx, |project, cx| {
                            project.find_or_create_worktree(path, true, cx)
                        })
                        .detach_and_log_err(cx);
                }
            })
            .ok();
    })
    .detach();
}
