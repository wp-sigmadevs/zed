use super::*;
use std::collections::HashSet;

fn bundled() -> &'static dyn AssetSource {
    Box::leak(Box::new(assets::Assets))
}

#[test]
fn test_every_icon_the_theme_names_is_bundled() {
    let assets = bundled();
    let files: HashSet<String> = assets
        .list(ICONS_PREFIX)
        .unwrap()
        .into_iter()
        .map(|path| path[ICONS_PREFIX.len()..].to_string())
        .collect();
    let theme: serde_json::Value = serde_json::from_slice(
        &assets.load(&format!("{ICONS_PREFIX}{THEME_FILE}")).unwrap().unwrap(),
    )
    .unwrap();
    let mut named = Vec::new();
    collect_paths(&theme, &mut named);
    assert!(named.len() > 1000, "{} icon paths", named.len());
    let missing: Vec<_> = named
        .iter()
        .filter(|path| !files.contains(path.trim_start_matches("./")))
        .collect();
    assert!(missing.is_empty(), "missing: {:?}", &missing[..missing.len().min(5)]);
}

fn collect_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if text.ends_with(".svg") => out.push(text.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_paths(item, out)),
        serde_json::Value::Object(map) => map.values().for_each(|item| collect_paths(item, out)),
        _ => {}
    }
}

#[test]
fn test_install_writes_once_and_loads_with_files_on_disk() {
    let base = tempfile::tempdir().unwrap();
    let root = install(bundled(), base.path()).unwrap();
    assert!(root.join(THEME_FILE).is_file());
    assert!(root.join("icons/files/php-bb80b3.svg").is_file());

    // Installed already: no asset is copied again (listing them would fail here).
    let marker = root.join("icons/files/php-bb80b3.svg");
    fs::write(&marker, "kept").unwrap();
    assert_eq!(install(&NoListing, base.path()).unwrap(), root);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "kept");
    // No temporary folder left behind.
    assert_eq!(fs::read_dir(base.path()).unwrap().count(), 1);

    let registry = ThemeRegistry::new(Box::new(assets::Assets));
    load(&registry, &root).unwrap();
    let theme = registry.get_icon_theme("Archipelago Icons").unwrap();
    let php = &theme.file_icons[&theme.file_suffixes["php"]].path;
    // An absolute path renders as an external image (in colour); an `icons/`
    // path would be an embedded single-colour mask.
    assert!(Path::new(php.as_ref()).is_absolute(), "{php}");
    assert!(Path::new(php.as_ref()).is_file(), "{php}");
}

/// The bundled assets, except that listing them fails.
struct NoListing;

impl AssetSource for NoListing {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        assets::Assets.load(path)
    }

    fn list(&self, _: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        anyhow::bail!("listing not expected")
    }
}

#[test]
fn test_indent_label() {
    assert_eq!(indent_label(4, false), "4 spaces");
    assert_eq!(indent_label(2, false), "2 spaces");
    assert_eq!(indent_label(4, true), "Tab");
}

#[gpui::test]
fn test_indent_item_follows_the_active_editor(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, BorrowAppContext as _};
    use workspace::StatusItemView as _;
    cx.update(|cx| {
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        assets::Assets.load_test_fonts(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    // The pane owns the editor in the app; this root view owns it here.
    let (root, cx) = cx.add_window_view(|window, cx| {
        let editor = cx.new(|cx| editor::Editor::multi_line(window, cx));
        let indicator = cx.new(|cx| {
            let mut indicator = IndentIndicator::default();
            indicator.set_active_pane_item(Some(&editor), window, cx);
            indicator
        });
        Root { editor, indicator }
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("zenith-indent").is_some(), "drawn");
    let indicator = root.read_with(cx, |root, _| root.indicator.clone());
    assert_eq!(indicator.read_with(cx, |item, cx| item.label(cx)).as_deref(), Some("4 spaces"));

    cx.update(|_, cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.all_languages.defaults.hard_tabs = Some(true);
            });
        });
    });
    assert_eq!(indicator.read_with(cx, |item, cx| item.label(cx)).as_deref(), Some("Tab"));
}

struct Root {
    editor: gpui::Entity<editor::Editor>,
    indicator: gpui::Entity<IndentIndicator>,
}

