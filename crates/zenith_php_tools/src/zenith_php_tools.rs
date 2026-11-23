//! PHP project detection and tool resolution (plan 35, milestone 1). Pure
//! functions over paths, so they test against temp directories; nothing here
//! downloads or runs a tool.

pub mod php_config;
mod php_modal;
mod phpantom;
mod string_completions;

pub use string_completions::{StringCompletionSource, add_string_completions};

use http_client::HttpClient;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectKind {
    WordPress,
    Laravel,
    Php,
}

/// WordPress: a plugin or theme header, `wp-config.php` or `wp-content`.
/// Laravel: `artisan` and `laravel/framework` in `composer.json`.
pub fn detect_project(root: &Path) -> ProjectKind {
    if root.join("wp-config.php").exists()
        || root.join("wp-content").is_dir()
        || has_header(&root.join("style.css"), "Theme Name:")
        || top_level_php_files(root).any(|file| has_header(&file, "Plugin Name:"))
    {
        return ProjectKind::WordPress;
    }
    if root.join("artisan").is_file() && composer_requires(root, "laravel/framework") {
        return ProjectKind::Laravel;
    }
    ProjectKind::Php
}

fn top_level_php_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "php"))
}

/// Up to `max` bytes of a regular file, symlinks followed. `None` for a device,
/// pipe, folder or unreadable path: a project can plant `x.php -> /dev/zero`,
/// and an unbounded read of it never ends. The check is on the opened handle, so
/// a path swapped for a pipe cannot slip through; the open is non-blocking (a
/// plain open of a pipe with no writer blocks) and a regular file ignores that.
pub fn read_regular(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// WordPress reads headers from the first 8 KB.
fn has_header(path: &Path, header: &str) -> bool {
    let Some(bytes) = read_regular(path, 8192) else {
        return false;
    };
    String::from_utf8_lossy(&bytes).contains(header)
}

/// A `composer.json` is small; a bigger one is not read.
const COMPOSER_JSON_MAX: u64 = 1024 * 1024;

fn composer_requires(root: &Path, package: &str) -> bool {
    let Some(text) = read_regular(&root.join("composer.json"), COMPOSER_JSON_MAX) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&text) else {
        return false;
    };
    ["require", "require-dev"].iter().any(|section| {
        json.get(section)
            .and_then(|deps| deps.get(package))
            .is_some()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Phpcs,
    Phpcbf,
    Phpstan,
    Pint,
    PhpCsFixer,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Tool::Phpcs => "phpcs",
            Tool::Phpcbf => "phpcbf",
            Tool::Phpstan => "phpstan",
            Tool::Pint => "pint",
            Tool::PhpCsFixer => "php-cs-fixer",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Project,
    Managed,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

/// The project's own `vendor/bin` first, then the Zenith-managed tools for the
/// project's profile, then the system `PATH`.
pub fn resolve(
    project: &Path,
    tool: Tool,
    managed_bin: Option<&Path>,
    path_dirs: &[PathBuf],
) -> Option<Resolved> {
    let found = |path: PathBuf, source| path.is_file().then_some(Resolved { path, source });
    found(
        project.join("vendor/bin").join(tool.binary()),
        Source::Project,
    )
    .or_else(|| managed_bin.and_then(|bin| found(bin.join(tool.binary()), Source::Managed)))
    .or_else(|| {
        path_dirs
            .iter()
            .find_map(|dir| found(dir.join(tool.binary()), Source::System))
    })
}

/// Tool sets Zenith can install for the user, by project type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    WordPress,
    Laravel,
    Php,
}

impl Profile {
    pub fn for_project(kind: ProjectKind) -> Self {
        match kind {
            ProjectKind::WordPress => Profile::WordPress,
            ProjectKind::Laravel => Profile::Laravel,
            ProjectKind::Php => Profile::Php,
        }
    }

    fn dir_name(self) -> &'static str {
        match self {
            Profile::WordPress => "wordpress",
            Profile::Laravel => "laravel",
            Profile::Php => "php",
        }
    }

    /// The `composer.json` Zenith installs from (`profiles/<name>/`).
    pub fn composer_manifest(self) -> &'static str {
        match self {
            Profile::WordPress => include_str!("../profiles/wordpress/composer.json"),
            Profile::Laravel => include_str!("../profiles/laravel/composer.json"),
            Profile::Php => include_str!("../profiles/php/composer.json"),
        }
    }

    /// The lock that pins every version, so an install gives the same tools each time.
    /// Regenerate with `composer update --no-install` in `profiles/<name>/` when
    /// bumping the set.
    pub fn composer_lock(self) -> &'static str {
        match self {
            Profile::WordPress => include_str!("../profiles/wordpress/composer.lock"),
            Profile::Laravel => include_str!("../profiles/laravel/composer.lock"),
            Profile::Php => include_str!("../profiles/php/composer.lock"),
        }
    }

    pub fn composer_json(self) -> serde_json::Value {
        serde_json::from_str(self.composer_manifest()).unwrap_or_default()
    }
}

/// `~/.local/share/zenith/tools/php`: where managed tools live, never anywhere a
/// user's own Composer, `PATH` or shell profile reads.
pub fn managed_root(home: &Path) -> PathBuf {
    home.join(".local/share/zenith/tools/php")
}

/// `<managed root>/<profile>/vendor/bin`.
pub fn managed_bin(home: &Path, profile: Profile) -> PathBuf {
    managed_root(home)
        .join(profile.dir_name())
        .join("vendor/bin")
}

/// Where a PHP binary may live, best first: Herd, Homebrew, then the `PATH`.
pub fn php_candidates(home: &Path, path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join("Library/Application Support/Herd/bin"),
        home.join(".config/herd-lite/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    dirs.extend(path_dirs.iter().cloned());
    let mut seen = Vec::new();
    dirs.into_iter()
        .map(|dir| dir.join("php"))
        .filter(|candidate| {
            let new = !seen.contains(candidate);
            seen.push(candidate.clone());
            new
        })
        .collect()
}

/// `8.5.0` from `php -v`'s first line, `PHP 8.5.0 (cli) (built: …)`.
pub fn parse_php_version(output: &str) -> Option<String> {
    let rest = output.lines().next()?.strip_prefix("PHP ")?;
    let version = rest.split_whitespace().next()?;
    version
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit())
        .then(|| version.to_string())
}

