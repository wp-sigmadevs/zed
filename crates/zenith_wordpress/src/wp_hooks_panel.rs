use editor::Editor;
use gpui::prelude::*;
use gpui::{
    Action, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Role, SharedString, Styled, UniformListScrollHandle, WeakEntity,
    Window, px, uniform_list,
};
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use text::Point;
use ui::prelude::*;
use ui::{ButtonStyle, Color, Icon, IconName, Label, LabelCommon, ListItem, ListItemSpacing};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{OpenOptions, Workspace};

use super::{CallbackTarget, HookKind, HookSummary, WordPressIndex};

gpui::actions!(
    zenith_wordpress,
    [
        /// Toggles focus on the WordPress Hooks panel.
        ToggleWpHooksPanel
    ]
);

/// Every row of the list has this height, which is what lets `uniform_list` draw only what is
/// on screen.
const ROW_HEIGHT: f32 = 26.;
const ITEM_INDENT: f32 = 16.;

/// One line of the list. A hook line holds an index into [`WpHooksPanel::hooks`]; the lines of an
/// expanded hook follow it, flat, so every row keeps the same height.
enum Row {
    Hook(usize),
    Definition {
        hook: usize,
        ordinal: usize,
        path: PathBuf,
        line: usize,
    },
    Listener {
        hook: usize,
        ordinal: usize,
        path: PathBuf,
        line: usize,
        priority: i32,
        callback: String,
    },
}

pub struct WpHooksPanel {
    focus_handle: FocusHandle,
    pub(crate) index: Arc<RwLock<WordPressIndex>>,
    search_query: String,
    filter_kind: Option<HookKind>,
    expanded_hooks: HashSet<String>,
    workspace: WeakEntity<Workspace>,
    /// Every hook of the index, sorted by name. Rebuilt when the index changes, not per frame.
    hooks: Vec<HookSummary>,
    /// What the list shows: the hooks passing the filter and the lines of the expanded ones.
    rows: Vec<Row>,
    /// Hook lines in `rows`.
    shown_hooks: usize,
    scroll_handle: UniformListScrollHandle,
}

