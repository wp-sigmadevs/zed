pub mod nextjs_panel;
mod setup;
pub use nextjs_panel::{
    NextFilter, NextJsFilter, NextJsPanel, PreviousFilter, ShowAllRoutes, ShowPages,
    ShowRouteHandlers, ToggleNextJsPanel,
};
pub use setup::{NextJsSettings, init};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use zenith_php_tools::read_regular;

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

/// The App Router special files, by file name: `page`, `layout`, `loading`, `error`, `route`,
/// `not-found` or `template` with a js, jsx, ts, tsx or mjs extension. `page.test.tsx` and
/// `layout.css` are not special files.
fn kind_of_file_name(name: &str) -> Option<AppRouterKind> {
    let (stem, extension) = name.rsplit_once('.')?;
    if !matches!(extension, "js" | "jsx" | "ts" | "tsx" | "mjs") {
        return None;
    }
    Some(match stem {
        "page" => AppRouterKind::Page,
        "layout" => AppRouterKind::Layout,
        "loading" => AppRouterKind::Loading,
        "error" => AppRouterKind::Error,
        "route" => AppRouterKind::RouteHandler,
        "not-found" => AppRouterKind::NotFound,
        "template" => AppRouterKind::Template,
        _ => return None,
    })
}

/// What the index takes from a path under a worktree root (unix separators): the app dir it
/// lies under (`app` or `src/app`) and its kind. Only special files below an app dir count, and
/// nothing below `node_modules/`, `.next/` or `vendor/`.
pub(crate) fn classify(rel: &str) -> Option<(&'static str, AppRouterKind)> {
    let app_dir = ["app", "src/app"].into_iter().find(|dir| {
        rel.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    })?;
    if rel
        .split('/')
        .any(|part| matches!(part, "node_modules" | ".next" | "vendor"))
    {
        return None;
    }
    let kind = kind_of_file_name(rel.rsplit('/').next()?)?;
    Some((app_dir, kind))
}

/// Lines longer than this are not looked at for HTTP methods: a minified or hostile file is one
/// huge line.
/// ponytail: a handler declaration is a short line. Raise it if a generated one is missed.
const MAX_METHOD_LINE_BYTES: usize = 4 * 1024;

const HTTP_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// The App Router routes of the project, one per special file. The file is the key, so one file
/// is added or forgotten in one step, and two files that give the same URL (`app/` and
/// `src/app/`, or a page and its layout) are both kept.
#[derive(Default)]
pub struct NextJsIndex {
    routes: HashMap<PathBuf, AppRoute>,
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

    /// The route of `file_path` (below `app_root`), or `None` when it is not a special file.
    /// Parses `content` (read for a route handler only); needs no lock, so callers do it before
    /// taking one.
    pub fn route_for(app_root: &Path, file_path: &Path, content: &str) -> Option<AppRoute> {
        let relative = file_path.strip_prefix(app_root).ok()?;
        let kind = kind_of_file_name(&relative.file_name()?.to_string_lossy())?;
        let parent_dir = relative.parent().unwrap_or(Path::new(""));
        let (url_path, is_dynamic, dynamic_params) = calculate_route_path(parent_dir);
        let http_methods = if kind == AppRouterKind::RouteHandler {
            extract_http_methods(content)
        } else {
            Vec::new()
        };
        Some(AppRoute {
            url_path,
            kind,
            file_path: file_path.to_path_buf(),
            is_dynamic,
            dynamic_params,
            http_methods,
        })
    }

    pub fn index_app_file(&mut self, app_root: &Path, file_path: &Path, content: &str) {
        if let Some(route) = Self::route_for(app_root, file_path, content) {
            self.insert(route);
        }
    }

    pub fn insert(&mut self, route: AppRoute) {
        self.routes.insert(route.file_path.clone(), route);
    }

    pub fn remove_file(&mut self, file_path: &Path) {
        self.routes.remove(file_path);
    }

    /// Forgets every file under `root`.
    pub fn remove_dir(&mut self, root: &Path) {
        self.routes.retain(|path, _| !path.starts_with(root));
    }

