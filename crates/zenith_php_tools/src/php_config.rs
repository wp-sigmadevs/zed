//! The logic behind "Configure PHP Include Paths…" (plan 39, owner-approved additions (b)):
//! which PHP interpreters exist, the user-only interpreter override, the project's
//! `.phpantom.toml` `[php] version`, the `vendor/*/*` package list and the PHPantom restart.
//! No UI here.
//!
//! PHPantom's own rules (`src/config/mod.rs` and `src/config/indexing.rs` at `b759b41da6`):
//! the key is `[php] version` (a string such as `"8.3"`) and `[indexing] include` (an array
//! of strings); the project file and the global one are merged by `merge_toml`: tables merge
//! key by key, every other value (scalars and arrays) from the project replaces the global one.
//! So a project `include` array replaces Zenith's whole library list. Checked against the
//! `b759b41da6` binary with `phpantom_lsp analyze` (2026-11-23): a project file with only
//! `[php] version` keeps the global `include`; one with `include = [..]` or `[]` drops it.

use futures::{
    StreamExt as _,
    future::{Either, join_all},
};
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, Task};
use language::LanguageServerName;
use lsp::LanguageServerSelector;
use project::{Fs, Project};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use toml_edit::{DocumentMut, Item, Table, Value};
use util::ResultExt as _;

/// The project-root file PHPantom reads.
pub const CONFIG_FILE_NAME: &str = ".phpantom.toml";

/// The language levels the modal offers; PHPantom accepts any `major.minor[.patch]`.
pub const LANGUAGE_LEVELS: &[&str] = &[
    "7.0", "7.1", "7.2", "7.3", "7.4", "8.0", "8.1", "8.2", "8.3", "8.4", "8.5",
];

/// The name PHPantom's language server registers under (`lsp.phpantom` in settings).
const PHPANTOM_SERVER: &str = "phpantom";

// ---------------------------------------------------------------------------------------------
// Interpreters
// ---------------------------------------------------------------------------------------------

/// A PHP binary that answered `php -v`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhpInterpreter {
    pub path: PathBuf,
    /// `8.5.0`.
    pub version: String,
}

/// The version `candidate -v` reports, or `None` when it is not a PHP that runs.
pub(crate) async fn probe_php(candidate: &Path) -> Option<String> {
    if !candidate.is_file() {
        return None;
    }
    let output = util::command::new_command(candidate)
        .arg("-v")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    crate::parse_php_version(&String::from_utf8_lossy(&output.stdout))
}

/// A `php -v` that has not answered after this long is dropped from the list.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Every candidate that answers `php -v` within [`PROBE_TIMEOUT`], in the order given. Probes
/// run side by side; call it from a background task.
pub async fn probe_interpreters(
    executor: &BackgroundExecutor,
    candidates: &[PathBuf],
) -> Vec<PhpInterpreter> {
    probe_all(executor, candidates, |path| async move {
        probe_php(&path).await
    })
    .await
}

/// The second, longer wait `find_php` gives every candidate when none answered: the first
/// `php -v` after an install or a reboot can take longer than [`PROBE_TIMEOUT`] (Gatekeeper
/// scans the binary).
pub(crate) const SLOW_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `probe`'s answer, or `None` when it has not finished after `timeout`. The probe is
/// dropped then, which kills its child (`kill_on_drop`).
pub(crate) async fn within_probe_timeout(
    executor: &BackgroundExecutor,
    timeout: Duration,
    probe: impl Future<Output = Option<String>>,
) -> Option<String> {
    match futures::future::select(Box::pin(probe), executor.timer(timeout)).await {
        Either::Left((version, _)) => version,
        Either::Right(_) => None,
    }
}

async fn probe_all<F: Future<Output = Option<String>>>(
    executor: &BackgroundExecutor,
    candidates: &[PathBuf],
    probe: impl Fn(PathBuf) -> F,
) -> Vec<PhpInterpreter> {
    let versions = join_all(
        candidates
            .iter()
            .map(|path| within_probe_timeout(executor, PROBE_TIMEOUT, probe(path.clone()))),
    )
    .await;
    candidates
        .iter()
        .zip(versions)
        .filter_map(|(path, version)| {
            Some(PhpInterpreter {
                path: path.clone(),
                version: version?,
            })
        })
        .collect()
}

