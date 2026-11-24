//! The tab right-click menu in PhpStorm's layout (plan 44 item 1).
//!
//! `workspace` cannot depend on this crate, so `init` registers a builder with
//! `workspace::set_tab_menu_builder`; the pane asks it for a menu on every right-click and keeps
//! upstream's menu when it returns `None` (the setting is off). Every item runs on the
//! right-clicked tab: the tab is activated first, then the action is dispatched from the pane.
//! Items that upstream hides or greys out (no file, no repo, nothing to the left) do the same
//! here. Submenus are one level deep.

use std::{path::PathBuf, rc::Rc};

use gpui::{Action, App, Context, Entity, EntityId, SharedString, WeakEntity, Window};
use project::{Project, ProjectPath};
use settings::Settings as _;
use ui::{ContextMenu, ContextMenuEntry, ContextMenuItem};
use workspace::{
    CloseActiveItem, CloseAllItems, CloseCleanItems, CloseItemsToTheLeft, CloseItemsToTheRight,
    CloseMultibufferItems, CloseOtherItems, OpenTerminal, Pane, ReopenClosedItem,
    RevealInProjectPanel, SplitDown, SplitRight, TabMenuTarget, TogglePinTab, Workspace,
};

use crate::{FeatureToggles, tools};

/// What a menu item does when chosen.
#[derive(Clone)]
pub(crate) enum Cmd {
    /// Activate the tab, then dispatch the action from the pane.
    Dispatch(Rc<dyn Action>),
    Copy(String),
    Reveal(PathBuf),
    ToggleReadOnly,
    /// Open or copy the file's permalink on its git host.
    Permalink {
        copy: bool,
    },
    /// The file's git history, resolved from this tab's path, not from the active editor.
    FileHistory,
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub label: SharedString,
    pub cmd: Cmd,
    pub disabled: bool,
}

