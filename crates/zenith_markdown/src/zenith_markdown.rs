//! PhpStorm's three-button Markdown switch at the right end of the tab bar:
//! Editor only, Editor and Preview (split), Preview only. It sits on every
//! centre pane, shown while the active item is a Markdown file or the preview
//! of one, and only when the Zenith rails are active. The same three layouts
//! are the actions below.
//!
//! The three layouts all live in ONE tab of ONE pane: the editor, the preview,
//! or a `MarkdownSplitView` holding both side by side. A click swaps the tab's
//! item in place.

mod split_view;

use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, Subscription, WeakEntity, Window,
    actions,
};
use markdown_preview::{CloseAndReturnToEditor, markdown_preview_view::MarkdownPreviewView};
use settings::SettingsStore;
pub use split_view::MarkdownSplitView;
use ui::{Tooltip, prelude::*};
use workspace::{
    ItemHandle, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    dock::RailsActive,
};

actions!(
    zenith_markdown,
    [
        /// Shows the Markdown file as source only.
        ShowEditorOnly,
        /// Shows the Markdown source and its preview side by side.
        ShowEditorAndPreview,
        /// Shows only the preview of the Markdown file.
        ShowPreviewOnly,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        workspace.register_action(|workspace, _: &ShowEditorOnly, window, cx| {
            switch_active(workspace, Mode::Editor, window, cx)
        });
        workspace.register_action(|workspace, _: &ShowEditorAndPreview, window, cx| {
            switch_active(workspace, Mode::Split, window, cx)
        });
        workspace.register_action(|workspace, _: &ShowPreviewOnly, window, cx| {
            switch_active(workspace, Mode::Preview, window, cx)
        });
        for pane in workspace.panes().to_vec() {
            attach(&pane, window, cx);
        }
        cx.subscribe_in(&cx.entity(), window, |_, _, event, window, cx| {
            if let workspace::Event::PaneAdded(pane) = event {
                attach(pane, window, cx);
            }
        })
        .detach();
    })
    .detach();
}

fn attach(pane: &Entity<Pane>, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace = cx.weak_entity();
    let weak_pane = pane.downgrade();
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let switcher = cx.new(|cx| MarkdownViewSwitcher::new(workspace, weak_pane, cx));
            zenith_rails::tab_bar_end(
                switcher.clone(),
                0,
                |pane, cx| {
                    pane.active_item()
                        .is_some_and(|item| markdown_editor(item.as_ref(), cx).is_some())
                },
                cx,
            );
            toolbar.add_item(switcher, window, cx);
            let spacer = cx.new(PreviewToolbarSpacer::new);
            toolbar.add_item(spacer, window, cx);
        });
    });
}

/// Runs on the workspace, so the switch itself waits until this update has finished.
fn switch_active(
    workspace: &Workspace,
    mode: Mode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let pane = workspace.active_pane().clone();
    let Some(item) = pane.read(cx).active_item() else {
        return;
    };
    let target = Target {
        workspace: cx.entity(),
        pane,
        item,
    };
    window.defer(cx, move |window, cx| target.switch(mode, window, cx));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Editor,
    Split,
    Preview,
}

pub struct MarkdownViewSwitcher {
    workspace: WeakEntity<Workspace>,
    pane: WeakEntity<Pane>,
    active_item: Option<Box<dyn ItemHandle>>,
    location: ToolbarItemLocation,
    // Whether the active item is a Markdown file; what the tab bar's visibility rides on.
    markdown: bool,
    _settings_subscription: Subscription,
    // The language of a freshly opened buffer can arrive after the item is
    // activated, so the editor is watched until the file is known to be Markdown.
    _editor_subscription: Option<Subscription>,
}

impl EventEmitter<ToolbarItemEvent> for MarkdownViewSwitcher {}

fn markdown_editor(item: &dyn ItemHandle, cx: &App) -> Option<Entity<Editor>> {
    item.act_as::<Editor>(cx)
        .filter(|editor| MarkdownPreviewView::is_markdown_file(editor, cx))
}

fn is_preview(item: &dyn ItemHandle) -> bool {
    item.downcast::<MarkdownPreviewView>().is_some()
}

/// What the buttons act on: the source editor and how the file is shown now.
struct Layout {
    editor: Entity<Editor>,
    mode: Mode,
    split: Option<Entity<MarkdownSplitView>>,
}

impl Layout {
    fn new(item: &dyn ItemHandle, cx: &App) -> Option<Self> {
        let editor = markdown_editor(item, cx)?;
        let split = item.downcast::<MarkdownSplitView>();
        let mode = if split.is_some() {
            Mode::Split
        } else if is_preview(item) {
            Mode::Preview
        } else {
            Mode::Editor
        };
        Some(Self {
            editor,
            mode,
            split,
        })
    }
}