/// The interpreters for the modal's dropdown: the user's override first, then Herd, Homebrew
/// and the `PATH` (the same list the tools use). Runs on the background executor.
pub fn list_interpreters(cx: &App) -> Task<Vec<PhpInterpreter>> {
    let executor = cx.background_executor().clone();
    #[cfg(test)]
    let test_candidates = TEST_CANDIDATES.with(|candidates| candidates.borrow().clone());
    cx.background_spawn(async move {
        #[cfg(test)]
        if let Some(candidates) = test_candidates {
            return probe_interpreters(&executor, &candidates).await;
        }
        probe_interpreters(&executor, &crate::environment().0).await
    })
}

#[cfg(test)]
thread_local! {
    /// Replaces the machine's candidates in [`list_interpreters`] (per test thread).
    pub(crate) static TEST_CANDIDATES: std::cell::RefCell<Option<Vec<PathBuf>>> = Default::default();
}

/// `binary` first in `candidates`, when it is absolute and executable; otherwise unchanged.
pub fn with_override(mut candidates: Vec<PathBuf>, binary: Option<&Path>) -> Vec<PathBuf> {
    if let Some(binary) = binary.filter(|binary| binary.is_absolute() && is_executable(binary)) {
        candidates.retain(|candidate| candidate != binary);
        candidates.insert(0, binary.to_path_buf());
    }
    candidates
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// `zenith.php_tools.php_binary`. It is read from the merged *user* settings: project and
/// local settings files are parsed as `ProjectSettingsContent`, which has no `zenith` section,
/// so a repository cannot set it.
#[derive(Clone, Debug, Default, PartialEq, Eq, RegisterSetting)]
pub struct PhpToolsSettings {
    pub php_binary: Option<PathBuf>,
}

impl Settings for PhpToolsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            php_binary: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.php_tools.as_ref())
                .and_then(|tools| tools.php_binary.as_deref())
                .map(str::trim)
                .filter(|binary| !binary.is_empty())
                .map(PathBuf::from),
        }
    }
}

/// The override `environment()` applies; `environment()` has no `App`, so settings changes
/// are copied here by [`init`].
static PHP_BINARY_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub(crate) fn php_binary_override() -> Option<PathBuf> {
    PHP_BINARY_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn sync_override(cx: &App) {
    let binary = PhpToolsSettings::try_get(cx).and_then(|settings| settings.php_binary.clone());
    *PHP_BINARY_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = binary;
}

pub(crate) fn init(cx: &mut App) {
    sync_override(cx);
    cx.observe_global::<SettingsStore>(|cx| sync_override(cx))
        .detach();
}

// ---------------------------------------------------------------------------------------------
// `.phpantom.toml`
// ---------------------------------------------------------------------------------------------

/// `^\d+\.\d+(\.\d+)?$`.
pub fn is_valid_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none_or(|patch| numeric(Some(patch)))
        && parts.next().is_none()
}

/// What the project's `.phpantom.toml` sets that the modal cares about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectPhpantomConfig {
    /// `[php] version`; `None` is "Auto (composer.json)".
    pub version: Option<String>,
    /// `[indexing] include`. When present (even empty) it replaces Zenith's global library
    /// list for this project, so the Include Path tab should warn.
    pub include: Option<Vec<String>>,
}

pub fn parse_project_config(text: &str) -> anyhow::Result<ProjectPhpantomConfig> {
    let doc: DocumentMut = text.parse()?;
    let version = doc
        .get("php")
        .and_then(Item::as_table_like)
        .and_then(|php| php.get("version"))
        .and_then(Item::as_str)
        .map(str::to_string);
    let include = doc
        .get("indexing")
        .and_then(Item::as_table_like)
        .and_then(|indexing| indexing.get("include"))
        .and_then(Item::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect()
        });
    Ok(ProjectPhpantomConfig { version, include })
}