/// `8.5` for a status bar.
pub fn short_version(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// Worth a PHP status: a WordPress or Laravel project, or one with `composer.json`
/// or PHP files at its root.
pub fn looks_like_php(root: &Path) -> bool {
    detect_project(root) != ProjectKind::Php
        || root.join("composer.json").is_file()
        || top_level_php_files(root).next().is_some()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhpInfo {
    pub kind: ProjectKind,
    /// `None` when no PHP binary ran.
    pub version: Option<String>,
    pub php_path: Option<PathBuf>,
}

/// The first candidate whose `php -v` answers.
pub async fn find_php(candidates: &[PathBuf]) -> Option<(PathBuf, String)> {
    for candidate in candidates {
        if let Some(version) = php_config::probe_php(candidate).await {
            return Some((candidate.clone(), version));
        }
    }
    None
}

pub async fn detect_php(root: &Path, candidates: &[PathBuf]) -> PhpInfo {
    let found = find_php(candidates).await;
    PhpInfo {
        kind: detect_project(root),
        version: found.as_ref().map(|(_, version)| version.clone()),
        php_path: found.map(|(path, _)| path),
    }
}

/// What "PHP tools: status" shows.
pub fn status_report(
    root: &Path,
    info: &PhpInfo,
    managed_bin: Option<&Path>,
    path_dirs: &[PathBuf],
) -> String {
    let kind = match info.kind {
        ProjectKind::WordPress => "WordPress",
        ProjectKind::Laravel => "Laravel",
        ProjectKind::Php => "PHP",
    };
    let mut lines = vec![format!("Project: {kind}")];
    lines.push(match (&info.version, &info.php_path) {
        (Some(version), Some(path)) => format!("PHP {version}: {}", path.display()),
        _ => "PHP: not found (install Herd or Homebrew PHP)".to_string(),
    });
    for tool in [
        Tool::Phpcs,
        Tool::Phpcbf,
        Tool::Phpstan,
        Tool::Pint,
        Tool::PhpCsFixer,
    ] {
        lines.push(match resolve(root, tool, managed_bin, path_dirs) {
            Some(Resolved { path, source }) => {
                let source = match source {
                    Source::Project => "project",
                    Source::Managed => "Zenith",
                    Source::System => "system",
                };
                format!("{}: {source}, {}", tool.binary(), path.display())
            }
            None => format!("{}: not found", tool.binary()),
        });
    }
    lines.join("\n")
}

// ---- consent and install ----

/// What the user told Zenith about installing a profile's tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Consent {
    #[default]
    Ask,
    Yes,
    Never,
}

impl Profile {
    fn key(self) -> &'static str {
        self.dir_name()
    }

    fn name(self) -> &'static str {
        match self {
            Profile::WordPress => "WordPress",
            Profile::Laravel => "Laravel",
            Profile::Php => "PHP",
        }
    }
}

/// Where the last install's Composer output goes.
pub fn install_log(home: &Path, profile: Profile) -> PathBuf {
    managed_root(home)
        .join(profile.dir_name())
        .join("install.log")
}

fn consent_file(home: &Path) -> PathBuf {
    managed_root(home).join("consent.json")
}

pub fn load_consent(home: &Path, profile: Profile) -> Consent {
    let Ok(text) = fs::read_to_string(consent_file(home)) else {
        return Consent::Ask;
    };
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    match json.get(profile.key()).and_then(|value| value.as_str()) {
        Some("yes") => Consent::Yes,
        Some("never") => Consent::Never,
        _ => Consent::Ask,
    }
}

/// `Ask` is not stored: "Not now" is for this session only.
pub fn save_consent(home: &Path, profile: Profile, consent: Consent) -> std::io::Result<()> {
    let file = consent_file(home);
    let mut json: serde_json::Map<String, serde_json::Value> = fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    match consent {
        Consent::Yes => json.insert(profile.key().into(), "yes".into()),
        Consent::Never => json.insert(profile.key().into(), "never".into()),
        Consent::Ask => json.remove(profile.key()),
    };
    fs::create_dir_all(managed_root(home))?;
    fs::write(file, serde_json::to_string_pretty(&json)?)
}

/// Whether to show the install banner: a trusted, local PHP project that has not
/// answered, with none of the profile's tools in sight.
pub fn should_offer(consent: Consent, trusted: bool, local: bool, has_any_tool: bool) -> bool {
    consent == Consent::Ask && trusted && local && !has_any_tool
}

/// One external command, for a runner to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

/// Installs the profile's tools into the managed folder with the user's Composer.
/// Composer's home is inside the managed folder, so the user's global Composer
/// config, cache and auth stay untouched. Returns Composer's output.
pub async fn install<F, Fut>(
    home: &Path,
    profile: Profile,
    php: &Path,
    composer: &Path,
    run: F,
) -> Result<String, String>
where
    F: Fn(Invocation) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let dir = managed_root(home).join(profile.dir_name());
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    for (name, text) in [
        ("composer.json", profile.composer_manifest()),
        ("composer.lock", profile.composer_lock()),
    ] {
        fs::write(dir.join(name), text)
            .map_err(|error| format!("Could not write {name}: {error}"))?;
    }
    run(Invocation {
        program: php.to_path_buf(),
        args: vec![
            composer.display().to_string(),
            "install".into(),
            "--no-interaction".into(),
            "--no-progress".into(),
            "--prefer-dist".into(),
        ],
        cwd: dir.clone(),
        env: vec![
            (
                "COMPOSER_HOME".into(),
                dir.join(".composer-home").display().to_string(),
            ),
            ("COMPOSER_NO_INTERACTION".into(), "1".into()),
        ],
    })
    .await
}