/// Everything a button click needs, owned so no switcher entity is leased while
/// the workspace changes (the toolbar updates the switcher on every such change).
#[derive(Clone)]
struct Target {
    workspace: Entity<Workspace>,
    pane: Entity<Pane>,
    item: Box<dyn ItemHandle>,
}

impl Target {
    fn mode(&self, cx: &App) -> Option<Mode> {
        Some(Layout::new(self.item.as_ref(), cx)?.mode)
    }

    fn switch(&self, target: Mode, window: &mut Window, cx: &mut App) {
        let Self {
            workspace,
            pane,
            item,
        } = self.clone();
        workspace.update(cx, |workspace, cx| {
            let Some(Layout {
                editor,
                mode,
                split,
            }) = Layout::new(item.as_ref(), cx)
            else {
                return;
            };
            match (mode, target) {
                (Mode::Editor | Mode::Preview, Mode::Split) => {
                    enter_split(workspace, &pane, item.as_ref(), editor, window, cx);
                }
                (Mode::Editor, Mode::Preview) => {
                    open_preview_only(workspace, editor, pane, window, cx);
                }
                (Mode::Preview, Mode::Editor) => {
                    if let Some(preview) = item.downcast::<MarkdownPreviewView>() {
                        return_to_editor(&preview, window, cx);
                    }
                }
                (Mode::Split, Mode::Editor | Mode::Preview) => {
                    let Some(split) = split else {
                        return;
                    };
                    let preview = leave_split(&pane, item.as_ref(), &split, window, cx);
                    if target == Mode::Editor {
                        return_to_editor(&preview, window, cx);
                    }
                }
                (Mode::Editor, Mode::Editor)
                | (Mode::Split, Mode::Split)
                | (Mode::Preview, Mode::Preview) => {}
            }
        });
    }
}

/// Puts `new` where `old` is in `pane`, keeping its tab position and pin.
fn swap_item(
    pane: &Entity<Pane>,
    old: &dyn ItemHandle,
    new: Box<dyn ItemHandle>,
    window: &mut Window,
    cx: &mut App,
) {
    pane.update(cx, |pane, cx| {
        let Some(index) = pane.index_for_item(old) else {
            return;
        };
        let pinned = index < pane.pinned_count();
        pane.remove_item(old.item_id(), false, false, window, cx);
        // The new item has the old one's project entry, so `Pane::add_item` would
        // otherwise treat the removed tab as a duplicate to merge; it is already gone.
        pane.add_item(new, true, true, Some(index), window, cx);
        if pinned {
            pane.set_pinned_count(pane.pinned_count() + 1);
        }
    });
}

/// Replaces the editor (or the preview) tab with one tab showing both.
fn enter_split(
    workspace: &mut Workspace,
    pane: &Entity<Pane>,
    item: &dyn ItemHandle,
    editor: Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let preview = match item.downcast::<MarkdownPreviewView>() {
        Some(preview) => {
            // A dirty editor is kept beside its preview; the split takes it over.
            if let Some(editor_pane) = workspace.pane_for_item_id(editor.entity_id()) {
                editor_pane.update(cx, |pane, cx| {
                    pane.remove_item(editor.entity_id(), false, true, window, cx);
                });
            }
            preview
        }
        None => MarkdownPreviewView::create_markdown_view(workspace, editor.clone(), window, cx),
    };
    let split = cx.new(|cx| MarkdownSplitView::new(editor, preview, cx));
    swap_item(pane, item, Box::new(split), window, cx);
}

/// Replaces the split tab with the preview tab. An editor with unsaved changes
/// stays open beside it (same rule as `open_preview_only`), or its edits would
/// be lost with the split.
fn leave_split(
    pane: &Entity<Pane>,
    item: &dyn ItemHandle,
    split: &Entity<MarkdownSplitView>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<MarkdownPreviewView> {
    let (editor, preview) = {
        let split = split.read(cx);
        (split.editor().clone(), split.preview().clone())
    };
    swap_item(pane, item, Box::new(preview.clone()), window, cx);
    if ItemHandle::is_dirty(&editor, cx) {
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(editor), false, false, None, window, cx);
            if let Some(index) = pane.index_for_item(&preview) {
                pane.activate_item(index, true, true, window, cx);
            }
        });
    }
    preview
}