pub(crate) enum Row {
    Item(Entry),
    Menu(&'static str, Vec<Entry>),
    Separator,
}

/// What the layout depends on; plain data so the layout is testable without a window.
#[derive(Default, Clone)]
pub(crate) struct Facts {
    pub ix: usize,
    pub total_items: usize,
    pub has_pinned: bool,
    pub is_pinned: bool,
    pub has_clean_items: bool,
    pub has_multibuffer_items: bool,
    pub read_only_label: Option<&'static str>,
    pub abs_path: Option<PathBuf>,
    pub relative_path: Option<String>,
    pub has_git_repo: bool,
    pub visible_in_project_panel: bool,
    pub is_local: bool,
    pub is_remote: bool,
}

fn facts(target: &TabMenuTarget) -> Facts {
    Facts {
        ix: target.ix,
        total_items: target.total_items,
        has_pinned: target.has_pinned,
        is_pinned: target.is_pinned,
        has_clean_items: target.has_clean_items,
        has_multibuffer_items: target.has_multibuffer_items,
        read_only_label: target.read_only_label,
        abs_path: target.abs_path.clone(),
        relative_path: target.relative_path.clone(),
        has_git_repo: target.has_git_repo,
        visible_in_project_panel: target.visible_in_project_panel,
        is_local: target.is_local,
        is_remote: target.is_remote,
    }
}

fn entry(label: impl Into<SharedString>, action: impl Action, disabled: bool) -> Entry {
    Entry {
        label: label.into(),
        cmd: Cmd::Dispatch(Rc::new(action)),
        disabled,
    }
}

fn menu(label: &'static str, entries: Vec<Entry>) -> Row {
    Row::Menu(label, entries)
}

/// The menu in order.
pub(crate) fn rows(f: &Facts) -> Vec<Row> {
    let close_all = |close_pinned| CloseAllItems {
        save_intent: None,
        close_pinned,
    };
    let mut rows = vec![
        Row::Item(entry(
            "Close",
            CloseActiveItem {
                save_intent: None,
                close_pinned: true,
            },
            false,
        )),
        Row::Item(entry(
            "Close Others",
            CloseOtherItems {
                save_intent: None,
                close_pinned: false,
            },
            f.total_items <= 1,
        )),
    ];
    if f.has_multibuffer_items {
        rows.push(Row::Item(entry(
            "Close Multibuffers",
            CloseMultibufferItems {
                save_intent: None,
                close_pinned: false,
            },
            false,
        )));
    }
    rows.extend([
        Row::Separator,
        Row::Item(entry(
            "Close Tabs to the Left",
            CloseItemsToTheLeft {
                close_pinned: false,
            },
            f.ix == 0,
        )),
        Row::Item(entry(
            "Close Tabs to the Right",
            CloseItemsToTheRight {
                close_pinned: false,
            },
            f.ix + 1 >= f.total_items,
        )),
        Row::Separator,
        Row::Item(entry(
            "Close Unmodified",
            CloseCleanItems {
                close_pinned: false,
            },
            !f.has_clean_items,
        )),
        Row::Item(entry(
            "Close All but Pinned",
            close_all(false),
            !f.has_pinned,
        )),
        Row::Item(entry("Close All", close_all(true), false)),
        Row::Separator,
        Row::Item(entry(
            if f.is_pinned { "Unpin Tab" } else { "Pin Tab" },
            TogglePinTab,
            false,
        )),
        Row::Item(entry("Reopen Closed Tab", ReopenClosedItem, false)),
        Row::Separator,
    ]);

    let copy = |label: &'static str, text: String| Entry {
        label: label.into(),
        cmd: Cmd::Copy(text),
        disabled: false,
    };
    let mut copies = Vec::new();
    if let Some(path) = &f.abs_path {
        copies.push(copy("Absolute Path", path.to_string_lossy().into_owned()));
    }
    if let Some(path) = &f.relative_path {
        copies.push(copy("Path From Project Root", path.clone()));
    }
    if let Some(name) = f.abs_path.as_deref().and_then(|path| path.file_name()) {
        copies.push(copy("File Name", name.to_string_lossy().into_owned()));
    }
    if f.has_git_repo {
        copies.push(Entry {
            label: "File Permalink".into(),
            cmd: Cmd::Permalink { copy: true },
            disabled: false,
        });
    }
    rows.push(menu("Copy Path/Reference", copies));

    let mut open_in = Vec::new();
    if let (true, Some(path)) = (f.is_local, &f.abs_path) {
        open_in.push(Entry {
            label: ui::utils::reveal_in_file_manager_label(f.is_remote).into(),
            cmd: Cmd::Reveal(path.clone()),
            disabled: false,
        });
    }
    if let Some(parent) = f.abs_path.as_deref().and_then(|path| path.parent()) {
        open_in.push(entry(
            "Terminal",
            OpenTerminal {
                working_directory: parent.to_path_buf(),
                local: false,
            },
            false,
        ));
    }
    if f.has_git_repo {
        open_in.push(Entry {
            label: "File Permalink".into(),
            cmd: Cmd::Permalink { copy: false },
            disabled: false,
        });
    }
    rows.push(menu("Open In", open_in));

    // Snapshots are recorded for local projects only (the handler returns silently otherwise).
    let history = (f.abs_path.is_some() && f.is_local && !f.is_remote).then(|| {
        entry(
            "Show History",
            zed_actions::zenith_local_history::ShowFileHistory,
            false,
        )
    });
    rows.push(menu("Local History", history.into_iter().collect()));
    let git_history = f.has_git_repo.then(|| Entry {
        label: "File History".into(),
        cmd: Cmd::FileHistory,
        disabled: false,
    });
    rows.push(menu("Git", git_history.into_iter().collect()));

