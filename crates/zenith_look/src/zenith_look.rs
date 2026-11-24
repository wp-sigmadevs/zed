//! Zenith's look (plan 34 step 1): the Archipelago Icons theme ships inside the
//! app. Icons bundled as assets render single-colour (embedded SVGs are masks),
//! so the theme is written to the data dir once and loaded from there, the way
//! an installed extension's is.

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, AssetSource};
use std::{
    fs,
    path::{Path, PathBuf},
};
use theme::ThemeRegistry;
use util::ResultExt as _;
use workspace::Workspace;

mod bulb;
mod hints;
mod hover_card;
mod indent;
mod palette;
pub use indent::{IndentIndicator, indent_label};

pub const ICONS_PREFIX: &str = "icon_themes/archipelago-icons/";
const THEME_FILE: &str = "icon_themes/archipelago-icons.json";

/// `zenith.look.enabled`: read once at init, so a change takes effect after a restart.
#[derive(Clone, Copy, settings::RegisterSetting)]
pub struct LookSettings {
    pub enabled: bool,
}

impl settings::Settings for LookSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let look = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.look.as_ref());
        Self {
            enabled: look.and_then(|look| look.enabled).unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    // Branding hiding is fork-wide, not part of the optional look.
    palette::init(cx);
    if !<LookSettings as settings::Settings>::get_global(cx).enabled {
        return;
    }
    bulb::init(cx);
    hints::init(cx);
    hover_card::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let item = cx.new(|_| IndentIndicator::default());
        workspace
            .status_bar()
            .update(cx, |bar, cx| bar.add_right_item(item, window, cx));
    })
    .detach();
    let assets = cx.asset_source().clone();
    let registry = ThemeRegistry::global(cx);
    let base = paths::data_dir().join("icon_themes");
    cx.spawn(async move |cx| {
        let loaded = cx
            .background_executor()
            .spawn(async move {
                let root = install(assets.as_ref(), &base)?;
                load(&registry, &root)
            })
            .await;
        if loaded.log_err().is_some() {
            cx.update(theme_settings::reload_icon_theme);
        }
    })
    .detach();
}

/// Writes the bundled theme under `base/archipelago-icons-<version>` unless that
/// version is already there, and returns its folder. Written to a temporary
/// folder first and renamed, so a crash never leaves half a theme behind.
pub fn install(assets: &dyn AssetSource, base: &Path) -> Result<PathBuf> {
    let manifest = assets
        .load(&format!("{ICONS_PREFIX}extension.toml"))?
        .context("bundled icon theme missing")?;
    let version = std::str::from_utf8(&manifest)?
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .map(|version| version.trim_matches('"').to_string())
        .context("icon theme version")?;
    let root = base.join(format!("archipelago-icons-{version}"));
    if root.join(THEME_FILE).is_file() {
        return Ok(root);
    }
    let temp = base.join(format!(".archipelago-icons-{version}-{}", std::process::id()));
    for path in assets.list(ICONS_PREFIX)? {
        let target = temp.join(&path[ICONS_PREFIX.len()..]);
        let contents = assets.load(&path)?.with_context(|| format!("asset {path}"))?;
        fs::create_dir_all(target.parent().context("asset path")?)?;
        fs::write(&target, contents)?;
    }
    if let Err(error) = fs::rename(&temp, &root) {
        fs::remove_dir_all(&temp).ok();
        // Another Zenith window finished first.
        if !root.join(THEME_FILE).is_file() {
            return Err(error).context("installing icon theme");
        }
    }
    Ok(root)
}

/// Icon paths resolve against `root`, so they are files on disk and keep their colours.
pub fn load(registry: &ThemeRegistry, root: &Path) -> Result<()> {
    let theme_path = root.join(THEME_FILE);
    let contents = fs::read(&theme_path)?;
    registry.load_icon_theme(&theme_path, root, contents)
}

#[cfg(test)]
mod tests;