impl gpui::Render for Root {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div()
            .size_full()
            .child(self.editor.clone())
            .child(self.indicator.clone())
    }
}

mod hover_card_tests {
    use crate::hover_card::{self, diagnostic_text};
    use editor::{
        Anchor, DiagnosticRenderer, Editor, EditorSnapshot,
        hover_popover::{
            hover_at,
            zenith_card::{HoverCardDiagnostic, HoverCardSeverity},
        },
        test::editor_lsp_test_context::EditorLspTestContext,
    };
    use futures::StreamExt as _;
    use gpui::{App, AppContext as _, BorrowAppContext as _, Bounds, Context, Pixels, ScrollHandle, SharedString, Window};
    use indoc::indoc;
    use markdown::Markdown;
    use settings::{Settings as _, SettingsStore};
    use std::{ops::Range, sync::Arc, time::Duration};

    struct TestDiagnosticRenderer;

    impl DiagnosticRenderer for TestDiagnosticRenderer {
        fn render_group(
            &self,
            _: Vec<language::DiagnosticEntryRef<'_, text::Point>>,
            _: text::BufferId,
            _: EditorSnapshot,
            _: gpui::WeakEntity<Editor>,
            _: Option<Arc<language::LanguageRegistry>>,
            _: &mut App,
        ) -> Vec<editor::display_map::BlockProperties<Anchor>> {
            Vec::new()
        }

        fn render_hover(
            &self,
            _: Vec<language::DiagnosticEntryRef<'_, text::Point>>,
            _: Range<text::Point>,
            _: text::BufferId,
            _: Option<Arc<language::LanguageRegistry>>,
            cx: &mut App,
        ) -> Option<gpui::Entity<Markdown>> {
            Some(cx.new(|cx| Markdown::new("diagnostic".into(), None, None, cx)))
        }

        fn open_link(&self, _: &mut Editor, _: SharedString, _: &mut Window, _: &mut Context<Editor>) {}
    }

