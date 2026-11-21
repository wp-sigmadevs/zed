pub mod nextjs_panel;
pub use nextjs_panel::{NextJsPanel, ToggleNextJsPanel};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppRouterKind {
    Page,
    Layout,
    Loading,
    Error,
    RouteHandler,
    NotFound,
    Template,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComponentBoundary {
    ServerComponent,
    ClientComponent,
    ServerAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRoute {
    pub url_path: String,
    pub kind: AppRouterKind,
    pub file_path: PathBuf,
    pub is_dynamic: bool,
    pub dynamic_params: Vec<String>,
    pub http_methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RscDiagnostic {
    pub file_path: PathBuf,
    pub line: usize,
    pub message: String,
}

#[derive(Default)]
pub struct NextJsIndex {
    routes_by_path: HashMap<String, Vec<AppRoute>>,
    files_indexed: HashSet<PathBuf>,
}

impl NextJsIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_nextjs_project(root_path: &Path) -> bool {
        root_path.join("next.config.js").is_file()
            || root_path.join("next.config.mjs").is_file()
            || root_path.join("next.config.ts").is_file()
            || manifest_contains(&root_path.join("package.json"), "\"next\"")
    }

    pub fn index_app_file(&mut self, app_root: &Path, file_path: &Path, content: &str) {
        let Ok(relative) = file_path.strip_prefix(app_root) else {
            return;
        };

        let file_name = relative
            .file_name()
            .map(|f| f.to_string_lossy())
            .unwrap_or_default();

        let kind = if file_name.starts_with("page.") {
            AppRouterKind::Page
        } else if file_name.starts_with("layout.") {
            AppRouterKind::Layout
        } else if file_name.starts_with("loading.") {
            AppRouterKind::Loading
        } else if file_name.starts_with("error.") {
            AppRouterKind::Error
        } else if file_name.starts_with("route.") {
            AppRouterKind::RouteHandler
        } else if file_name.starts_with("not-found.") {
            AppRouterKind::NotFound
        } else if file_name.starts_with("template.") {
            AppRouterKind::Template
        } else {
            return;
        };

        let parent_dir = relative.parent().unwrap_or(Path::new(""));
        let (url_path, is_dynamic, dynamic_params) = calculate_route_path(parent_dir);

        let http_methods = if kind == AppRouterKind::RouteHandler {
            extract_http_methods(content)
        } else {
            Vec::new()
        };

        let route = AppRoute {
            url_path: url_path.clone(),
            kind,
            file_path: file_path.to_path_buf(),
            is_dynamic,
            dynamic_params,
            http_methods,
        };

        self.remove_file(file_path);
        self.routes_by_path.entry(url_path).or_default().push(route);
        self.files_indexed.insert(file_path.to_path_buf());
    }

    pub fn remove_file(&mut self, file_path: &Path) {
        if !self.files_indexed.remove(file_path) {
            return;
        }
        for routes in self.routes_by_path.values_mut() {
            routes.retain(|r| r.file_path != file_path);
        }
        self.routes_by_path.retain(|_, v| !v.is_empty());
    }

    pub fn find_routes_for_path(&self, url_path: &str) -> Vec<&AppRoute> {
        self.routes_by_path
            .get(url_path)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub fn route_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .routes_by_path
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    pub fn check_component_safety(&self, file_path: &Path, content: &str) -> Vec<RscDiagnostic> {
        analyze_component_safety(file_path, content)
    }
}

pub fn calculate_route_path(relative_dir: &Path) -> (String, bool, Vec<String>) {
    let mut segments = Vec::new();
    let mut is_dynamic = false;
    let mut dynamic_params = Vec::new();

    for comp in relative_dir.components() {
        let seg = comp.as_os_str().to_string_lossy();

        if seg.starts_with('(') && seg.ends_with(')') {
            continue;
        }

        if seg.starts_with('@') {
            continue;
        }

        if seg.starts_with("[[...") && seg.ends_with("]]") {
            let param = seg[5..seg.len() - 2].to_string();
            dynamic_params.push(param.clone());
            segments.push(format!("*{{{param}}}?"));
            is_dynamic = true;
        } else if seg.starts_with("[...") && seg.ends_with(']') {
            let param = seg[4..seg.len() - 1].to_string();
            dynamic_params.push(param.clone());
            segments.push(format!("*{{{param}}}"));
            is_dynamic = true;
        } else if seg.starts_with('[') && seg.ends_with(']') {
            let param = seg[1..seg.len() - 1].to_string();
            dynamic_params.push(param.clone());
            segments.push(format!(":{param}"));
            is_dynamic = true;
        } else {
            segments.push(seg.to_string());
        }
    }

    let url_path = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };

    (url_path, is_dynamic, dynamic_params)
}

pub fn extract_http_methods(content: &str) -> Vec<String> {
    let methods = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    let mut found = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("export") {
            continue;
        }

        for &method in &methods {
            let func_pattern = format!("function {method}");
            let const_pattern = format!("const {method} =");
            let async_pattern = format!("async function {method}");

            if trimmed.contains(&func_pattern)
                || trimmed.contains(&const_pattern)
                || trimmed.contains(&async_pattern)
            {
                if !found.contains(&method.to_string()) {
                    found.push(method.to_string());
                }
            }
        }
    }

    found
}

pub fn detect_component_boundary(content: &str) -> ComponentBoundary {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        if trimmed == "\"use client\";"
            || trimmed == "'use client';"
            || trimmed == "\"use client\""
            || trimmed == "'use client'"
        {
            return ComponentBoundary::ClientComponent;
        }
        if trimmed == "\"use server\";"
            || trimmed == "'use server';"
            || trimmed == "\"use server\""
            || trimmed == "'use server'"
        {
            return ComponentBoundary::ServerAction;
        }
        if !trimmed.is_empty() && !trimmed.starts_with("import") {
            break;
        }
    }

    ComponentBoundary::ServerComponent
}

