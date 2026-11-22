//! `scratches_node` when the `scratches` feature is off: the library hooks see no scratch rows.

use super::*;

pub(super) fn scratch_node_menu(menu: ContextMenu) -> ContextMenu {
    menu
}

impl ProjectPanel {
    pub(super) fn zenith_is_scratch(&self, _worktree_id: WorktreeId, _cx: &App) -> bool {
        false
    }

    pub(super) fn zenith_scratch_rows(&mut self, _cx: &mut Context<Self>) {}

    pub(super) fn zenith_scratch_decorate(
        &self,
        _entry: &Entry,
        _worktree_id: WorktreeId,
        _filename: &str,
        _icon: &Option<SharedString>,
        _depth: usize,
        _cx: &App,
    ) -> Option<(String, Option<SharedString>, usize)> {
        None
    }

    pub(super) fn zenith_scratch_menu(
        &self,
        _worktree_id: WorktreeId,
        _entry_id: ProjectEntryId,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Entity<ContextMenu>> {
        None
    }
}
