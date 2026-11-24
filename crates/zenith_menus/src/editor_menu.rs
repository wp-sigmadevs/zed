//! The editor's right-click menu in PhpStorm's layout (plan 44 item 2).
//!
//! Installed through `Editor::set_custom_context_menu`, which makes upstream skip its own
//! full-mode check and its "move the cursor to the click" step, so both are redone here.
//! Actions are looked up by name: one that is not registered (its feature is compiled out)
//! is left out of the menu, and a submenu left empty is left out too.

use std::rc::Rc;

use editor::{DisplayPoint, Editor, SelectionEffects};
use gpui::{Action, App, Context, Entity, FocusHandle, Subscription, Window};
use project::DisableAiSettings;
use ui::ContextMenu;

/// One row of the menu layout: submenus are one level deep, items are `(label, action name)`.
pub(crate) enum Spec {
    Item(&'static str, &'static str),
    Menu(&'static str, Vec<(&'static str, &'static str)>),
    Separator,
}

/// A row of the menu once its action names are resolved.
#[derive(Debug, PartialEq)]
pub(crate) enum Entry<T> {
    Item(&'static str, T),
    Menu(&'static str, Vec<(&'static str, T)>),
    Separator,
}

/// Resolves every name, drops unresolved items and submenus left empty, and collapses
/// separators so none leads, trails or doubles.
pub(crate) fn entries<T>(
    spec: &[Spec],
    mut resolve: impl FnMut(&str) -> Option<T>,
) -> Vec<Entry<T>> {
    let mut out = Vec::new();
    let mut separator_due = false;
    for row in spec {
        let entry = match row {
            Spec::Separator => {
                separator_due = !out.is_empty();
                continue;
            }
            Spec::Item(label, name) => resolve(name).map(|action| Entry::Item(label, action)),
            Spec::Menu(label, items) => {
                let items: Vec<_> = items
                    .iter()
                    .filter_map(|(label, name)| Some((*label, resolve(name)?)))
                    .collect();
                (!items.is_empty()).then_some(Entry::Menu(label, items))
            }
        };
        if let Some(entry) = entry {
            if std::mem::take(&mut separator_due) {
                out.push(Entry::Separator);
            }
            out.push(entry);
        }
    }
    out
}

pub(crate) fn spec() -> Vec<Spec> {
    use Spec::{Item, Menu, Separator};
    vec![
        Item("Show Context Actions", "editor::ToggleCodeActions"),
        Menu(
            "AI Actions",
            vec![
                ("Add to Thread", "agent::AddSelectionToThread"),
                ("Inline Assist", "assistant::InlineAssist"),
            ],
        ),
        Separator,
        Item("Cut", "editor::Cut"),
        Item("Copy", "editor::Copy"),
        Item("Paste", "editor::Paste"),
        Menu(
            "Copy / Paste Special",
            vec![
                ("Copy and Trim", "editor::CopyAndTrim"),
                ("Copy Permalink to Line", "editor::CopyPermalinkToLine"),
                ("Copy File Name", "editor::CopyFileName"),
                ("Copy File Location", "editor::CopyFileLocation"),
            ],
        ),
        Separator,
        Menu(
            "String Manipulation",
            vec![
                ("To Upper Case", "editor::ConvertToUpperCase"),
                ("To Lower Case", "editor::ConvertToLowerCase"),
                ("Toggle Case", "editor::ToggleCase"),
                ("To Camel Case", "editor::ConvertToLowerCamelCase"),
                ("To Pascal Case", "editor::ConvertToUpperCamelCase"),
                ("To Snake Case", "editor::ConvertToSnakeCase"),
                ("To Kebab Case", "editor::ConvertToKebabCase"),
                ("To Title Case", "editor::ConvertToTitleCase"),
                ("To Sentence Case", "editor::ConvertToSentenceCase"),
                ("Sort Lines", "editor::SortLinesCaseSensitive"),
                (
                    "Sort Lines Ignoring Case",
                    "editor::SortLinesCaseInsensitive",
                ),
                ("Sort Lines by Length", "editor::SortLinesByLength"),
                ("Reverse Lines", "editor::ReverseLines"),
                ("Shuffle Lines", "editor::ShuffleLines"),
                ("Remove Duplicate Lines", "editor::UniqueLinesCaseSensitive"),
            ],
        ),
        Separator,
        Menu(
            "Go To",
            vec![
                ("Definition", "editor::GoToDefinition"),
                ("Declaration", "editor::GoToDeclaration"),
                ("Type Declaration", "editor::GoToTypeDefinition"),
                ("Implementation", "editor::GoToImplementation"),
                ("Usages", "editor::FindAllReferences"),
                ("Incoming Calls", "call_hierarchy::ShowIncomingCalls"),
                ("Outgoing Calls", "call_hierarchy::ShowOutgoingCalls"),
                ("Class...", "zenith_search::GoToClass"),
                ("File...", "zenith_search::GoToFile"),
                ("Symbol...", "zenith_search::GoToSymbol"),
                ("Line...", "go_to_line::Toggle"),
            ],
        ),
        Separator,
        Menu(
            "Folding",
            vec![
                ("Fold", "editor::Fold"),
                ("Unfold", "editor::UnfoldLines"),
                ("Toggle Fold", "editor::ToggleFold"),
                ("Fold Recursively", "editor::FoldRecursive"),
                ("Unfold Recursively", "editor::UnfoldRecursive"),
                ("Fold Selection", "editor::FoldSelectedRanges"),
                ("Fold All", "editor::FoldAll"),
                ("Unfold All", "editor::UnfoldAll"),
            ],
        ),
        Separator,
        Item("Rename...", "editor::Rename"),
        Menu(
            "Refactor",
            vec![
                ("Refactor This...", "zenith_refactor::RefactorThis"),
                ("Extract Method...", "zenith_refactor::ExtractMethod"),
                ("Extract Variable...", "zenith_refactor::ExtractVariable"),
                ("Change Signature...", "zenith_refactor::ChangeSignature"),
                ("Move Class...", "zenith_refactor::MoveClass"),
                ("Safe Delete...", "zenith_refactor::SafeDelete"),
            ],
        ),
        Separator,
        Menu(
            "Open In",
            vec![
                (
                    ui::utils::reveal_in_file_manager_label(false),
                    "editor::RevealInFileManager",
                ),
                ("Terminal", "workspace::OpenInTerminal"),
            ],
        ),
        Separator,
        Menu(
            "Local History",
            vec![
                ("Show History", "zenith_local_history::ShowFileHistory"),
                (
                    "Recover Deleted Files...",
                    "zenith_local_history::RecoverDeletedFiles",
                ),
            ],
        ),
        Menu(
            "Git",
            vec![
                ("File History", "git::FileHistory"),
                ("Blame", "git::Blame"),
                ("Copy Permalink to Line", "editor::CopyPermalinkToLine"),
            ],
        ),
        Separator,
        Item(
            "Compare with Clipboard",
            "editor::DiffClipboardWithSelection",
        ),
    ]
}

/// Names the agent panel owns; left out when AI is disabled for the buffer.
fn is_ai_action(name: &str) -> bool {
    name.starts_with("agent::") || name.starts_with("assistant::")
}

/// The custom context menu: moves the cursor under the click the way upstream's default menu
/// does, then builds the menu.
pub(crate) fn deploy(
    editor: &mut Editor,
    point: DisplayPoint,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Entity<ContextMenu>> {
    #[cfg(test)]
    tests::DEPLOYED.with(|n| n.set(n.get() + 1));

    let display_map = editor.display_snapshot(cx);
    let inside_selection = editor
        .selections
        .all_display(&display_map)
        .iter()
        .any(|selection| (selection.start..selection.end).contains(&point));
    if !inside_selection {
        // The cursor goes to the click so the dispatched actions act on it.
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_display_ranges([point..point]);
        });
    }

