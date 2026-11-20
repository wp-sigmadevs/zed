//! "New since branch" mode (plan 21): with the toggle on, PHPCS and PHPStan
//! findings on lines that were already there at the merge base with the default
//! branch are hidden, and "inspect changed files" lists the branch's files too.
//! The git work is in `since_branch`; this is the per-project switch, the base
//! lookup and the line filter.

use crate::{Analyzer, InspectionSettings, Problem, inspect_files, notify, since_branch};
use db::kvp::KeyValueStore;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Entity, Task};
use project::{Project, ProjectPath, WorktreeId};
use settings::Settings as _;
use since_branch::NewLines;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use workspace::Workspace;

/// A base lookup that every file of a batch awaits.
pub(crate) type SharedBase = futures::future::Shared<Task<Result<String, String>>>;

/// Test-only: how many base lookups this app has run.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct BaseLookups(pub(crate) usize);

#[cfg(test)]
impl gpui::Global for BaseLookups {}

const NAMESPACE: &str = "zenith_inspections";

/// The switch per project folder, and the files it has cut findings from. The
/// switch is read from the key-value store once per folder, then only from here.
#[derive(Default)]
pub(crate) struct NewSinceBranch {
    on: HashMap<PathBuf, bool>,
    hidden: HashSet<PathBuf>,
}

impl gpui::Global for NewSinceBranch {}

pub(crate) fn is_on(root: &Path, cx: &mut App) -> bool {
    if let Some(on) = cx
        .try_global::<NewSinceBranch>()
        .and_then(|state| state.on.get(root))
    {
        return *on;
    }
    let store = KeyValueStore::global(cx);
    let stored = store
        .scoped(NAMESPACE)
        .read(&root.to_string_lossy())
        .log_err()
        .flatten();
    let on = stored.as_deref() == Some("on");
    cx.default_global::<NewSinceBranch>()
        .on
        .insert(root.to_path_buf(), on);
    on
}

pub(crate) fn set_on(root: &Path, on: bool, cx: &mut App) {
    cx.default_global::<NewSinceBranch>()
        .on
        .insert(root.to_path_buf(), on);
    let store = KeyValueStore::global(cx);
    let key = root.to_string_lossy().into_owned();
    db::write_and_log(cx, move || async move {
        let value = if on { "on" } else { "off" };
        store.scoped(NAMESPACE).write(key, value.into()).await
    });
}

/// Remembers whether the last run on `path` cut findings, so a toggle re-runs it.
pub(crate) fn mark_hidden(path: &Path, hidden: bool, cx: &mut App) {
    let files = &mut cx.default_global::<NewSinceBranch>().hidden;
    if hidden {
        files.insert(path.to_path_buf());
    } else {
        files.remove(path);
    }
}

/// Findings on new lines. `All` (untracked file, or git failed) keeps everything.
///
/// ponytail: PHPStan errors that a change causes on lines it did not touch (a
/// renamed method's callers) are hidden too. Upgrade: analyse the base revision
/// in a temporary worktree and compare findings instead of lines.
pub(crate) fn keep_new(problems: Vec<Problem>, new_lines: &NewLines) -> Vec<Problem> {
    match new_lines {
        NewLines::All => problems,
        NewLines::Ranges(ranges) => problems
            .into_iter()
            .filter(|problem| ranges.iter().any(|range| range.contains(&problem.line)))
            .collect(),
    }
}

