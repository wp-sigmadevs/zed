pub mod laravel_panel;
mod setup;
pub use laravel_panel::{LaravelPanel, LaravelPanelTab, ShowRoutes, ShowViews, ToggleLaravelPanel};
pub use setup::{LaravelSettings, init};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewDefinition {
    pub dot_name: String,
    pub file_path: PathBuf,
    pub is_blade: bool,
    pub is_component: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDefinition {
    pub name: String,
    pub method: String,
    pub uri: String,
    pub action: Option<String>,
    pub file_path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDefinition {
    pub key: String,
    pub file_path: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvDefinition {
    pub key: String,
    pub file_path: PathBuf,
    pub line: usize,
    pub example_value: Option<String>,
}

#[derive(Default)]
pub struct LaravelIndex {
    views: HashMap<String, ViewDefinition>,
    components: HashMap<String, ViewDefinition>,
    routes: HashMap<String, RouteDefinition>,
    configs: HashMap<String, ConfigDefinition>,
    env_vars: HashMap<String, EnvDefinition>,
    /// Files whose views are in `views`, and route files that were read (even one with no named
    /// route). What the caps count.
    view_files: HashSet<PathBuf>,
    route_files: HashSet<PathBuf>,
}

impl LaravelIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_laravel_project(root_path: &Path) -> bool {
        root_path.join("artisan").is_file()
            || root_path.join("bootstrap/app.php").is_file()
            || manifest_contains(&root_path.join("composer.json"), "laravel/framework")
    }

    pub fn index_view_path(&mut self, base_views_path: &Path, file_path: &Path) {
        let Ok(relative) = file_path.strip_prefix(base_views_path) else {
            return;
        };

        let path_str = relative.to_string_lossy();
        let (stem, is_blade) = if let Some(stripped) = path_str.strip_suffix(".blade.php") {
            (stripped, true)
        } else if let Some(stripped) = path_str.strip_suffix(".php") {
            (stripped, false)
        } else {
            return;
        };

        let dot_name = stem.replace('/', ".");
        let is_component = stem.starts_with("components/") || stem.starts_with("components.");

        let definition = ViewDefinition {
            dot_name: dot_name.clone(),
            file_path: file_path.to_path_buf(),
            is_blade,
            is_component,
        };

        if is_component {
            let component_name = stem
                .strip_prefix("components/")
                .or_else(|| stem.strip_prefix("components."))
                .unwrap_or(stem)
                .replace('/', ".");
            let tag_name = format!("x-{}", component_name.replace('.', "-"));
            self.components.insert(tag_name, definition.clone());
        }

        self.views.insert(dot_name, definition);
        self.view_files.insert(file_path.to_path_buf());
    }

    pub fn find_view(&self, dot_name: &str) -> Option<&ViewDefinition> {
        self.views.get(dot_name)
    }

    pub fn view_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .views
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    pub fn find_component(&self, tag_name: &str) -> Option<&ViewDefinition> {
        self.components.get(tag_name)
    }

    pub fn component_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .components
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    pub fn index_route_file(&mut self, file_path: PathBuf, content: &str) {
        self.remove_routes_for_file(&file_path);
        let parsed_routes = parse_route_file(&file_path, content);
        for route in parsed_routes {
            self.routes.insert(route.name.clone(), route);
        }
        self.route_files.insert(file_path);
    }

    /// Every named route, by name.
    pub fn routes_sorted(&self) -> Vec<RouteDefinition> {
        let mut routes: Vec<_> = self.routes.values().cloned().collect();
        routes.sort_by(|a, b| a.name.cmp(&b.name));
        routes
    }

    /// Every view, by dot name.
    pub fn views_sorted(&self) -> Vec<ViewDefinition> {
        let mut views: Vec<_> = self.views.values().cloned().collect();
        views.sort_by(|a, b| a.dot_name.cmp(&b.dot_name));
        views
    }

    pub fn contains_file(&self, file_path: &Path) -> bool {
        self.view_files.contains(file_path) || self.route_files.contains(file_path)
    }

    /// How many view files lie under `root`.
    pub fn view_count_under(&self, root: &Path) -> usize {
        self.view_files
            .iter()
            .filter(|file| file.starts_with(root))
            .count()
    }

    /// How many route files lie under `root`.
    pub fn route_file_count_under(&self, root: &Path) -> usize {
        self.route_files
            .iter()
            .filter(|file| file.starts_with(root))
            .count()
    }

    /// Forgets the views and routes of a file that was deleted or can no longer be read.
    pub fn remove_file(&mut self, file_path: &Path) {
        if self.contains_file(file_path) {
            self.forget(|path| path == file_path);
        }
    }

    /// Forgets every file under `root` (a worktree that left the project).
    pub fn remove_dir(&mut self, root: &Path) {
        self.forget(|path| path.starts_with(root));
    }

    // ponytail: scans every entry per removal, fine at the 20k-view cap; key entries by file if
    // bulk deletes get slow.
    fn forget(&mut self, gone: impl Fn(&Path) -> bool) {
        self.view_files.retain(|file| !gone(file));
        self.route_files.retain(|file| !gone(file));
        self.views.retain(|_, view| !gone(&view.file_path));
        self.components.retain(|_, view| !gone(&view.file_path));
        self.routes.retain(|_, route| !gone(&route.file_path));
    }

    pub fn find_route(&self, name: &str) -> Option<&RouteDefinition> {
        self.routes.get(name)
    }

    pub fn route_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .routes
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    pub fn index_config_file(&mut self, config_name: &str, file_path: PathBuf, content: &str) {
        self.remove_configs_for_file(&file_path);
        let keys = parse_config_keys(config_name, &file_path, content);
        for config in keys {
            self.configs.insert(config.key.clone(), config);
        }
    }

    pub fn find_config(&self, key: &str) -> Option<&ConfigDefinition> {
        self.configs.get(key)
    }

    pub fn config_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .configs
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    pub fn index_env_file(&mut self, file_path: PathBuf, content: &str) {
        self.remove_env_for_file(&file_path);
        let vars = parse_env_file(&file_path, content);
        for var in vars {
            self.env_vars.insert(var.key.clone(), var);
        }
    }

    pub fn find_env(&self, key: &str) -> Option<&EnvDefinition> {
        self.env_vars.get(key)
    }

    pub fn env_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .env_vars
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    fn remove_routes_for_file(&mut self, file_path: &Path) {
        self.routes.retain(|_, v| v.file_path != file_path);
    }

    fn remove_configs_for_file(&mut self, file_path: &Path) {
        self.configs.retain(|_, v| v.file_path != file_path);
    }

    fn remove_env_for_file(&mut self, file_path: &Path) {
        self.env_vars.retain(|_, v| v.file_path != file_path);
    }
}

pub fn parse_route_file(file_path: &Path, content: &str) -> Vec<RouteDefinition> {
    let mut routes = Vec::new();

    let methods = ["get", "post", "put", "patch", "delete", "any", "match"];

    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        if !trimmed.contains("Route::") {
            continue;
        }

        let mut matched_method = None;
        for &m in &methods {
            let needle = format!("Route::{m}(");
            if let Some(pos) = line.find(&needle) {
                matched_method = Some((m.to_uppercase(), pos + needle.len()));
                break;
            }
        }

        let Some((method, after_paren_idx)) = matched_method else {
            continue;
        };

        let call_content = &line[after_paren_idx..];
        let Some((uri, remainder)) = extract_string_literal(call_content) else {
            continue;
        };

        let mut action = None;
        let trimmed_rem = remainder.trim_start();
        if let Some(after_comma) = trimmed_rem.strip_prefix(',') {
            let after_comma = after_comma.trim_start();
            if after_comma.starts_with('[') {
                if let Some(close_bracket) = after_comma.find(']') {
                    action = Some(after_comma[..=close_bracket].trim().to_string());
                }
            } else if after_comma.starts_with('\'') || after_comma.starts_with('"') {
                if let Some((act, _)) = extract_string_literal(after_comma) {
                    action = Some(act);
                }
            }
        }

        let name_marker = "->name(";
        if let Some(name_pos) = line.find(name_marker) {
            let after_name = &line[name_pos + name_marker.len()..];
            if let Some((name, _)) = extract_string_literal(after_name) {
                routes.push(RouteDefinition {
                    name,
                    method,
                    uri,
                    action,
                    file_path: file_path.to_path_buf(),
                    line: line_index + 1,
                    column: name_pos + 1,
                });
            }
        }
    }

    routes
}

pub fn parse_config_keys(
    config_name: &str,
    file_path: &Path,
    content: &str,
) -> Vec<ConfigDefinition> {
    let mut configs = Vec::new();
    let mut key_stack: Vec<(String, usize)> = Vec::new();

    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        while let Some(&(_, parent_indent)) = key_stack.last() {
            if indent <= parent_indent {
                key_stack.pop();
            } else {
                break;
            }
        }

        if let Some(arrow_idx) = trimmed.find("=>") {
            let left = trimmed[..arrow_idx].trim();
            let right = trimmed[arrow_idx + 2..].trim();

            if let Some((key_name, _)) = extract_string_literal(left) {
                let full_key = if key_stack.is_empty() {
                    format!("{config_name}.{key_name}")
                } else {
                    let parent_keys: Vec<&str> =
                        key_stack.iter().map(|(k, _)| k.as_str()).collect();
                    format!("{}.{}.{}", config_name, parent_keys.join("."), key_name)
                };

                configs.push(ConfigDefinition {
                    key: full_key,
                    file_path: file_path.to_path_buf(),
                    line: line_index + 1,
                });

                if right.starts_with('[') || right.to_lowercase().starts_with("array(") {
                    key_stack.push((key_name, indent));
                }
            }
        }
    }

    configs
}