    rows.extend([
        Row::Separator,
        Row::Item(entry("Split Right", SplitRight::default(), false)),
        Row::Item(entry("Split Down", SplitDown::default(), false)),
        Row::Separator,
    ]);
    if f.visible_in_project_panel {
        rows.push(Row::Item(entry(
            "Reveal in Project Panel",
            RevealInProjectPanel::default(),
            false,
        )));
    }
    if let Some(label) = f.read_only_label {
        rows.push(Row::Item(Entry {
            label: label.into(),
            cmd: Cmd::ToggleReadOnly,
            disabled: false,
        }));
    }
    rows.push(Row::Item(entry(
        "Configure Editor Tabs...",
        zed_actions::OpenSettingsAt {
            path: "tab_bar.show".into(),
            target: None,
        },
        false,
    )));
    rows
}

/// Drops submenus left empty and collapses separators so none leads, trails or doubles.
pub(crate) fn tidy(rows: Vec<Row>) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let mut separator_due = false;
    for row in rows {
        match row {
            Row::Separator => separator_due = !out.is_empty(),
            Row::Menu(_, ref entries) if entries.is_empty() => {}
            row => {
                if std::mem::take(&mut separator_due) {
                    out.push(Row::Separator);
                }
                out.push(row);
            }
        }
    }
    out
}

/// What a menu item needs besides its own data: the clicked tab and the project it is in.
pub(crate) struct Ctx {
    item_id: EntityId,
    project: Option<WeakEntity<Project>>,
    project_path: Option<ProjectPath>,
    workspace: WeakEntity<Workspace>,
}

/// The clicked tab's index now; it may have moved or closed since the menu opened.
fn tab_index(pane: &Pane, item_id: EntityId) -> Option<usize> {
    pane.items().position(|item| item.item_id() == item_id)
}

pub(crate) fn run(
    cmd: &Cmd,
    ctx: &Ctx,
    pane: &mut Pane,
    window: &mut Window,
    cx: &mut Context<Pane>,
) {
    let Some(ix) = tab_index(pane, ctx.item_id) else {
        return;
    };
    match cmd {
        Cmd::Dispatch(action) => {
            // `activate_item` with `activate_pane` makes the clicked tab's pane the workspace's
            // active one (centre panes) before the deferred dispatch below runs.
            pane.activate_item(ix, true, false, window, cx);
            window.focus(&gpui::Focusable::focus_handle(pane, cx), cx);
            window.dispatch_action(action.boxed_clone(), cx);
        }
        Cmd::Copy(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone())),
        Cmd::Reveal(path) => match ctx.project.as_ref().and_then(WeakEntity::upgrade) {
            Some(project) => project.update(cx, |project, cx| project.reveal_path(path, cx)),
            None => cx.reveal_path(path),
        },
        Cmd::ToggleReadOnly => {
            if let Some(item) = pane.item_for_index(ix) {
                item.toggle_read_only(window, cx);
            }
        }
        Cmd::Permalink { copy } => {
            if let (Some(project), Some(path)) = (
                ctx.project.as_ref().and_then(WeakEntity::upgrade),
                ctx.project_path.clone(),
            ) {
                let open = if *copy {
                    workspace::copy_file_permalink
                } else {
                    workspace::open_file_permalink
                };
                open(project, path, ctx.workspace.clone(), window, cx);
            }
        }
        Cmd::FileHistory => {
            let Some(path) = ctx.project_path.clone() else {
                return;
            };
            // The opener updates the workspace and, through it, this pane: run it after this
            // handler, which holds the pane.
            let workspace = ctx.workspace.clone();
            window.defer(cx, move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        git_ui_core::open_file_history(workspace, &path, window, cx)
                    })
                    .ok();
            });
        }
    }
}

fn item(
    entry: &Entry,
    pane: &Entity<Pane>,
    ctx: &Rc<Ctx>,
    window: &mut Window,
) -> ContextMenuEntry {
    let (cmd, ctx) = (entry.cmd.clone(), ctx.clone());
    let mut item = ContextMenuEntry::new(entry.label.clone())
        .disabled(entry.disabled)
        .handler(window.handler_for(pane, move |pane, window, cx| {
            run(&cmd, &ctx, pane, window, cx)
        }));
    if let Cmd::Dispatch(action) = &entry.cmd {
        item = item.action(action.boxed_clone());
    }
    item
}

