//! PhpStorm-style navigation bar at the left of the status bar:
//! `project › folder › … › file › Class › method`. Each segment opens a popup: a folder lists
//! its children (a folder drills in, a file opens), the file lists its sibling files, a symbol
//! lists its sibling symbols and jumps to the chosen one.

mod segments;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, rc::Rc, sync::Arc};

use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use file_icons::FileIcons;
use gpui::{
    Anchor, Bounds, DismissEvent, Entity, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    actions, anchored, deferred,
};
use language::ToOffset as _;
use project::{Project, ProjectPath, WorktreeId};
use ui::{ButtonLike, ContextMenu, ContextMenuEntry, prelude::*};
use util::rel_path::RelPath;
use workspace::{
    HideStatusItem, StatusItemView, Workspace,
    item::{ItemHandle, WeakItemHandle},
};

pub use segments::{
    Segment, SegmentKind, fit, path_segments, sibling_symbols, sort_children, symbol_path,
};

actions!(
    zenith,
    [
        /// Opens the navigation bar's last segment popup (PhpStorm: Jump to Navigation Bar).
        JumpToNavigationBar,
    ]
);

actions!(
    zenith_navbar,
    [
        /// Opens the popup of the segment before the open one.
        SelectPreviousSegment,
        /// Opens the popup of the segment after the open one.
        SelectNextSegment,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let project = workspace.project().clone();
        let bar = cx.new(|_| NavBar::new(project, workspace.weak_handle()));
        workspace
            .status_bar()
            .update(cx, |status_bar, cx| status_bar.add_left_item(bar, window, cx));
        workspace.register_action(|workspace, _: &JumpToNavigationBar, window, cx| {
            if let Some(bar) = workspace.status_bar().read(cx).item_of_type::<NavBar>() {
                bar.update(cx, |bar, cx| bar.jump(window, cx));
            }
        });
    })
    .detach();
}

/// Lists `dir` of a worktree, folders first; files only when `files_only`.
pub fn children(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    dir: &RelPath,
    files_only: bool,
    cx: &App,
) -> Vec<(Arc<RelPath>, bool)> {
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return Vec::new();
    };
    // shortcut: folders the worktree has not scanned yet (ignored ones) list empty; expand them
    // through the project if users miss them.
    let mut children: Vec<_> = worktree
        .read(cx)
        .child_entries(dir)
        .filter(|entry| !files_only || !entry.is_dir())
        .map(|entry| (entry.path.clone(), entry.is_dir()))
        .collect();
    sort_children(&mut children);
    children
}

struct OpenMenu {
    segment: usize,
    menu: Entity<ContextMenu>,
    _dismiss: Subscription,
}

pub struct NavBar {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    item: Option<Box<dyn WeakItemHandle>>,
    menu: Option<OpenMenu>,
    /// Focus to give back when the popup closes from the keyboard.
    restore_focus: Option<FocusHandle>,
    /// Bounds of each rendered slot, written at prepaint, to anchor the popup.
    slot_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    _editor_subscription: Option<Subscription>,
    _symbols_task: Option<Task<()>>,
}

impl NavBar {
    fn new(project: Entity<Project>, workspace: WeakEntity<Workspace>) -> Self {
        Self {
            project,
            workspace,
            item: None,
            menu: None,
            restore_focus: None,
            slot_bounds: Rc::default(),
            _editor_subscription: None,
            _symbols_task: None,
        }
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.item.as_ref()?.upgrade()?.project_path(cx)
    }

    fn editor(&self, cx: &App) -> Option<Entity<Editor>> {
        self.item.as_ref()?.upgrade()?.act_as::<Editor>(cx)
    }