    let ai_disabled = DisableAiSettings::is_ai_disabled_for_buffer(
        editor.buffer().read(cx).as_singleton().as_ref(),
        cx,
    );
    let rows = entries(&spec(), |name| {
        if ai_disabled && is_ai_action(name) {
            return None;
        }
        cx.build_action(name, None).ok()
    });
    let focus = window.focused(cx);
    Some(ContextMenu::build(window, cx, move |menu, _, _| {
        let menu = fill(
            menu.on_blur_subscription(Subscription::new(|| {})),
            rows,
            &focus,
        );
        with_context(menu, &focus)
    }))
}

fn with_context(menu: ContextMenu, focus: &Option<FocusHandle>) -> ContextMenu {
    match focus {
        Some(focus) => menu.context(focus.clone()),
        None => menu,
    }
}

fn fill(
    mut menu: ContextMenu,
    rows: Vec<Entry<Box<dyn Action>>>,
    focus: &Option<FocusHandle>,
) -> ContextMenu {
    for row in rows {
        menu = match row {
            Entry::Separator => menu.separator(),
            Entry::Item(label, action) => menu.action(label, action),
            Entry::Menu(label, items) => {
                let items = Rc::new(items);
                let focus = focus.clone();
                menu.submenu(label, move |mut sub, _, _| {
                    for (label, action) in items.iter() {
                        sub = sub.action(*label, action.boxed_clone());
                    }
                    with_context(sub, &focus)
                })
            }
        };
    }
    menu
}