/// Deletes a profile's managed folder: its tools, Composer home and log. Zenith's
/// `composer.phar` and the consent answers sit one level up and stay. A symlinked
/// folder loses only the link, never what it points at.
pub fn remove(home: &Path, profile: Profile) -> std::io::Result<()> {
    match fs::remove_dir_all(managed_root(home).join(profile.dir_name())) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

// ---- Composer for users who have none ----

pub const COMPOSER_PHAR_URL: &str = "https://getcomposer.org/download/latest-stable/composer.phar";

/// Where Zenith keeps its own copy of Composer.
pub fn composer_phar(home: &Path) -> PathBuf {
    managed_root(home).join("composer.phar")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn fetch(client: &Arc<dyn HttpClient>, url: &str) -> Result<Vec<u8>, String> {
    use futures::AsyncReadExt as _;
    let mut response = client
        .get(url, http_client::AsyncBody::empty(), true)
        .await
        .map_err(|error| format!("Could not download {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download {url}: HTTP {}",
            response.status()
        ));
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("Could not read {url}: {error}"))?;
    Ok(bytes)
}

/// Downloads `composer.phar` and keeps it only if its SHA-256 matches the one
/// Composer publishes next to it.
pub async fn download_composer(
    client: Arc<dyn HttpClient>,
    home: &Path,
) -> Result<PathBuf, String> {
    let phar = fetch(&client, COMPOSER_PHAR_URL).await?;
    let published = fetch(&client, &format!("{COMPOSER_PHAR_URL}.sha256")).await?;
    let expected = String::from_utf8_lossy(&published)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let actual = sha256_hex(&phar);
    if expected != actual {
        return Err(format!(
            "composer.phar does not match its published checksum (expected {expected}, got {actual}); not installed"
        ));
    }
    let path = composer_phar(home);
    fs::create_dir_all(managed_root(home)).map_err(|error| error.to_string())?;
    fs::write(&path, phar).map_err(|error| format!("Could not save composer.phar: {error}"))?;
    Ok(path)
}

// ---- status bar item and "PHP tools: status" ----

use editor::Editor;
use gpui::{
    Action as _, App, AppContext as _, Context, Entity, IntoElement, PromptLevel, Render,
    SharedString, WeakEntity, Window, actions,
};
use project::{Project, WorktreeId};
use settings::Settings as _;
use ui::{Button, ButtonCommon as _, ButtonSize, Clickable as _, LabelSize, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, OpenOptions, StatusItemView, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(
    zenith_php_tools,
    [
        /// Shows the PHP version and where each PHP tool comes from.
        Status,
        /// Installs this project type's PHP tools at the versions this release pins.
        Update,
        /// Deletes this project type's Zenith-managed PHP tools and installs them again.
        Reinstall,
        /// Deletes this project type's Zenith-managed PHP tools.
        Remove,
        /// Opens the last PHP tools install log, read-only.
        ShowInstallLog
    ]
);

pub fn init(cx: &mut App) {
    php_config::init(cx);
    php_modal::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        let project = workspace.project().clone();
        let this_workspace = cx.entity().downgrade();
        let item = cx.new(|cx| PhpStatus::new(project, this_workspace, cx));
        workspace
            .status_bar()
            .update(cx, |bar, cx| bar.add_right_item(item, window, cx));
        workspace.register_action(|workspace, _: &Status, window, cx| {
            show_status(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &Update, window, cx| {
            manage(workspace, Manage::Update, window, cx)
        });
        workspace.register_action(|workspace, _: &Reinstall, window, cx| {
            manage(workspace, Manage::Reinstall, window, cx)
        });
        workspace.register_action(|workspace, _: &Remove, window, cx| {
            manage(workspace, Manage::Remove, window, cx)
        });
        workspace.register_action(|workspace, _: &ShowInstallLog, window, cx| {
            show_install_log(workspace, window, cx)
        });
    })
    .detach();
}

/// PHP candidates (the user's `zenith.php_tools.php_binary` first) and the `PATH` directories.
pub fn environment() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let candidates = php_config::with_override(
        php_candidates(&home, &path_dirs),
        php_config::php_binary_override().as_deref(),
    );
    (candidates, path_dirs)
}

/// The first visible folder that is not a library (added with "Add Library Path…"): the project
/// the PHP settings belong to. The project panel's menu gate picks the same one.
fn first_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .find(|root| !zenith_external_libraries::is_library_worktree(root, cx))
}

fn show_status(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(root) = first_root(workspace.project(), cx) else {
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let report = cx
            .background_spawn(async move {
                let (candidates, path_dirs) = environment();
                let info = detect_php(&root, &candidates).await;
                let managed = std::env::var_os("HOME")
                    .map(|home| managed_bin(Path::new(&home), Profile::for_project(info.kind)));
                status_report(&root, &info, managed.as_deref(), &path_dirs)
            })
            .await;
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_notification(NotificationId::unique::<Status>(), cx, |cx| {
                    cx.new(|cx| MessageNotification::new(report, cx))
                });
            })
            .ok();
    })
    .detach();
}

/// `PHP 8.5` in the status bar of a PHP project; nothing elsewhere.
pub struct PhpStatus {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    info: Option<PhpInfo>,
    applied_include: phpantom::AppliedInclude,
}