    /// The segments for the active item; empty when it has no project path.
    pub fn segments(&self, cx: &App) -> Vec<Segment> {
        let Some(path) = self.project_path(cx) else {
            return Vec::new();
        };
        let Some(worktree) = self.project.read(cx).worktree_for_id(path.worktree_id, cx) else {
            return Vec::new();
        };
        let worktree = worktree.read(cx);
        let is_dir = worktree
            .entry_for_path(&path.path)
            .is_some_and(|entry| entry.is_dir());
        // The editor's breadcrumbs: the file path, then the symbols around the caret.
        let symbols = self
            .editor(cx)
            .and_then(|editor| ItemHandle::breadcrumbs(&editor, cx))
            .map(|(crumbs, _)| {
                crumbs
                    .into_iter()
                    .skip(1)
                    .map(|crumb| crumb.text.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        path_segments(worktree.root_name_str(), &path.path, is_dir, symbols)
    }

    /// The segment whose popup is open.
    pub fn open_segment(&self) -> Option<usize> {
        self.menu.as_ref().map(|open| open.segment)
    }

    fn jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.segments(cx).len();
        if count > 0 {
            self.open_menu(count - 1, None, window, cx);
        }
    }

    fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open_segment() else {
            return;
        };
        let count = self.segments(cx).len();
        let next = if forward {
            open + 1
        } else {
            open.wrapping_sub(1)
        };
        if next < count {
            self.open_menu(next, None, window, cx);
        }
    }

    /// Opens segment `ix`'s popup; `dir` lists that folder instead (drilled in from a popup).
    fn open_menu(
        &mut self,
        ix: usize,
        dir: Option<Arc<RelPath>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let segments = self.segments(cx);
        let (Some(segment), Some(path)) = (segments.get(ix), self.project_path(cx)) else {
            return;
        };
        let worktree_id = path.worktree_id;
        match segment.kind {
            SegmentKind::Dir => {
                let menu = self.folder_menu(
                    ix,
                    worktree_id,
                    &segment.path,
                    dir.unwrap_or_else(|| segment.path.clone()),
                    false,
                    window,
                    cx,
                );
                self.show_menu(ix, menu, window, cx);
            }
            SegmentKind::File => {
                let parent = segment
                    .path
                    .parent()
                    .map_or_else(RelPath::empty_arc, |parent| parent.into_arc());
                let menu =
                    self.folder_menu(ix, worktree_id, &parent, parent.clone(), true, window, cx);
                self.show_menu(ix, menu, window, cx);
            }
            SegmentKind::Symbol(level) => self.open_symbol_menu(ix, level, window, cx),
        }
    }

    fn folder_menu(
        &self,
        ix: usize,
        worktree_id: WorktreeId,
        segment_dir: &RelPath,
        dir: Arc<RelPath>,
        files_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let children = children(&self.project, worktree_id, &dir, files_only, cx);
        let up = (dir.as_ref() != segment_dir)
            .then(|| dir.parent().map(|parent| parent.into_arc()))
            .flatten();
        let bar = cx.weak_entity();
        let workspace = self.workspace.clone();
        ContextMenu::build(window, cx, move |mut menu, _, cx| {
            if let Some(up) = up {
                menu = menu
                    .header(dir.as_unix_str().to_string())
                    .item(drill_entry("..", up, ix, bar.clone(), cx));
            }
            if children.is_empty() {
                menu = menu.label("Empty");
            }
            for (path, is_dir) in children {
                let name = path.file_name().unwrap_or_default().to_string();
                let entry = if is_dir {
                    drill_entry(&name, path, ix, bar.clone(), cx)
                } else {
                    let workspace = workspace.clone();
                    with_icon(
                        ContextMenuEntry::new(name),
                        FileIcons::get_icon(path.as_std_path(), cx),
                    )
                    .handler(move |window, cx| {
                        let path = ProjectPath {
                            worktree_id,
                            path: path.clone(),
                        };
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace
                                    .open_path(path, None, true, window, cx)
                                    .detach_and_log_err(cx)
                            })
                            .ok();
                    })
                };
                menu = menu.item(entry);
            }
            menu
        })
    }

    fn open_symbol_menu(
        &mut self,
        ix: usize,
        level: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor(cx) else {
            return;
        };
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        let Some(buffer_id) = snapshot.as_singleton().map(|buffer| buffer.remote_id()) else {
            return;
        };
        let outline = editor.update(cx, |editor, cx| editor.buffer_outline_items(buffer_id, cx));
        self._symbols_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outline = outline.await;
            this.update_in(cx, |this, window, cx| {
                let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
                let head = editor.read(cx).selections.newest_anchor().head();
                let Some((buffer, cursor)) = snapshot.point_to_buffer_offset(head) else {
                    return;
                };
                let spans: Vec<_> = outline
                    .iter()
                    .map(|item| {
                        (
                            item.depth,
                            item.range.start.to_offset(buffer)..item.range.end.to_offset(buffer),
                        )
                    })
                    .collect();
                let path = symbol_path(&spans, &cursor.0);
                // Breadcrumbs may come from another source than the outline; use its last level.
                let level = level.min(path.len().saturating_sub(1));
                let symbols: Vec<_> = sibling_symbols(&spans, &path, level)
                    .into_iter()
                    .filter_map(|ix| {
                        let item = &outline[ix];
                        Some((
                            item.text.clone(),
                            snapshot.anchor_in_buffer(item.selection_range.start)?,
                        ))
                    })
                    .collect();
                let weak_editor = editor.downgrade();
                let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if symbols.is_empty() {
                        menu = menu.label("No symbols");
                    }
                    for (text, anchor) in symbols {
                        let editor = weak_editor.clone();
                        menu = menu.entry(text, None, move |window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.change_selections(
                                        SelectionEffects::scroll(Autoscroll::center()),
                                        window,
                                        cx,
                                        |selections| selections.select_anchor_ranges([anchor..anchor]),
                                    );
                                    window.focus(&editor.focus_handle(cx), cx);
                                })
                                .ok();
                        });
                    }
                    menu
                });
                this.show_menu(ix, menu, window, cx);
            })
            .ok();
        }));
    }

    fn show_menu(
        &mut self,
        ix: usize,
        menu: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu.is_none() {
            self.restore_focus = window.focused(cx);
        }
        let dismiss = cx.subscribe_in(&menu, window, |this, menu, _: &DismissEvent, window, cx| {
            if this.menu.as_ref().is_some_and(|open| open.menu == *menu) {
                let had_focus = menu.focus_handle(cx).contains_focused(window, cx);
                this.menu = None;
                // Closed by a click elsewhere: leave focus where the user put it.
                if (had_focus || window.focused(cx).is_none())
                    && let Some(focus) = this.restore_focus.take()
                {
                    window.focus(&focus, cx);
                }
                cx.notify();
            }
        });
        window.focus(&menu.focus_handle(cx), cx);
        menu.update(cx, |menu, cx| menu.select_first(&menu::SelectFirst, window, cx));
        self.menu = Some(OpenMenu {
            segment: ix,
            menu,
            _dismiss: dismiss,
        });
        cx.notify();
    }
}

