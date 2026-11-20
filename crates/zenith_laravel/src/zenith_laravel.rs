pub mod laravel_panel;
pub use laravel_panel::{LaravelPanel, LaravelPanelTab, ToggleLaravelPanel};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

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
    indexed_files: HashSet<PathBuf>,
}

impl LaravelIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_laravel_project(root_path: &Path) -> bool {
        root_path.join("artisan").is_file()
            || root_path.join("bootstrap/app.php").is_file()
            || root_path.join("composer.json").is_file()
                && std::fs::read_to_string(root_path.join("composer.json"))
                    .map(|c| c.contains("laravel/framework"))
                    .unwrap_or(false)
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
        self.indexed_files.insert(file_path.to_path_buf());
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
        self.indexed_files.insert(file_path);
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
        self.indexed_files.insert(file_path);
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
        self.indexed_files.insert(file_path);
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

pub fn parse_config_keys(config_name: &str, file_path: &Path, content: &str) -> Vec<ConfigDefinition> {
    let mut configs = Vec::new();
    let mut key_stack: Vec<(String, usize)> = Vec::new();

    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*") || trimmed.starts_with('*') {
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
                    let parent_keys: Vec<&str> = key_stack.iter().map(|(k, _)| k.as_str()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_views_and_components() {
        let mut index = LaravelIndex::new();
        let base = Path::new("/app/resources/views");

        index.index_view_path(base, Path::new("/app/resources/views/auth/login.blade.php"));
        index.index_view_path(base, Path::new("/app/resources/views/auth/register.blade.php"));
        index.index_view_path(base, Path::new("/app/resources/views/dashboard.blade.php"));
        index.index_view_path(base, Path::new("/app/resources/views/components/alert.blade.php"));

        let view = index.find_view("auth.login").expect("view should exist");
        assert_eq!(view.dot_name, "auth.login");
        assert!(view.is_blade);
        assert!(!view.is_component);

        let completions = index.view_completions("auth.");
        assert_eq!(completions, vec!["auth.login", "auth.register"]);

        let component = index.find_component("x-alert").expect("component should exist");
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
        assert_eq!(route.action.as_deref(), Some("[UserController::class, 'index']"));
        assert_eq!(route.line, 2);

        let completions = index.route_completions("users.");
        assert_eq!(completions, vec!["users.destroy", "users.index", "users.store"]);
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

        let nested = index.find_config("app.database.default").expect("nested config should exist");
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
}