    /// An editor hovered over `test` with BOTH a diagnostic and docs showing,
    /// and the real card renderer registered.
    async fn hovered_with_diagnostic_and_docs(cx: &mut gpui::TestAppContext) -> EditorLspTestContext {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            hover_card::init(cx);
            editor::set_diagnostic_renderer(TestDiagnosticRenderer, cx);
        });
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(indoc! {"
            fn ˇtest() {}
        "});
        let range = lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 7));
        cx.lsp
            .notify::<lsp::notification::PublishDiagnostics>(lsp::PublishDiagnosticsParams {
                uri: cx.buffer_lsp_url.clone(),
                diagnostics: vec![lsp::Diagnostic {
                    range,
                    severity: Some(lsp::DiagnosticSeverity::ERROR),
                    source: Some("phpcs".into()),
                    code: Some(lsp::NumberOrString::String("Sniff.Code".into())),
                    message: "message".into(),
                    ..Default::default()
                }],
                version: None,
            });
        let mut requests =
            cx.set_request_handler::<lsp::request::HoverRequest, _, _>(move |_, _, _| async move {
                Ok(Some(lsp::Hover {
                    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                        kind: lsp::MarkupKind::Markdown,
                        value: "some docs".to_string(),
                    }),
                    range: Some(range),
                }))
            });
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            let anchor = editor.selections.newest_anchor().head();
            hover_at(editor, Some(anchor), None, window, cx)
        });
        let delay = cx.read(|cx: &App| editor::EditorSettings::get_global(cx).hover_popover_delay.0);
        cx.background_executor
            .advance_clock(Duration::from_millis(delay + 100));
        requests.next().await;
        cx.run_until_parked();
        cx.update_editor(|editor, _, _| {
            assert!(editor.hover_state.diagnostic_popover.is_some());
            assert_eq!(editor.hover_state.info_popovers.len(), 1);
        });
        cx
    }

    /// Forgets every popover's bounds, redraws, and returns the bounds the
    /// draw wrote: (diagnostic popover, info popover).
    fn redraw(cx: &mut EditorLspTestContext) -> (Option<Bounds<Pixels>>, Option<Bounds<Pixels>>) {
        cx.update_editor(|editor, _, cx| {
            let state = &editor.hover_state;
            state.diagnostic_popover.iter().for_each(|p| p.last_bounds.set(None));
            state.info_popovers.iter().for_each(|p| p.last_bounds.set(None));
            cx.notify();
        });
        cx.run_until_parked();
        cx.update_editor(|editor, _, _| {
            let state = &editor.hover_state;
            (
                state.diagnostic_popover.as_ref().and_then(|p| p.last_bounds.get()),
                state.info_popovers[0].last_bounds.get(),
            )
        })
    }

    #[gpui::test]
    async fn test_card_merges_diagnostic_and_docs(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered_with_diagnostic_and_docs(cx).await;
        // One card: the diagnostic and the docs share one painted rectangle.
        let (diagnostic, docs) = redraw(&mut cx);
        assert!(diagnostic.is_some(), "the card was painted");
        assert_eq!(diagnostic, docs);
    }

    #[gpui::test]
    async fn test_card_off_uses_upstream(cx: &mut gpui::TestAppContext) {
        let mut cx = hovered_with_diagnostic_and_docs(cx).await;
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(settings::ZenithSettingsContent {
                        hover_card: Some(settings::ZenithHoverCardSettingsContent {
                            enabled: Some(false),
                        }),
                        ..Default::default()
                    });
                });
            });
        });
        // Upstream's two popovers: both painted, each in its own rectangle.
        let (diagnostic, docs) = redraw(&mut cx);
        assert!(diagnostic.is_some() && docs.is_some(), "both popovers painted");
        assert_ne!(diagnostic, docs);
    }

    #[test]
    fn test_diagnostic_text_source_first() {
        let text = |source: Option<&str>, code: Option<&str>| {
            diagnostic_text(&HoverCardDiagnostic {
                severity: HoverCardSeverity::Error,
                source: source.map(|s| s.to_string().into()),
                code: code.map(|c| c.to_string().into()),
                message: "message".into(),
                range_start: Anchor::Min,
                scroll_handle: ScrollHandle::new(),
            })
        };
        assert_eq!(text(Some("phpcs"), Some("Sniff.Code")), "phpcs: Sniff.Code: message");
        assert_eq!(text(None, Some("Sniff.Code")), "Sniff.Code: message");
        assert_eq!(text(Some("phpcs"), None), "phpcs: message");
        assert_eq!(text(None, None), "message");
    }
}

#[test]
fn test_parameter_hints_only_before_literals() {
    use crate::hints::starts_with_literal as literal;
    for argument in [
        "'text', 'domain' )",
        "\"warning\")",
        "`tpl`",
        "<<<EOT",
        "42)",
        "3.14,",
        ".5",
        "-1)",
        "- 2",
        "+7",
        "-.5",
        "0x1F",
        "true)",
        "FALSE,",
        "null )",
        "NULL",
        "[]",
        "['a' => 1]",
        "array()",
        "ARRAY ('a')",
        "  'leading space'",
    ] {
        assert!(literal(argument), "{argument:?} is a literal");
    }
    for argument in [
        "$original_id,",
        "$this->slug)",
        "get_option('x')",
        "SD_EDI_ROOT_FILE)",
        "__FILE__",
        "self::FORMAT",
        "Foo::class",
        "new Foo()",
        "array_merge($a, $b)",
        "nullable_thing",
        "true_value",
        "-$x",
        "-'a'",
        "!$flag",
        "fn() => 1",
        "(int) $x",
        "",
    ] {
        assert!(!literal(argument), "{argument:?} is not a literal");
    }
}

#[gpui::test]
async fn test_look_setting_off_installs_nothing(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let mut store = settings::SettingsStore::test(cx);
        store.update_user_settings(cx, |content| {
            content.zenith = Some(settings::ZenithSettingsContent {
                look: Some(settings::ZenithLookSettingsContent {
                    enabled: Some(false),
                }),
                ..Default::default()
            });
        });
        cx.set_global(store);
    });
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        command_palette_hooks::init(cx);
        init(cx);
        app_state
    });
    let project = project::Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
    cx.run_until_parked();
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    workspace.read_with(cx, |workspace, cx| {
        let status_bar = workspace.status_bar().read(cx);
        assert!(status_bar.item_of_type::<IndentIndicator>().is_none());
    });
}