/// Gives every full-mode editor that has a project the PhpStorm menu. Editors made later
/// that set their own menu (git panel, message editors) overwrite it.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, _| {
        if editor.mode().is_full() && editor.project().is_some() {
            editor.set_custom_context_menu(deploy);
        }
    })
    .detach();
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        /// How many times `deploy` ran on this thread, so a test can tell the menu is ours.
        pub(crate) static DEPLOYED: Cell<usize> = const { Cell::new(0) };
    }

    fn resolver<'a>(known: &'a [&'a str]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |name| known.contains(&name).then(|| name.to_string())
    }

    #[test]
    fn unresolved_items_and_empty_submenus_are_skipped() {
        let spec = vec![
            Spec::Item("A", "x::A"),
            Spec::Item("B", "x::B"),
            Spec::Menu("M", vec![("C", "x::C"), ("D", "x::D")]),
            Spec::Menu("Gone", vec![("E", "x::E")]),
        ];
        let rows = entries(&spec, resolver(&["x::A", "x::D"]));
        assert_eq!(
            rows,
            vec![
                Entry::Item("A", "x::A".to_string()),
                Entry::Menu("M", vec![("D", "x::D".to_string())]),
            ]
        );
    }

    #[test]
    fn separators_never_lead_trail_or_double() {
        use Spec::{Item, Separator};
        let spec = vec![
            Separator,
            Item("A", "x::A"),
            Separator,
            Item("Gone", "x::Gone"),
            Separator,
            Item("B", "x::B"),
            Separator,
        ];
        let rows = entries(&spec, resolver(&["x::A", "x::B"]));
        assert_eq!(
            rows,
            vec![
                Entry::Item("A", "x::A".to_string()),
                Entry::Separator,
                Entry::Item("B", "x::B".to_string()),
            ]
        );
    }

    #[test]
    fn nothing_resolves_gives_an_empty_menu() {
        assert!(entries(&spec(), resolver(&[])).is_empty());
    }

    #[gpui::test]
    fn the_core_names_resolve(cx: &mut gpui::TestAppContext) {
        // Upstream renaming one of these would silently drop it from the menu.
        let core = [
            "editor::ToggleCodeActions",
            "editor::Cut",
            "editor::Copy",
            "editor::Paste",
            "editor::CopyAndTrim",
            "editor::CopyPermalinkToLine",
            "editor::ConvertToUpperCase",
            "editor::ToggleCase",
            "editor::SortLinesCaseSensitive",
            "editor::ReverseLines",
            "editor::UniqueLinesCaseSensitive",
            "editor::GoToDefinition",
            "editor::GoToDeclaration",
            "editor::GoToTypeDefinition",
            "editor::GoToImplementation",
            "editor::FindAllReferences",
            "call_hierarchy::ShowIncomingCalls",
            "go_to_line::Toggle",
            "editor::Fold",
            "editor::UnfoldLines",
            "editor::FoldAll",
            "editor::UnfoldAll",
            "editor::Rename",
            "editor::RevealInFileManager",
            "workspace::OpenInTerminal",
            "git::FileHistory",
            "git::Blame",
            "agent::AddSelectionToThread",
            "assistant::InlineAssist",
            "editor::DiffClipboardWithSelection",
            "zenith_local_history::ShowFileHistory",
        ];
        cx.update(|cx| {
            for name in core {
                assert!(cx.build_action(name, None).is_ok(), "{name} not registered");
            }
        });
    }

    #[gpui::test]
    fn full_menu_resolves_every_name_upstream_owns(cx: &mut gpui::TestAppContext) {
        let rows = cx.update(|cx| entries(&spec(), |name| cx.build_action(name, None).ok()));
        let labels: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                Entry::Item(label, _) | Entry::Menu(label, _) => Some(*label),
                Entry::Separator => None,
            })
            .collect();
        // Refactor and Go To class/file/symbol come from crates this one does not link.
        for expected in [
            "Show Context Actions",
            "AI Actions",
            "Cut",
            "Copy",
            "Paste",
            "Copy / Paste Special",
            "String Manipulation",
            "Go To",
            "Folding",
            "Rename...",
            "Open In",
            "Git",
            "Compare with Clipboard",
        ] {
            assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
        }
        assert!(!labels.contains(&"Refactor"));
    }

    use editor::{EditorMode, display_map::DisplayRow};
    use gpui::AppContext as _;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use multi_buffer::MultiBuffer;
    use project::Project;
    use workspace::AppState;

    fn init_test(cx: &mut TestAppContext) -> std::sync::Arc<AppState> {
        DEPLOYED.with(|n| n.set(0));
        cx.update(|cx| {
            let state = AppState::test(cx);
            editor::init(cx);
            crate::editor_menu::init(cx);
            state
        })
    }

    async fn editor_with<'a>(
        mode: EditorMode,
        text: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Editor>, &'a mut VisualTestContext) {
        let state = init_test(cx);
        let project = Project::test(state.fs.clone(), [], cx).await;
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let buffer = cx.new(|cx| language::Buffer::local(text, cx));
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
            Editor::new(mode, buffer, Some(project), window, cx)
        });
        (editor, cx)
    }

    fn caret(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (DisplayPoint, DisplayPoint) {
        editor.update_in(cx, |editor, _, cx| {
            let map = editor.display_snapshot(cx);
            let newest = editor.selections.newest_display(&map);
            (newest.start, newest.end)
        })
    }

    #[gpui::test]
    async fn right_click_outside_the_selection_moves_the_cursor_and_builds_the_menu(
        cx: &mut TestAppContext,
    ) {
        let (editor, cx) = editor_with(EditorMode::full(), "one\ntwo\nthree", cx).await;
        let point = DisplayPoint::new(DisplayRow(1), 2);
        let menu = editor.update_in(cx, |editor, window, cx| deploy(editor, point, window, cx));
        assert!(menu.is_some());
        assert_eq!(caret(&editor, cx), (point, point));
    }

    #[gpui::test]
    async fn right_click_inside_the_selection_keeps_it(cx: &mut TestAppContext) {
        let (editor, cx) = editor_with(EditorMode::full(), "one\ntwo\nthree", cx).await;
        let start = DisplayPoint::new(DisplayRow(1), 0);
        let end = DisplayPoint::new(DisplayRow(1), 3);
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |s| {
                s.select_display_ranges([start..end]);
            });
            deploy(editor, DisplayPoint::new(DisplayRow(1), 1), window, cx);
        });
        assert_eq!(caret(&editor, cx), (start, end));
    }

    #[gpui::test]
    async fn a_full_editor_with_a_project_gets_our_menu(cx: &mut TestAppContext) {
        let (editor, cx) = editor_with(EditorMode::full(), "one", cx).await;
        editor.update_in(cx, |editor, window, cx| {
            editor.open_context_menu(&editor::actions::OpenContextMenu, window, cx);
            assert!(editor.has_mouse_context_menu());
        });
        assert_eq!(DEPLOYED.with(Cell::get), 1);
    }

    #[gpui::test]
    async fn an_editor_that_is_not_full_mode_keeps_upstreams_behaviour(cx: &mut TestAppContext) {
        let (editor, cx) = editor_with(EditorMode::SingleLine, "one", cx).await;
        editor.update_in(cx, |editor, window, cx| {
            editor.open_context_menu(&editor::actions::OpenContextMenu, window, cx);
            assert!(!editor.has_mouse_context_menu());
        });
        assert_eq!(DEPLOYED.with(Cell::get), 0);
    }
}
