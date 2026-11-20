//! PhpStorm-style import folding: when a file opens, its first run of
//! `use` / `import` statements is collapsed to `use ...`. Click the chip or the
//! gutter chevron to expand. Runs once per editor, never overrides folds the
//! editor already has (restored or user-made), and only with the Zenith rails
//! active.

use std::{collections::HashSet, ops::Range};

use editor::{Editor, EditorEvent, EditorMode};
use gpui::{App, Context, EntityId, Global, Window};
use language::{BufferSnapshot, Point};
use workspace::{Workspace, dock::RailsActive};

/// Language name -> syntax node kind of one import statement. Add a row to
/// support another language; the first child of the node must be its keyword.
const IMPORT_KINDS: &[(&str, &str)] = &[
    ("PHP", "namespace_use_declaration"),
    ("JavaScript", "import_statement"),
    ("TypeScript", "import_statement"),
    ("TSX", "import_statement"),
];

/// Editors that have not yet had their one auto-fold decision.
#[derive(Default)]
struct Pending(HashSet<EntityId>);

impl Global for Pending {}

pub fn init(cx: &mut App) {
    cx.set_global(Pending::default());

    cx.observe_new(|editor: &mut Editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        if !cx.has_global::<RailsActive>()
            || !matches!(editor.mode(), EditorMode::Full { .. })
            || !editor.buffer().read(cx).is_singleton()
        {
            return;
        }
        let id = cx.entity_id();
        cx.global_mut::<Pending>().0.insert(id);
        cx.on_release(move |_, cx| {
            cx.global_mut::<Pending>().0.remove(&id);
        })
        .detach();
        // The tree may arrive after the editor is in the workspace.
        cx.subscribe_in(&cx.entity(), window, |editor, _, event, window, cx| {
            if matches!(event, EditorEvent::Reparsed(_)) {
                fold_imports(editor, window, cx);
            }
        })
        .detach();
    })
    .detach();

    // Persisted folds are restored in `Item::added_to_workspace`, which runs
    // before `ItemAdded`: decide only after that.
    cx.observe_new(|_: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(&cx.entity(), window, |_, _, event, window, cx| {
            if let workspace::Event::ItemAdded { item } = event
                && let Some(editor) = item.downcast::<Editor>()
            {
                editor.update(cx, |editor, cx| fold_imports(editor, window, cx));
            }
        })
        .detach();
    })
    .detach();
}

/// The range to fold: from just after the first statement's keyword (and the
/// space after it) to the end of the last statement of the first contiguous
/// run of imports. `None` when there is no run, or it fits on one line.
fn import_fold_range(snapshot: &BufferSnapshot) -> Option<Range<Point>> {
    let (layer, kind) = snapshot.syntax_layers().find_map(|layer| {
        let name = layer.language.name();
        IMPORT_KINDS
            .iter()
            .find(|(language, _)| *language == name.as_ref())
            .map(|(_, kind)| (layer, *kind))
    })?;
    let root = layer.node();
    let mut first = None;
    let mut last = None;
    for index in 0..root.child_count() {
        let child = root.child(index)?;
        if child.kind() == kind {
            first.get_or_insert(child);
            last = Some(child);
        } else if first.is_some() && child.kind() != "comment" {
            break;
        }
    }
    let (first, last) = (first?, last?);
    let keyword = first.child(0).filter(|keyword| !keyword.is_named())?;
    let mut start = keyword.end_byte();
    if snapshot.chars_at(start).next() == Some(' ') {
        start += 1;
    }
    let range = snapshot.offset_to_point(start)..snapshot.offset_to_point(last.end_byte());
    (range.start.row < range.end.row).then_some(range)
}

