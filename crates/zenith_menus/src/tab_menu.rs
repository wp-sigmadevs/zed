//! The tab right-click menu in PhpStorm's layout (plan 44 item 1).
//!
//! `workspace` cannot depend on this crate, so `init` registers a builder with
//! `workspace::set_tab_menu_builder`; the pane asks it for a menu on every right-click and keeps
//! upstream's menu when it returns `None` (the setting is off). Every item runs on the
//! right-clicked tab: the tab is activated first, then the action is dispatched from the pane.
//! Items that upstream hides or greys out (no file, no repo, nothing to the left) do the same
//! here. Submenus are one level deep.

use std::{path::PathBuf, rc::Rc};

use gpui::{Action, App, Context, Entity, SharedString, Window};
use settings::Settings as _;
use ui::{ContextMenu, ContextMenuEntry, ContextMenuItem};
use workspace::{
    CloseActiveItem, CloseAllItems, CloseCleanItems, CloseItemsToTheLeft, CloseItemsToTheRight,
    CloseMultibufferItems, CloseOtherItems, OpenTerminal, Pane, ReopenClosedItem,
    RevealInProjectPanel, SplitDown, SplitRight, TabMenuTarget, TogglePinTab,
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
    }
}

fn entry(label: impl Into<SharedString>, action: impl Action, disabled: bool) -> Entry {
    Entry {
        label: label.into(),
        cmd: Cmd::Dispatch(Rc::new(action)),
        disabled,
    }
}

fn named(
    label: &'static str,
    name: &str,
    resolve: &dyn Fn(&str) -> Option<Box<dyn Action>>,
) -> Option<Entry> {
    let action: Rc<dyn Action> = resolve(name)?.into();
    Some(Entry {
        label: label.into(),
        cmd: Cmd::Dispatch(action),
        disabled: false,
    })
}

fn menu(label: &'static str, entries: Vec<Entry>) -> Row {
    Row::Menu(label, entries)
}

/// The menu in order. `resolve` looks an action up by name (git actions live in a crate this
/// one does not link); a name that does not resolve leaves its item out.
pub(crate) fn rows(f: &Facts, resolve: &dyn Fn(&str) -> Option<Box<dyn Action>>) -> Vec<Row> {
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
        copies.extend(named("File Permalink", "git::CopyFilePermalink", resolve));
    }
    rows.push(menu("Copy Path/Reference", copies));

    let mut open_in = Vec::new();
    if let (true, Some(path)) = (f.is_local, &f.abs_path) {
        open_in.push(Entry {
            label: ui::utils::reveal_in_file_manager_label(false).into(),
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
        open_in.extend(named("File Permalink", "git::OpenFilePermalink", resolve));
    }
    rows.push(menu("Open In", open_in));

    let history = (f.abs_path.is_some())
        .then(|| {
            named(
                "Show History",
                "zenith_local_history::ShowFileHistory",
                resolve,
            )
        })
        .flatten();
    rows.push(menu("Local History", history.into_iter().collect()));
    let git_history = f
        .has_git_repo
        .then(|| named("File History", "git::FileHistory", resolve))
        .flatten();
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

fn run(cmd: &Cmd, ix: usize, pane: &mut Pane, window: &mut Window, cx: &mut Context<Pane>) {
    match cmd {
        Cmd::Dispatch(action) => {
            pane.activate_item(ix, true, false, window, cx);
            window.focus(&gpui::Focusable::focus_handle(pane, cx), cx);
            window.dispatch_action(action.boxed_clone(), cx);
        }
        Cmd::Copy(text) => cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone())),
        Cmd::Reveal(path) => cx.reveal_path(path),
        Cmd::ToggleReadOnly => {
            if let Some(item) = pane.item_for_index(ix) {
                item.toggle_read_only(window, cx);
            }
        }
    }
}

fn item(entry: &Entry, pane: &Entity<Pane>, ix: usize, window: &mut Window) -> ContextMenuEntry {
    let cmd = entry.cmd.clone();
    let mut item = ContextMenuEntry::new(entry.label.clone())
        .disabled(entry.disabled)
        .handler(window.handler_for(pane, move |pane, window, cx| {
            run(&cmd, ix, pane, window, cx)
        }));
    if let Cmd::Dispatch(action) = &entry.cmd {
        item = item.action(action.boxed_clone());
    }
    item
}

fn deploy(target: TabMenuTarget, window: &mut Window, cx: &mut App) -> Option<Entity<ContextMenu>> {
    #[cfg(test)]
    tests::DEPLOYED.with(|n| n.set(n.get() + 1));

    let rows = tidy(rows(&facts(&target), &|name| {
        cx.build_action(name, None).ok()
    }));
    let TabMenuTarget {
        pane,
        ix,
        extra_actions,
        menu_context,
        ..
    } = target;
    let rows = Rc::new(rows);
    Some(ContextMenu::build(
        window,
        cx,
        move |mut menu, window, _| {
            for row in rows.iter() {
                menu = match row {
                    Row::Separator => menu.separator(),
                    Row::Item(entry) => {
                        menu.item(ContextMenuItem::Entry(item(entry, &pane, ix, window)))
                    }
                    Row::Menu(label, entries) => {
                        let (entries, pane) = (entries.clone(), pane.clone());
                        menu.submenu(*label, move |mut sub, window, _| {
                            for entry in &entries {
                                sub = sub
                                    .item(ContextMenuItem::Entry(item(entry, &pane, ix, window)));
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

    use gpui::{AppContext as _, BorrowAppContext as _, Modifiers, MouseButton, TestAppContext};
    use project::{FakeFs, Project};
    use workspace::{Workspace, item::test::TestItem};

    use super::*;

    thread_local! {
        /// How many menus `deploy` built on this thread, so a test can tell the menu is ours.
        pub(crate) static DEPLOYED: Cell<usize> = const { Cell::new(0) };
    }

    fn no_names(_: &str) -> Option<Box<dyn Action>> {
        None
    }

    /// Labels of the tidy menu, submenus as `Name>`, separators as `-`; `!` marks a greyed item.
    fn shape(f: &Facts) -> Vec<String> {
        let label = |e: &Entry| format!("{}{}", e.label, if e.disabled { "!" } else { "" });
        tidy(rows(f, &no_names))
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
    fn git_items_need_a_repo_and_a_registered_action() {
        let f = Facts {
            has_git_repo: true,
            ..file_facts()
        };
        let resolve = |name: &str| -> Option<Box<dyn Action>> {
            name.starts_with("git::")
                .then(|| Box::new(TogglePinTab) as Box<dyn Action>)
        };
        let labels: Vec<_> = tidy(rows(&f, &resolve))
            .into_iter()
            .filter_map(|row| match row {
                Row::Menu("Git", es) => Some(es.len()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, [1]);
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

    async fn right_click_first_tab(cx: &mut TestAppContext, register: bool, enabled: bool) {
        DEPLOYED.with(|n| n.set(0));
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
    }

    #[gpui::test]
    async fn the_registered_builder_makes_the_tab_menu(cx: &mut TestAppContext) {
        right_click_first_tab(cx, true, true).await;
        assert_eq!(DEPLOYED.with(Cell::get), 1);
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
}
