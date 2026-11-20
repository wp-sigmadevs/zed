//! One tab that shows a Markdown file as editor (left) and rendered preview
//! (right), like PhpStorm's "Editor and Preview". The tab is the editor's: every
//! `Item` method answers for the editor, the way `SplittableEditor` does
//! (`crates/editor/src/split.rs`). The preview is the stock `MarkdownPreviewView`
//! bound to the same editor, so edits and cursor moves reach it unchanged.

use std::{any::Any, any::TypeId, sync::Arc};

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, AnyEntity, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Font,
    InteractiveElement, IntoElement, Render, ScrollEvent, Subscription, Task, Window, point, px,
};
use language::{Capability, HighlightedText};
use markdown_preview::markdown_preview_view::MarkdownPreviewView;
use project::{Project, ProjectPath};
use ui::{SharedString, prelude::*};
use workspace::{
    ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{Item, ItemBufferKind, ItemEvent, SaveOptions, TabContentParams},
    searchable::SearchableItemHandle,
};

/// Two scroll positions closer than this (as a fraction of the scroll range)
/// count as the same, which is what stops the editor and preview from chasing
/// each other.
const SCROLL_EPSILON: f64 = 1e-3;

pub struct MarkdownSplitView {
    editor: Entity<Editor>,
    preview: Entity<MarkdownPreviewView>,
    // Where both panes were last put, as a fraction of their scroll range.
    synced: f64,
    // The workspace only watches the tab, so the editor's events (edits, dirty
    // state, title) are re-emitted on it.
    _forward_editor_events: Subscription,
}

impl MarkdownSplitView {
    pub fn new(
        editor: Entity<Editor>,
        preview: Entity<MarkdownPreviewView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _forward_editor_events = cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::ScrollPositionChanged { .. } = event {
                this.follow_editor(cx);
            }
            cx.emit(event.clone())
        });
        Self {
            editor,
            preview,
            synced: 0.,
            _forward_editor_events,
        }
    }

    /// Editor scrolled: put the preview at the same fraction of its own range.
    ///
    /// ponytail: proportional, not block-to-block. The preview's rendered block
    /// positions are private to `MarkdownElement`; its autoscroll-to-source-index
    /// only scrolls a position into view and gives nothing to read back. Upgrade
    /// to source-index mapping if drift on image-heavy documents matters.
    pub(crate) fn follow_editor(&mut self, cx: &mut Context<Self>) {
        let Some(fraction) = editor_scroll_fraction(&self.editor, cx) else {
            return;
        };
        if (fraction - self.synced).abs() < SCROLL_EPSILON {
            return;
        }
        self.synced = fraction;
        let scroll = self.preview.read(cx).scroll_handle.clone();
        let offset = scroll.offset();
        scroll.set_offset(point(offset.x, -(scroll.max_offset().y * fraction as f32)));
        self.preview.update(cx, |_, cx| cx.notify());
    }

    /// Preview scrolled by the user: put the editor at the same fraction.
    fn follow_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scroll = self.preview.read(cx).scroll_handle.clone();
        let max = scroll.max_offset().y;
        if max <= px(0.) {
            return;
        }
        let fraction = f64::from(-scroll.offset().y / max).clamp(0., 1.);
        // Remembered first: the editor reports the move back as its own scroll,
        // which `follow_editor` then recognises as already in place.
        self.synced = fraction;
        self.editor.update(cx, |editor, cx| {
            if let Some((_, max_top)) = scroll_extent(editor, cx) {
                let x = editor.scroll_position(cx).x;
                editor.set_scroll_position(point(x, fraction * max_top), window, cx);
            }
        });
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn preview(&self) -> &Entity<MarkdownPreviewView> {
        &self.preview
    }
}

/// The editor's top visible display row, and the largest value it can have.
/// `None` until the editor has been laid out once.
pub(crate) fn scroll_extent(editor: &mut Editor, cx: &mut Context<Editor>) -> Option<(f64, f64)> {
    let visible_lines = editor.visible_line_count()?;
    let last_row = f64::from(editor.max_point(cx).row().0);
    let max_top = (last_row + 1. - visible_lines).max(0.);
    Some((editor.scroll_position(cx).y, max_top))
}