fn with_icon(entry: ContextMenuEntry, icon: Option<SharedString>) -> ContextMenuEntry {
    match icon {
        Some(icon) => entry.custom_icon_path(icon),
        None => entry,
    }
}

/// A folder row: choosing it lists that folder in the same segment's popup.
fn drill_entry(
    label: &str,
    dir: Arc<RelPath>,
    ix: usize,
    bar: WeakEntity<NavBar>,
    cx: &App,
) -> ContextMenuEntry {
    with_icon(
        ContextMenuEntry::new(label.to_string()),
        FileIcons::get_folder_icon(false, dir.as_std_path(), cx),
    )
    .handler(move |window, cx| {
        let (bar, dir) = (bar.clone(), dir.clone());
        // After this popup has dismissed itself.
        window.defer(cx, move |window, cx| {
            bar.update(cx, |bar, cx| bar.open_menu(ix, Some(dir), window, cx))
                .ok();
        });
    })
}

impl Render for NavBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let segments = self.segments(cx);
        // shortcut: ~7 px per small-label character and half the window for the bar; measure
        // the labels if long paths cut too early or too late.
        let budget = (f32::from(window.viewport_size().width) / 2. / 7.) as usize;
        let lens: Vec<_> = segments.iter().map(|s| s.label.chars().count()).collect();
        let slots = fit(&lens, budget.max(24));
        let open = self.open_segment();
        let anchor_slot = open.and_then(|open| {
            slots
                .iter()
                .position(|slot| *slot == Some(open))
                .or_else(|| slots.iter().position(Option::is_none))
        });
        let target = anchor_slot.and_then(|slot| self.slot_bounds.borrow().get(slot).copied());
        let bounds = self.slot_bounds.clone();
        let file_icon = segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::File)
            .and_then(|segment| FileIcons::get_icon(segment.path.as_std_path(), cx));

        let slot_elements = slots.into_iter().enumerate().map(|(slot, shown)| {
            let content = match shown {
                None => Label::new("…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                Some(ix) => {
                    let segment = &segments[ix];
                    let icon = (segment.kind == SegmentKind::File)
                        .then(|| file_icon.clone())
                        .flatten();
                    ButtonLike::new(("zenith-navbar-segment", ix))
                        .toggle_state(open == Some(ix))
                        .aria_label(segment.label.clone())
                        .child(
                            h_flex()
                                .gap_1()
                                .when_some(icon, |this, icon| {
                                    this.child(
                                        Icon::from_path(icon)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .child(Label::new(segment.label.clone()).size(LabelSize::Small)),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_menu(ix, None, window, cx)
                        }))
                        .into_any_element()
                }
            };
            h_flex()
                .gap_1()
                .when(slot > 0, |this| {
                    this.child(Label::new("›").size(LabelSize::Small).color(Color::Muted))
                })
                .child(content)
        });

        h_flex()
            .on_children_prepainted(move |children, _, _| *bounds.borrow_mut() = children)
            .key_context("NavigationBar")
            .debug_selector(|| "zenith-navbar".into())
            .min_w_0()
            .gap_1()
            .on_action(cx.listener(|this, _: &SelectPreviousSegment, window, cx| {
                this.step(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectNextSegment, window, cx| {
                this.step(true, window, cx)
            }))
            .children(slot_elements)
            .children(self.menu.as_ref().map(|open| {
                deferred(
                    anchored()
                        .anchor(Anchor::BottomLeft)
                        .when_some(target, |this, target| this.attach(target, Anchor::TopLeft))
                        .child(open.menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl StatusItemView for NavBar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item = active_pane_item.map(|item| item.downgrade_item());
        self._editor_subscription = active_pane_item
            .and_then(|item| item.act_as::<Editor>(cx))
            .map(|editor| {
                cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                    if matches!(
                        event,
                        EditorEvent::OutlineSymbolsChanged
                            | EditorEvent::BreadcrumbsChanged
                            | EditorEvent::TitleChanged
                            | EditorEvent::FileHandleChanged
                    ) {
                        cx.notify();
                    }
                })
            });
        cx.notify();
    }

    // No setting yet: adding one to `settings_content` rebuilds the core crates.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