fn fold_imports(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let id = cx.entity_id();
    if !cx.global::<Pending>().0.contains(&id) || editor.workspace().is_none() {
        return;
    }
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    let snapshot = buffer.read(cx).snapshot();
    // No syntax tree yet: wait for the next `Reparsed`.
    if snapshot.syntax_layers().next().is_none() {
        return;
    }
    cx.global_mut::<Pending>().0.remove(&id);
    let Some(range) = import_fold_range(&snapshot) else {
        return;
    };
    let display = editor.display_snapshot(cx);
    let has_folds = display
        .folds_in_range(Point::zero()..display.buffer_snapshot().max_point())
        .next()
        .is_some();
    let cursor_inside = editor
        .selections
        .all::<Point>(&display)
        .iter()
        .any(|selection| selection.start <= range.end && selection.end >= range.start);
    if has_folds || cursor_inside {
        return;
    }
    editor.fold_ranges(vec![range], false, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext};
    use project::{Project, ProjectPath};
    use serde_json::json;
    use std::sync::Arc;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, MultiWorkspace};

    const THREE: &str = "import a from \"a\";\nimport b from \"b\";\nimport c from \"c\";\n\nconst x = 1;\n";

    /// Opens `a.ts` with `contents`; `before` runs on every new editor first
    /// (stand-in for restored folds / a restored cursor).
    async fn open(
        contents: &str,
        rails: bool,
        before: impl Fn(&mut Editor, &mut Window, &mut Context<Editor>) + 'static,
        cx: &mut TestAppContext,
    ) -> Entity<Editor> {
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            AppState::test(cx);
            editor::init(cx);
            if rails {
                cx.set_global(RailsActive);
            }
            cx.observe_new(move |editor: &mut Editor, window, cx| {
                if let Some(window) = window {
                    before(editor, window, cx);
                }
            })
            .detach();
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "a.ts": contents }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(Arc::new(language::Language::new(
                language::LanguageConfig {
                    name: "TypeScript".into(),
                    matcher: language::LanguageMatcher {
                        path_suffixes: vec!["ts".into()],
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                },
                Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            )))
        });
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .update(cx, |multi, _, _| multi.workspace().clone())
            .unwrap();
        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("a.ts").into(),
        });
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        })
    }

    fn display(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
        editor.update(cx, |editor, cx| editor.display_text(cx))
    }

    fn no_op(_: &mut Editor, _: &mut Window, _: &mut Context<Editor>) {}

    #[gpui::test]
    async fn test_run_of_imports_is_folded_after_the_keyword(cx: &mut TestAppContext) {
        let editor = open(THREE, true, no_op, cx).await;
        assert_eq!(display(&editor, cx), "import ⋯\n\nconst x = 1;\n");
        editor.update(cx, |editor, cx| {
            // Buffer text untouched, not dirty, nothing to undo.
            assert_eq!(editor.text(cx), THREE);
            let buffer = editor.buffer().read(cx).as_singleton().unwrap();
            assert!(!buffer.read(cx).is_dirty());
            assert!(buffer.read(cx).peek_undo_stack().is_none());
        });
    }

    #[gpui::test]
    async fn test_multi_line_group_folds_but_single_line_import_does_not(cx: &mut TestAppContext) {
        let multi = "import {\n  a,\n  b,\n} from \"x\";\nconst y = 2;\n";
        let editor = open(multi, true, no_op, cx).await;
        assert_eq!(display(&editor, cx), "import ⋯\nconst y = 2;\n");
    }

    #[gpui::test]
    async fn test_single_line_import_is_left_alone(cx: &mut TestAppContext) {
        let single = "import a from \"a\";\nconst y = 2;\n";
        let editor = open(single, true, no_op, cx).await;
        assert_eq!(display(&editor, cx), single);
    }

    #[gpui::test]
    async fn test_existing_folds_are_not_overridden(cx: &mut TestAppContext) {
        // Stand-in for folds restored from the database.
        let editor = open(
            THREE,
            true,
            |editor, window, cx| {
                editor.fold_ranges(vec![Point::new(4, 0)..Point::new(4, 12)], false, window, cx);
            },
            cx,
        )
        .await;
        assert_eq!(
            display(&editor, cx),
            "import a from \"a\";\nimport b from \"b\";\nimport c from \"c\";\n\n⋯\n"
        );
    }

    #[gpui::test]
    async fn test_cursor_inside_the_run_blocks_the_fold(cx: &mut TestAppContext) {
        let editor = open(
            THREE,
            true,
            |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |s| {
                    s.select_ranges([Point::new(1, 3)..Point::new(1, 3)])
                });
            },
            cx,
        )
        .await;
        assert_eq!(display(&editor, cx), THREE);
    }

    #[gpui::test]
    async fn test_no_refold_after_unfold_and_edit(cx: &mut TestAppContext) {
        let editor = open(THREE, true, no_op, cx).await;
        assert_eq!(display(&editor, cx), "import ⋯\n\nconst x = 1;\n");
        editor.update(cx, |editor, cx| {
            editor.unfold_ranges(&[Point::new(0, 0)..Point::new(2, 18)], true, false, cx);
        });
        editor.update(cx, |editor, cx| {
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "// hi\n")], None, cx)
            });
        });
        cx.run_until_parked();
        assert_eq!(
            display(&editor, cx),
            format!("// hi\n{THREE}"),
            "edit after unfold must not fold again"
        );
    }

    #[gpui::test]
    async fn test_nothing_without_rails(cx: &mut TestAppContext) {
        let editor = open(THREE, false, no_op, cx).await;
        assert_eq!(display(&editor, cx), THREE);
    }

    #[test]
    fn test_import_table_covers_php_and_js_family() {
        let kinds = |name: &str| IMPORT_KINDS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k);
        assert_eq!(kinds("PHP"), Some("namespace_use_declaration"));
        for name in ["JavaScript", "TypeScript", "TSX"] {
            assert_eq!(kinds(name), Some("import_statement"));
        }
    }
}
