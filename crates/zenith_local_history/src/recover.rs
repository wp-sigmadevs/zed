//! "Recover Deleted Files": lists the files that have local history but no longer
//! exist in an open folder, and writes the newest revision of the chosen one back.
//!
//! A restore creates a file from data in a database, so it is deliberately strict:
//! the recorded path must be a plain relative path, it must stay inside the folder
//! with no symbolic link on the way, it never overwrites anything, and it never
//! brings back a path that is private, ignored or excluded now. It writes through
//! the project's `Fs`, only in a trusted local folder.

use crate::{
    LocalHistorySettings, Revision, capture,
    history_view::format_time,
    store::{self, MAX_CONTENT_BYTES},
};
use fs::{CreateOptions, Fs, RemoveOptions};
use fuzzy_nucleo::{StringMatch, StringMatchCandidate, StringMatchOptions, match_strings};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, Role, SharedString, Styled, Task, WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use project::Project;
use settings::{Settings, SettingsLocation, WorktreeId};
use std::{fmt, path::PathBuf, sync::Arc};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::rel_path::RelPath;
use workspace::{ModalView, Workspace};
use worktree::{Worktree, WorktreeSettings};
use zed_actions::zenith_local_history::RecoverDeletedFiles;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RecoverDeletedFiles, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

/// Why a file is not restored. Shown to the user as the status text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Refusal {
    NotLocal,
    Untrusted,
    FolderGone,
    BadPath,
    Protected,
    Exists,
    Symlink,
    TooLarge,
    Pruned,
    Io(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLocal => write!(f, "Recover Deleted Files works in local projects only."),
            Self::Untrusted => write!(f, "This folder is not trusted. Trust it to recover files."),
            Self::FolderGone => write!(f, "No project folder is open."),
            Self::BadPath => write!(f, "The recorded path is not a safe path inside the folder."),
            Self::Protected => write!(
                f,
                "The path is private, ignored or excluded, so it is not restored."
            ),
            Self::Exists => write!(f, "A file or folder already exists at that path."),
            Self::Symlink => write!(f, "The path passes through a symbolic link."),
            Self::TooLarge => write!(f, "The revision is larger than the history size limit."),
            Self::Pruned => write!(f, "The revision was removed by the retention limits."),
            Self::Io(error) => write!(f, "Could not write the file: {error}"),
        }
    }
}

/// A file with history that is gone from its folder.
#[derive(Clone, Debug)]
struct Deleted {
    worktree_id: WorktreeId,
    /// The folder's name, shown when more than one folder is open.
    folder: SharedString,
    revision: Revision,
}

/// Running a project's files is trusting it: only in a worktree the user trusts.
/// True when the trust feature is off.
fn worktree_trusted(project: &Entity<Project>, worktree_id: WorktreeId, cx: &mut App) -> bool {
    let Some(store) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return true;
    };
    let worktree_store = project.read(cx).worktree_store();
    store.update(cx, |store, cx| {
        store.can_trust(&worktree_store, worktree_id, cx)
    })
}

/// A recorded path as a [`RelPath`], or `BadPath`. Strict: empty, absolute, `.`,
/// `..`, repeated, leading or trailing separators, backslashes, NUL and anything
/// inside `.git` are all refused.
fn validate_rel(rel_path: &str) -> Result<&RelPath, Refusal> {
    if rel_path.is_empty() || rel_path.contains(['\0', '\\']) {
        return Err(Refusal::BadPath);
    }
    // `from_unix_str` fails on an absolute path and on any path it would normalise.
    let rel = RelPath::from_unix_str(rel_path).map_err(|_| Refusal::BadPath)?;
    // It also trims a leading `./` and a trailing `/` without complaint.
    if rel.as_unix_str() != rel_path
        || rel
            .components()
            .any(|part| part.eq_ignore_ascii_case(".git"))
    {
        return Err(Refusal::BadPath);
    }
    Ok(rel)
}

/// Private, secret-named, excluded or ignored now. A gitignore pattern that names
/// the file itself cannot be evaluated for a path that does not exist; the
/// directory it lies in can.
fn protected(worktree: &Worktree, rel: &RelPath, cx: &App) -> bool {
    let settings = WorktreeSettings::get(
        Some(SettingsLocation {
            worktree_id: worktree.id(),
            path: rel,
        }),
        cx,
    );
    store::is_secret_file(&worktree.absolutize(rel))
        || settings.is_path_private(rel)
        || settings.is_path_excluded(rel)
        || worktree.is_path_ignored(rel)
}

