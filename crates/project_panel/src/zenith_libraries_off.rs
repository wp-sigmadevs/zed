//! `zenith_libraries` when the `external_libraries` feature is off: every hook leaves the
//! panel as upstream draws it.

use super::*;

impl ProjectPanel {
    pub(super) fn zenith_section(
        &mut self,
        _entry: Option<ProjectEntryId>,
        _expand: Option<bool>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    pub(super) fn zenith_libraries(&mut self, _cx: &mut Context<Self>) {}

    pub(super) fn zenith_depth(&self, _entry: &Entry, depth: usize, _cx: &App) -> usize {
        depth
    }

    pub(super) fn zenith_section_menu(
        &mut self,
        _placement: ContextMenuPlacement,
        _entry_id: ProjectEntryId,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    pub(super) fn zenith_decorate(
        &self,
        _entry: &Entry,
        _worktree_id: WorktreeId,
        filename: String,
        icon: Option<SharedString>,
        depth: usize,
        _cx: &App,
    ) -> (String, Option<SharedString>, usize) {
        (filename, icon, depth)
    }

    pub(super) fn zenith_dnd(
        &self,
        settings: ProjectPanelSettings,
        _entry_id: ProjectEntryId,
        _details: &EntryDetails,
        _cx: &App,
    ) -> ProjectPanelSettings {
        settings
    }

    pub(super) fn zenith_menu(
        &self,
        _worktree_id: WorktreeId,
        _entry_id: ProjectEntryId,
        menu: Entity<ContextMenu>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        menu
    }
}