impl PhpStatus {
    fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&project, |this, _, event, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
            ) {
                this.refresh(cx);
            }
        })
        .detach();
        cx.observe_global::<Installing>(|_, cx| cx.notify())
            .detach();
        cx.observe_global::<settings::SettingsStore>(|this, cx| {
            phpantom::sync(&this.project, &mut this.applied_include, cx)
        })
        .detach();
        let mut this = Self {
            project,
            workspace,
            info: None,
            applied_include: Default::default(),
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        phpantom::sync(&self.project, &mut self.applied_include, cx);
        let Some(root) = first_root(&self.project, cx) else {
            self.info = None;
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let info = cx
                .background_spawn({
                    let root = root.clone();
                    async move {
                        if looks_like_php(&root) {
                            Some(detect_php(&root, &environment().0).await)
                        } else {
                            None
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.info = info.clone();
                cx.notify();
                if let Some(info) = info {
                    this.workspace
                        .update(cx, |workspace, cx| maybe_offer(workspace, &root, &info, cx))
                        .ok();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl StatusItemView for PhpStatus {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

impl Render for PhpStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(info) = &self.info else {
            return div().into_any_element();
        };
        let installing = installing(cx);
        let label = match &info.version {
            _ if installing => "Installing PHP tools…".to_string(),
            Some(version) => format!("PHP {}", short_version(version)),
            None => "PHP: not found".to_string(),
        };
        div()
            .debug_selector(|| "zenith-php-status".into())
            .child(
                Button::new("zenith-php-status", label)
                    .label_size(LabelSize::Small)
                    .size(ButtonSize::Compact)
                    .tooltip(Tooltip::text("PHP tools: status"))
                    .on_click(|_, window, cx| window.dispatch_action(Status.boxed_clone(), cx)),
            )
            .into_any_element()
    }
}

// ---- the install banner ----

use workspace::notifications::NotificationId as BannerId;

struct InstallBanner;
struct InstallProgress;

/// True while an install runs; the status bar item shows it.
struct Installing(bool);

impl gpui::Global for Installing {}

fn installing(cx: &App) -> bool {
    cx.try_global::<Installing>().is_some_and(|state| state.0)
}

/// The user's Composer, else the copy Zenith downloaded earlier; `None` means
/// Zenith downloads one when an install starts.
fn find_composer(home: &Path, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs = path_dirs.to_vec();
    dirs.extend([
        home.join("Library/Application Support/Herd/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    dirs.into_iter()
        .map(|dir| dir.join("composer"))
        .find(|candidate| candidate.is_file())
        .or_else(|| Some(composer_phar(home)).filter(|phar| phar.is_file()))
}

/// Whether a worktree's tools may run, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    /// Upstream does not trust the worktree.
    Untrusted,
    /// Upstream trusts it (a single file, or an invisible worktree), but its folder
    /// is not inside a trusted folder.
    OutsideTrustedFolders,
}

/// `folder` is `root` or below it, by name: a `..` or `.` step could name any place.
fn folder_inside(folder: &Path, root: &Path) -> bool {
    folder.starts_with(root)
        && !folder
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
}

/// Running a project's tools runs its code: only in a worktree the user trusts.
/// True when the trust feature is off.
///
/// Upstream trusts a single-file worktree once any folder is trusted, and an
/// invisible one always. Tools run in the file's own folder (`vendor/bin`, rulesets,
/// `phpstan.neon`), so for those two the folder must lie inside a trusted
/// directory worktree.
pub fn worktree_trusted(project: &Entity<Project>, worktree_id: WorktreeId, cx: &mut App) -> bool {
    worktree_trust(project, worktree_id, cx) == Trust::Trusted
}

/// [`worktree_trusted`], with the reason when it is not.
pub fn worktree_trust(project: &Entity<Project>, worktree_id: WorktreeId, cx: &mut App) -> Trust {
    let Some(store) = project::trusted_worktrees::TrustedWorktrees::try_get_global(cx) else {
        return Trust::Trusted;
    };
    let worktree_store = project.read(cx).worktree_store();
    let trust_all = project::project_settings::ProjectSettings::get_global(cx)
        .session
        .trust_all_worktrees;
    store.update(cx, |store, cx| {
        if !store.can_trust(&worktree_store, worktree_id, cx) {
            return Trust::Untrusted;
        }
        let Some(worktree) = worktree_store.read(cx).worktree_for_id(worktree_id, cx) else {
            return Trust::Untrusted;
        };
        let (visible, single_file, abs_path) = {
            let worktree = worktree.read(cx);
            (
                worktree.is_visible(),
                worktree.is_single_file(),
                worktree.abs_path(),
            )
        };
        if trust_all || (visible && !single_file) {
            return Trust::Trusted;
        }
        let folder = if single_file {
            abs_path.parent().unwrap_or(&abs_path)
        } else {
            &abs_path
        };
        let folders: Vec<_> = worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .filter(|other| !other.read(cx).is_single_file())
            .map(|other| (other.read(cx).id(), other.read(cx).abs_path()))
            .filter(|(id, root)| *id != worktree_id && folder_inside(folder, root))
            .collect();
        if folders
            .into_iter()
            .any(|(id, _)| store.can_trust(&worktree_store, id, cx))
        {
            Trust::Trusted
        } else {
            Trust::OutsideTrustedFolders
        }
    })
}

/// Offers to install the profile's tools: once per session until answered, and
/// only in a trusted local project with PHP and Composer present and no tool found.
fn maybe_offer(
    workspace: &mut Workspace,
    root: &Path,
    info: &PhpInfo,
    cx: &mut Context<Workspace>,
) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let profile = Profile::for_project(info.kind);
    let (_, path_dirs) = environment();
    let managed = managed_bin(&home, profile);
    let has_any_tool = [
        Tool::Phpcs,
        Tool::Phpcbf,
        Tool::Phpstan,
        Tool::Pint,
        Tool::PhpCsFixer,
    ]
    .into_iter()
    .any(|tool| resolve(root, tool, Some(&managed), &path_dirs).is_some());
    let project = workspace.project().clone();
    let local = project.read(cx).is_local();
    let worktree_id = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).id());
    let trusted = worktree_id.is_none_or(|id| worktree_trusted(&project, id, cx));
    let Some(php) = info.php_path.clone() else {
        return;
    };
    let composer = find_composer(&home, &path_dirs);
    if !should_offer(load_consent(&home, profile), trusted, local, has_any_tool) {
        return;
    }
    let weak = cx.entity().downgrade();
    let download_note = if composer.is_none() {
        " It will also download Composer, checked against its published checksum."
    } else {
        ""
    };
    let label = match profile {
        Profile::WordPress => "WordPress tools: PHPCS with the WordPress standards, PHPStan",
        Profile::Laravel => "Laravel tools: Pint, PHPStan with Larastan",
        Profile::Php => "PHP tools: PHPStan, PHP-CS-Fixer",
    };
    workspace.show_notification(BannerId::unique::<InstallBanner>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(
                format!("Zenith can set up {label}, in its own folder.{download_note} Closing this asks again next time."),
                cx,
            )
            .primary_message("Install")
            .primary_on_click({
                let (weak, php, composer) = (weak.clone(), php.clone(), composer.clone());
                move |window, cx| {
                    let (php, composer) = (php.clone(), composer.clone());
                    weak.update(cx, |workspace, cx| {
                        save_consent(&home_dir(), profile, Consent::Yes).ok();
                        start_install(workspace, profile, php, composer, false, window, cx);
                    })
                    .ok();
                }
            })
            .secondary_message("Never")
            .secondary_on_click(move |_, cx| {
                save_consent(&home_dir(), profile, Consent::Never).ok();
                weak.update(cx, |workspace, cx| {
                    workspace.dismiss_notification(&BannerId::unique::<InstallBanner>(), cx)
                })
                .ok();
            })
        })
    });
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn start_install(
    workspace: &mut Workspace,
    profile: Profile,
    php: PathBuf,
    composer: Option<PathBuf>,
    reinstall: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let http: Arc<dyn HttpClient> = workspace.client().http_client();
    workspace.dismiss_notification(&BannerId::unique::<InstallBanner>(), cx);
    workspace.show_notification(BannerId::unique::<InstallProgress>(), cx, |cx| {
        cx.new(|cx| MessageNotification::new("Installing PHP tools…", cx))
    });
    cx.set_global(Installing(true));
    let home = home_dir();
    cx.spawn_in(window, async move |workspace, cx| {
        let (run_php, run_composer) = (php.clone(), composer.clone());
        let result = cx
            .background_spawn(async move {
                if reinstall {
                    remove(&home, profile)
                        .map_err(|error| format!("Could not remove the old tools: {error}"))?;
                }
                let composer = match run_composer {
                    Some(composer) => composer,
                    None => download_composer(http, &home).await?,
                };
                install(&home, profile, &run_php, &composer, run_command).await
            })
            .await;
        let log = install_log(&home_dir(), profile);
        let text = match &result {
            Ok(output) | Err(output) => output.clone(),
        };
        // Best effort: the notification still carries an error if this fails.
        std::fs::write(&log, text).ok();
        workspace
            .update_in(cx, |workspace, _, cx| match result {
                Ok(_) => {
                    workspace.show_notification(
                    BannerId::unique::<InstallProgress>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(
                                format!(
                                    "PHP tools installed. Run \"zenith php tools: status\" to see them. Log: {}",
                                    log.display()
                                ),
                                cx,
                            )
                        })
                    },
                    );
                }
                Err(error) => {
                    let weak = cx.entity().downgrade();
                    workspace.show_notification(
                        BannerId::unique::<InstallProgress>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    format!(
                                        "Installing PHP tools failed (log: {}):\n{error}",
                                        log.display()
                                    ),
                                    cx,
                                )
                                .primary_message("Retry")
                                .primary_on_click(move |window, cx| {
                                    let (php, composer) = (php.clone(), composer.clone());
                                    weak.update(cx, |workspace, cx| {
                                        start_install(
                                            workspace, profile, php, composer, reinstall, window,
                                            cx,
                                        )
                                    })
                                    .ok();
                                })
                            })
                        },
                    );
                }
            })
            .ok();
        cx.update(|_, cx| cx.set_global(Installing(false))).ok();
    })
    .detach();
}