fn deploy(target: TabMenuTarget, window: &mut Window, cx: &mut App) -> Option<Entity<ContextMenu>> {
    #[cfg(test)]
    {
        tests::DEPLOYED.with(|n| n.set(n.get() + 1));
        tests::LAST_ITEM.with(|slot| slot.set(Some(target.item_id)));
    }

    let rows = Rc::new(tidy(rows(&facts(&target))));
    let TabMenuTarget {
        pane,
        item_id,
        project,
        project_path,
        workspace,
        extra_actions,
        menu_context,
        ..
    } = target;
    let ctx = Rc::new(Ctx {
        item_id,
        project,
        project_path,
        workspace,
    });
    Some(ContextMenu::build(
        window,
        cx,
        move |mut menu, window, _| {
            let Some(pane) = pane.upgrade() else {
                return menu;
            };
            for row in rows.iter() {
                menu = match row {
                    Row::Separator => menu.separator(),
                    Row::Item(entry) => {
                        menu.item(ContextMenuItem::Entry(item(entry, &pane, &ctx, window)))
                    }
                    Row::Menu(label, entries) => {
                        let (entries, pane, ctx) = (entries.clone(), pane.downgrade(), ctx.clone());
                        menu.submenu(*label, move |mut sub, window, _| {
                            let Some(pane) = pane.upgrade() else {
                                return sub;
                            };
                            for entry in &entries {
                                sub = sub
                                    .item(ContextMenuItem::Entry(item(entry, &pane, &ctx, window)));
                            }
                            sub
                        })
                    }
                };
            }
            if !extra_actions.is_empty() {
                menu = menu.separator();
                for (label, action) in &extra_actions {
                    menu = menu.action(label.clone(), action.boxed_clone());
                }
            }
            menu.context(menu_context.clone())
        },
    ))
}

/// Gives every tab the PhpStorm menu unless `zenith.tab_menu.enabled` is off. The setting is read
/// on each right-click, so it applies without a restart.
pub(crate) fn init(cx: &mut App) {
    workspace::set_tab_menu_builder(cx, |target, window, cx| {
        enabled(cx).then(|| deploy(target, window, cx)).flatten()
    });
}