    pub fn contains_file(&self, file_path: &Path) -> bool {
        self.routes.contains_key(file_path)
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    pub fn route_count_under(&self, root: &Path) -> usize {
        self.routes
            .keys()
            .filter(|path| path.starts_with(root))
            .count()
    }

    /// Every route, by URL and then by file.
    pub fn routes_sorted(&self) -> Vec<AppRoute> {
        let mut routes: Vec<AppRoute> = self.routes.values().cloned().collect();
        routes.sort_by(|a, b| (&a.url_path, &a.file_path).cmp(&(&b.url_path, &b.file_path)));
        routes
    }

    pub fn find_routes_for_path(&self, url_path: &str) -> Vec<&AppRoute> {
        let mut routes: Vec<&AppRoute> = self
            .routes
            .values()
            .filter(|route| route.url_path == url_path)
            .collect();
        routes.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        routes
    }

    pub fn route_completions(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .routes
            .values()
            .filter(|route| route.url_path.starts_with(prefix))
            .map(|route| route.url_path.clone())
            .collect();
        matches.sort();
        matches.dedup();
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

/// The HTTP verbs a route handler exports (`export async function GET`, `export const POST =`),
/// each once, in the order found. Lines over [`MAX_METHOD_LINE_BYTES`] are skipped.
pub fn extract_http_methods(content: &str) -> Vec<String> {
    let mut found: Vec<&str> = Vec::new();
    for line in content.lines() {
        if line.len() > MAX_METHOD_LINE_BYTES {
            continue;
        }
        let trimmed = line.trim();
        if !trimmed.starts_with("export") {
            continue;
        }
        for method in HTTP_METHODS {
            if !found.contains(&method)
                && (trimmed.contains(&format!("function {method}"))
                    || trimmed.contains(&format!("const {method} =")))
            {
                found.push(method);
            }
        }
        if found.len() == HTTP_METHODS.len() {
            break;
        }
    }
    found.into_iter().map(String::from).collect()
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

/// The route path typed so far when `line` (a line up to the cursor) ends inside a string given
/// to an `href` attribute (`href="…"`, `href={'…'}`, `` href={`…`} ``, as on `<Link>` and `<a>`)
/// or to `router.push(…)` / `router.replace(…)`. Only an absolute path counts: `''` or `'/…'`.
pub fn route_path_prefix(line: &str) -> Option<&str> {
    let quote_at = line.rfind(|character: char| matches!(character, '\'' | '"' | '`'))?;
    let prefix = &line[quote_at + 1..];
    if !(prefix.is_empty() || prefix.starts_with('/'))
        || prefix
            .chars()
            .any(|character| character.is_whitespace() || character == '$')
    {
        return None;
    }
    let before = line[..quote_at].trim_end();
    let before = before.strip_suffix('{').map_or(before, str::trim_end);
    if let Some(attribute) = before.strip_suffix('=') {
        let name = attribute
            .trim_end()
            .rsplit(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
            })
            .next()?;
        return (name == "href").then_some(prefix);
    }
    let call = before.strip_suffix('(')?;
    let (receiver, method) = call.rsplit_once('.')?;
    (matches!(method.trim(), "push" | "replace") && receiver.trim_end().ends_with("router"))
        .then_some(prefix)
}

/// A manifest is small; only this much of one is read.
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

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

    #[test]
    fn test_only_app_router_files_below_an_app_dir_are_classified() {
        for (yes, dir, kind) in [
            ("app/page.tsx", "app", AppRouterKind::Page),
            ("app/(shop)/cart/page.jsx", "app", AppRouterKind::Page),
            (
                "src/app/blog/[slug]/layout.js",
                "src/app",
                AppRouterKind::Layout,
            ),
            ("app/loading.ts", "app", AppRouterKind::Loading),
            ("app/error.tsx", "app", AppRouterKind::Error),
            ("app/api/x/route.mjs", "app", AppRouterKind::RouteHandler),
            ("src/app/not-found.tsx", "src/app", AppRouterKind::NotFound),
            ("app/a/template.js", "app", AppRouterKind::Template),
        ] {
            assert_eq!(classify(yes), Some((dir, kind)), "{yes}");
        }
        for no in [
            "page.tsx",
            "pages/index.tsx",
            "app",
            "app/",
            "application/page.tsx",
            "lib/app/page.tsx",
            "src/lib/app/page.tsx",
            "app/page.css",
            "app/page.md",
            "app/page.d.ts",
            "app/page.test.tsx",
            "app/layout.tsx.bak",
            "app/route.php",
            "app/components/Button.tsx",
            "app/node_modules/pkg/page.tsx",
            "app/.next/page.tsx",
            "app/vendor/x/page.tsx",
            "node_modules/next/app/page.tsx",
            ".next/app/page.tsx",
        ] {
            assert_eq!(classify(no), None, "{no}");
        }
    }

    #[test]
    fn test_a_file_that_is_no_app_router_file_gets_no_route() {
        let mut index = NextJsIndex::new();
        let root = Path::new("/p/app");
        index.index_app_file(root, Path::new("/p/app/page.test.tsx"), "");
        index.index_app_file(root, Path::new("/p/app/Button.tsx"), "");
        index.index_app_file(root, Path::new("/elsewhere/page.tsx"), "");
        assert_eq!(index.route_count(), 0);
    }

    #[test]
    fn test_http_methods_skip_long_lines_and_repeat_nothing() {
        let long = format!(
            "export async function GET() {{ /* {} */ }}",
            "x".repeat(MAX_METHOD_LINE_BYTES)
        );
        let content = format!("{long}\nexport async function POST() {{}}\n");
        assert_eq!(extract_http_methods(&content), ["POST"]);

        let twice = "export async function GET() {}\nexport const GET = () => {};\nexport function GET() {}\n";
        assert_eq!(extract_http_methods(twice), ["GET"]);

        let all = HTTP_METHODS
            .iter()
            .chain(HTTP_METHODS.iter())
            .map(|method| format!("export async function {method}() {{}}\n"))
            .collect::<String>();
        assert_eq!(extract_http_methods(&all), HTTP_METHODS);
    }

    #[test]
    fn test_files_with_one_url_are_all_kept_and_leave_one_by_one() {
        let mut index = NextJsIndex::new();
        let files = [
            ("/p/app", "/p/app/about/page.tsx"),
            ("/p/src/app", "/p/src/app/about/page.tsx"),
            ("/p/app", "/p/app/about/layout.tsx"),
        ];
        for (root, file) in files {
            index.index_app_file(Path::new(root), Path::new(file), "");
        }
        assert_eq!(index.find_routes_for_path("/about").len(), 3);
        assert_eq!(index.route_completions("/"), ["/about"]);

        index.remove_file(Path::new("/p/app/about/page.tsx"));
        let left: Vec<_> = index
            .find_routes_for_path("/about")
            .into_iter()
            .map(|route| route.file_path.clone())
            .collect();
        assert_eq!(
            left,
            [
                PathBuf::from("/p/app/about/layout.tsx"),
                PathBuf::from("/p/src/app/about/page.tsx")
            ]
        );
        index.remove_dir(Path::new("/p/src"));
        assert_eq!(index.route_count(), 1);
    }

    #[test]
    fn test_route_path_prefix_inside_an_href_or_router_string() {
        for (line, prefix) in [
            ("  <Link href=\"", ""),
            ("  <Link href=\"/bl", "/bl"),
            ("  <a href='/about/te", "/about/te"),
            ("  <Link href={'/api", "/api"),
            ("  <Link href={ \"/x", "/x"),
            ("  <Link href={`/blog/", "/blog/"),
            ("    router.push('/dash", "/dash"),
            ("    router.replace( \"/", "/"),
            ("    this.router.push(`/a", "/a"),
        ] {
            assert_eq!(route_path_prefix(line), Some(prefix), "{line}");
        }
        for line in [
            "  <Link href=\"/a\" className=\"",
            "  <div className=\"fl",
            "  <a data-href=\"/",
            "  <Link href={`/blog/${slug}/",
            "  <a href=\"https://",
            "  <a href=\"relative",
            "    router.prefetch('/",
            "    history.push('/",
            "    router.push({ pathname: '/",
            "  const x = '/",
            "  <Link href=\"/a b",
            "",
        ] {
            assert_eq!(route_path_prefix(line), None, "{line}");
        }
    }
}