/// The open, trusted, local folders that can have deleted files.
fn eligible_worktrees(project: &Entity<Project>, cx: &mut App) -> Result<Vec<WorktreeId>, Refusal> {
    if !project.read(cx).is_local() {
        return Err(Refusal::NotLocal);
    }
    let folders: Vec<_> = project
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| {
            worktree
                .read(cx)
                .root_entry()
                .is_some_and(|entry| entry.is_dir())
        })
        .map(|worktree| worktree.read(cx).id())
        .collect();
    if folders.is_empty() {
        return Err(Refusal::FolderGone);
    }
    let trusted: Vec<_> = folders
        .into_iter()
        .filter(|id| worktree_trusted(project, *id, cx))
        .collect();
    if trusted.is_empty() {
        Err(Refusal::Untrusted)
    } else {
        Ok(trusted)
    }
}

/// Files with history that are not in the folder now, newest first. Paths that
/// could not be restored anyway (unsafe, protected) are left out.
fn listed(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    revisions: Vec<Revision>,
    cx: &App,
) -> Vec<Deleted> {
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return Vec::new();
    };
    let worktree = worktree.read(cx);
    let folder: SharedString = worktree.root_name_str().to_owned().into();
    revisions
        .into_iter()
        .filter(|revision| {
            validate_rel(&revision.rel_path).is_ok_and(|rel| {
                worktree.entry_for_path(rel).is_none() && !protected(worktree, rel, cx)
            })
        })
        .map(|revision| Deleted {
            worktree_id,
            folder: folder.clone(),
            revision,
        })
        .collect()
}

fn load(project: Entity<Project>, folders: Vec<WorktreeId>, cx: &mut App) -> Task<Vec<Deleted>> {
    cx.spawn(async move |cx| {
        let mut deleted = Vec::new();
        for worktree_id in folders {
            let Some((scan, latest)) = cx.update(|cx| {
                let worktree = project.read(cx).worktree_for_id(worktree_id, cx)?;
                let root = capture::root_key(&worktree.read(cx).abs_path())?;
                let scan = worktree.read(cx).as_local()?.scan_complete();
                Some((scan, capture::latest(cx, root)))
            }) else {
                continue;
            };
            // Before the first scan ends every file would look deleted.
            scan.await;
            let revisions = latest.await.unwrap_or_default();
            deleted.extend(cx.update(|cx| listed(&project, worktree_id, revisions, cx)));
        }
        deleted.sort_by_key(|file| std::cmp::Reverse(file.revision.ts));
        deleted
    })
}

struct Prepared {
    fs: Arc<dyn Fs>,
    root: PathBuf,
    rel: Arc<RelPath>,
    content: Task<anyhow::Result<Option<Vec<u8>>>>,
    max_bytes: u64,
}

/// Every check that needs the app, before anything is read or written.
fn prepare(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    revision: &Revision,
    cx: &mut App,
) -> Result<Prepared, Refusal> {
    let worktree = project
        .read(cx)
        .worktree_for_id(worktree_id, cx)
        .filter(|worktree| {
            let worktree = worktree.read(cx);
            worktree.is_visible() && worktree.root_entry().is_some_and(|entry| entry.is_dir())
        })
        .ok_or(Refusal::FolderGone)?;
    // Not a flag that could be wrong: only a local worktree has a path on this
    // machine, and the folder to write into is taken from it.
    let root = worktree
        .read(cx)
        .as_local()
        .map(|local| local.abs_path().clone())
        .ok_or(Refusal::NotLocal)?;
    if !worktree_trusted(project, worktree_id, cx) {
        return Err(Refusal::Untrusted);
    }
    // The folder is looked up by the caller's id, never by the recorded root.
    if capture::root_key(&root).as_deref() != Some(revision.worktree_root.as_str()) {
        return Err(Refusal::BadPath);
    }
    let rel = validate_rel(&revision.rel_path)?;
    let worktree = worktree.read(cx);
    if protected(worktree, rel, cx) {
        return Err(Refusal::Protected);
    }
    if worktree.entry_for_path(rel).is_some() {
        return Err(Refusal::Exists);
    }
    let max_bytes = LocalHistorySettings::get_global(cx)
        .max_file_size_kb
        .saturating_mul(1024)
        .min(MAX_CONTENT_BYTES);
    Ok(Prepared {
        fs: project.read(cx).fs().clone(),
        root: root.to_path_buf(),
        rel: rel.into(),
        content: capture::content(cx, revision.blob_hash.clone()),
        max_bytes,
    })
}

/// The muted second line of a row; names the folder when several are open.
fn row_detail(when: &str, folder: Option<&str>) -> String {
    match folder {
        Some(folder) => format!("Last revision {when} · {folder}"),
        None => format!("Last revision {when}"),
    }
}