/// `text` with `[php] version` set to `level`, or removed for `None` (Auto), leaving every other
/// key, comment and the layout as they were. An Auto that empties `[php]` takes the table out
/// too, unless a comment sits above it. A text that does not parse is an error.
pub fn edit_language_level(text: &str, level: Option<&str>) -> anyhow::Result<String> {
    if let Some(level) = level {
        anyhow::ensure!(
            is_valid_version(level),
            "`{level}` is not a PHP version like 8.3 or 8.3.1"
        );
    }
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|error| anyhow::anyhow!("{CONFIG_FILE_NAME} is not valid TOML: {error}"))?;
    let Some(level) = level else {
        remove_version(&mut doc)?;
        return Ok(doc.to_string());
    };
    if doc.get("php").is_none() {
        doc.insert("php", Item::Table(Table::new()));
    }
    let php = doc
        .get_mut("php")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow::anyhow!("`php` in {CONFIG_FILE_NAME} is not a table"))?;
    match php.get_mut("version") {
        // Keep the spacing and any trailing comment of the value being replaced.
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            let mut new = Value::from(level);
            *new.decor_mut() = decor;
            *old = new;
        }
        _ => {
            php.insert("version", toml_edit::value(level));
        }
    }
    Ok(doc.to_string())
}

fn remove_version(doc: &mut DocumentMut) -> anyhow::Result<()> {
    let Some(php) = doc.get_mut("php") else {
        return Ok(());
    };
    let php_table = php
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("`php` in {CONFIG_FILE_NAME} is not a table"))?;
    if php_table.remove("version").is_none() || !php_table.is_empty() {
        return Ok(());
    }
    // `[php]` held only the version: drop it, unless a comment above it is the user's.
    let has_comment = php
        .as_table()
        .and_then(|table| table.decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| prefix.contains('#'));
    if !has_comment {
        doc.remove("php");
    }
    Ok(())
}

/// `path` with links followed; an error when that lands outside `root`. A hard link to an
/// outside file is not caught (it canonicalizes inside); git never creates hard links, so a
/// cloned repo cannot plant one.
async fn resolve_inside_project(fs: &dyn Fs, root: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    let Ok(target) = fs.canonicalize(path).await else {
        return Ok(path.to_path_buf());
    };
    let root = fs
        .canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    anyhow::ensure!(
        target.starts_with(&root),
        "{} is a link to {}, outside the project; not using it",
        path.display(),
        target.display()
    );
    Ok(target)
}

/// `<root>/.phpantom.toml`'s settings; a missing file reads as the defaults.
pub async fn read_project_config(
    fs: &dyn Fs,
    root: &Path,
) -> anyhow::Result<ProjectPhpantomConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    if !fs.is_file(&path).await {
        return Ok(ProjectPhpantomConfig::default());
    }
    let target = resolve_inside_project(fs, root, &path).await?;
    parse_project_config(&fs.load(&target).await?)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// Writes the language level (`None` = Auto) into `<root>/.phpantom.toml`, atomically and only
/// when the text changes. An unparsable file is left as it is and the error says so; Auto with
/// no file does nothing. The caller restarts PHPantom afterwards ([`restart_phpantom`]).
pub async fn write_language_level(
    fs: &dyn Fs,
    root: &Path,
    level: Option<&str>,
) -> anyhow::Result<()> {
    let path = root.join(CONFIG_FILE_NAME);
    let exists = fs.is_file(&path).await;
    if !exists && level.is_none() {
        return Ok(());
    }
    // A symlinked file (dotfiles) is read and written at its target, so the link survives; a
    // target outside the project is refused before anything is read (a cloned repo must not
    // make us read or write elsewhere). A dangling link does not canonicalize and is replaced.
    let target = resolve_inside_project(fs, root, &path).await?;
    let text = if exists {
        fs.load(&target).await?
    } else {
        String::new()
    };
    let edited = edit_language_level(&text, level)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    if edited == text {
        return Ok(());
    }
    fs.atomic_write(target, edited).await
}

/// Restarts PHPantom for the project's open PHP buffers so it re-reads `.phpantom.toml`.
/// A project without an open PHP buffer has nothing to restart: the server reads the file when
/// it starts.
pub fn restart_phpantom(project: &Entity<Project>, cx: &mut App) {
    project.update(cx, |project, cx| {
        let buffers: Vec<_> = project
            .opened_buffers(cx)
            .into_iter()
            .filter(|buffer| {
                buffer
                    .read(cx)
                    .language()
                    .is_some_and(|language| language.name() == "PHP")
            })
            .collect();
        if !buffers.is_empty() {
            project.restart_language_servers_for_buffers(
                buffers,
                std::iter::once(phpantom_selector()).collect(),
                cx,
            );
        }
    });
}

fn phpantom_selector() -> LanguageServerSelector {
    LanguageServerSelector::Name(LanguageServerName::new_static(PHPANTOM_SERVER))
}

