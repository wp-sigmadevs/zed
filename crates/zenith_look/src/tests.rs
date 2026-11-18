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