fn io(error: impl fmt::Display) -> Refusal {
    Refusal::Io(error.to_string())
}

/// Creates the file from `bytes` at `root/rel`. Walks the path from the root: no
/// component may be a symbolic link, nothing may exist at the target, and the
/// deepest existing folder must resolve to a place inside the root. Only plain
/// names remain to be created, so the parents stay inside the folder.
// ponytail: a folder component swapped for a symlink between the walk and the write
// is not detected (the file itself is safe, see the write step); that needs a local
// attacker racing the user. The canonical check above is a backstop for it and for a
// root that is not what it seems, not the main defence: the walk is.
async fn write_new(
    fs: &dyn Fs,
    root: PathBuf,
    rel: Arc<RelPath>,
    bytes: Vec<u8>,
) -> Result<(), Refusal> {
    let canonical_root = fs.canonicalize(&root).await.map_err(io)?;
    let names: Vec<&str> = rel.components().collect();
    let mut path = root.clone();
    let mut existing = root.clone();
    for (ix, name) in names.iter().enumerate() {
        path.push(name);
        let Some(metadata) = fs.metadata(&path).await.map_err(io)? else {
            break;
        };
        if ix + 1 == names.len() {
            return Err(if metadata.is_symlink {
                Refusal::Symlink
            } else {
                Refusal::Exists
            });
        }
        if metadata.is_symlink {
            return Err(Refusal::Symlink);
        }
        if !metadata.is_dir {
            return Err(io("a file is in the way of a folder"));
        }
        existing = path.clone();
    }
    if !fs
        .canonicalize(&existing)
        .await
        .map_err(io)?
        .starts_with(&canonical_root)
    {
        return Err(Refusal::Symlink);
    }
    let target = root.join(rel.as_std_path());
    if let Some(parent) = target.parent()
        && parent != existing
    {
        fs.create_dir(parent).await.map_err(io)?;
    }
    // Recorded text is always UTF-8; anything else is not a revision of ours.
    let text = String::from_utf8(bytes).map_err(io)?;
    // `Fs` cannot write through the handle that creates the file, so two steps. The
    // first is exclusive (`O_EXCL`): it fails if anything is there, a dangling symlink
    // included, and never follows a link. The second is a temporary file renamed over
    // the name, which replaces whatever is at the name now, a link swapped in since,
    // and never writes through it (a plain `write` reopens by path and would).
    // Side effect: the file gets the temporary file's private mode (0600).
    fs.create_file(
        &target,
        CreateOptions {
            overwrite: false,
            ignore_if_exists: false,
        },
    )
    .await
    .map_err(io)?;
    if let Err(error) = fs.atomic_write(target.clone(), text).await {
        fs.remove_file(&target, RemoveOptions::default()).await.ok();
        return Err(io(error));
    }
    Ok(())
}

/// Writes `revision` back as a new file in the folder. Never overwrites.
fn restore(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    revision: &Revision,
    cx: &mut App,
) -> Task<Result<(), Refusal>> {
    let Prepared {
        fs,
        root,
        rel,
        content,
        max_bytes,
    } = match prepare(project, worktree_id, revision, cx) {
        Ok(prepared) => prepared,
        Err(refusal) => return Task::ready(Err(refusal)),
    };
    cx.background_spawn(async move {
        let bytes = content.await.map_err(io)?.ok_or(Refusal::Pruned)?;
        if bytes.len() as u64 > max_bytes {
            return Err(Refusal::TooLarge);
        }
        write_new(fs.as_ref(), root, rel, bytes).await
    })
}

fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().clone();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let eligible = eligible_worktrees(&project, cx);
        let blocked = eligible.as_ref().err().cloned();
        let delegate = RecoverDelegate::new(cx.entity().downgrade(), project.clone(), blocked);
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(32.))
                .show_scrollbar(false)
        });
        let load = match eligible {
            Ok(folders) => {
                let several = folders.len() > 1;
                let found = load(project, folders, cx);
                let picker = picker.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let deleted = found.await;
                    picker
                        .update_in(cx, |picker, window, cx| {
                            picker.delegate.loading = false;
                            picker.delegate.several_folders = several;
                            picker.delegate.entries = deleted;
                            picker.refresh(window, cx);
                        })
                        .ok();
                })
            }
            Err(_) => Task::ready(()),
        };
        RecoverModal {
            picker,
            _load: load,
        }
    });
}

struct RecoverModal {
    picker: Entity<Picker<RecoverDelegate>>,
    _load: Task<()>,
}

impl ModalView for RecoverModal {}
impl EventEmitter<DismissEvent> for RecoverModal {}