pub fn parse_env_file(file_path: &Path, content: &str) -> Vec<EnvDefinition> {
    let mut envs = Vec::new();

    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(eq_idx) = trimmed.find('=') {
            let key = trimmed[..eq_idx].trim();
            let value = trimmed[eq_idx + 1..].trim();

            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                envs.push(EnvDefinition {
                    key: key.to_string(),
                    file_path: file_path.to_path_buf(),
                    line: line_index + 1,
                    example_value: if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    },
                });
            }
        }
    }

    envs
}

fn extract_string_literal(input: &str) -> Option<(String, &str)> {
    let trimmed = input.trim_start();
    let quote = trimmed.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }

    let mut escaped = false;
    let mut end_index = None;
    for (i, ch) in trimmed[1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            end_index = Some(i + 1);
            break;
        }
    }

    let end_pos = end_index?;
    let literal = &trimmed[1..end_pos];
    let remainder = &trimmed[end_pos + 1..];
    Some((literal.to_string(), remainder))
}

/// A manifest is small; only this much of one is read.
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

fn manifest_contains(path: &Path, needle: &str) -> bool {
    zenith_php_tools::read_regular(path, MANIFEST_MAX_BYTES)
        .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_views_and_components() {
        let mut index = LaravelIndex::new();
        let base = Path::new("/app/resources/views");

        index.index_view_path(base, Path::new("/app/resources/views/auth/login.blade.php"));
        index.index_view_path(
            base,
            Path::new("/app/resources/views/auth/register.blade.php"),
        );
        index.index_view_path(base, Path::new("/app/resources/views/dashboard.blade.php"));
        index.index_view_path(
            base,
            Path::new("/app/resources/views/components/alert.blade.php"),
        );

        let view = index.find_view("auth.login").expect("view should exist");
        assert_eq!(view.dot_name, "auth.login");
        assert!(view.is_blade);
        assert!(!view.is_component);

        let completions = index.view_completions("auth.");
        assert_eq!(completions, vec!["auth.login", "auth.register"]);

        let component = index
            .find_component("x-alert")
            .expect("component should exist");
        assert_eq!(component.dot_name, "components.alert");
        assert!(component.is_component);
    }

    #[test]
    fn test_routes_parsing() {
        let mut index = LaravelIndex::new();
        let content = r#"<?php
Route::get('/users', [UserController::class, 'index'])->name('users.index');
Route::post('/users', [UserController::class, 'store'])->name('users.store');
Route::delete('/users/{id}', 'UserController@destroy')->name('users.destroy');
"#;
        index.index_route_file(PathBuf::from("routes/web.php"), content);

        let route = index.find_route("users.index").expect("route should exist");
        assert_eq!(route.name, "users.index");
        assert_eq!(route.method, "GET");
        assert_eq!(route.uri, "/users");
        assert_eq!(
            route.action.as_deref(),
            Some("[UserController::class, 'index']")
        );
        assert_eq!(route.line, 2);

        let completions = index.route_completions("users.");
        assert_eq!(
            completions,
            vec!["users.destroy", "users.index", "users.store"]
        );
    }

    #[test]
    fn test_config_parsing() {
        let mut index = LaravelIndex::new();
        let content = r#"<?php
return [
    'name' => 'My Laravel App',
    'env' => 'production',
    'database' => [
        'default' => 'mysql',
    ],
];
"#;
        index.index_config_file("app", PathBuf::from("config/app.php"), content);

        let config = index.find_config("app.name").expect("config should exist");
        assert_eq!(config.key, "app.name");
        assert_eq!(config.line, 3);

        let nested = index
            .find_config("app.database.default")
            .expect("nested config should exist");
        assert_eq!(nested.key, "app.database.default");
        assert_eq!(nested.line, 6);
    }

    #[test]
    fn test_env_parsing() {
        let mut index = LaravelIndex::new();
        let content = r#"
APP_NAME=Laravel
APP_ENV=local
APP_KEY=base64:xxx
DB_CONNECTION=mysql
DB_HOST=127.0.0.1
"#;
        index.index_env_file(PathBuf::from(".env"), content);

        let env_var = index.find_env("DB_HOST").expect("env should exist");
        assert_eq!(env_var.key, "DB_HOST");
        assert_eq!(env_var.example_value.as_deref(), Some("127.0.0.1"));

        let completions = index.env_completions("APP_");
        assert_eq!(completions, vec!["APP_ENV", "APP_KEY", "APP_NAME"]);
    }

    #[test]
    fn test_removing_a_file_removes_what_it_gave_the_index() {
        let mut index = LaravelIndex::new();
        let base = Path::new("/app/resources/views");
        let card = base.join("components/card.blade.php");
        index.index_view_path(base, &card);
        index.index_view_path(base, &base.join("home.blade.php"));
        let web = PathBuf::from("/app/routes/web.php");
        index.index_route_file(
            web.clone(),
            "<?php\nRoute::get('/', 'H@i')->name('home');\n",
        );
        assert_eq!(
            (
                index.view_count_under(Path::new("/app")),
                index.route_file_count_under(Path::new("/app"))
            ),
            (2, 1)
        );

        index.remove_file(&card);
        assert!(index.find_view("components.card").is_none());
        assert!(index.find_component("x-card").is_none());
        assert!(!index.contains_file(&card));
        assert!(index.find_view("home").is_some());

        index.remove_file(&web);
        assert!(index.find_route("home").is_none());
        assert!(!index.contains_file(&web));

        index.remove_dir(Path::new("/app"));
        assert!(index.views_sorted().is_empty());
        assert_eq!(index.view_count_under(Path::new("/app")), 0);
    }

    #[test]
    fn test_routes_and_views_come_back_sorted() {
        let mut index = LaravelIndex::new();
        let base = Path::new("/a/resources/views");
        for name in ["zed", "alpha", "mid"] {
            index.index_view_path(base, &base.join(format!("{name}.blade.php")));
        }
        let source: String = ["q", "z", "c", "a", "m", "x", "b", "k"]
            .iter()
            .map(|n| format!("Route::get('/{n}', 'C@m')->name('{n}');\n"))
            .collect();
        index.index_route_file(PathBuf::from("/a/routes/web.php"), &source);
        let views: Vec<_> = index
            .views_sorted()
            .into_iter()
            .map(|v| v.dot_name)
            .collect();
        let routes: Vec<_> = index.routes_sorted().into_iter().map(|r| r.name).collect();
        assert_eq!(views, ["alpha", "mid", "zed"]);
        assert_eq!(routes, ["a", "b", "c", "k", "m", "q", "x", "z"]);
    }

    #[test]
    fn test_project_detection_reads_manifests_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("composer.json");
        assert!(!LaravelIndex::is_laravel_project(dir.path()));

        std::fs::write(&manifest, r#"{"require": {"laravel/framework": "^1"}}"#).unwrap();
        assert!(LaravelIndex::is_laravel_project(dir.path()));

        // The marker sits past the read limit, so it is not seen.
        let mut big = " ".repeat(MANIFEST_MAX_BYTES as usize + 16);
        big.push_str("laravel/framework");
        std::fs::write(&manifest, big).unwrap();
        assert!(!LaravelIndex::is_laravel_project(dir.path()));
    }

    /// A pipe in place of the manifest must not block detection.
    #[cfg(unix)]
    #[test]
    fn test_project_detection_ignores_a_pipe_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            std::ffi::CString::new(dir.path().join("composer.json").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(!LaravelIndex::is_laravel_project(dir.path()));
    }
}
