//! The scratch store as an invisible worktree of every local project, so the project panel can
//! list it under "Scratches and Consoles". An invisible worktree stays out of project search,
//! the file finder, trust and the window's root paths, and it is not scanned for git
//! repositories.

use super::*;

/// Whether the project panel node is on: the Scratches feature and its own setting.
fn node_enabled(cx: &App) -> bool {
    ScratchesSettings::try_get(cx).is_some_and(|s| s.enabled && s.project_panel_node)
}

/// Whether `worktree` is the scratch store held open for the project panel. A visible worktree
/// is the project's own folder (the store setting may point inside one), never the store.
pub fn is_store_worktree(worktree: &Worktree, cx: &App) -> bool {
    !worktree.is_visible()
        && node_enabled(cx)
        && worktree.abs_path().as_ref() == store_dir(cx).as_path()
}

/// The store's worktree in `project`, while the project panel node is on. Local projects only:
/// the store is a folder on this machine.
pub fn store_worktree(project: &Project, cx: &App) -> Option<Entity<Worktree>> {
    project
        .is_local()
        .then(|| {
            project
                .worktrees(cx)
                .find(|worktree| is_store_worktree(worktree.read(cx), cx))
        })
        .flatten()
}

impl ScratchesPanel {
    /// Opens the store as an invisible worktree of the project, or lets it go when the node is
    /// off or the project is not local. Safe to call on every settings change.
    pub(crate) fn sync_store_worktree(&mut self, cx: &mut Context<Self>) {
        if !node_enabled(cx) || !self.project.read(cx).is_local() {
            self.store_worktree = None;
            self.store_worktree_dir = None;
            self.store_worktree_task = Task::ready(());
            return;
        }
        if self.store_worktree_dir.as_ref() == Some(&self.dir) {
            return;
        }
        self.store_worktree = None;
        self.store_worktree_dir = Some(self.dir.clone());
        let (project, fs, dir) = (self.project.clone(), self.fs.clone(), self.dir.clone());
        self.store_worktree_task = cx.spawn(async move |this, cx| {
            fs.create_dir(&dir).await.log_err();
            let open = project.update(cx, |project, cx| {
                project.find_or_create_worktree(&dir, false, cx)
            });
            let Some((worktree, _)) = open.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                // `find_or_create_worktree` hands back a visible worktree that already contains
                // the folder; that one is the project's and not ours to hold.
                if this.store_worktree_dir.as_ref() == Some(&dir) && !worktree.read(cx).is_visible()
                {
                    this.store_worktree = Some(worktree);
                }
            })
            .ok();
        });
    }
}
