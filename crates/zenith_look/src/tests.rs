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