impl WpHooksPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        index: Arc<RwLock<WordPressIndex>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            index,
            search_query: String::new(),
            filter_kind: None,
            expanded_hooks: HashSet::new(),
            workspace,
            hooks: Vec::new(),
            rows: Vec::new(),
            shown_hooks: 0,
            scroll_handle: UniformListScrollHandle::new(),
        };
        panel.reload_hooks();
        panel
    }

    /// The index changed (the indexer finished a round): take its hooks again.
    pub(crate) fn index_changed(&mut self, cx: &mut Context<Self>) {
        self.reload_hooks();
        cx.notify();
    }

    fn reload_hooks(&mut self) {
        self.hooks = read_index(&self.index).hook_summaries();
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        let query = self.search_query.to_lowercase();
        let index = self.index.clone();
        let index = read_index(&index);
        self.rows.clear();
        self.shown_hooks = 0;
        for (ix, hook) in self.hooks.iter().enumerate() {
            if self.filter_kind.is_some_and(|kind| hook.kind != kind)
                || (!query.is_empty() && !hook.name.to_lowercase().contains(&query))
            {
                continue;
            }
            self.shown_hooks += 1;
            self.rows.push(Row::Hook(ix));
            if !self.expanded_hooks.contains(&hook.name) {
                continue;
            }
            for (ordinal, def) in index.find_definitions(&hook.name).into_iter().enumerate() {
                self.rows.push(Row::Definition {
                    hook: ix,
                    ordinal,
                    path: def.file_path.clone(),
                    line: def.line,
                });
            }
            for (ordinal, listener) in index.find_listeners(&hook.name).into_iter().enumerate() {
                self.rows.push(Row::Listener {
                    hook: ix,
                    ordinal,
                    path: listener.file_path.clone(),
                    line: listener.line,
                    priority: listener.priority,
                    callback: callback_text(&listener.callback),
                });
            }
        }
    }

    pub fn toggle_expand(&mut self, hook_name: &str, cx: &mut Context<Self>) {
        if self.expanded_hooks.contains(hook_name) {
            self.expanded_hooks.remove(hook_name);
        } else {
            self.expanded_hooks.insert(hook_name.to_string());
        }
        self.rebuild_rows();
        cx.notify();
    }

    pub fn set_filter_kind(&mut self, kind: Option<HookKind>, cx: &mut Context<Self>) {
        self.filter_kind = kind;
        self.rebuild_rows();
        cx.notify();
    }

    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query;
        self.rebuild_rows();
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let item = |id: SharedString| {
            ListItem::new(id)
                .spacing(ListItemSpacing::Dense)
                .height(px(ROW_HEIGHT))
                .rounded()
                .aria_role(Role::TreeItem)
        };
        let location = |path: &Path, line: usize| {
            format!(
                "{}:{line}",
                path.file_name()
                    .map(|f| f.to_string_lossy())
                    .unwrap_or_default()
            )
        };
        match &self.rows[ix] {
            Row::Hook(hook) => {
                let hook = &self.hooks[*hook];
                let expanded = self.expanded_hooks.contains(&hook.name);
                let name = hook.name.clone();
                item(format!("hook-header-{}", hook.name).into())
                    .aria_label(format!(
                        "{}, {} listeners, {} definitions",
                        hook.name, hook.listeners, hook.definitions
                    ))
                    .toggle(Some(expanded))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expand(&name, cx)))
                    .child(Label::new(hook.name.clone()).weight(gpui::FontWeight::MEDIUM))
                    .end_slot(
                        Label::new(format!("{}L / {}D", hook.listeners, hook.definitions))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
            Row::Definition {
                hook,
                ordinal,
                path,
                line,
            } => {
                let (name, path, line) = (&self.hooks[*hook].name, path.clone(), *line);
                let workspace = self.workspace.clone();
                let text = location(&path, line);
                item(format!("def-{name}-{}-{line}-{ordinal}", path.display()).into())
                    .indent_level(1)
                    .indent_step_size(px(ITEM_INDENT))
                    .aria_label(format!("Definition at {text}"))
                    .on_click(move |_, window, cx| {
                        open_at_line(&workspace, path.clone(), line, window, cx)
                    })
                    .start_slot(
                        Label::new("[def]")
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(Label::new(text).size(LabelSize::Small))
                    .into_any_element()
            }
            Row::Listener {
                hook,
                ordinal,
                path,
                line,
                priority,
                callback,
            } => {
                let (name, path, line) = (&self.hooks[*hook].name, path.clone(), *line);
                let workspace = self.workspace.clone();
                let text = location(&path, line);
                item(format!("listener-{name}-{}-{line}-{ordinal}", path.display()).into())
                    .indent_level(1)
                    .indent_step_size(px(ITEM_INDENT))
                    .aria_label(format!("Listener {callback} at {text}"))
                    .on_click(move |_, window, cx| {
                        open_at_line(&workspace, path.clone(), line, window, cx)
                    })
                    .start_slot(
                        Label::new(format!("[{priority}]"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(callback.clone()).size(LabelSize::Small))
                    .end_slot(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
                    .into_any_element()
            }
        }
    }
}

/// How a listener's callback reads in the list.
fn callback_text(callback: &CallbackTarget) -> String {
    match callback {
        CallbackTarget::Function(f) => format!("{f}()"),
        CallbackTarget::Method {
            class_name,
            method_name,
            is_static,
        } => {
            let sep = if *is_static { "::" } else { "->" };
            format!(
                "{}{sep}{method_name}()",
                class_name.as_deref().unwrap_or("$this")
            )
        }
        CallbackTarget::Closure => "Closure".to_string(),
        CallbackTarget::Unknown(u) => u.clone(),
    }
}

impl Focusable for WpHooksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WpHooksPanel {}

impl Panel for WpHooksPanel {
    fn persistent_name() -> &'static str {
        "WpHooksPanel"
    }

    fn panel_key() -> &'static str {
        "wp_hooks_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(340.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::BookCopy)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("WordPress Hooks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleWpHooksPanel)
    }

    fn activation_priority(&self) -> u32 {
        ACTIVATION_PRIORITY
    }
}

impl Render for WpHooksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_button = |id: &'static str, label: &'static str, kind: Option<HookKind>| {
            Button::new(id, label)
                .style(if self.filter_kind == kind {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter_kind(kind, cx)))
        };
        v_flex()
            .id("wp-hooks-panel")
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .overflow_hidden()
            .child(
                h_flex()
                    .p_2()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::BookCopy).color(Color::Accent))
                            .child(
                                Label::new("WordPress Hooks").weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(format!("{} hooks", self.shown_hooks))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(filter_button("filter-all", "All", None))
                    .child(filter_button(
                        "filter-actions",
                        "Actions",
                        Some(HookKind::Action),
                    ))
                    .child(filter_button(
                        "filter-filters",
                        "Filters",
                        Some(HookKind::Filter),
                    )),
            )
            .child(
                // Only the rows in view are built; a core tree has thousands of hooks.
                uniform_list(
                    "wp-hook-rows",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .flex_1()
                .px_1(),
            )
    }
}