pub(crate) fn editor_scroll_fraction(editor: &Entity<Editor>, cx: &mut App) -> Option<f64> {
    editor.update(cx, |editor, cx| {
        let (top, max_top) = scroll_extent(editor, cx)?;
        Some(if max_top > 0. {
            (top / max_top).clamp(0., 1.)
        } else {
            0.
        })
    })
}

impl EventEmitter<EditorEvent> for MarkdownSplitView {}

impl Focusable for MarkdownSplitView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for MarkdownSplitView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ponytail: fixed 50/50 divider; add a drag handle when asked for.
        h_flex()
            .key_context("ZenithMarkdownSplit")
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.editor.clone()),
            )
            .child(
                div()
                    .w_px()
                    .h_full()
                    .flex_none()
                    .bg(cx.theme().colors().border),
            )
            .child(
                // The preview scrolls itself first (bubble order), so the offset
                // read here is already the new one.
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .on_scroll(cx.listener(|this, _: &ScrollEvent, window, cx| {
                        this.follow_preview(window, cx)
                    }))
                    .child(self.preview.clone()),
            )
    }
}

impl Item for MarkdownSplitView {
    type Event = EditorEvent;

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        Item::tab_content_text(self.editor.read(cx), detail, cx)
    }

    fn suggested_filename(&self, cx: &App) -> SharedString {
        Item::suggested_filename(self.editor.read(cx), cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Item::tab_tooltip_text(self.editor.read(cx), cx)
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon> {
        Item::tab_icon(self.editor.read(cx), window, cx)
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement {
        Item::tab_content(self.editor.read(cx), params, window, cx)
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        <Editor as Item>::to_item_events(event, f)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        Item::for_each_project_item(self.editor.read(cx), cx, f)
    }

    fn buffer_kind(&self, cx: &App) -> ItemBufferKind {
        Item::buffer_kind(self.editor.read(cx), cx)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        Item::active_project_path(self.editor.read(cx), cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        Item::is_dirty(self.editor.read(cx), cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        Item::has_conflict(self.editor.read(cx), cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        Item::has_deleted_file(self.editor.read(cx), cx)
    }

    fn capability(&self, cx: &App) -> Capability {
        Item::capability(self.editor.read(cx), cx)
    }

    fn read_only_toggle_buffer(&self, cx: &App) -> Option<Entity<language::Buffer>> {
        Item::read_only_toggle_buffer(self.editor.read(cx), cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        Item::can_save(self.editor.read(cx), cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        Item::can_save_as(self.editor.read(cx), cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.update(cx, |editor, cx| {
            Item::save(editor, options, project, window, cx)
        })
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.update(cx, |editor, cx| {
            Item::save_as(editor, project, path, window, cx)
        })
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor
            .update(cx, |editor, cx| Item::reload(editor, project, window, cx))
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| Item::navigate(editor, data, window, cx))
    }

    // History entries are recorded by the editor, so they must name this tab.
    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            Item::set_nav_history(editor, history, window, cx)
        });
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| Item::deactivated(editor, window, cx));
    }

    fn workspace_deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            Item::workspace_deactivated(editor, window, cx)
        });
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            Item::added_to_workspace(editor, workspace, window, cx)
        });
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Item::as_searchable(self.editor.read(cx), &self.editor, cx)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        Item::breadcrumb_location(self.editor.read(cx), cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Item::breadcrumbs(self.editor.read(cx), cx)
    }

    fn breadcrumb_prefix(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.editor
            .update(cx, |editor, cx| Item::breadcrumb_prefix(editor, window, cx))
    }

    fn pixel_position_of_cursor(&self, cx: &App) -> Option<gpui::Point<gpui::Pixels>> {
        Item::pixel_position_of_cursor(self.editor.read(cx), cx)
    }

    fn preserve_preview(&self, cx: &App) -> bool {
        Item::preserve_preview(self.editor.read(cx), cx)
    }

    // The tab answers to `Editor` (and `MultiBuffer`) like the editor does, so
    // the toolbar, search bar and `workspace.active_item_as::<Editor>` keep working.
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            Item::act_as_type(self.editor.read(cx), type_id, &self.editor, cx)
        }
    }
}