// ---------------------------------------------------------------------------------------------
// Vendor packages
// ---------------------------------------------------------------------------------------------

async fn child_dirs(fs: &dyn Fs, dir: &Path) -> Vec<(String, PathBuf)> {
    let Some(mut entries) = fs.read_dir(dir).await.log_err() else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    while let Some(entry) = entries.next().await {
        let Some(path) = entry.log_err() else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if fs.is_dir(&path).await {
            dirs.push((name.to_string(), path));
        }
    }
    dirs
}

/// `vendor/<vendor>/<package>` folders under `root`, as `"<vendor>/<package>"`, in byte order. A
/// missing `vendor` folder gives an empty list.
pub async fn vendor_packages(fs: &dyn Fs, root: &Path) -> Vec<String> {
    let vendor = root.join("vendor");
    if !fs.is_dir(&vendor).await {
        return Vec::new();
    }
    let mut packages = Vec::new();
    for (vendor_name, vendor_dir) in child_dirs(fs, &vendor).await {
        for (package, _) in child_dirs(fs, &vendor_dir).await {
            packages.push(format!("{vendor_name}/{package}"));
        }
    }
    packages.sort();
    packages
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use project::FakeFs;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt as _;

    const USER_FILE: &str = "# my notes\n[diagnostics]\nunused = false # keep\n\n[php]\n# pinned for CI\nversion = \"8.1\" # was 8.0\n\n[indexing]\nexclude = [\"build\"]\n";

    #[test]
    fn test_version_write_keeps_other_keys_and_comments() {
        let edited = edit_language_level(USER_FILE, Some("8.4")).unwrap();
        assert_eq!(
            edited,
            USER_FILE.replace("\"8.1\"", "\"8.4\""),
            "only the value changes"
        );
        // A file without `[php]` gets the table appended; the rest stays byte for byte.
        let without = "# top\n[indexing]\ninclude = [\"/a\"] # mine\n";
        let edited = edit_language_level(without, Some("8.2")).unwrap();
        assert!(edited.starts_with(without), "{edited}");
        assert!(edited.contains("[php]\nversion = \"8.2\""), "{edited}");
    }

    #[test]
    fn test_auto_removes_the_key_and_an_empty_php_table() {
        let edited = edit_language_level(USER_FILE, None).unwrap();
        assert!(!edited.contains("version"), "{edited}");
        // The comment on the key goes with it; every other line is intact.
        assert!(!edited.contains("# pinned for CI"), "{edited}");
        assert!(edited.contains("# my notes"), "{edited}");
        assert!(edited.contains("unused = false # keep"), "{edited}");
        assert!(edited.contains("exclude = [\"build\"]"), "{edited}");

        let ours = "[indexing]\ninclude = [\"/a\"]\n\n[php]\nversion = \"8.3\"\n";
        let edited = edit_language_level(ours, None).unwrap();
        assert!(!edited.contains("php"), "{edited}");
        assert!(edited.contains("include = [\"/a\"]"), "{edited}");

        // A comment above `[php]` is the user's: the empty table stays to carry it.
        let commented = "# the language level\n[php]\nversion = \"8.3\"\n";
        assert_eq!(
            edit_language_level(commented, None).unwrap(),
            "# the language level\n[php]\n"
        );

        // `[php]` with another key keeps it; an Auto on a file with no version changes nothing.
        let other = "[php]\nextra = 1\nversion = \"8.3\"\n";
        assert_eq!(
            edit_language_level(other, None).unwrap(),
            "[php]\nextra = 1\n"
        );
        assert_eq!(
            edit_language_level("[php]\n", None).unwrap(),
            "[php]\n",
            "an empty table that was not ours stays"
        );
    }

    #[test]
    fn test_invalid_toml_and_invalid_versions_are_errors() {
        assert!(edit_language_level("not = [toml", Some("8.3")).is_err());
        assert!(edit_language_level("not = [toml", None).is_err());
        assert!(edit_language_level("php = 3\n", Some("8.3")).is_err());
        for bad in [
            "", "8", "8.", "8.x", "8.3.", "8.3.1.2", " 8.3", "v8.3", "8.3-dev",
        ] {
            assert!(edit_language_level("", Some(bad)).is_err(), "{bad:?}");
        }
        for good in ["7.0", "8.5", "8.3.1", "10.12"] {
            assert!(is_valid_version(good), "{good}");
        }
        assert!(LANGUAGE_LEVELS.iter().all(|level| is_valid_version(level)));
    }

    #[gpui::test]
    async fn test_unparsable_file_is_left_untouched(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/p", json!({ ".phpantom.toml": "not = [toml" }))
            .await;
        let error = write_language_level(fs.as_ref(), Path::new("/p"), Some("8.3"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(".phpantom.toml"), "{error}");
        assert_eq!(
            fs.load(Path::new("/p/.phpantom.toml")).await.unwrap(),
            "not = [toml"
        );
        assert!(
            write_language_level(fs.as_ref(), Path::new("/p"), None)
                .await
                .is_err()
        );
        assert!(
            read_project_config(fs.as_ref(), Path::new("/p"))
                .await
                .is_err()
        );
    }

    #[gpui::test]
    async fn test_symlink_out_of_the_project_is_not_written(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/dot",
            json!({ "outside.toml": "[indexing]\nexclude = [\"x\"]\n", "inside": {} }),
        )
        .await;
        fs.insert_tree("/p", json!({ "composer.json": "{}", "real.toml": "" }))
            .await;
        fs.insert_symlink("/p/.phpantom.toml", PathBuf::from("/dot/outside.toml"))
            .await;
        let error = write_language_level(fs.as_ref(), Path::new("/p"), Some("8.3"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outside the project"), "{error}");
        assert_eq!(
            fs.load(Path::new("/dot/outside.toml")).await.unwrap(),
            "[indexing]\nexclude = [\"x\"]\n"
        );

        // A link that stays inside the project is followed and survives.
        fs.remove_file(Path::new("/p/.phpantom.toml"), Default::default())
            .await
            .unwrap();
        fs.insert_symlink("/p/.phpantom.toml", PathBuf::from("real.toml"))
            .await;
        write_language_level(fs.as_ref(), Path::new("/p"), Some("8.3"))
            .await
            .unwrap();
        assert!(
            fs.load(Path::new("/p/real.toml"))
                .await
                .unwrap()
                .contains("8.3")
        );
        assert!(fs.read_link(Path::new("/p/.phpantom.toml")).await.is_ok());
    }

    #[gpui::test]
    async fn test_symlink_out_of_the_project_is_refused_without_reading(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/dot", json!({ "secret.txt": "TOP-SECRET-LINE\nsecond" }))
            .await;
        fs.insert_tree("/p", json!({ "composer.json": "{}" })).await;
        fs.insert_symlink("/p/.phpantom.toml", PathBuf::from("/dot/secret.txt"))
            .await;
        let read = read_project_config(fs.as_ref(), Path::new("/p"))
            .await
            .unwrap_err()
            .to_string();
        let write = write_language_level(fs.as_ref(), Path::new("/p"), Some("8.3"))
            .await
            .unwrap_err()
            .to_string();
        for error in [read, write] {
            assert!(error.contains("outside the project"), "{error}");
            assert!(!error.contains("TOP-SECRET"), "{error}");
        }
    }

    #[gpui::test]
    async fn test_write_round_trips_through_the_file(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/p", json!({ "composer.json": "{}" })).await;
        let root = Path::new("/p");
        // Auto with no file creates nothing.
        write_language_level(fs.as_ref(), root, None).await.unwrap();
        assert!(!fs.is_file(&root.join(CONFIG_FILE_NAME)).await);

        write_language_level(fs.as_ref(), root, Some("8.2"))
            .await
            .unwrap();
        let config = read_project_config(fs.as_ref(), root).await.unwrap();
        assert_eq!(config.version.as_deref(), Some("8.2"));
        assert_eq!(config.include, None);

        write_language_level(fs.as_ref(), root, None).await.unwrap();
        assert_eq!(
            read_project_config(fs.as_ref(), root).await.unwrap(),
            ProjectPhpantomConfig::default()
        );
        assert_eq!(
            fs.load(&root.join(CONFIG_FILE_NAME)).await.unwrap(),
            "",
            "the `[php]` table we created is gone with its key"
        );
    }

    #[test]
    fn test_project_include_is_detected() {
        let config =
            parse_project_config("[indexing]\ninclude = [\"/lib\", \"vendor/x\"]\n").unwrap();
        assert_eq!(
            config.include,
            Some(vec!["/lib".to_string(), "vendor/x".to_string()])
        );
        assert_eq!(
            parse_project_config("[indexing]\ninclude = []\n")
                .unwrap()
                .include,
            Some(Vec::new()),
            "an empty array still replaces the global list"
        );
        for none in [
            "",
            "[indexing]\nexclude = [\"a\"]\n",
            "[php]\nversion = \"8.3\"\n",
        ] {
            assert_eq!(parse_project_config(none).unwrap().include, None, "{none}");
        }
        assert!(parse_project_config("[indexing\n").is_err());
    }

    #[gpui::test]
    async fn test_vendor_packages_are_sorted_two_level_dirs(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/p",
            json!({
                "vendor": {
                    "autoload.php": "",
                    "bin": { "phpunit": "" },
                    "symfony": { "console": { "src": { "A.php": "" } }, "yaml": {}, "README": "" },
                    "acme": { "zeta": {}, "alpha": { "deep": { "x": {} } } },
                    "acme-extras": { "one": {} },
                },
            }),
        )
        .await;
        assert_eq!(
            vendor_packages(fs.as_ref(), Path::new("/p")).await,
            [
                "acme-extras/one",
                "acme/alpha",
                "acme/zeta",
                "symfony/console",
                "symfony/yaml"
            ],
            "byte order of the whole `vendor/package` string"
        );
        assert!(
            vendor_packages(fs.as_ref(), Path::new("/none"))
                .await
                .is_empty()
        );
    }

    fn php_stub(dir: &Path, name: &str, version: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho 'PHP {version} (cli) (built: Nov 1 2026)'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn test_override_goes_first_when_executable() {
        let dir = tempfile::tempdir().unwrap();
        let mine = php_stub(dir.path(), "mine", "8.4.1", 0o755);
        let plain = php_stub(dir.path(), "plain", "8.4.1", 0o644);
        let herd = PathBuf::from("/herd/php");
        let brew = PathBuf::from("/brew/php");
        let list = vec![herd.clone(), brew.clone(), mine.clone()];

        assert_eq!(
            with_override(list.clone(), Some(&mine)),
            [mine.clone(), herd, brew],
            "moved to the front, not duplicated"
        );
        assert_eq!(with_override(list.clone(), None), list);
        assert_eq!(
            with_override(list.clone(), Some(&plain)),
            list,
            "not executable"
        );
        assert_eq!(
            with_override(list.clone(), Some(Path::new("/nope/php"))),
            list,
            "missing"
        );
        assert_eq!(
            with_override(list.clone(), Some(Path::new("php"))),
            list,
            "relative paths are ignored"
        );
    }

    #[gpui::test]
    async fn test_probe_keeps_only_candidates_that_answer(cx: &mut TestAppContext) {
        // Real processes run on real threads.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let good = php_stub(dir.path(), "php84", "8.4.1", 0o755);
        let older = php_stub(dir.path(), "php74", "7.4.33", 0o755);
        let broken = dir.path().join("broken");
        std::fs::write(&broken, "#!/bin/sh\necho 'not php'\n").unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = probe_interpreters(
            &cx.executor(),
            &[
                good.clone(),
                dir.path().join("missing"),
                broken,
                older.clone(),
            ],
        )
        .await;
        assert_eq!(
            found,
            [
                PhpInterpreter {
                    path: good,
                    version: "8.4.1".into()
                },
                PhpInterpreter {
                    path: older,
                    version: "7.4.33".into()
                },
            ]
        );
    }

    #[gpui::test]
    async fn test_a_hanging_probe_is_dropped_after_the_timeout(cx: &mut TestAppContext) {
        let candidates = [
            PathBuf::from("/hangs/php"),
            PathBuf::from("/fine/php"),
            PathBuf::from("/also-hangs/php"),
        ];
        let executor = cx.executor();
        let mut task = cx.background_spawn({
            let executor = executor.clone();
            async move {
                probe_all(&executor, &candidates, |path| {
                    let hangs = path.to_string_lossy().contains("hangs");
                    async move {
                        if hangs {
                            futures::future::pending().await
                        } else {
                            Some("8.4.1".to_string())
                        }
                    }
                })
                .await
            }
        });
        cx.run_until_parked();
        // Just short of the timeout the hanging probes are still pending.
        executor.advance_clock(PROBE_TIMEOUT - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(futures::poll!(&mut task).is_pending());
        executor.advance_clock(Duration::from_millis(2));
        assert_eq!(
            task.await,
            [PhpInterpreter {
                path: PathBuf::from("/fine/php"),
                version: "8.4.1".into()
            }]
        );
    }

    #[test]
    fn test_restart_selects_phpantom_by_name() {
        assert_eq!(
            phpantom_selector(),
            LanguageServerSelector::Name("phpantom".into())
        );
    }

    #[gpui::test]
    async fn test_restart_restarts_phpantom_only(cx: &mut TestAppContext) {
        use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher};
        cx.update(|cx| {
            workspace::AppState::test(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/p", json!({ "a.php": "<?php\n" })).await;
        let project = Project::test(fs, [Path::new("/p")], cx).await;
        let registry = project.read_with(cx, |project, _| project.languages().clone());
        registry.add(std::sync::Arc::new(Language::new(
            LanguageConfig {
                name: "PHP".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["php".into()],
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
            None,
        )));
        let mut phpantom = registry.register_fake_lsp(
            "PHP",
            FakeLspAdapter {
                name: "phpantom",
                ..Default::default()
            },
        );
        let mut other = registry.register_fake_lsp(
            "PHP",
            FakeLspAdapter {
                name: "other-php-server",
                ..Default::default()
            },
        );
        let (_buffer, _handle) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(Path::new("/p/a.php"), cx)
            })
            .await
            .unwrap();
        let first = phpantom.next().await.expect("phpantom starts");
        let other_server = other.next().await.expect("the other server starts");
        let mut phpantom_shutdown =
            first.set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| async { Ok(()) });
        let mut other_shutdown = other_server
            .set_request_handler::<lsp::request::Shutdown, _, _>(|_, _| async { Ok(()) });

        cx.update(|cx| restart_phpantom(&project, cx));
        phpantom_shutdown.next().await.expect("phpantom is stopped");
        phpantom.next().await.expect("and started again");
        cx.run_until_parked();
        assert!(
            other_shutdown.try_recv().is_err(),
            "other PHP servers keep running"
        );
        assert!(other.try_recv().is_err(), "and are not started again");
    }

    fn init_user_settings(cx: &mut TestAppContext, php_binary: Option<&str>) {
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store.update_user_settings(cx, |content| {
                content
                    .zenith
                    .get_or_insert_default()
                    .php_tools
                    .get_or_insert_default()
                    .php_binary = php_binary.map(str::to_string);
            });
            cx.set_global(store);
        });
    }

    #[gpui::test]
    fn test_override_is_read_from_user_settings(cx: &mut TestAppContext) {
        init_user_settings(cx, Some(" /opt/php/bin/php "));
        cx.update(|cx| {
            assert_eq!(
                PhpToolsSettings::get_global(cx).php_binary.as_deref(),
                Some(Path::new("/opt/php/bin/php"))
            );
        });
        init_user_settings(cx, Some("  "));
        cx.update(|cx| assert_eq!(PhpToolsSettings::get_global(cx).php_binary, None));
    }

    #[gpui::test]
    fn test_override_in_project_settings_is_ignored(cx: &mut TestAppContext) {
        const PROJECT: &str = r#"{ "zenith": { "php_tools": { "php_binary": "/tmp/evil/php" } } }"#;
        for user in [None, Some("/usr/local/bin/php")] {
            init_user_settings(cx, user);
            cx.update(|cx| {
                let worktree_id = settings::WorktreeId::from_usize(1);
                SettingsStore::update_global(cx, |store, cx| {
                    store
                        .set_local_settings(
                            worktree_id,
                            settings::LocalSettingsPath::InWorktree(
                                util::rel_path::RelPath::empty().into(),
                            ),
                            settings::LocalSettingsKind::Settings,
                            Some(PROJECT),
                            cx,
                        )
                        .unwrap();
                });
                let location = settings::SettingsLocation {
                    worktree_id,
                    path: util::rel_path::RelPath::empty(),
                };
                let expected = user.map(PathBuf::from);
                assert_eq!(
                    PhpToolsSettings::get(Some(location), cx).php_binary,
                    expected
                );
                assert_eq!(PhpToolsSettings::get_global(cx).php_binary, expected);
                sync_override(cx);
                assert_eq!(php_binary_override(), expected);
            });
        }
        cx.update(|_| *PHP_BINARY_OVERRIDE.lock().unwrap() = None);
    }
}