fn enabled(cx: &App) -> bool {
    tools::feature("tab_menu")
        .and_then(|(index, _)| FeatureToggles::get_global(cx).0.get(index).copied())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::{
        AppContext as _, BorrowAppContext as _, Focusable as _, Modifiers, MouseButton,
        TestAppContext,
    };
    use project::{FakeFs, Project};
    use workspace::{Workspace, item::test::TestItem};

    use super::*;

    thread_local! {
        /// How many menus `deploy` built on this thread, so a test can tell the menu is ours.
        pub(crate) static DEPLOYED: Cell<usize> = const { Cell::new(0) };
        /// The tab the last built menu was for.
        pub(crate) static LAST_ITEM: Cell<Option<EntityId>> = const { Cell::new(None) };
    }

    /// Labels of the tidy menu, submenus as `Name>`, separators as `-`; `!` marks a greyed item.
    fn shape(f: &Facts) -> Vec<String> {
        let label = |e: &Entry| format!("{}{}", e.label, if e.disabled { "!" } else { "" });
        tidy(rows(f))
            .iter()
            .flat_map(|row| match row {
                Row::Separator => vec!["-".to_string()],
                Row::Item(e) => vec![label(e)],
                Row::Menu(name, es) => std::iter::once(format!("{name}>"))
                    .chain(es.iter().map(|e| format!("  {}", label(e))))
                    .collect(),
            })
            .collect()
    }

    fn file_facts() -> Facts {
        Facts {
            ix: 1,
            total_items: 3,
            has_clean_items: true,
            abs_path: Some("/p/src/a.php".into()),
            relative_path: Some("src/a.php".into()),
            is_local: true,
            ..Facts::default()
        }
    }

    #[test]
    fn greys_out_what_upstream_greys_out() {
        let only = Facts {
            total_items: 1,
            ..Facts::default()
        };
        let menu = shape(&only);
        for greyed in [
            "Close Others!",
            "Close Tabs to the Left!",
            "Close Tabs to the Right!",
            "Close Unmodified!",
            "Close All but Pinned!",
        ] {
            assert!(menu.contains(&greyed.to_string()), "{greyed} in {menu:?}");
        }
        let middle = shape(&file_facts());
        for on in [
            "Close Others",
            "Close Tabs to the Left",
            "Close Tabs to the Right",
        ] {
            assert!(middle.contains(&on.to_string()), "{on} in {middle:?}");
        }
    }

    #[test]
    fn pin_label_follows_the_tab_and_file_menus_follow_the_file() {
        let pinned = Facts {
            is_pinned: true,
            has_pinned: true,
            ..file_facts()
        };
        let menu = shape(&pinned);
        assert!(menu.contains(&"Unpin Tab".to_string()));
        assert!(menu.contains(&"Close All but Pinned".to_string()));
        assert!(menu.contains(&"  File Name".to_string()));
        assert!(menu.contains(&"  Terminal".to_string()));
        // No file (untitled, multibuffer): the file submenus are gone, not empty.
        let bare = shape(&Facts::default());
        assert!(bare.contains(&"Pin Tab".to_string()));
        assert!(!bare.iter().any(|row| row.ends_with('>')), "{bare:?}");
        // No repo: git items stay out (they also need a resolvable action name).
        assert!(
            !menu
                .iter()
                .any(|row| row.contains("Permalink") || row == "Git>")
        );
    }

    #[test]
    fn separators_never_lead_trail_or_double() {
        for f in [Facts::default(), file_facts()] {
            let menu = shape(&f);
            assert_ne!(menu.first().map(String::as_str), Some("-"));
            assert_ne!(menu.last().map(String::as_str), Some("-"));
            assert!(!menu.windows(2).any(|w| w[0] == "-" && w[1] == "-"));
        }
    }

    #[test]
    fn git_items_need_a_repo_and_map_to_direct_calls() {
        let cmds = |f: &Facts| -> Vec<&'static str> {
            tidy(rows(f))
                .into_iter()
                .flat_map(|row| match row {
                    Row::Menu(_, es) => es,
                    Row::Item(e) => vec![e],
                    Row::Separator => vec![],
                })
                .filter_map(|e| match e.cmd {
                    Cmd::Permalink { copy: true } => Some("copy-permalink"),
                    Cmd::Permalink { copy: false } => Some("open-permalink"),
                    Cmd::FileHistory => Some("history"),
                    _ => None,
                })
                .collect()
        };
        assert!(cmds(&file_facts()).is_empty());
        let repo = Facts {
            has_git_repo: true,
            ..file_facts()
        };
        assert_eq!(cmds(&repo), ["copy-permalink", "open-permalink", "history"]);
    }

    #[test]
    fn reveal_label_follows_remote() {
        for is_remote in [false, true] {
            let f = Facts {
                is_remote,
                ..file_facts()
            };
            let want = format!("  {}", ui::utils::reveal_in_file_manager_label(is_remote));
            assert!(shape(&f).contains(&want), "{want}");
        }
    }

    #[test]
    fn local_history_needs_a_local_project() {
        let has = |f: &Facts| shape(f).contains(&"  Show History".to_string());
        assert!(has(&file_facts()));
        let remote = Facts {
            is_remote: true,
            ..file_facts()
        };
        assert!(!has(&remote));
        let not_local = Facts {
            is_local: false,
            ..file_facts()
        };
        assert!(!has(&not_local));
    }

    #[test]
    fn tidy_drops_empty_submenus_and_stray_separators() {
        let item = |label: &str| Row::Item(entry(label.to_string(), TogglePinTab, false));
        let rows = vec![
            Row::Separator,
            item("A"),
            Row::Separator,
            Row::Menu("Gone", vec![]),
            Row::Separator,
            item("B"),
            Row::Separator,
        ];
        let kinds: Vec<_> = tidy(rows)
            .iter()
            .map(|row| match row {
                Row::Separator => "-".to_string(),
                Row::Item(e) => e.label.to_string(),
                Row::Menu(name, _) => name.to_string(),
            })
            .collect();
        assert_eq!(kinds, ["A", "-", "B"]);
    }

    /// Right-clicks tab 0 while tab 1 is active; returns tab 0's id.
    async fn right_click_first_tab(
        cx: &mut TestAppContext,
        register: bool,
        enabled: bool,
    ) -> EntityId {
        DEPLOYED.with(|n| n.set(0));
        LAST_ITEM.with(|slot| slot.set(None));
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            if register {
                crate::init(cx);
            }
        });
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    crate::tools::set_feature(content, "tab_menu", enabled);
                });
            });
        });
        let project = Project::test(FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            for label in ["a.php", "b.php"] {
                let item = Box::new(cx.new(|cx| TestItem::new(cx).with_label(label)));
                pane.add_item(item, true, true, None, window, cx);
            }
        });
        cx.run_until_parked();
        let tab = cx.debug_bounds("TAB-0").expect("tab");
        cx.simulate_mouse_down(tab.center(), MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| pane.items().next().unwrap().item_id())
    }

    #[gpui::test]
    async fn the_registered_builder_makes_the_tab_menu(cx: &mut TestAppContext) {
        let first = right_click_first_tab(cx, true, true).await;
        assert_eq!(DEPLOYED.with(Cell::get), 1);
        // The menu is for the clicked tab, not the active one.
        assert_eq!(LAST_ITEM.with(Cell::get), Some(first));
    }

    #[gpui::test]
    async fn without_the_builder_upstreams_menu_is_kept(cx: &mut TestAppContext) {
        right_click_first_tab(cx, false, true).await;
        assert_eq!(DEPLOYED.with(Cell::get), 0);
    }

    #[gpui::test]
    async fn the_setting_switches_the_menu_off(cx: &mut TestAppContext) {
        right_click_first_tab(cx, true, false).await;
        assert_eq!(DEPLOYED.with(Cell::get), 0);
    }

    async fn two_tabs(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<Pane>,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let project = Project::test(FakeFs::new(cx.executor()), None, cx).await;
        // A multi-workspace window, which is what attaches workspace actions in tests.
        let (multi, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            for label in ["a.php", "b.php"] {
                let item = Box::new(cx.new(|cx| TestItem::new(cx).with_label(label)));
                pane.add_item(item, true, true, None, window, cx);
            }
        });
        cx.run_until_parked();
        (workspace, pane, cx)
    }

    fn ctx_for(
        workspace: &Entity<Workspace>,
        pane: &Entity<Pane>,
        ix: usize,
        project_path: Option<ProjectPath>,
        cx: &mut gpui::VisualTestContext,
    ) -> Ctx {
        Ctx {
            item_id: pane.read_with(cx, |pane, _| pane.items().nth(ix).unwrap().item_id()),
            project: None,
            project_path,
            workspace: workspace.downgrade(),
        }
    }

    #[gpui::test]
    async fn close_on_a_background_tab_closes_that_tab(cx: &mut TestAppContext) {
        let (workspace, pane, cx) = two_tabs(cx).await;
        // Tab 1 is active; the menu was opened on tab 0.
        assert_eq!(pane.read_with(cx, |pane, _| pane.active_item_index()), 1);
        let ctx = ctx_for(&workspace, &pane, 0, None, cx);
        let kept = pane.read_with(cx, |pane, _| pane.items().nth(1).unwrap().item_id());
        let close = Cmd::Dispatch(Rc::new(CloseActiveItem {
            save_intent: Some(workspace::SaveIntent::Skip),
            close_pinned: true,
        }));
        pane.update_in(cx, |pane, window, cx| run(&close, &ctx, pane, window, cx));
        cx.run_until_parked();
        let left = pane.read_with(cx, |pane, _| {
            pane.items().map(|item| item.item_id()).collect::<Vec<_>>()
        });
        assert_eq!(left, [kept]);
    }

    #[gpui::test]
    async fn a_tab_that_is_gone_is_left_alone(cx: &mut TestAppContext) {
        let (workspace, pane, cx) = two_tabs(cx).await;
        let ctx = ctx_for(&workspace, &pane, 0, None, cx);
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(ctx.item_id, workspace::SaveIntent::Skip, window, cx)
                .detach();
        });
        cx.run_until_parked();
        let close = Cmd::Dispatch(Rc::new(CloseActiveItem {
            save_intent: Some(workspace::SaveIntent::Skip),
            close_pinned: true,
        }));
        pane.update_in(cx, |pane, window, cx| run(&close, &ctx, pane, window, cx));
        cx.run_until_parked();
        assert_eq!(pane.read_with(cx, |pane, _| pane.items_len()), 1);
    }

    thread_local! {
        static HISTORY_FOR: std::cell::RefCell<Option<ProjectPath>> = const { std::cell::RefCell::new(None) };
    }

    #[gpui::test]
    async fn file_history_uses_the_clicked_tabs_path(cx: &mut TestAppContext) {
        let (workspace, pane, cx) = two_tabs(cx).await;
        cx.update(|_, cx| {
            git_ui_core::set_file_history_opener(
                |workspace, path, _, cx| {
                    HISTORY_FOR.with(|slot| *slot.borrow_mut() = Some(path.clone()));
                    // As the real opener does: it opens or activates an item in the active pane.
                    workspace.active_pane().update(cx, |_, _| {});
                },
                cx,
            );
        });
        let path = ProjectPath {
            worktree_id: project::WorktreeId::from_usize(1),
            path: util::rel_path::rel_path("src/a.php").into(),
        };
        let ctx = ctx_for(&workspace, &pane, 0, Some(path.clone()), cx);
        pane.update_in(cx, |pane, window, cx| {
            run(&Cmd::FileHistory, &ctx, pane, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(HISTORY_FOR.with(|slot| slot.borrow().clone()), Some(path));
    }

    gpui::actions!(zenith_tab_menu_test, [Probe]);

    thread_local! {
        static ACTIVE_PANE_AT_PROBE: Cell<Option<EntityId>> = const { Cell::new(None) };
    }

    #[gpui::test]
    async fn a_workspace_action_sees_the_clicked_tabs_pane_as_active(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.observe_new(|workspace: &mut Workspace, _, _| {
                workspace.register_action(|workspace, _: &Probe, _, _| {
                    let id = workspace.active_pane().entity_id();
                    ACTIVE_PANE_AT_PROBE.with(|slot| slot.set(Some(id)));
                });
            })
            .detach();
        });
        let (workspace, first, cx) = two_tabs(cx).await;
        let second = workspace.update_in(cx, |workspace, window, cx| {
            workspace.split_pane(first.clone(), workspace::SplitDirection::Right, window, cx)
        });
        second.update_in(cx, |pane, window, cx| {
            let item = Box::new(cx.new(|cx| TestItem::new(cx).with_label("c.php")));
            pane.add_item(item, true, true, None, window, cx);
        });
        first.update_in(cx, |pane, window, cx| {
            window.focus(&pane.focus_handle(cx), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            workspace.read_with(cx, |w, _| w.active_pane().entity_id()),
            first.entity_id()
        );
        // The menu was opened on the only tab of the other, inactive pane.
        let ctx = ctx_for(&workspace, &second, 0, None, cx);
        let probe = Cmd::Dispatch(Rc::new(Probe));
        second.update_in(cx, |pane, window, cx| run(&probe, &ctx, pane, window, cx));
        cx.run_until_parked();
        assert_eq!(
            ACTIVE_PANE_AT_PROBE.with(Cell::get),
            Some(second.entity_id())
        );
    }
}