struct ToolsNotice;

fn notice(workspace: &mut Workspace, text: impl Into<SharedString>, cx: &mut Context<Workspace>) {
    let text = text.into();
    workspace.show_notification(NotificationId::unique::<ToolsNotice>(), cx, |cx| {
        cx.new(|cx| MessageNotification::new(text, cx))
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Manage {
    Update,
    Reinstall,
    Remove,
}

/// "update", "reinstall" and "remove" for the first folder's project type. Only
/// on request, in a trusted local project, and never while an install runs.
fn manage(
    workspace: &mut Workspace,
    action: Manage,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some((worktree_id, root)) = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| {
            (
                worktree.read(cx).id(),
                worktree.read(cx).abs_path().to_path_buf(),
            )
        })
    else {
        return;
    };
    if !project.read(cx).is_local() {
        return notice(
            workspace,
            "PHP tools are managed for local projects only.",
            cx,
        );
    }
    if !worktree_trusted(&project, worktree_id, cx) {
        return notice(workspace, "Trust this project to manage its PHP tools.", cx);
    }
    if installing(cx) {
        return notice(
            workspace,
            "PHP tools are being installed; try again when that ends.",
            cx,
        );
    }
    let confirm = (action == Manage::Remove).then(|| {
        window.prompt(
            PromptLevel::Warning,
            "Remove Zenith's PHP tools for this project type?",
            Some("Deletes Zenith's own copy of the tools and its install log. Your project, your Composer and your PATH are not touched."),
            &["Remove", "Cancel"],
            cx,
        )
    });
    let home = home_dir();
    cx.spawn_in(window, async move |workspace, cx| {
        if let Some(confirm) = confirm
            && !matches!(confirm.await, Ok(0))
        {
            return anyhow::Ok(());
        }
        let (profile, php, composer) = cx
            .background_spawn({
                let home = home.clone();
                async move {
                    let profile = Profile::for_project(detect_project(&root));
                    if action == Manage::Remove {
                        return (profile, None, None);
                    }
                    let (candidates, path_dirs) = environment();
                    let php = find_php(&candidates).await.map(|(php, _)| php);
                    (profile, php, find_composer(&home, &path_dirs))
                }
            })
            .await;
        if action == Manage::Remove {
            let busy = workspace.update(cx, |workspace, cx| {
                let busy = installing(cx);
                if busy {
                    notice(
                        workspace,
                        "PHP tools are being installed; nothing removed.",
                        cx,
                    );
                }
                busy
            })?;
            if busy {
                return Ok(());
            }
            let result = cx
                .background_spawn(async move { remove(&home, profile) })
                .await;
            let text = match result {
                Ok(()) => format!("Removed Zenith's {} tools.", profile.name()),
                Err(error) => format!(
                    "Could not remove Zenith's {} tools: {error}",
                    profile.name()
                ),
            };
            workspace.update(cx, |workspace, cx| notice(workspace, text, cx))?;
            return Ok(());
        }
        workspace.update_in(cx, |workspace, window, cx| {
            let Some(php) = php else {
                return notice(
                    workspace,
                    "PHP not found: install Herd or Homebrew PHP first.",
                    cx,
                );
            };
            if installing(cx) {
                return notice(
                    workspace,
                    "PHP tools are being installed; try again when that ends.",
                    cx,
                );
            }
            // Asking for the tools is the consent the banner would have asked for.
            save_consent(&home, profile, Consent::Yes).log_err();
            start_install(
                workspace,
                profile,
                php,
                composer,
                action == Manage::Reinstall,
                window,
                cx,
            );
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Opens the first folder's profile `install.log` in a read-only editor.
fn show_install_log(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(root) = first_root(workspace.project(), cx) else {
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let log = cx
            .background_spawn(async move {
                let log = install_log(&home_dir(), Profile::for_project(detect_project(&root)));
                log.is_file().then_some(log)
            })
            .await;
        let Some(log) = log else {
            workspace.update(cx, |workspace, cx| {
                notice(
                    workspace,
                    "No PHP tools install log yet for this project type.",
                    cx,
                )
            })?;
            return anyhow::Ok(());
        };
        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(log, OpenOptions::default(), window, cx)
        })?;
        if let Some(editor) = open.await?.downcast::<Editor>() {
            cx.update(|_, cx| editor.update(cx, |editor, _| editor.set_read_only(true)))?;
        }
        Ok(())
    })
    .detach_and_log_err(cx);
}

/// Runs one `Invocation` for real; the error is Composer's own output.
async fn run_command(invocation: Invocation) -> Result<String, String> {
    let output = util::command::new_command(&invocation.program)
        .args(&invocation.args)
        .current_dir(&invocation.cwd)
        .envs(invocation.env.iter().map(|(key, value)| (key, value)))
        .output()
        .await
        .map_err(|error| format!("Could not run {}: {error}", invocation.program.display()))?;
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
    if output.status.success() {
        Ok(text(&output.stdout))
    } else {
        Err(
            format!("{}\n{}", text(&output.stderr), text(&output.stdout))
                .trim()
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, file: &str, text: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn test_read_regular_never_reads_an_endless_file() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "small.txt", "hello world");
        assert_eq!(
            read_regular(&dir.path().join("small.txt"), 5),
            Some(b"hello".to_vec())
        );
        // A symlink to an endless source: an unbounded read never comes back.
        symlink("/dev/zero", dir.path().join("endless.php")).unwrap();
        assert_eq!(read_regular(&dir.path().join("endless.php"), 8192), None);
        assert_eq!(read_regular(dir.path(), 8192), None);
        // A pipe with no writer would block a plain open; it is refused on the handle.
        #[allow(clippy::disallowed_methods, reason = "mkfifo has no std API")]
        let made = std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe.php"))
            .status()
            .unwrap()
            .success();
        assert!(made);
        assert_eq!(read_regular(&dir.path().join("pipe.php"), 8192), None);
        assert_eq!(read_regular(&dir.path().join("missing"), 8192), None);
        // A symlink to a regular file is followed.
        symlink(dir.path().join("small.txt"), dir.path().join("link.txt")).unwrap();
        assert_eq!(
            read_regular(&dir.path().join("link.txt"), 100),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn test_detect_project_survives_endless_project_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        symlink("/dev/zero", dir.path().join("plugin.php")).unwrap();
        symlink("/dev/zero", dir.path().join("style.css")).unwrap();
        symlink("/dev/zero", dir.path().join("composer.json")).unwrap();
        write(dir.path(), "artisan", "");
        // Returns at all, and finds nothing in the endless files.
        assert_eq!(detect_project(dir.path()), ProjectKind::Php);
    }

    #[test]
    fn test_folder_inside_refuses_dot_steps() {
        let root = Path::new("/work/trusted");
        assert!(folder_inside(Path::new("/work/trusted"), root));
        assert!(folder_inside(Path::new("/work/trusted/sub"), root));
        assert!(!folder_inside(Path::new("/work/other"), root));
        // Names the trusted folder first, then climbs out of it.
        assert!(!folder_inside(Path::new("/work/trusted/../other"), root));
        assert!(!folder_inside(Path::new("/work/trusted/sub/.."), root));
        assert!(!folder_inside(Path::new("./work/trusted"), Path::new(".")));
    }

    #[test]
    fn test_detects_project_kinds() {
        let plugin = tempfile::tempdir().unwrap();
        write(
            plugin.path(),
            "my-plugin.php",
            "<?php\n/*\nPlugin Name: My Plugin\n*/",
        );
        assert_eq!(detect_project(plugin.path()), ProjectKind::WordPress);

        let theme = tempfile::tempdir().unwrap();
        write(theme.path(), "style.css", "/*\nTheme Name: Mine\n*/");
        assert_eq!(detect_project(theme.path()), ProjectKind::WordPress);

        let site = tempfile::tempdir().unwrap();
        write(site.path(), "wp-config.php", "<?php");
        assert_eq!(detect_project(site.path()), ProjectKind::WordPress);

        let laravel = tempfile::tempdir().unwrap();
        write(laravel.path(), "artisan", "#!/usr/bin/env php");
        write(
            laravel.path(),
            "composer.json",
            r#"{"require": {"laravel/framework": "^11.0"}}"#,
        );
        assert_eq!(detect_project(laravel.path()), ProjectKind::Laravel);

        // `artisan` alone, or a plain library, is not Laravel.
        let library = tempfile::tempdir().unwrap();
        write(library.path(), "artisan", "");
        write(
            library.path(),
            "composer.json",
            r#"{"require": {"php": ">=8.1"}}"#,
        );
        write(library.path(), "src/Thing.php", "<?php\nclass Thing {}");
        assert_eq!(detect_project(library.path()), ProjectKind::Php);
    }

    #[test]
    fn test_project_tools_beat_system_tools() {
        let project = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        write(system.path(), "phpcs", "");
        write(system.path(), "phpstan", "");
        write(project.path(), "vendor/bin/phpcs", "");
        let path_dirs = [system.path().to_path_buf()];

        let phpcs = resolve(project.path(), Tool::Phpcs, None, &path_dirs).unwrap();
        assert_eq!(phpcs.source, Source::Project);
        assert_eq!(phpcs.path, project.path().join("vendor/bin/phpcs"));

        let phpstan = resolve(project.path(), Tool::Phpstan, None, &path_dirs).unwrap();
        assert_eq!(phpstan.source, Source::System);

        assert_eq!(resolve(project.path(), Tool::Pint, None, &path_dirs), None);
    }

    #[test]
    fn test_php_version_from_php_v() {
        let output = "PHP 8.5.0 (cli) (built: Nov 18 2026) (NTS)\nCopyright (c) The PHP Group\n";
        assert_eq!(parse_php_version(output).as_deref(), Some("8.5.0"));
        assert_eq!(short_version("8.5.0"), "8.5");
        assert_eq!(parse_php_version("zsh: command not found: php"), None);
        assert_eq!(parse_php_version("PHP Warning: something"), None);
    }

    #[test]
    fn test_php_candidates_prefer_herd_then_homebrew_without_duplicates() {
        let home = Path::new("/Users/a");
        let path = [
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/bin"),
        ];
        let found = php_candidates(home, &path);
        assert_eq!(
            found.first().unwrap(),
            &home.join("Library/Application Support/Herd/bin/php")
        );
        assert_eq!(
            found
                .iter()
                .filter(|path| **path == Path::new("/opt/homebrew/bin/php"))
                .count(),
            1
        );
        assert_eq!(found.last().unwrap(), Path::new("/usr/bin/php"));
    }

    #[cfg(unix)]
    fn fake_php(dir: &Path, version_line: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("php");
        fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn test_detect_php_runs_the_first_working_candidate() {
        let broken = tempfile::tempdir().unwrap();
        let working = tempfile::tempdir().unwrap();
        let broken_php = fake_php(broken.path(), "not php at all");
        let working_php = fake_php(working.path(), "PHP 8.5.0 (cli)");
        let project = tempfile::tempdir().unwrap();
        write(project.path(), "composer.json", "{}");

        let info = futures::executor::block_on(detect_php(
            project.path(),
            &[
                PathBuf::from("/nonexistent/php"),
                broken_php,
                working_php.clone(),
            ],
        ));
        assert_eq!(info.version.as_deref(), Some("8.5.0"));
        assert_eq!(info.php_path, Some(working_php));
        assert!(looks_like_php(project.path()));

        let none = futures::executor::block_on(detect_php(project.path(), &[]));
        assert_eq!(none.version, None);
        let report = status_report(project.path(), &none, None, &[]);
        assert!(report.contains("PHP: not found"));
        assert!(report.contains("phpcs: not found"));
    }

    #[test]
    fn test_status_report_names_where_each_tool_comes_from() {
        let project = tempfile::tempdir().unwrap();
        write(project.path(), "vendor/bin/phpstan", "");
        let info = PhpInfo {
            kind: ProjectKind::Laravel,
            version: Some("8.4.1".into()),
            php_path: Some(PathBuf::from("/opt/homebrew/bin/php")),
        };
        let report = status_report(project.path(), &info, None, &[]);
        assert!(report.starts_with("Project: Laravel\nPHP 8.4.1: /opt/homebrew/bin/php"));
        assert!(report.contains("phpstan: project,"));
        assert!(report.contains("pint: not found"));
    }

    #[gpui::test]
    async fn test_first_root_is_the_first_folder_that_is_not_a_library(
        cx: &mut gpui::TestAppContext,
    ) {
        use settings::settings_content::{
            ZenithExternalLibrariesSettingsContent, ZenithSettingsContent,
        };
        cx.update(|cx| {
            workspace::AppState::test(cx);
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.zenith = Some(ZenithSettingsContent {
                        external_libraries: Some(ZenithExternalLibrariesSettingsContent {
                            paths: Some(vec!["/libs/pkg".to_string()]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                });
            });
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree("/libs/pkg", serde_json::json!({ "composer.json": "{}" }))
            .await;
        fs.insert_tree("/site", serde_json::json!({ "composer.json": "{}" }))
            .await;
        let project = Project::test(fs, [Path::new("/libs/pkg"), Path::new("/site")], cx).await;
        cx.update(|cx| {
            assert_eq!(first_root(&project, cx), Some(PathBuf::from("/site")));
        });
    }

    #[test]
    fn test_empty_folder_is_not_a_php_project() {
        let empty = tempfile::tempdir().unwrap();
        assert!(!looks_like_php(empty.path()));
    }

    #[gpui::test]
    async fn test_status_item_shows_in_php_projects_only(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            app_state
        });
        for (has_composer, expected) in [(true, true), (false, false)] {
            // The PHP check reads the real disk; the project lives in the fake one.
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_str().unwrap().to_string();
            let name = if has_composer {
                "composer.json"
            } else {
                "notes.txt"
            };
            fs::write(dir.path().join(name), "{}").unwrap();
            app_state
                .fs
                .as_fake()
                .insert_tree(&root, serde_json::json!({ name: "{}" }))
                .await;
            let root = root.as_str();
            let project =
                Project::test(app_state.fs.clone(), [std::path::Path::new(root)], cx).await;
            let (_, cx) = cx.add_window_view(|window, cx| {
                workspace::MultiWorkspace::test_new(project, window, cx)
            });
            cx.executor().allow_parking();
            cx.executor()
                .advance_clock(std::time::Duration::from_secs(1));
            // The PHP lookup runs on a real thread; give it a moment.
            for _ in 0..50 {
                cx.run_until_parked();
                if cx.debug_bounds("zenith-php-status").is_some() == expected {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(cx.debug_bounds("zenith-php-status").is_some(), expected);
        }
    }

    #[test]
    fn test_resolution_order_is_project_then_managed_then_system() {
        let project = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        let path_dirs = [system.path().to_path_buf()];
        for tool in ["phpcs", "phpstan", "pint"] {
            write(system.path(), tool, "");
        }
        for tool in ["phpcs", "phpstan"] {
            write(managed.path(), tool, "");
        }
        write(project.path(), "vendor/bin/phpcs", "");

        let source = |tool| {
            resolve(project.path(), tool, Some(managed.path()), &path_dirs).map(|r| r.source)
        };
        assert_eq!(source(Tool::Phpcs), Some(Source::Project));
        assert_eq!(source(Tool::Phpstan), Some(Source::Managed));
        assert_eq!(source(Tool::Pint), Some(Source::System));
        assert_eq!(source(Tool::PhpCsFixer), None);
    }

    #[test]
    fn test_profiles_and_managed_folder() {
        assert_eq!(
            managed_bin(Path::new("/Users/a"), Profile::WordPress),
            Path::new("/Users/a/.local/share/zenith/tools/php/wordpress/vendor/bin")
        );
        assert_eq!(Profile::for_project(ProjectKind::Laravel), Profile::Laravel);
        let wordpress = Profile::WordPress.composer_json();
        let require = wordpress["require"].as_object().unwrap();
        for package in [
            "squizlabs/php_codesniffer",
            "wp-coding-standards/wpcs",
            "phpstan/phpstan",
        ] {
            assert!(require.contains_key(package), "{package}");
        }
        assert!(Profile::Laravel.composer_json()["require"]["laravel/pint"].is_string());
        assert!(Profile::Php.composer_json()["require"]["friendsofphp/php-cs-fixer"].is_string());
        // The installer plugin must be allowed, or Composer stops to ask.
        assert_eq!(
            wordpress["config"]["allow-plugins"]["dealerdirect/phpcodesniffer-composer-installer"],
            true
        );
    }

    #[test]
    fn test_consent_is_remembered_per_profile_and_never_defaults_to_yes() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load_consent(home.path(), Profile::WordPress), Consent::Ask);
        save_consent(home.path(), Profile::WordPress, Consent::Never).unwrap();
        save_consent(home.path(), Profile::Laravel, Consent::Yes).unwrap();
        assert_eq!(
            load_consent(home.path(), Profile::WordPress),
            Consent::Never
        );
        assert_eq!(load_consent(home.path(), Profile::Laravel), Consent::Yes);
        assert_eq!(load_consent(home.path(), Profile::Php), Consent::Ask);
        // "Not now" forgets the answer.
        save_consent(home.path(), Profile::WordPress, Consent::Ask).unwrap();
        assert_eq!(load_consent(home.path(), Profile::WordPress), Consent::Ask);
        assert_eq!(load_consent(home.path(), Profile::Laravel), Consent::Yes);
    }

    #[test]
    fn test_offer_only_when_asked_trusted_local_and_missing() {
        assert!(should_offer(Consent::Ask, true, true, false));
        assert!(!should_offer(Consent::Never, true, true, false));
        assert!(!should_offer(Consent::Yes, true, true, false));
        assert!(!should_offer(Consent::Ask, false, true, false), "untrusted");
        assert!(!should_offer(Consent::Ask, true, false, false), "remote");
        assert!(!should_offer(Consent::Ask, true, true, true), "tools exist");
    }

    #[test]
    fn test_install_runs_composer_inside_the_managed_folder_only() {
        use std::cell::RefCell;
        let home = tempfile::tempdir().unwrap();
        let calls = RefCell::new(Vec::new());
        let result = futures::executor::block_on(install(
            home.path(),
            Profile::WordPress,
            Path::new("/opt/homebrew/bin/php"),
            Path::new("/usr/local/bin/composer"),
            |invocation| {
                calls.borrow_mut().push(invocation);
                async { Ok("Installing dependencies".to_string()) }
            },
        ));
        assert_eq!(result.as_deref(), Ok("Installing dependencies"));
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1);
        let dir = managed_root(home.path()).join("wordpress");
        assert_eq!(calls[0].cwd, dir);
        assert!(calls[0].args.contains(&"--no-interaction".to_string()));
        assert!(calls[0].env.contains(&(
            "COMPOSER_HOME".to_string(),
            dir.join(".composer-home").display().to_string()
        )));
        // The manifest and the lock it installs from were written there.
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("composer.json")).unwrap()).unwrap();
        assert!(manifest["require"]["wp-coding-standards/wpcs"].is_string());
        let lock: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("composer.lock")).unwrap()).unwrap();
        assert!(
            lock["packages"]
                .as_array()
                .is_some_and(|packages| !packages.is_empty())
        );
    }

    #[test]
    fn test_every_profile_installs_from_its_own_lock_in_its_own_folder() {
        let home = tempfile::tempdir().unwrap();
        for profile in [Profile::WordPress, Profile::Laravel, Profile::Php] {
            let cwd = std::cell::RefCell::new(None);
            futures::executor::block_on(install(
                home.path(),
                profile,
                Path::new("php"),
                Path::new("composer"),
                |invocation| {
                    cwd.replace(Some(invocation.cwd));
                    async { Ok(String::new()) }
                },
            ))
            .unwrap();
            let dir = managed_root(home.path()).join(profile.dir_name());
            assert_eq!(cwd.into_inner(), Some(dir.clone()));
            assert_eq!(
                fs::read_to_string(dir.join("composer.lock")).unwrap(),
                profile.composer_lock()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_remove_deletes_one_profile_and_keeps_composer_and_answers() {
        let home = tempfile::tempdir().unwrap();
        let root = managed_root(home.path());
        write(&root, "wordpress/vendor/bin/phpcs", "");
        write(&root, "wordpress/install.log", "log");
        write(&root, "laravel/vendor/bin/pint", "");
        write(&root, "composer.phar", "phar");
        save_consent(home.path(), Profile::WordPress, Consent::Yes).unwrap();

        remove(home.path(), Profile::WordPress).unwrap();
        assert!(!root.join("wordpress").exists());
        assert!(root.join("laravel/vendor/bin/pint").is_file());
        assert!(composer_phar(home.path()).is_file());
        assert_eq!(load_consent(home.path(), Profile::WordPress), Consent::Yes);
        // Nothing to remove is not an error.
        remove(home.path(), Profile::WordPress).unwrap();

        // A symlinked profile folder loses the link, not its target.
        let elsewhere = tempfile::tempdir().unwrap();
        write(elsewhere.path(), "keep.txt", "mine");
        std::os::unix::fs::symlink(elsewhere.path(), root.join("php")).unwrap();
        remove(home.path(), Profile::Php).unwrap();
        assert!(!root.join("php").exists());
        assert!(elsewhere.path().join("keep.txt").is_file());
    }

    #[test]
    fn test_install_failure_returns_the_composer_error() {
        let home = tempfile::tempdir().unwrap();
        let result = futures::executor::block_on(install(
            home.path(),
            Profile::Php,
            Path::new("php"),
            Path::new("composer"),
            |_| async { Err("Your requirements could not be resolved".to_string()) },
        ));
        assert_eq!(
            result,
            Err("Your requirements could not be resolved".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_run_command_returns_output_or_the_error_text() {
        let run = |script: &str| {
            futures::executor::block_on(run_command(Invocation {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                env: vec![("ZENITH_TEST".into(), "yes".into())],
            }))
        };
        assert_eq!(run("echo $ZENITH_TEST"), Ok("yes".to_string()));
        let error = run("echo 'could not resolve' >&2; exit 1").unwrap_err();
        assert!(error.contains("could not resolve"), "{error}");
    }

    #[test]
    fn test_every_profile_lock_pins_what_its_manifest_requires() {
        for profile in [Profile::WordPress, Profile::Laravel, Profile::Php] {
            let manifest = profile.composer_json();
            let lock: serde_json::Value = serde_json::from_str(profile.composer_lock()).unwrap();
            let locked: Vec<&str> = lock["packages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|package| package["name"].as_str())
                .collect();
            for package in manifest["require"].as_object().unwrap().keys() {
                assert!(locked.contains(&package.as_str()), "{profile:?}: {package}");
            }
            // The lock was made for the manifest next to it.
            assert!(lock["content-hash"].is_string());
        }
    }

    #[test]
    fn test_install_log_sits_next_to_the_manifest() {
        assert_eq!(
            install_log(Path::new("/Users/a"), Profile::Php),
            Path::new("/Users/a/.local/share/zenith/tools/php/php/install.log")
        );
    }

    #[test]
    fn test_composer_download_is_kept_only_when_the_checksum_matches() {
        let phar = b"#!/usr/bin/env php\n<?php // pretend composer".to_vec();
        let good = sha256_hex(&phar);
        let serve = |checksum: String| {
            let phar = phar.clone();
            http_client::FakeHttpClient::create(move |request| {
                let body = if request.uri().to_string().ends_with(".sha256") {
                    format!("{checksum}  composer.phar\n").into_bytes()
                } else {
                    phar.clone()
                };
                async move {
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(http_client::AsyncBody::from(body))
                        .unwrap())
                }
            })
        };

        let home = tempfile::tempdir().unwrap();
        let path =
            futures::executor::block_on(download_composer(serve(good), home.path())).unwrap();
        assert_eq!(path, composer_phar(home.path()));
        assert_eq!(fs::read(&path).unwrap(), phar);

        let other = tempfile::tempdir().unwrap();
        let error =
            futures::executor::block_on(download_composer(serve("0".repeat(64)), other.path()))
                .unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert!(!composer_phar(other.path()).exists());

        let missing = futures::executor::block_on(download_composer(
            http_client::FakeHttpClient::with_404_response(),
            other.path(),
        ))
        .unwrap_err();
        assert!(missing.contains("HTTP 404"), "{missing}");
    }
}