/// Shows the preview in `pane` and drops the editor, unless it has unsaved
/// changes (same rule as the auto-preview in `MarkdownPreviewView::register`).
fn open_preview_only(
    workspace: &mut Workspace,
    editor: Entity<Editor>,
    pane: Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    MarkdownPreviewView::open_preview_in_pane(workspace, editor.clone(), pane.clone(), window, cx);
    if !ItemHandle::is_dirty(&editor, cx) {
        pane.update(cx, |pane, cx| {
            pane.remove_item(editor.entity_id(), false, false, window, cx);
        });
    }
}

/// The preview swaps itself for the editor (and keeps the auto-preview from
/// reopening). It must already be a tab of a pane.
fn return_to_editor(preview: &Entity<MarkdownPreviewView>, window: &mut Window, cx: &mut App) {
    preview.update(cx, |preview, cx| {
        preview.close_and_return_to_editor(&CloseAndReturnToEditor, window, cx)
    });
}

impl MarkdownViewSwitcher {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        pane: WeakEntity<Pane>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            workspace,
            pane,
            active_item: None,
            location: ToolbarItemLocation::Hidden,
            markdown: false,
            // The tab bar settings decide where the buttons go.
            _settings_subscription: cx
                .observe_global::<SettingsStore>(|this, cx| this.update_location(cx)),
            _editor_subscription: None,
        }
    }

    /// The toolbar row only stands in while the rails cannot draw the tab bar's end.
    fn location_for(item: Option<&dyn ItemHandle>, cx: &App) -> ToolbarItemLocation {
        if cx.has_global::<RailsActive>()
            && !zenith_rails::draws_tab_bar_end(cx)
            && item.is_some_and(|item| markdown_editor(item, cx).is_some())
        {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    /// Re-resolves where the buttons go; redraws only when that or the file kind changed.
    fn update_location(&mut self, cx: &mut Context<Self>) {
        let location = Self::location_for(self.active_item.as_deref(), cx);
        let markdown = self
            .active_item
            .as_deref()
            .is_some_and(|item| markdown_editor(item, cx).is_some());
        if location == self.location && markdown == self.markdown {
            return;
        }
        self.markdown = markdown;
        if location != self.location {
            self.location = location;
            cx.emit(ToolbarItemEvent::ChangeLocation(location));
        }
        cx.notify();
        // The tab bar decides whether to draw the buttons while it draws itself.
        if let Some(pane) = self.pane.upgrade() {
            pane.update(cx, |_, cx| cx.notify());
        }
    }

    fn target(&self) -> Option<Target> {
        Some(Target {
            workspace: self.workspace.upgrade()?,
            pane: self.pane.upgrade()?,
            item: self.active_item.clone()?,
        })
    }

    pub fn mode(&self, cx: &App) -> Option<Mode> {
        self.target()?.mode(cx)
    }
}

impl ToolbarItemView for MarkdownViewSwitcher {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_item = active_pane_item.map(ItemHandle::boxed_clone);
        self._editor_subscription = active_pane_item
            .and_then(|item| item.act_as::<Editor>(cx))
            .map(|editor| cx.observe(&editor, |this, _, cx| this.update_location(cx)));
        self.location = Self::location_for(active_pane_item, cx);
        self.markdown = active_pane_item.is_some_and(|item| markdown_editor(item, cx).is_some());
        cx.notify();
        self.location
    }
}

impl Render for MarkdownViewSwitcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (Some(target), Some(current)) = (self.target(), self.mode(cx)) else {
            return h_flex().into_any_element();
        };
        let button = |id: &'static str, icon: IconName, label: &'static str, mode: Mode| {
            let target = target.clone();
            let button = IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(current == mode)
                .aria_label(label)
                .tooltip(Tooltip::text(label))
                .on_click(move |_, window, cx| target.switch(mode, window, cx));
            div().debug_selector(move || id.into()).child(button)
        };
        h_flex()
            .debug_selector(|| "zenith-markdown-switcher".into())
            .gap_0p5()
            .child(button(
                "zenith-markdown-editor",
                IconName::Code,
                "Editor Only",
                Mode::Editor,
            ))
            .child(button(
                "zenith-markdown-split",
                IconName::ZenithColumns,
                "Editor and Preview",
                Mode::Split,
            ))
            .child(button(
                "zenith-markdown-preview",
                IconName::Eye,
                "Preview Only",
                Mode::Preview,
            ))
            .into_any_element()
    }
}

