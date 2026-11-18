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
}