/// The default branch of the repository holding the folder, looked up on every
/// call. The text of an `Err` completes "Showing all findings".
pub(crate) fn base_branch(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> Task<Result<String, String>> {
    // SECURITY: every since-branch git call starts here, and git runs a project's
    // `.git/config` (filters, fsmonitor), so an untrusted folder gets no base and
    // therefore no git (see the module doc of `since_branch`).
    if !zenith_php_tools::worktree_trusted(project, worktree_id, cx) {
        return Task::ready(Err("The project is not trusted.".into()));
    }
    #[cfg(test)]
    {
        cx.default_global::<BaseLookups>().0 += 1;
    }
    let folder = ProjectPath {
        worktree_id,
        path: util::rel_path::RelPath::empty().into(),
    };
    let repository = project
        .read(cx)
        .git_store()
        .read(cx)
        .repository_and_path_for_project_path(&folder, cx)
        .map(|(repository, _)| repository);
    let Some(repository) = repository else {
        return Task::ready(Err("No git repository was found.".into()));
    };
    let request = repository.update(cx, |repository, _| repository.default_branch(true));
    cx.background_spawn(async move {
        match request.await {
            Ok(Ok(Some(base))) => Ok(base.to_string()),
            Ok(Ok(None)) => Err("No default branch was found.".into()),
            Ok(Err(error)) => Err(format!("The default branch could not be read: {error:#}")),
            Err(_) => Err("The default branch lookup was cancelled.".into()),
        }
    })
}

/// Files whose findings the toggle changes: those showing PHPCS or PHPStan
/// diagnostics now, and those where the filter cut every finding.
pub(crate) fn files_to_rerun(project: &Entity<Project>, cx: &App) -> Vec<(WorktreeId, PathBuf)> {
    let analyzer_ids = [Analyzer::Phpcs.server_id(), Analyzer::Phpstan.server_id()];
    let project = project.read(cx);
    let mut paths: HashSet<PathBuf> = project
        .diagnostic_summaries(false, cx)
        .filter(|(_, server_id, summary)| {
            analyzer_ids.contains(server_id) && summary.error_count + summary.warning_count > 0
        })
        .filter_map(|(path, _, _)| project.absolute_path(&path, cx))
        .collect();
    if let Some(state) = cx.try_global::<NewSinceBranch>() {
        paths.extend(state.hidden.iter().cloned());
    }
    let mut files: Vec<_> = paths
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "php"))
        .filter_map(|path| {
            let project_path = project.project_path_for_absolute_path(&path, cx)?;
            Some((project_path.worktree_id, path))
        })
        .collect();
    files.sort();
    files
}

pub(crate) fn toggle(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    let folders: Vec<(WorktreeId, PathBuf)> = project
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| !worktree.read(cx).is_single_file())
        .map(|worktree| {
            (
                worktree.read(cx).id(),
                worktree.read(cx).abs_path().to_path_buf(),
            )
        })
        .collect();
    let Some((first_id, first_root)) = folders.first().cloned() else {
        return notify(workspace, "Open a folder first.".into(), cx);
    };
    let on = !is_on(&first_root, cx);
    for (_, root) in &folders {
        set_on(root, on, cx);
    }
    let base = base_branch(&project, first_id, cx).shared();
    let files = files_to_rerun(&project, cx);
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        let mut message = match (on, base.clone().await) {
            (false, _) => "New since branch: off. Showing all findings.".to_string(),
            (true, Ok(base)) => {
                format!("New since branch: on. Showing findings on lines changed since {base}.")
            }
            (true, Err(error)) => {
                format!("New since branch: on. {error} Showing all findings.")
            }
        };
        if !files.is_empty() {
            message.push_str(&format!(" Re-inspecting {} PHP files.", files.len()));
        }
        let bases = on.then_some((first_id, base)).into_iter().collect();
        inspect_files(project, files, message, max_parallel, bases, workspace, cx).await;
    })
    .detach();
}

/// Message for a run that fell back to all findings (shown once per session).
pub(crate) fn fallback_message(reason: &str) -> String {
    format!(
        "New since branch: {}. Showing all findings.",
        reason.trim_end_matches('.')
    )
}

/// Message for "inspect changed files" when the branch's files could not be listed.
pub(crate) fn listing_fallback_message(reason: &str) -> String {
    format!(
        "New since branch: {}. Listing the changes git reports only.",
        reason.trim_end_matches('.')
    )
}
