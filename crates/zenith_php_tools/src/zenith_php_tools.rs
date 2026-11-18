//! PHP project detection and tool resolution (plan 35, milestone 1). Pure
//! functions over paths, so they test against temp directories; nothing here
//! downloads or runs a tool.

use std::{
    fs,
    path::{Path, PathBuf},
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

/// WordPress reads headers from the first 8 KB.
fn has_header(path: &Path, header: &str) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]).contains(header)
}

fn composer_requires(root: &Path, package: &str) -> bool {
    let Ok(text) = fs::read_to_string(root.join("composer.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
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
    for candidate in candidates.iter().filter(|candidate| candidate.is_file()) {
        let Ok(output) = util::command::new_command(candidate)
            .arg("-v")
            .output()
            .await
        else {
            continue;
        };
        if let Some(version) = parse_php_version(&String::from_utf8_lossy(&output.stdout)) {
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

// ---- status bar item and "PHP tools: status" ----

use gpui::{
    Action as _, App, AppContext as _, Context, Entity, IntoElement, Render, WeakEntity, Window,
    actions,
};
use project::Project;
use ui::{Button, ButtonCommon as _, ButtonSize, Clickable as _, LabelSize, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

actions!(
    zenith_php_tools,
    [
        /// Shows the PHP version and where each PHP tool comes from.
        Status
    ]
);

pub fn init(cx: &mut App) {
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
    })
    .detach();
}

fn environment() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    (php_candidates(&home, &path_dirs), path_dirs)
}

fn first_root(project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
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
        let mut this = Self {
            project,
            workspace,
            info: None,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
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
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let Some(info) = &self.info else {
            return div().into_any_element();
        };
        let label = match &info.version {
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

fn find_composer(path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut dirs = path_dirs.to_vec();
    dirs.extend([
        home.join("Library/Application Support/Herd/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    dirs.into_iter()
        .map(|dir| dir.join("composer"))
        .find(|candidate| candidate.is_file())
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
    let trusted = match (
        project::trusted_worktrees::TrustedWorktrees::try_get_global(cx),
        worktree_id,
    ) {
        (Some(store), Some(worktree_id)) => {
            let worktree_store = project.read(cx).worktree_store();
            store.update(cx, |store, cx| {
                store.can_trust(&worktree_store, worktree_id, cx)
            })
        }
        _ => true,
    };
    let (Some(php), Some(composer)) = (info.php_path.clone(), find_composer(&path_dirs)) else {
        return;
    };
    if !should_offer(load_consent(&home, profile), trusted, local, has_any_tool) {
        return;
    }
    let weak = cx.entity().downgrade();
    let label = match profile {
        Profile::WordPress => "WordPress tools: PHPCS with the WordPress standards, PHPStan",
        Profile::Laravel => "Laravel tools: Pint, PHPStan with Larastan",
        Profile::Php => "PHP tools: PHPStan, PHP-CS-Fixer",
    };
    workspace.show_notification(BannerId::unique::<InstallBanner>(), cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(
                format!("Zenith can set up {label}, in its own folder. Closing this asks again next time."),
                cx,
            )
            .primary_message("Install")
            .primary_on_click({
                let (weak, php, composer) = (weak.clone(), php.clone(), composer.clone());
                move |window, cx| {
                    let (php, composer) = (php.clone(), composer.clone());
                    weak.update(cx, |workspace, cx| {
                        save_consent(&home_dir(), profile, Consent::Yes).ok();
                        start_install(workspace, profile, php, composer, window, cx);
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
    composer: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.dismiss_notification(&BannerId::unique::<InstallBanner>(), cx);
    workspace.show_notification(BannerId::unique::<InstallProgress>(), cx, |cx| {
        cx.new(|cx| MessageNotification::new("Installing PHP tools…", cx))
    });
    let home = home_dir();
    cx.spawn_in(window, async move |workspace, cx| {
        let (run_php, run_composer) = (php.clone(), composer.clone());
        let result = cx
            .background_spawn(async move {
                install(&home, profile, &run_php, &run_composer, run_command).await
            })
            .await;
        workspace
            .update_in(cx, |workspace, _, cx| match result {
                Ok(_) => workspace.show_notification(
                    BannerId::unique::<InstallProgress>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(
                                "PHP tools installed. Run \"zenith php tools: status\" to see them.",
                                cx,
                            )
                        })
                    },
                ),
                Err(error) => {
                    let weak = cx.entity().downgrade();
                    workspace.show_notification(
                        BannerId::unique::<InstallProgress>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    format!("Installing PHP tools failed:\n{error}"),
                                    cx,
                                )
                                .primary_message("Retry")
                                .primary_on_click(move |window, cx| {
                                    let (php, composer) = (php.clone(), composer.clone());
                                    weak.update(cx, |workspace, cx| {
                                        start_install(workspace, profile, php, composer, window, cx)
                                    })
                                    .ok();
                                })
                            })
                        },
                    );
                }
            })
            .ok();
    })
    .detach();
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
}