/// An empty left-hand toolbar item shown over a preview tab while the buttons sit in the
/// toolbar (the tab bar cannot hold them).
///
/// `Toolbar::render` lays its two groups out with `justify_between`, and a
/// preview has no breadcrumbs, so the right-hand group (our buttons) is the only
/// child and lands at the left edge. Any left item makes the left group claim
/// the free width and pushes the buttons to the right, where they sit over an
/// editor. The clean fix is `.ml_auto()` on the right group in
/// `crates/workspace/src/toolbar.rs`; this keeps the change out of upstream.
pub struct PreviewToolbarSpacer {
    active_item: Option<Box<dyn ItemHandle>>,
    location: ToolbarItemLocation,
    _settings_subscription: Subscription,
}

impl EventEmitter<ToolbarItemEvent> for PreviewToolbarSpacer {}

impl PreviewToolbarSpacer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            active_item: None,
            location: ToolbarItemLocation::Hidden,
            // The tab bar settings decide whether the buttons need the spacer.
            _settings_subscription: cx.observe_global::<SettingsStore>(|this, cx| {
                let location = Self::location_for(this.active_item.as_deref(), cx);
                if location != this.location {
                    this.location = location;
                    cx.emit(ToolbarItemEvent::ChangeLocation(location));
                }
            }),
        }
    }

    fn location_for(item: Option<&dyn ItemHandle>, cx: &App) -> ToolbarItemLocation {
        let over_preview = cx.has_global::<RailsActive>()
            && !zenith_rails::draws_tab_bar_end(cx)
            && item.is_some_and(|item| is_preview(item) && markdown_editor(item, cx).is_some());
        if over_preview {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl ToolbarItemView for PreviewToolbarSpacer {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_item = active_pane_item.map(ItemHandle::boxed_clone);
        self.location = Self::location_for(active_pane_item, cx);
        self.location
    }
}

impl Render for PreviewToolbarSpacer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::EditorEvent;
    use fs::FakeFs;
    use gpui::{
        Action as _, Bounds, Modifiers, Pixels, ScrollDelta, ScrollEvent, VisualTestContext, point,
    };
    use gpui::{TestAppContext, WindowHandle};
    use project::{Project, ProjectPath};
    use serde_json::json;
    use std::{cell::Cell, rc::Rc, sync::Arc};
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    struct Fixture {
        window: WindowHandle<MultiWorkspace>,
        workspace: Entity<Workspace>,
        project: Entity<Project>,
        pane: Entity<Pane>,
        switcher: Entity<MarkdownViewSwitcher>,
        spacer: Entity<PreviewToolbarSpacer>,
    }

    async fn fixture(cx: &mut TestAppContext) -> Fixture {
        fixture_with_rails(cx, true).await
    }

    async fn fixture_with_rails(cx: &mut TestAppContext, rails: bool) -> Fixture {
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            AppState::test(cx);
            editor::init(cx);
            markdown_preview::init(cx);
            if rails {
                zenith_rails::init(cx);
            }
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "a.rs": "fn main() {}",
                "b.md": "# Title\n",
                "long.md": (0..300).map(|i| format!("Paragraph {i}\n\n")).collect::<String>(),
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(language::Language::new(
                language::LanguageConfig {
                    name: "Markdown".into(),
                    matcher: Arc::new(language::LanguageMatcher {
                        path_suffixes: vec!["md".into()],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )))
        });
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .update(cx, |multi, _, _| multi.workspace().clone())
            .unwrap();
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let (switcher, spacer) = pane.read_with(cx, |pane, cx| {
            let toolbar = pane.toolbar().read(cx);
            (toolbar.item_of_type(), toolbar.item_of_type())
        });
        Fixture {
            window,
            workspace,
            project,
            pane,
            switcher: switcher.expect("init() adds the switcher to every pane's toolbar"),
            spacer: spacer.expect("init() adds the spacer to every pane's toolbar"),
        }
    }

    impl Fixture {
        async fn open(&self, path: &str, cx: &mut TestAppContext) {
            let project_path = self.project.read_with(cx, |project, cx| ProjectPath {
                worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
                path: rel_path(path).into(),
            });
            self.window
                .update(cx, |_, window, cx| {
                    self.workspace.update(cx, |workspace, cx| {
                        workspace.open_path(project_path, None, true, window, cx)
                    })
                })
                .unwrap()
                .await
                .unwrap();
            cx.run_until_parked();
        }

        fn active(&self, cx: &mut TestAppContext) -> Box<dyn ItemHandle> {
            self.pane
                .read_with(cx, |pane, _| pane.active_item())
                .expect("an item is open")
        }

        fn location(&self, cx: &mut TestAppContext) -> ToolbarItemLocation {
            let item = self.pane.read_with(cx, |pane, _| pane.active_item());
            self.window
                .update(cx, |_, window, cx| {
                    self.switcher.update(cx, |switcher, cx| {
                        switcher.set_active_pane_item(item.as_deref(), window, cx)
                    })
                })
                .unwrap()
        }

        /// The drawn bounds of a debug-selected element after a fresh frame.
        fn bounds(
            &self,
            selector: &'static str,
            cx: &mut TestAppContext,
        ) -> Option<Bounds<Pixels>> {
            cx.run_until_parked();
            cx.update(|cx| {
                cx.update_window(self.window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx)
                })
            })
            .unwrap();
            VisualTestContext::from_window(self.window.into(), cx).debug_bounds(selector)
        }

        fn mode(&self, cx: &mut TestAppContext) -> Option<Mode> {
            self.switcher
                .read_with(cx, |switcher, cx| switcher.mode(cx))
        }

        fn dispatch(&self, action: Box<dyn gpui::Action>, cx: &mut TestAppContext) {
            self.window
                .update(cx, |_, window, cx| window.dispatch_action(action, cx))
                .unwrap();
            cx.run_until_parked();
        }

        fn activate(&self, ix: usize, cx: &mut TestAppContext) {
            self.window
                .update(cx, |_, window, cx| {
                    self.pane.update(cx, |pane, cx| {
                        pane.activate_item(ix, true, true, window, cx)
                    })
                })
                .unwrap();
            cx.run_until_parked();
        }

        fn click(&self, mode: Mode, cx: &mut TestAppContext) {
            let target = self
                .switcher
                .read_with(cx, |switcher, _| switcher.target())
                .unwrap();
            self.window
                .update(cx, |_, window, cx| target.switch(mode, window, cx))
                .unwrap();
            cx.run_until_parked();
        }

        /// What each tab of each pane of the workspace is, in pane and tab order.
        fn tabs(&self, cx: &mut TestAppContext) -> Vec<Vec<&'static str>> {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace
                    .panes()
                    .iter()
                    .map(|pane| {
                        pane.read(cx)
                            .items()
                            .map(|item| {
                                if item.downcast::<MarkdownSplitView>().is_some() {
                                    "split"
                                } else if is_preview(item.as_ref()) {
                                    "preview"
                                } else {
                                    "editor"
                                }
                            })
                            .collect()
                    })
                    .collect()
            })
        }

        fn active_is_preview(&self, cx: &mut TestAppContext) -> bool {
            self.pane.read_with(cx, |pane, _| {
                pane.active_item()
                    .is_some_and(|item| is_preview(item.as_ref()))
            })
        }

        fn active_editor(&self, cx: &mut TestAppContext) -> Entity<Editor> {
            let item = self.active(cx);
            cx.update(|cx| item.act_as::<Editor>(cx))
                .expect("an editor is open")
        }

        fn active_dirty(&self, cx: &mut TestAppContext) -> bool {
            let item = self.active(cx);
            cx.update(|cx| item.is_dirty(cx))
        }

        /// Lays the window out (and lets the preview parse) so scroll ranges exist.
        fn draw(&self, cx: &mut TestAppContext) {
            for _ in 0..3 {
                cx.run_until_parked();
                // Not `WindowHandle::update`: drawing needs the root view unleased.
                cx.update(|cx| {
                    cx.update_window(self.window.into(), |_, window, cx| {
                        window.draw(cx).clear(cx)
                    })
                    .unwrap()
                });
            }
        }

        /// The open split view, laid out and with its scroll ranges known.
        fn open_split(&self, cx: &mut TestAppContext) -> Entity<MarkdownSplitView> {
            cx.executor().allow_parking();
            self.click(Mode::Split, cx);
            self.draw(cx);
            self.active(cx)
                .downcast::<MarkdownSplitView>()
                .expect("a split tab")
        }

        /// How many scroll events the editor has emitted so far.
        fn count_editor_scrolls(
            &self,
            split: &Entity<MarkdownSplitView>,
            cx: &mut TestAppContext,
        ) -> Rc<Cell<usize>> {
            let count = Rc::new(Cell::new(0));
            let editor = split.read_with(cx, |split, _| split.editor().clone());
            cx.update(|cx| {
                let count = count.clone();
                cx.subscribe(&editor, move |_, event: &EditorEvent, _| {
                    if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                        count.set(count.get() + 1);
                    }
                })
                .detach();
            });
            count
        }

        /// Types into the buffer of the active item, leaving it unsaved.
        fn edit(&self, cx: &mut TestAppContext) {
            let editor = self.active_editor(cx);
            editor.update(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx).as_singleton().unwrap();
                buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "x")], None, cx));
            });
        }
    }

    #[gpui::test]
    async fn buttons_follow_the_active_tab_into_the_tab_bar_end(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("a.rs", cx).await;
        assert_eq!(fixture.location(cx), ToolbarItemLocation::Hidden);
        assert_eq!(fixture.mode(cx), None);
        assert!(fixture.bounds("zenith-markdown-switcher", cx).is_none());
        assert!(
            fixture.bounds("zenith-tab-bar-end", cx).is_none(),
            "no empty end slot beside a Rust file"
        );
        fixture.open("b.md", cx).await;
        // Never a toolbar row: the buttons live in the tab bar.
        assert_eq!(fixture.location(cx), ToolbarItemLocation::Hidden);
        assert_eq!(fixture.mode(cx), Some(Mode::Editor));
        let end = fixture
            .bounds("zenith-tab-bar-end", cx)
            .expect("tab bar end");
        let buttons = fixture
            .bounds("zenith-markdown-switcher", cx)
            .expect("markdown buttons");
        assert!(end.contains(&buttons.origin) && buttons.right() <= end.right());
        // Back on the Rust tab they are gone again.
        fixture.activate(0, cx);
        assert!(fixture.bounds("zenith-markdown-switcher", cx).is_none());
        assert!(fixture.bounds("zenith-tab-bar-end", cx).is_none());
    }

    fn set_tab_bar(cx: &mut TestAppContext, show: bool, buttons: bool) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    let tab_bar = content.tab_bar.get_or_insert_default();
                    tab_bar.show = Some(show);
                    tab_bar.show_tab_bar_buttons = Some(buttons);
                });
            });
        });
    }

    #[gpui::test]
    async fn buttons_stay_in_the_toolbar_when_the_tab_bar_cannot_hold_them(
        cx: &mut TestAppContext,
    ) {
        for (show, buttons) in [(true, false), (false, true)] {
            let fixture = fixture(cx).await;
            fixture.open("b.md", cx).await;
            assert_eq!(fixture.location(cx), ToolbarItemLocation::Hidden);
            set_tab_bar(cx, show, buttons);
            cx.run_until_parked();
            assert_eq!(
                fixture.location(cx),
                ToolbarItemLocation::PrimaryRight,
                "show {show}, buttons {buttons}"
            );
            // Not in a tab bar end, drawn in the toolbar row.
            assert!(fixture.bounds("zenith-tab-bar-end", cx).is_none());
            assert!(fixture.bounds("zenith-markdown-switcher", cx).is_some());
            set_tab_bar(cx, true, true);
            cx.run_until_parked();
            assert_eq!(fixture.location(cx), ToolbarItemLocation::Hidden);
            assert!(fixture.bounds("zenith-tab-bar-end", cx).is_some());
        }
    }

    #[gpui::test]
    async fn the_spacer_follows_the_tab_bar_settings_without_a_tab_switch(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        fixture.click(Mode::Preview, cx);
        let location = |cx: &mut TestAppContext| fixture.spacer.read_with(cx, |s, _| s.location);
        // The tab bar holds the buttons: no spacer.
        let item = fixture.active(cx);
        fixture
            .window
            .update(cx, |_, window, cx| {
                fixture.spacer.update(cx, |spacer, cx| {
                    spacer.set_active_pane_item(Some(item.as_ref()), window, cx)
                })
            })
            .unwrap();
        assert_eq!(location(cx), ToolbarItemLocation::Hidden);
        set_tab_bar(cx, true, false);
        cx.run_until_parked();
        assert_eq!(location(cx), ToolbarItemLocation::PrimaryLeft);
        set_tab_bar(cx, true, true);
        cx.run_until_parked();
        assert_eq!(location(cx), ToolbarItemLocation::Hidden);
    }

    #[gpui::test]
    async fn switcher_stays_hidden_without_rails(cx: &mut TestAppContext) {
        let fixture = fixture_with_rails(cx, false).await;
        fixture.open("b.md", cx).await;
        assert_eq!(fixture.location(cx), ToolbarItemLocation::Hidden);
        assert!(fixture.bounds("zenith-markdown-switcher", cx).is_none());
    }

    #[gpui::test]
    async fn buttons_stay_in_the_tab_bar_in_every_mode(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        for mode in [Mode::Editor, Mode::Split, Mode::Preview] {
            fixture.click(mode, cx);
            assert_eq!(fixture.mode(cx), Some(mode));
            assert!(
                fixture.bounds("zenith-markdown-switcher", cx).is_some(),
                "{mode:?}"
            );
        }
    }

    #[gpui::test]
    async fn actions_switch_the_layout_of_the_active_tab(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("a.rs", cx).await;
        fixture.open("b.md", cx).await;
        for (action, mode) in [
            (ShowPreviewOnly.boxed_clone(), Mode::Preview),
            (ShowEditorAndPreview.boxed_clone(), Mode::Split),
            (ShowEditorOnly.boxed_clone(), Mode::Editor),
        ] {
            fixture.dispatch(action, cx);
            assert_eq!(fixture.mode(cx), Some(mode));
        }
        // On a Rust tab the actions do nothing.
        fixture.open("a.rs", cx).await;
        fixture.dispatch(ShowPreviewOnly.boxed_clone(), cx);
        assert_eq!(fixture.mode(cx), None);
    }

    #[gpui::test]
    async fn clicking_a_drawn_button_switches_the_layout(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        for (button, mode) in [
            ("zenith-markdown-preview", Mode::Preview),
            ("zenith-markdown-split", Mode::Split),
            ("zenith-markdown-editor", Mode::Editor),
        ] {
            let bounds = fixture.bounds(button, cx).expect("button is drawn");
            VisualTestContext::from_window(fixture.window.into(), cx)
                .simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
            assert_eq!(fixture.mode(cx), Some(mode), "{button}");
        }
    }

    #[gpui::test]
    async fn each_button_shows_its_layout_in_one_tab(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        assert_eq!(fixture.tabs(cx), vec![vec!["editor"]]);

        fixture.click(Mode::Split, cx);
        assert_eq!(fixture.mode(cx), Some(Mode::Split));
        assert_eq!(fixture.tabs(cx), vec![vec!["split"]]);

        fixture.click(Mode::Preview, cx);
        assert_eq!(fixture.mode(cx), Some(Mode::Preview));
        assert_eq!(fixture.tabs(cx), vec![vec!["preview"]]);

        fixture.click(Mode::Editor, cx);
        assert_eq!(fixture.mode(cx), Some(Mode::Editor));
        assert_eq!(fixture.tabs(cx), vec![vec!["editor"]]);
    }

    #[gpui::test]
    async fn preview_to_split_and_split_to_editor(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        fixture.click(Mode::Preview, cx);
        fixture.click(Mode::Split, cx);
        assert_eq!(fixture.mode(cx), Some(Mode::Split));
        assert_eq!(fixture.tabs(cx), vec![vec!["split"]]);

        fixture.click(Mode::Editor, cx);
        assert_eq!(fixture.mode(cx), Some(Mode::Editor));
        assert_eq!(fixture.tabs(cx), vec![vec!["editor"]]);
    }

    #[gpui::test]
    async fn split_tab_is_the_file_and_acts_as_its_editor(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        let editor = fixture.active_editor(cx);
        fixture.click(Mode::Split, cx);

        let split = fixture.active(cx);
        let view = split.downcast::<MarkdownSplitView>().expect("a split tab");
        assert_eq!(cx.update(|cx| split.tab_content_text(0, cx)), "b.md");
        assert_eq!(
            cx.update(|cx| split.project_path(cx)).map(|path| path.path),
            Some(rel_path("b.md").into())
        );
        assert_eq!(fixture.active_editor(cx), editor);
        // The preview shows the same editor, so it follows its edits and cursor.
        assert_eq!(view.read_with(cx, |v, _| v.editor().clone()), editor);
        let preview = view.read_with(cx, |v, _| v.preview().clone());
        let preview: Box<dyn ItemHandle> = Box::new(preview);
        assert_eq!(cx.update(|cx| preview.act_as::<Editor>(cx)), Some(editor));
    }

    #[gpui::test]
    async fn switching_keeps_the_tab_in_place(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        fixture.open("a.rs", cx).await;
        fixture.open("b.md", cx).await;
        assert_eq!(fixture.tabs(cx), vec![vec!["editor", "editor"]]);
        let first = fixture
            .pane
            .read_with(cx, |pane, _| pane.active_item_index());
        assert_eq!(first, 0);

        for (mode, tabs) in [
            (Mode::Split, vec!["split", "editor"]),
            (Mode::Preview, vec!["preview", "editor"]),
            (Mode::Split, vec!["split", "editor"]),
            (Mode::Editor, vec!["editor", "editor"]),
        ] {
            fixture.click(mode, cx);
            assert_eq!(fixture.tabs(cx), vec![tabs], "{mode:?}");
            let index = fixture
                .pane
                .read_with(cx, |pane, _| pane.active_item_index());
            assert_eq!(index, 0, "{mode:?}");
        }
    }

    #[gpui::test]
    async fn unsaved_edits_are_not_lost_by_switching(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("b.md", cx).await;
        fixture.edit(cx);

        fixture.click(Mode::Split, cx);
        assert_eq!(fixture.tabs(cx), vec![vec!["split"]]);
        assert!(
            fixture.active_dirty(cx),
            "the split tab shows the dirty dot"
        );

        // The dirty editor stays open beside the preview, as with Preview Only.
        fixture.click(Mode::Preview, cx);
        assert_eq!(fixture.tabs(cx), vec![vec!["preview", "editor"]]);
        assert!(fixture.active_is_preview(cx));

        fixture.click(Mode::Split, cx);
        assert_eq!(fixture.tabs(cx), vec![vec!["split"]]);

        fixture.click(Mode::Editor, cx);
        assert_eq!(fixture.tabs(cx), vec![vec!["editor"]]);
        assert!(fixture.active_dirty(cx));
    }

    #[gpui::test]
    async fn scrolling_the_editor_scrolls_the_preview(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("long.md", cx).await;
        let split = fixture.open_split(cx);
        let (editor, preview) = split.read_with(cx, |split, _| {
            (split.editor().clone(), split.preview().clone())
        });
        let scroll = preview.read_with(cx, |preview, _| preview.scroll_handle.clone());
        assert!(
            scroll.max_offset().y > gpui::px(0.),
            "the preview is laid out"
        );
        assert_eq!(scroll.offset().y, gpui::px(0.));
        let scrolls = fixture.count_editor_scrolls(&split, cx);

        fixture
            .window
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_position(point(0., 100.), window, cx)
                })
            })
            .unwrap();
        fixture.draw(cx);

        let fraction = cx
            .update(|cx| split_view::editor_scroll_fraction(&editor, cx))
            .expect("the editor is laid out");
        assert!(fraction > 0.1 && fraction < 1., "{fraction}");
        let expected = -(scroll.max_offset().y * fraction as f32);
        assert!(
            (scroll.offset().y - expected).abs() < gpui::px(1.),
            "{:?} vs {expected:?}",
            scroll.offset().y
        );
        // One scroll in, no echo back out.
        assert_eq!(scrolls.get(), 1);
    }

    #[gpui::test]
    async fn scrolling_the_preview_scrolls_the_editor(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("long.md", cx).await;
        let split = fixture.open_split(cx);
        let (editor, preview) = split.read_with(cx, |split, _| {
            (split.editor().clone(), split.preview().clone())
        });
        let scroll = preview.read_with(cx, |preview, _| preview.scroll_handle.clone());
        let top_before = editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
        assert_eq!(top_before, 0.);
        let scrolls = fixture.count_editor_scrolls(&split, cx);

        let mut visual = VisualTestContext::from_window(fixture.window.into(), cx);
        visual.simulate_event(ScrollEvent {
            position: scroll.bounds().center(),
            delta: ScrollDelta::Pixels(point(gpui::px(0.), gpui::px(-400.))),
            ..Default::default()
        });
        fixture.draw(cx);

        let offset = scroll.offset().y;
        assert!(offset < gpui::px(0.), "the preview scrolled: {offset:?}");
        let top = editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
        assert!(top > 0., "the editor followed: {top}");
        let fraction = cx
            .update(|cx| split_view::editor_scroll_fraction(&editor, cx))
            .unwrap();
        let preview_fraction = f64::from(-offset / scroll.max_offset().y);
        assert!((fraction - preview_fraction).abs() < 0.01);
        // The editor is told once; its report is not bounced back to the preview.
        assert!(scrolls.get() <= 2, "{} scroll events", scrolls.get());
        assert_eq!(scroll.offset().y, offset);
    }

    #[gpui::test]
    async fn an_unchanged_editor_scroll_leaves_the_preview_alone(cx: &mut TestAppContext) {
        let fixture = fixture(cx).await;
        fixture.open("long.md", cx).await;
        let split = fixture.open_split(cx);
        let (editor, preview) = split.read_with(cx, |split, _| {
            (split.editor().clone(), split.preview().clone())
        });
        let scroll = preview.read_with(cx, |preview, _| preview.scroll_handle.clone());
        fixture
            .window
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_scroll_position(point(0., 100.), window, cx)
                })
            })
            .unwrap();
        fixture.draw(cx);

        // The editor has not moved since it was last followed, so a repeat of its
        // scroll report (the echo of a preview-driven scroll) must not touch the preview.
        let elsewhere = gpui::px(-50.);
        scroll.set_offset(point(gpui::px(0.), elsewhere));
        split.update(cx, |split, cx| split.follow_editor(cx));
        assert_eq!(scroll.offset().y, elsewhere);
    }
}