/// Where a 1-based `line` of the index sits in the editor: column 0 of that line.
/// The column stored in the index is a byte offset into a scan that may be stale, so it is
/// not used.
pub(crate) fn line_to_point(line: usize) -> Point {
    Point::new(line.saturating_sub(1).try_into().unwrap_or(u32::MAX), 0)
}

/// Opens `path` and puts the cursor on `line` (1-based, as in the index).
fn open_at_line(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    line: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let open = workspace.update(cx, |workspace, cx| {
        workspace.open_abs_path(path, OpenOptions::default(), window, cx)
    });
    window
        .spawn(cx, async move |cx| {
            if let Some(editor) = open.await?.downcast::<Editor>() {
                cx.update(|window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point(line_to_point(line), window, cx)
                    })
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// Dock panels need unique priorities (a debug build panics on a clash). Taken: 0 agent,
/// 1 project, 2 terminal, 3 git, 5 collab, 6 outline, 7 debugger, 8 todo, 9 problems,
/// 10 scratches, 11 WordPress hooks, 12 Laravel, 13 Next.js, 99 search, 200 workspace tests.
const ACTIVATION_PRIORITY: u32 = 11;

/// Reads the shared index; a lock poisoned by a panicked indexer still yields the last index
/// instead of panicking the render.
pub(crate) fn read_index<T>(index: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    index.read().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn test_wp_hooks_panel_state(cx: &mut App) {
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        let panel = cx.new(|cx| WpHooksPanel::new(WeakEntity::new_invalid(), index, cx));

        panel.update(cx, |this, cx| {
            assert_eq!(this.filter_kind, None);
            this.set_filter_kind(Some(HookKind::Action), cx);
            assert_eq!(this.filter_kind, Some(HookKind::Action));

            assert_eq!(this.search_query, "");
            this.set_search_query("admin_".to_string(), cx);
            assert_eq!(this.search_query, "admin_");

            assert!(!this.expanded_hooks.contains("init"));
            this.toggle_expand("init", cx);
            assert!(this.expanded_hooks.contains("init"));
            this.toggle_expand("init", cx);
            assert!(!this.expanded_hooks.contains("init"));
        });
    }

    #[gpui::test]
    fn test_rows_follow_the_index_filter_and_expansion(cx: &mut App) {
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        let panel = cx.new(|cx| WpHooksPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        let core_hooks = panel.read_with(cx, |panel, _| panel.rows.len());
        assert!(core_hooks > 0, "core hooks are always listed");

        // A new hook shows up only once the panel is told the index changed.
        index.write().unwrap().index_source(
            PathBuf::from("p.php"),
            "<?php\napply_filters('zz_custom', 1);\nadd_filter('zz_custom', 'cb');\n",
        );
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), core_hooks));
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| assert_eq!(panel.rows.len(), core_hooks + 1));

        panel.update(cx, |panel, cx| {
            panel.set_search_query("ZZ_".into(), cx);
            assert_eq!(panel.rows.len(), 1);
            // Expanding adds the hook's lines as rows of their own (one definition, one listener).
            panel.toggle_expand("zz_custom", cx);
            assert_eq!(panel.rows.len(), 3);
            assert_eq!(panel.shown_hooks, 1);
            panel.set_filter_kind(Some(HookKind::Action), cx);
            assert!(panel.rows.is_empty());
        });
    }

    #[test]
    fn test_index_lines_are_one_based_editor_rows_are_zero_based() {
        assert_eq!(line_to_point(1), Point::new(0, 0));
        assert_eq!(line_to_point(42), Point::new(41, 0));
        // A line of 0 never occurs in the index; it must not underflow.
        assert_eq!(line_to_point(0), Point::new(0, 0));
    }

    #[test]
    fn test_read_index_survives_a_poisoned_lock() {
        let index = Arc::new(RwLock::new(7));
        let writer = index.clone();
        std::thread::spawn(move || {
            let _guard = writer.write().unwrap();
            panic!("indexer died");
        })
        .join()
        .unwrap_err();
        assert!(index.is_poisoned());
        assert_eq!(*read_index(&index), 7);
    }

    #[test]
    fn test_activation_priority_is_not_taken_by_another_panel() {
        const TAKEN_BY_OTHERS: [u32; 11] = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 99];
        assert!(!TAKEN_BY_OTHERS.contains(&ACTIVATION_PRIORITY));
        assert_ne!(ACTIVATION_PRIORITY, 200);
        assert_eq!(ACTIVATION_PRIORITY, 11);
    }
}
