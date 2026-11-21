use editor::Editor;
use gpui::prelude::*;
use gpui::{
    Action, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Role, ScrollStrategy, SharedString, Styled,
    UniformListScrollHandle, WeakEntity, Window, px, uniform_list,
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
        ToggleWpHooksPanel,
        /// Shows every hook in the WordPress Hooks panel.
        FilterAllHooks,
        /// Shows only action hooks in the WordPress Hooks panel.
        FilterActionHooks,
        /// Shows only filter hooks in the WordPress Hooks panel.
        FilterFilterHooks
    ]
);

/// Every row of the list has this height, which is what lets `uniform_list` draw only what is
/// on screen.
const ROW_HEIGHT: f32 = 26.;
const ITEM_INDENT: f32 = 16.;

/// A line's identity across rebuilds: its hook name and, for a definition (`true`) or listener
/// (`false`) line, its ordinal.
type RowKey = (String, Option<(bool, usize)>);

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
    /// The keyboard-selected line of `rows`.
    selected: Option<usize>,
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
            selected: None,
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
        let selected = self.selected_key();
        self.hooks = read_index(&self.index).hook_summaries();
        self.build_rows(selected);
    }

    fn rebuild_rows(&mut self) {
        let selected = self.selected_key();
        self.build_rows(selected);
    }

    /// Who the selected line is, so the selection can follow it when `rows` changes: the hook's
    /// name, and for one of its lines whether it is a definition and which one it is.
    fn selected_key(&self) -> Option<RowKey> {
        let (hook, child) = match self.rows.get(self.selected?)? {
            Row::Hook(hook) => (hook, None),
            Row::Definition { hook, ordinal, .. } => (hook, Some((true, *ordinal))),
            Row::Listener { hook, ordinal, .. } => (hook, Some((false, *ordinal))),
        };
        Some((self.hooks.get(*hook)?.name.clone(), child))
    }

    /// Rebuilds `rows`, then selects `selected` again: its own line, else its hook's line, else
    /// (it is gone) the old position kept within the list.
    fn build_rows(&mut self, selected: Option<RowKey>) {
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
        let previous = self.selected;
        let position = |key: &RowKey| {
            let mut keys = self.rows.iter().map(|row| match row {
                Row::Hook(hook) => (&self.hooks[*hook].name, None),
                Row::Definition { hook, ordinal, .. } => {
                    (&self.hooks[*hook].name, Some((true, *ordinal)))
                }
                Row::Listener { hook, ordinal, .. } => {
                    (&self.hooks[*hook].name, Some((false, *ordinal)))
                }
            });
            keys.position(|(name, child)| *name == key.0 && child == key.1)
        };
        self.selected = selected
            .and_then(|key| position(&key).or_else(|| position(&(key.0, None))))
            .or_else(|| {
                let last = self.rows.len().checked_sub(1)?;
                previous.map(|ix| ix.min(last))
            });
        if let Some(ix) = self.selected.filter(|ix| Some(*ix) != previous) {
            self.scroll_handle
                .scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return;
        };
        self.select(self.selected.map_or(0, |ix| (ix + 1).min(last)), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rows.is_empty() {
            return;
        }
        self.select(self.selected.map_or(0, |ix| ix.saturating_sub(1)), cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.select(0, cx);
        }
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.rows.len().checked_sub(1) {
            self.select(last, cx);
        }
    }

    /// Collapses a hook, or moves from one of its lines to the hook.
    fn select_parent(&mut self, _: &menu::SelectParent, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.and_then(|ix| self.rows.get(ix)) {
            Some(Row::Hook(hook)) => {
                let name = self.hooks[*hook].name.clone();
                if self.expanded_hooks.contains(&name) {
                    self.toggle_expand(&name, cx);
                }
            }
            Some(Row::Definition { hook, .. } | Row::Listener { hook, .. }) => {
                let hook = *hook;
                if let Some(ix) = self
                    .rows
                    .iter()
                    .position(|row| matches!(row, Row::Hook(owner) if *owner == hook))
                {
                    self.select(ix, cx);
                }
            }
            None => {}
        }
    }

    /// Expands a collapsed hook.
    fn select_child(&mut self, _: &menu::SelectChild, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(Row::Hook(hook)) = self.selected.and_then(|ix| self.rows.get(ix)) {
            let name = self.hooks[*hook].name.clone();
            if !self.expanded_hooks.contains(&name) {
                self.toggle_expand(&name, cx);
            }
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.selected {
            self.activate(ix, window, cx);
        }
    }

    /// Escape: hand the keyboard back to the editor.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.focus_center_pane(window, cx));
        }
    }

    /// Enter or click: a hook expands or collapses, a definition or listener opens its file at
    /// its line.
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.select(ix, cx);
        match self.rows.get(ix) {
            Some(Row::Hook(hook)) => {
                let name = self.hooks[*hook].name.clone();
                self.toggle_expand(&name, cx);
            }
            Some(Row::Definition { path, line, .. } | Row::Listener { path, line, .. }) => {
                open_at_line(&self.workspace, path.clone(), *line, window, cx);
            }
            None => {}
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
        let selected = self.selected == Some(ix);
        let item = |id: SharedString| {
            ListItem::new(id)
                .spacing(ListItemSpacing::Dense)
                .height(px(ROW_HEIGHT))
                .rounded()
                .toggle_state(selected)
                .aria_role(Role::TreeItem)
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
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
                item(format!("hook-header-{}", hook.name).into())
                    // ListItem reports `toggle` as a pressed state, not as expanded, so the
                    // state is spelled out in the label.
                    .aria_label(format!(
                        "{}, {}, {} listeners, {} definitions",
                        hook.name,
                        if expanded { "expanded" } else { "collapsed" },
                        hook.listeners,
                        hook.definitions
                    ))
                    .toggle(Some(expanded))
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
                let (name, line) = (&self.hooks[*hook].name, *line);
                let text = location(path, line);
                item(format!("def-{name}-{}-{line}-{ordinal}", path.display()).into())
                    .indent_level(1)
                    .indent_step_size(px(ITEM_INDENT))
                    .aria_label(format!("Definition of {name} at {text}"))
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
                let (name, line) = (&self.hooks[*hook].name, *line);
                let text = location(path, line);
                item(format!("listener-{name}-{}-{line}-{ordinal}", path.display()).into())
                    .indent_level(1)
                    .indent_step_size(px(ITEM_INDENT))
                    .aria_label(format!(
                        "Listener {callback} of {name}, priority {priority}, at {text}"
                    ))
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
        // `Button` defaults to `Role::Button`; the toggle state reaches assistive technology as
        // "pressed" and the label says it in words.
        let filter_button =
            |id: &'static str, label: &'static str, what: &'static str, kind: Option<HookKind>| {
                let active = self.filter_kind == kind;
                Button::new(id, label)
                    .style(if active {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Subtle
                    })
                    .toggle_state(active)
                    .aria_label(if active {
                        format!("{what}, selected")
                    } else {
                        what.to_string()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter_kind(kind, cx)))
            };
        v_flex()
            .id("wp-hooks-panel")
            .key_context("WpHooksPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::select_parent))
            .on_action(cx.listener(Self::select_child))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(
                cx.listener(|this, _: &FilterAllHooks, _, cx| this.set_filter_kind(None, cx)),
            )
            .on_action(cx.listener(|this, _: &FilterActionHooks, _, cx| {
                this.set_filter_kind(Some(HookKind::Action), cx)
            }))
            .on_action(cx.listener(|this, _: &FilterFilterHooks, _, cx| {
                this.set_filter_kind(Some(HookKind::Filter), cx)
            }))
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
                    .child(filter_button("filter-all", "All", "Show all hooks", None))
                    .child(filter_button(
                        "filter-actions",
                        "Actions",
                        "Show action hooks",
                        Some(HookKind::Action),
                    ))
                    .child(filter_button(
                        "filter-filters",
                        "Filters",
                        "Show filter hooks",
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
    use gpui::{Entity, TestAppContext, VisualTestContext};

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

    fn selected_hook(panel: &WpHooksPanel) -> Option<&str> {
        match panel.rows.get(panel.selected?)? {
            Row::Hook(hook) | Row::Definition { hook, .. } | Row::Listener { hook, .. } => {
                Some(&panel.hooks[*hook].name)
            }
        }
    }

    #[gpui::test]
    fn test_selection_follows_its_hook_when_the_index_changes(cx: &mut App) {
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        index.write().unwrap().index_source(
            PathBuf::from("m.php"),
            "<?php\napply_filters('zz_m', 1);\nadd_filter('zz_m', 'cb');\n",
        );
        let panel = cx.new(|cx| WpHooksPanel::new(WeakEntity::new_invalid(), index.clone(), cx));
        panel.update(cx, |panel, cx| {
            panel.set_search_query("zz_".into(), cx);
            panel.toggle_expand("zz_m", cx);
            // Rows: zz_m, its definition, its listener. Select the listener.
            panel.selected = Some(2);
        });

        // A hook sorting above the selected one arrives: the listener row moves down by one.
        index
            .write()
            .unwrap()
            .index_source(PathBuf::from("a.php"), "<?php\ndo_action('zz_a');\n");
        panel.update(cx, |panel, cx| panel.index_changed(cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.rows.len(), 4);
            assert_eq!(panel.selected, Some(3));
            assert!(matches!(panel.rows[3], Row::Listener { .. }));
            assert_eq!(selected_hook(panel), Some("zz_m"));
        });

        // Collapsing the hook removes the selected line: the hook's own line takes the selection.
        panel.update(cx, |panel, cx| panel.toggle_expand("zz_m", cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.selected, Some(1));
            assert_eq!(selected_hook(panel), Some("zz_m"));
        });
    }

    #[gpui::test]
    fn test_selection_follows_its_hook_when_the_filter_changes(cx: &mut App) {
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        index.write().unwrap().index_source(
            PathBuf::from("m.php"),
            "<?php\ndo_action('zz_action');\napply_filters('zz_filter', 1);\n",
        );
        let panel = cx.new(|cx| WpHooksPanel::new(WeakEntity::new_invalid(), index, cx));
        panel.update(cx, |panel, cx| {
            panel.set_search_query("zz_".into(), cx);
            panel.selected = Some(1);
            assert_eq!(selected_hook(panel), Some("zz_filter"));

            panel.set_filter_kind(Some(HookKind::Filter), cx);
            assert_eq!((panel.rows.len(), panel.selected), (1, Some(0)));
            assert_eq!(selected_hook(panel), Some("zz_filter"));

            panel.set_filter_kind(None, cx);
            assert_eq!((panel.rows.len(), panel.selected), (2, Some(1)));
            assert_eq!(selected_hook(panel), Some("zz_filter"));

            // The selected hook is filtered out: the position is kept within the list.
            panel.set_filter_kind(Some(HookKind::Action), cx);
            assert_eq!((panel.rows.len(), panel.selected), (1, Some(0)));
        });
    }

    struct Fixture {
        workspace: Entity<Workspace>,
        panel: Entity<WpHooksPanel>,
    }

    /// A workspace over `/p/p.php` (hooks `zz_alpha` and `zz_beta`) with the panel added by hand
    /// and the keys a keymap file would give it.
    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            let panel = Some("WpHooksPanel");
            cx.bind_keys([
                gpui::KeyBinding::new("down", menu::SelectNext, panel),
                gpui::KeyBinding::new("up", menu::SelectPrevious, panel),
                gpui::KeyBinding::new("right", menu::SelectChild, panel),
                gpui::KeyBinding::new("left", menu::SelectParent, panel),
                gpui::KeyBinding::new("enter", menu::Confirm, panel),
                gpui::KeyBinding::new("escape", menu::Cancel, panel),
            ]);
            app_state
        });
        let source = "<?php\napply_filters('zz_alpha', 1);\nadd_filter('zz_alpha', 'cb');\ndo_action('zz_beta');\n";
        app_state
            .fs
            .as_fake()
            .insert_tree("/p", serde_json::json!({ "p.php": source }))
            .await;
        let project = project::Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let index = Arc::new(RwLock::new(WordPressIndex::new()));
        index
            .write()
            .unwrap()
            .index_source(PathBuf::from("/p/p.php"), source);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.entity().downgrade();
            let panel = cx.new(|cx| WpHooksPanel::new(weak, index, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        panel.update(cx, |panel, cx| panel.set_search_query("zz_".into(), cx));
        (Fixture { workspace, panel }, cx)
    }

    fn selected(fixture: &Fixture, cx: &mut VisualTestContext) -> (Option<usize>, usize) {
        fixture
            .panel
            .read_with(cx, |panel, _| (panel.selected, panel.rows.len()))
    }

    #[gpui::test]
    async fn keyboard_selects_expands_collapses_and_opens(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        // The toggle action puts the keyboard on the panel itself.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<WpHooksPanel>(window, cx);
        });
        cx.run_until_parked();
        let focused = |cx: &mut VisualTestContext| {
            fixture.panel.update_in(cx, |panel, window, cx| {
                panel.focus_handle(cx).is_focused(window)
            })
        };
        assert!(focused(cx));

        // Rows: zz_alpha, zz_beta.
        assert_eq!(selected(&fixture, cx), (None, 2));
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&fixture, cx), (Some(0), 2));
        // Expanding adds the definition and the listener under the selected hook.
        cx.simulate_keystrokes("right");
        assert_eq!(selected(&fixture, cx), (Some(0), 4));
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&fixture, cx), (Some(1), 4));
        // Left on a line goes to its hook, left on the hook collapses it.
        cx.simulate_keystrokes("left");
        assert_eq!(selected(&fixture, cx), (Some(0), 4));
        cx.simulate_keystrokes("left");
        assert_eq!(selected(&fixture, cx), (Some(0), 2));
        // Enter on a hook toggles it too.
        cx.simulate_keystrokes("enter");
        assert_eq!(selected(&fixture, cx), (Some(0), 4));
        // Selection never leaves the list.
        cx.simulate_keystrokes("down down down down up");
        assert_eq!(selected(&fixture, cx), (Some(2), 4));

        // Enter on the definition (line 2) opens the file with the cursor there.
        cx.simulate_keystrokes("up");
        assert_eq!(selected(&fixture, cx), (Some(1), 4));
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let editor = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
            .expect("the file is open");
        let head = editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        });
        assert_eq!(head, Point::new(1, 0));

        // Escape hands the keyboard to the editor.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<WpHooksPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(focused(cx));
        cx.simulate_keystrokes("escape");
        assert!(!focused(cx));
        assert!(editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));
    }

    /// The shipped Zenith keymap gives the panel its toggle key and its left/right keys.
    #[gpui::test]
    fn test_zenith_keymap_binds_the_panel(cx: &mut App) {
        // The file itself, not the asset loader: a dev build reads assets from the checkout that
        // owns the test binary, which is not this one when the target dir is shared.
        let bindings = match settings::KeymapFile::load(
            include_str!("../../../assets/keymaps/zenith.json"),
            cx,
        ) {
            settings::KeymapFileLoadResult::Success { key_bindings }
            | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings,
            settings::KeymapFileLoadResult::JsonParseFailure { error } => panic!("{error:#}"),
        };
        let bound = |action: &dyn Action, key: &str, context: &str| {
            bindings.iter().any(|binding| {
                binding.action().partial_eq(action)
                    && binding.keystrokes().len() == 1
                    && binding.keystrokes()[0].inner().key == key
                    && binding.predicate().map(|p| p.to_string()).as_deref() == Some(context)
            })
        };
        assert!(bound(&ToggleWpHooksPanel, "7", "Workspace"));
        assert!(bound(&menu::SelectChild, "right", "WpHooksPanel"));
        assert!(bound(&menu::SelectParent, "left", "WpHooksPanel"));
    }

    #[gpui::test]
    async fn filter_actions_follow_the_panel(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<WpHooksPanel>(window, cx);
        });
        cx.run_until_parked();
        let filter = |cx: &mut VisualTestContext| fixture.panel.read_with(cx, |p, _| p.filter_kind);
        cx.dispatch_action(FilterActionHooks);
        assert_eq!(filter(cx), Some(HookKind::Action));
        cx.dispatch_action(FilterFilterHooks);
        assert_eq!(filter(cx), Some(HookKind::Filter));
        cx.dispatch_action(FilterAllHooks);
        assert_eq!(filter(cx), None);
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