impl Focusable for RecoverModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RecoverModal {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("zenith-recover-modal")
            .role(Role::Dialog)
            .aria_label("Recover deleted files")
            .child(self.picker.clone())
    }
}

struct RecoverDelegate {
    modal: WeakEntity<RecoverModal>,
    project: Entity<Project>,
    entries: Vec<Deleted>,
    matches: Vec<StringMatch>,
    selected: usize,
    loading: bool,
    /// More than one folder is open, so a row names its folder.
    several_folders: bool,
    blocked: Option<Refusal>,
    /// What the last restore did or why it did not; read out by screen readers.
    status: Option<SharedString>,
    busy: bool,
}

impl RecoverDelegate {
    fn new(
        modal: WeakEntity<RecoverModal>,
        project: Entity<Project>,
        blocked: Option<Refusal>,
    ) -> Self {
        Self {
            modal,
            project,
            entries: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            loading: blocked.is_none(),
            several_folders: false,
            blocked,
            status: None,
            busy: false,
        }
    }

    fn selected_file(&self) -> Option<&Deleted> {
        self.entries
            .get(self.matches.get(self.selected)?.candidate_id)
    }

    fn footer_text(&self) -> SharedString {
        if let Some(status) = &self.status {
            return status.clone();
        }
        match &self.blocked {
            Some(refusal) => refusal.to_string().into(),
            None if self.loading => "Looking for deleted files…".into(),
            None if self.entries.is_empty() => "No deleted files to restore.".into(),
            None => "Enter restores the selected file. Escape closes.".into(),
        }
    }
}

impl PickerDelegate for RecoverDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "zenith recover deleted files"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search deleted files".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim();
        self.matches = if query.is_empty() {
            (0..self.entries.len())
                .map(|ix| StringMatch {
                    candidate_id: ix,
                    score: 0.,
                    positions: Vec::new(),
                    string: self.entries[ix].revision.rel_path.clone().into(),
                })
                .collect()
        } else {
            let candidates: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .map(|(ix, file)| StringMatchCandidate::new(ix, &file.revision.rel_path))
                .collect();
            match_strings(&candidates, query, StringMatchOptions::default())
        };
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.busy {
            self.status = Some("Still restoring. Wait for it to finish.".into());
            return cx.notify();
        }
        if let Some(refusal) = &self.blocked {
            self.status = Some(refusal.to_string().into());
            return cx.notify();
        }
        let Some(file) = self.selected_file().cloned() else {
            self.status = Some("There is no file to restore.".into());
            return cx.notify();
        };
        let path = file.revision.rel_path.clone();
        self.busy = true;
        self.status = Some(format!("Restoring {path}…").into());
        cx.notify();
        let task = restore(&self.project, file.worktree_id, &file.revision, cx);
        let picker = cx.entity().downgrade();
        // Not the picker's own task: closing the modal must not cut a write in half.
        window
            .spawn(cx, async move |cx| {
                let result = task.await;
                picker
                    .update_in(cx, |picker, window, cx| {
                        let delegate = &mut picker.delegate;
                        delegate.busy = false;
                        delegate.status = Some(match result {
                            Ok(()) => {
                                delegate.entries.retain(|entry| {
                                    entry.worktree_id != file.worktree_id
                                        || entry.revision.rel_path != path
                                });
                                format!("Restored {path}.").into()
                            }
                            Err(refusal) => format!("{path} was not restored. {refusal}").into(),
                        });
                        picker.refresh(window, cx);
                    })
                    .ok();
            })
            .detach();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(match (&self.blocked, self.loading) {
            (Some(refusal), _) => refusal.to_string().into(),
            (None, true) => "Looking for deleted files…".into(),
            (None, false) => "No deleted files have local history.".into(),
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        let file = self.entries.get(found.candidate_id)?;
        let when = format_time(file.revision.ts);
        let path = file.revision.rel_path.clone();
        let detail = row_detail(&when, self.several_folders.then_some(&*file.folder));
        Some(
            ListItem::new(("zenith-recover-row", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .aria_role(Role::ListItem)
                .aria_label(format!("{path}, {detail}"))
                .when(selected, |item| item.aria_active_descendant())
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .child(HighlightedLabel::new(path, found.positions.clone()).truncate())
                        .child(
                            Label::new(detail)
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                ),
        )
    }

    fn render_footer(
        &self,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        let text = self.footer_text();
        Some(
            h_flex()
                .id("zenith-recover-status")
                // Always present, so a screen reader sees the text change when it does.
                .role(Role::Status)
                .aria_label(text.clone())
                .w_full()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Label::new(text).color(Color::Muted).size(LabelSize::Small))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