pub fn analyze_component_safety(file_path: &Path, content: &str) -> Vec<RscDiagnostic> {
    let boundary = detect_component_boundary(content);
    let mut diagnostics = Vec::new();

    if boundary != ComponentBoundary::ServerComponent {
        return diagnostics;
    }

    let client_hooks = [
        "useState",
        "useEffect",
        "useReducer",
        "useRef",
        "useLayoutEffect",
        "useContext",
        "useImperativeHandle",
    ];

    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }

        for &hook in &client_hooks {
            if let Some(pos) = line.find(hook) {
                let is_word_boundary_before = if pos > 0 {
                    let prev_char = line[..pos].chars().last();
                    prev_char
                        .map(|c| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(true)
                } else {
                    true
                };

                let after_pos = pos + hook.len();
                let is_call_after = if after_pos < line.len() {
                    let next_str = line[after_pos..].trim_start();
                    next_str.starts_with('(')
                } else {
                    false
                };

                if is_word_boundary_before && is_call_after {
                    diagnostics.push(RscDiagnostic {
                        file_path: file_path.to_path_buf(),
                        line: line_index + 1,
                        message: format!(
                            "React hook '{hook}' cannot be called in a Server Component. Add '\"use client\"' to the top of the file."
                        ),
                    });
                }
            }
        }
    }

    diagnostics
}

/// A manifest is small; only this much of one is read.
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

/// Up to `max` bytes of a regular file, symlinks followed. `None` for a device, pipe, folder or
/// unreadable path: a project can plant `composer.json -> /dev/zero`, and an unbounded read of
/// it never ends. The check is on the opened handle, and the open is non-blocking (a plain open
/// of a pipe with no writer blocks). Same logic as `zenith_php_tools::read_regular`.
fn read_regular(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut options = std::fs::OpenOptions::new();
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

fn manifest_contains(path: &Path, needle: &str) -> bool {
    read_regular(path, MANIFEST_MAX_BYTES)
        .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_router_path_calculation() {
        let (path, is_dyn, _) = calculate_route_path(Path::new(""));
        assert_eq!(path, "/");
        assert!(!is_dyn);

        let (path, is_dyn, _) = calculate_route_path(Path::new("about"));
        assert_eq!(path, "/about");
        assert!(!is_dyn);

        let (path, is_dyn, _) = calculate_route_path(Path::new("(marketing)/about"));
        assert_eq!(path, "/about");
        assert!(!is_dyn);

        let (path, is_dyn, params) = calculate_route_path(Path::new("posts/[slug]"));
        assert_eq!(path, "/posts/:slug");
        assert!(is_dyn);
        assert_eq!(params, vec!["slug"]);

        let (path, is_dyn, params) = calculate_route_path(Path::new("shop/[...categories]"));
        assert_eq!(path, "/shop/*{categories}");
        assert!(is_dyn);
        assert_eq!(params, vec!["categories"]);
    }

    #[test]
    fn test_route_handlers_methods() {
        let content = r#"
import { NextResponse } from 'next/server';

export async function GET(request: Request) {
    return NextResponse.json({ ok: true });
}

export async function POST(request: Request) {
    return NextResponse.json({ created: true });
}
"#;
        let mut index = NextJsIndex::new();
        let app_root = Path::new("/app/src/app");
        let route_file = Path::new("/app/src/app/api/webhooks/route.ts");

        index.index_app_file(app_root, route_file, content);

        let routes = index.find_routes_for_path("/api/webhooks");
        assert_eq!(routes.len(), 1);
        let route = routes[0];
        assert_eq!(route.kind, AppRouterKind::RouteHandler);
        assert_eq!(route.http_methods, vec!["GET", "POST"]);

        let completions = index.route_completions("/api");
        assert_eq!(completions, vec!["/api/webhooks"]);
    }

    #[test]
    fn test_rsc_safety_diagnostics() {
        let server_component = r#"
import { fetchUser } from '@/lib/db';

export default function UserProfile({ id }: { id: string }) {
    const [user, setUser] = useState(null);
    return <div>User</div>;
}
"#;
        let diagnostics = analyze_component_safety(Path::new("UserProfile.tsx"), server_component);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("React hook 'useState' cannot be called in a Server Component")
        );
        assert_eq!(diagnostics[0].line, 5);

        let client_component = r#""use client";
import { useState } from 'react';

export default function UserProfile() {
    const [user, setUser] = useState(null);
    return <div>User</div>;
}
"#;
        let client_diagnostics =
            analyze_component_safety(Path::new("UserProfile.tsx"), client_component);
        assert!(client_diagnostics.is_empty());
    }

    #[test]
    fn test_project_detection_reads_manifests_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("package.json");
        assert!(!NextJsIndex::is_nextjs_project(dir.path()));

        std::fs::write(&manifest, r#"{"dependencies": {"next": "14"}}"#).unwrap();
        assert!(NextJsIndex::is_nextjs_project(dir.path()));

        // The marker sits past the read limit, so it is not seen.
        let mut big = " ".repeat(MANIFEST_MAX_BYTES as usize + 16);
        big.push_str("\"next\"");
        std::fs::write(&manifest, big).unwrap();
        assert!(!NextJsIndex::is_nextjs_project(dir.path()));
    }

    /// A pipe in place of the manifest must not block detection.
    #[cfg(unix)]
    #[test]
    fn test_project_detection_ignores_a_pipe_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            std::ffi::CString::new(dir.path().join("package.json").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(!NextJsIndex::is_nextjs_project(dir.path()));
    }
}
