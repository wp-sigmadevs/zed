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
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

/// The project's own `vendor/bin` first, then the system `PATH`. Zenith-managed
/// tools slot in between with milestone 2.
pub fn resolve(project: &Path, tool: Tool, path_dirs: &[PathBuf]) -> Option<Resolved> {
    let project_bin = project.join("vendor/bin").join(tool.binary());
    if project_bin.is_file() {
        return Some(Resolved {
            path: project_bin,
            source: Source::Project,
        });
    }
    path_dirs
        .iter()
        .map(|dir| dir.join(tool.binary()))
        .find(|candidate| candidate.is_file())
        .map(|path| Resolved {
            path,
            source: Source::System,
        })
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
pub fn find_php(candidates: &[PathBuf]) -> Option<(PathBuf, String)> {
    candidates
        .iter()
        .filter(|candidate| candidate.is_file())
        .find_map(|candidate| {
            let output = std::process::Command::new(candidate)
                .arg("-v")
                .output()
                .ok()?;
            let version = parse_php_version(&String::from_utf8_lossy(&output.stdout))?;
            Some((candidate.clone(), version))
        })
}

pub fn detect_php(root: &Path, candidates: &[PathBuf]) -> PhpInfo {
    let found = find_php(candidates);
    PhpInfo {
        kind: detect_project(root),
        version: found.as_ref().map(|(_, version)| version.clone()),
        php_path: found.map(|(path, _)| path),
    }
}

/// What "PHP tools: status" shows.
pub fn status_report(root: &Path, info: &PhpInfo, path_dirs: &[PathBuf]) -> String {
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
        lines.push(match resolve(root, tool, path_dirs) {
            Some(Resolved { path, source }) => {
                let source = match source {
                    Source::Project => "project",
                    Source::System => "system",
                };
                format!("{}: {source}, {}", tool.binary(), path.display())
            }
            None => format!("{}: not found", tool.binary()),
        });
    }
    lines.join("\n")
}

// ---- status bar item and "PHP tools: status" ----

use gpui::{
    Action as _, App, AppContext as _, Context, Entity, IntoElement, Render, Window, actions,
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
        let item = cx.new(|cx| PhpStatus::new(project, cx));
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
                status_report(&root, &detect_php(&root, &candidates), &path_dirs)
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
    info: Option<PhpInfo>,
}

impl PhpStatus {
    fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
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
                .background_spawn(async move {
                    looks_like_php(&root).then(|| detect_php(&root, &environment().0))
                })
                .await;
            this.update(cx, |this, cx| {
                this.info = info;
                cx.notify();
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

        let phpcs = resolve(project.path(), Tool::Phpcs, &path_dirs).unwrap();
        assert_eq!(phpcs.source, Source::Project);
        assert_eq!(phpcs.path, project.path().join("vendor/bin/phpcs"));

        let phpstan = resolve(project.path(), Tool::Phpstan, &path_dirs).unwrap();
        assert_eq!(phpstan.source, Source::System);

        assert_eq!(resolve(project.path(), Tool::Pint, &path_dirs), None);
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

        let info = detect_php(
            project.path(),
            &[
                PathBuf::from("/nonexistent/php"),
                broken_php,
                working_php.clone(),
            ],
        );
        assert_eq!(info.version.as_deref(), Some("8.5.0"));
        assert_eq!(info.php_path, Some(working_php));
        assert!(looks_like_php(project.path()));

        let none = detect_php(project.path(), &[]);
        assert_eq!(none.version, None);
        let report = status_report(project.path(), &none, &[]);
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
        let report = status_report(project.path(), &info, &[]);
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
        use gpui::AppContext as _;
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
}
