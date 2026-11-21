mod setup;
pub mod wp_hooks_panel;
pub use setup::{WordPressSettings, init};
pub use wp_hooks_panel::{ToggleWpHooksPanel, WpHooksPanel};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookKind {
    Action,
    Filter,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CallbackTarget {
    Function(String),
    Method {
        class_name: Option<String>,
        method_name: String,
        is_static: bool,
    },
    Closure,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookParameter {
    pub name: String,
    pub type_hint: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDefinition {
    pub name: String,
    pub kind: HookKind,
    pub file_path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub docblock: Option<String>,
    pub since: Option<String>,
    pub parameters: Vec<HookParameter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookListener {
    pub hook_name: String,
    pub kind: HookKind,
    pub callback: CallbackTarget,
    pub priority: i32,
    pub accepted_args: usize,
    pub file_path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookHoverInfo {
    pub name: String,
    pub kind: HookKind,
    pub docblock: Option<String>,
    pub since: Option<String>,
    pub parameters: Vec<HookParameter>,
    pub definition_count: usize,
    pub listener_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCompletion {
    pub name: String,
    pub kind: HookKind,
    pub description: String,
    pub since: Option<String>,
}

pub struct CoreHookSpec {
    pub name: &'static str,
    pub kind: HookKind,
    pub since: &'static str,
    pub description: &'static str,
    pub parameters: &'static [(&'static str, &'static str, &'static str)],
}

pub const CORE_HOOKS: &[CoreHookSpec] = &[
    CoreHookSpec {
        name: "init",
        kind: HookKind::Action,
        since: "1.2.0",
        description: "Fires after WordPress has finished loading but before any headers are sent.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "admin_init",
        kind: HookKind::Action,
        since: "2.5.0",
        description: "Fires as an admin screen or script is being initialized.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "wp_enqueue_scripts",
        kind: HookKind::Action,
        since: "2.8.0",
        description: "Fires when scripts and styles are enqueued for the front-end.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "admin_enqueue_scripts",
        kind: HookKind::Action,
        since: "2.8.0",
        description: "Enqueue scripts and styles for all admin pages.",
        parameters: &[("hook_suffix", "string", "The current admin page.")],
    },
    CoreHookSpec {
        name: "plugins_loaded",
        kind: HookKind::Action,
        since: "1.5.0",
        description: "Fires once activated plugins have loaded.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "admin_menu",
        kind: HookKind::Action,
        since: "1.5.0",
        description: "Fires before the administration menu loads in the admin.",
        parameters: &[("context", "string", "The current admin menu context.")],
    },
    CoreHookSpec {
        name: "save_post",
        kind: HookKind::Action,
        since: "1.5.0",
        description: "Fires once a post has been saved.",
        parameters: &[
            ("post_ID", "int", "Post ID."),
            ("post", "WP_Post", "Post object."),
            (
                "update",
                "bool",
                "Whether this is an existing post being updated.",
            ),
        ],
    },
    CoreHookSpec {
        name: "wp_head",
        kind: HookKind::Action,
        since: "1.2.0",
        description: "Prints scripts or data in the head tag on the front end.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "wp_footer",
        kind: HookKind::Action,
        since: "1.5.1",
        description: "Prints scripts or data before the closing body tag on the front end.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "rest_api_init",
        kind: HookKind::Action,
        since: "4.4.0",
        description: "Fires when preparing to serve a REST API request.",
        parameters: &[("wp_rest_server", "WP_REST_Server", "Server object.")],
    },
    CoreHookSpec {
        name: "template_redirect",
        kind: HookKind::Action,
        since: "1.5.0",
        description: "Fires before determining which template to load.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "after_setup_theme",
        kind: HookKind::Action,
        since: "3.0.0",
        description: "Fires after the theme is loaded.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "widgets_init",
        kind: HookKind::Action,
        since: "2.2.0",
        description: "Fires after all default WordPress widgets have been registered.",
        parameters: &[],
    },
    CoreHookSpec {
        name: "the_content",
        kind: HookKind::Filter,
        since: "0.71",
        description: "Filters the post content.",
        parameters: &[("content", "string", "Content of the current post.")],
    },
    CoreHookSpec {
        name: "the_title",
        kind: HookKind::Filter,
        since: "0.71",
        description: "Filters the post title.",
        parameters: &[
            ("title", "string", "The post title."),
            ("id", "int", "The post ID."),
        ],
    },
    CoreHookSpec {
        name: "body_class",
        kind: HookKind::Filter,
        since: "2.8.0",
        description: "Filters the list of CSS body class names for the current post or page.",
        parameters: &[
            ("classes", "string[]", "An array of body class names."),
            (
                "class",
                "string[]",
                "An array of additional class names added to the body.",
            ),
        ],
    },
    CoreHookSpec {
        name: "plugin_action_links",
        kind: HookKind::Filter,
        since: "2.8.0",
        description: "Filters the action links displayed for each plugin in the Plugins list table.",
        parameters: &[
            ("actions", "string[]", "An array of plugin action links."),
            (
                "plugin_file",
                "string",
                "Path to the plugin file relative to the plugins directory.",
            ),
            ("plugin_data", "array", "An array of plugin data."),
            ("context", "string", "The plugin context."),
        ],
    },
    CoreHookSpec {
        name: "upload_mimes",
        kind: HookKind::Filter,
        since: "2.0.0",
        description: "Filters the list of allowed mime types and file extensions.",
        parameters: &[
            (
                "t",
                "array",
                "Mime types keyed by the file extension regex.",
            ),
            ("user", "int|WP_User", "User ID or object."),
        ],
    },
];

pub struct WordPressIndex {
    definitions_by_name: HashMap<String, Vec<HookDefinition>>,
    listeners_by_name: HashMap<String, Vec<HookListener>>,
    indexed_files: HashSet<PathBuf>,
    /// Definitions plus listeners held, kept in step with the two maps.
    hook_count: usize,
    hook_cap: usize,
}

impl Default for WordPressIndex {
    fn default() -> Self {
        Self::with_hook_cap(MAX_INDEXED_HOOKS)
    }
}

/// One hook as the panel lists it: no docblock, so building the list copies only names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSummary {
    pub name: String,
    pub kind: HookKind,
    pub listeners: usize,
    pub definitions: usize,
}

impl WordPressIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// An index that stops taking files once it holds `hook_cap` hooks.
    pub fn with_hook_cap(hook_cap: usize) -> Self {
        Self {
            definitions_by_name: HashMap::new(),
            listeners_by_name: HashMap::new(),
            indexed_files: HashSet::new(),
            hook_count: 0,
            hook_cap,
        }
    }

    /// Indexes one PHP file, replacing what it had before. Files over
    /// [`MAX_INDEXED_FILE_BYTES`] (generated or minified code) are left out of the index.
    /// Callers that hold a lock should run [`scan_php_source`] first and call
    /// [`Self::insert_scanned`], so the lock is not held for the parse.
    pub fn index_source(&mut self, file_path: PathBuf, content: &str) {
        if content.len() > MAX_INDEXED_FILE_BYTES {
            self.remove_file(&file_path);
            return;
        }
        let (definitions, listeners) = scan_php_source(&file_path, content);
        self.insert_scanned(file_path, definitions, listeners);
    }

    /// Replaces a file's hooks with a finished scan. Returns whether the file is now indexed:
    /// a file that would take the index past [`MAX_INDEXED_HOOKS`] is left out.
    pub fn insert_scanned(
        &mut self,
        file_path: PathBuf,
        definitions: Vec<HookDefinition>,
        listeners: Vec<HookListener>,
    ) -> bool {
        self.remove_file(&file_path);
        // ponytail: a flat ceiling on hooks held (about 500k fit in a few hundred MiB); a smarter
        // eviction (oldest file first) only matters if real trees reach it.
        let added = definitions.len() + listeners.len();
        if self.hook_count + added > self.hook_cap {
            return false;
        }
        self.hook_count += added;
        for def in definitions {
            self.definitions_by_name
                .entry(def.name.clone())
                .or_default()
                .push(def);
        }
        for listener in listeners {
            self.listeners_by_name
                .entry(listener.hook_name.clone())
                .or_default()
                .push(listener);
        }
        self.indexed_files.insert(file_path);
        true
    }

    pub fn contains_file(&self, file_path: &Path) -> bool {
        self.indexed_files.contains(file_path)
    }

    /// How many indexed files lie under `root`.
    pub fn file_count_under(&self, root: &Path) -> usize {
        self.indexed_files
            .iter()
            .filter(|file| file.starts_with(root))
            .count()
    }

    pub fn remove_file(&mut self, file_path: &Path) {
        if self.indexed_files.contains(file_path) {
            self.forget(|path| path == file_path);
        }
    }

    /// Forgets every file under `root` (a worktree that left the project).
    pub fn remove_dir(&mut self, root: &Path) {
        self.forget(|path| path.starts_with(root));
    }

    fn forget(&mut self, gone: impl Fn(&Path) -> bool) {
        self.indexed_files.retain(|file| !gone(file));
        let mut removed = 0;
        for defs in self.definitions_by_name.values_mut() {
            let before = defs.len();
            defs.retain(|d| !gone(&d.file_path));
            removed += before - defs.len();
        }
        for listeners in self.listeners_by_name.values_mut() {
            let before = listeners.len();
            listeners.retain(|l| !gone(&l.file_path));
            removed += before - listeners.len();
        }
        self.hook_count -= removed;
        self.definitions_by_name.retain(|_, v| !v.is_empty());
        self.listeners_by_name.retain(|_, v| !v.is_empty());
    }

    /// Every known hook (core ones included) by name, with how often it is defined and listened
    /// to. Same names and kinds as `completions("")`, without the docblocks.
    pub fn hook_summaries(&self) -> Vec<HookSummary> {
        let mut kinds: BTreeMap<&str, HookKind> = BTreeMap::new();
        for core_hook in CORE_HOOKS {
            kinds.insert(core_hook.name, core_hook.kind);
        }
        for (name, defs) in &self.definitions_by_name {
            if let Some(first) = defs.first() {
                kinds.entry(name).or_insert(first.kind);
            }
        }
        for (name, listeners) in &self.listeners_by_name {
            if let Some(first) = listeners.first() {
                kinds.entry(name).or_insert(first.kind);
            }
        }
        kinds
            .into_iter()
            .map(|(name, kind)| HookSummary {
                name: name.to_string(),
                kind,
                listeners: self.listeners_by_name.get(name).map_or(0, Vec::len),
                definitions: self.definitions_by_name.get(name).map_or(0, Vec::len),
            })
            .collect()
    }

    pub fn find_definitions(&self, hook_name: &str) -> Vec<&HookDefinition> {
        self.definitions_by_name
            .get(hook_name)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub fn find_listeners(&self, hook_name: &str) -> Vec<&HookListener> {
        self.listeners_by_name
            .get(hook_name)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub fn completions(&self, prefix: &str) -> Vec<HookCompletion> {
        let mut results = BTreeMap::new();

        for core_hook in CORE_HOOKS {
            if core_hook.name.starts_with(prefix) {
                results.insert(
                    core_hook.name.to_string(),
                    HookCompletion {
                        name: core_hook.name.to_string(),
                        kind: core_hook.kind,
                        description: core_hook.description.to_string(),
                        since: Some(core_hook.since.to_string()),
                    },
                );
            }
        }

        for (name, defs) in &self.definitions_by_name {
            if name.starts_with(prefix) && !results.contains_key(name) {
                if let Some(first_def) = defs.first() {
                    results.insert(
                        name.clone(),
                        HookCompletion {
                            name: name.clone(),
                            kind: first_def.kind,
                            description: first_def
                                .docblock
                                .as_deref()
                                .unwrap_or("Custom project hook")
                                .to_string(),
                            since: first_def.since.clone(),
                        },
                    );
                }
            }
        }

        for (name, listeners) in &self.listeners_by_name {
            if name.starts_with(prefix) && !results.contains_key(name) {
                if let Some(first_listener) = listeners.first() {
                    results.insert(
                        name.clone(),
                        HookCompletion {
                            name: name.clone(),
                            kind: first_listener.kind,
                            description: "Registered project hook listener".to_string(),
                            since: None,
                        },
                    );
                }
            }
        }

        results.into_values().collect()
    }

    pub fn hover(&self, hook_name: &str) -> Option<HookHoverInfo> {
        let defs = self.find_definitions(hook_name);
        let listeners = self.find_listeners(hook_name);

        if let Some(core_hook) = CORE_HOOKS.iter().find(|h| h.name == hook_name) {
            let parameters = core_hook
                .parameters
                .iter()
                .map(|&(name, hint, desc)| HookParameter {
                    name: name.to_string(),
                    type_hint: Some(hint.to_string()),
                    description: Some(desc.to_string()),
                })
                .collect();

            return Some(HookHoverInfo {
                name: core_hook.name.to_string(),
                kind: core_hook.kind,
                docblock: Some(core_hook.description.to_string()),
                since: Some(core_hook.since.to_string()),
                parameters,
                definition_count: defs.len(),
                listener_count: listeners.len(),
            });
        }

        if let Some(first_def) = defs.first() {
            return Some(HookHoverInfo {
                name: first_def.name.clone(),
                kind: first_def.kind,
                docblock: first_def.docblock.clone(),
                since: first_def.since.clone(),
                parameters: first_def.parameters.clone(),
                definition_count: defs.len(),
                listener_count: listeners.len(),
            });
        }

        if let Some(first_listener) = listeners.first() {
            return Some(HookHoverInfo {
                name: first_listener.hook_name.clone(),
                kind: first_listener.kind,
                docblock: None,
                since: None,
                parameters: Vec::new(),
                definition_count: defs.len(),
                listener_count: listeners.len(),
            });
        }

        None
    }
}

/// Largest PHP file the index reads.
pub const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Most definitions plus listeners the whole index holds; later files are left out.
pub const MAX_INDEXED_HOOKS: usize = 500_000;
/// Most lines one docblock may span; longer comments are cut there.
const MAX_DOCBLOCK_LINES: usize = 500;
/// Most characters kept of one docblock's description.
const MAX_DOCBLOCK_DESCRIPTION: usize = 8 * 1024;
/// Most bytes read from one docblock line (a longer one is cut, not skipped).
const MAX_DOCBLOCK_LINE_BYTES: usize = 1024;
/// Most `@param` entries kept per docblock.
const MAX_DOCBLOCK_PARAMS: usize = 64;
/// Most bytes of one code line searched for hooks; the rest of a longer line (minified code) is
/// not read. ponytail: a hook past this offset on a huge line is missed; split the scan by
/// statement if minified plugins need support.
const MAX_SCANNED_LINE_BYTES: usize = 4 * 1024;
/// Most hook calls taken from one line.
const MAX_HOOKS_PER_LINE: usize = 64;
/// Most definitions plus listeners taken from one file.
const MAX_HOOKS_PER_FILE: usize = 10_000;
/// Most bytes searched for the end of a callback array.
const MAX_CALLBACK_ARRAY_BYTES: usize = 512;

/// `text` cut to at most `max` bytes at a character boundary.
fn prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn scan_php_source(
    file_path: &Path,
    content: &str,
) -> (Vec<HookDefinition>, Vec<HookListener>) {
    let mut definitions = Vec::new();
    let mut listeners = Vec::new();

    let lines: Vec<&str> = content.lines().collect();
    let mut current_docblock: Option<(String, Option<String>, Vec<HookParameter>)> = None;

    // Each line is visited once: a docblock is consumed whole, then scanning goes on after it.
    let mut line_index = 0;
    while let Some(line) = lines.get(line_index) {
        if definitions.len() + listeners.len() >= MAX_HOOKS_PER_FILE {
            break;
        }
        let trimmed = line.trim();

        if trimmed.starts_with("/**") || trimmed.starts_with('*') {
            let (docblock, end) = parse_docblock_stream(&lines, line_index);
            current_docblock = Some(docblock);
            line_index = end + 1;
            continue;
        } else if !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with('#') {
            scan_line_for_hooks(
                file_path,
                prefix(line, MAX_SCANNED_LINE_BYTES),
                line_index + 1,
                &mut current_docblock,
                &mut definitions,
                &mut listeners,
            );
            current_docblock = None;
        }
        line_index += 1;
    }

    (definitions, listeners)
}

fn scan_line_for_hooks(
    file_path: &Path,
    line: &str,
    line_number: usize,
    docblock: &mut Option<(String, Option<String>, Vec<HookParameter>)>,
    definitions: &mut Vec<HookDefinition>,
    listeners: &mut Vec<HookListener>,
) {
    let mut hooks_on_line = 0;
    let patterns = [
        ("do_action", HookKind::Action, true),
        ("do_action_ref_array", HookKind::Action, true),
        ("apply_filters", HookKind::Filter, true),
        ("apply_filters_ref_array", HookKind::Filter, true),
        ("add_action", HookKind::Action, false),
        ("add_filter", HookKind::Filter, false),
    ];

    for &(keyword, kind, is_definition) in &patterns {
        let mut search_start = 0;
        while let Some(relative_pos) = line[search_start..].find(keyword) {
            if hooks_on_line >= MAX_HOOKS_PER_LINE {
                return;
            }
            let func_start = search_start + relative_pos;
            search_start = func_start + keyword.len();

            let before = if func_start > 0 {
                line[..func_start].chars().last()
            } else {
                None
            };
            if let Some(ch) = before {
                if ch.is_alphanumeric() || ch == '_' || ch == '>' || ch == ':' {
                    continue;
                }
            }

            let after_func = &line[func_start + keyword.len()..];
            let trimmed_after = after_func.trim_start();
            if !trimmed_after.starts_with('(') {
                continue;
            }

            let open_paren_idx =
                func_start + keyword.len() + (after_func.len() - trimmed_after.len());
            let inside_call = &line[open_paren_idx + 1..];

            if let Some((hook_name, rest_args)) = extract_first_string_literal(inside_call) {
                let column = open_paren_idx + 1;
                hooks_on_line += 1;
                if is_definition {
                    // The docblock documents the first call after it; handing it over (not
                    // cloning it) keeps a line of many calls from multiplying it.
                    let (desc, since, params) = match docblock.take() {
                        Some((d, s, p)) => (Some(d), s, p),
                        None => (None, None, Vec::new()),
                    };

                    definitions.push(HookDefinition {
                        name: hook_name,
                        kind,
                        file_path: file_path.to_path_buf(),
                        line: line_number,
                        column,
                        docblock: desc,
                        since,
                        parameters: params,
                    });
                } else {
                    let (callback, priority, accepted_args) = parse_listener_args(rest_args);
                    listeners.push(HookListener {
                        hook_name,
                        kind,
                        callback,
                        priority,
                        accepted_args,
                        file_path: file_path.to_path_buf(),
                        line: line_number,
                        column,
                    });
                }
            }
        }
    }
}

fn extract_first_string_literal(input: &str) -> Option<(String, &str)> {
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

fn parse_listener_args(rest: &str) -> (CallbackTarget, i32, usize) {
    let trimmed = rest.trim_start();
    if !trimmed.starts_with(',') {
        return (CallbackTarget::Unknown(String::new()), 10, 1);
    }
    let after_comma = trimmed[1..].trim_start();

    let mut priority = 10;
    let mut accepted_args = 1;

    let callback;
    let callback_end_pos;

    if after_comma.starts_with('\'') || after_comma.starts_with('"') {
        if let Some((name, remaining)) = extract_first_string_literal(after_comma) {
            callback = CallbackTarget::Function(name);
            callback_end_pos = after_comma.len() - remaining.len();
        } else {
            callback = CallbackTarget::Unknown(String::new());
            callback_end_pos = 0;
        }
    } else if after_comma.starts_with('[')
        || after_comma
            .get(..6)
            .is_some_and(|head| head.eq_ignore_ascii_case("array("))
    {
        let is_bracket = after_comma.starts_with('[');
        let closing = if is_bracket { ']' } else { ')' };
        // A callback array is short; do not search the rest of a long line for its end.
        let window = prefix(after_comma, MAX_CALLBACK_ARRAY_BYTES);
        if let Some(close_idx) = window.find(closing) {
            let inside_array = if is_bracket {
                &after_comma[1..close_idx]
            } else {
                &after_comma[6..close_idx]
            };
            callback = parse_array_callback(inside_array);
            callback_end_pos = close_idx + 1;
        } else {
            callback = CallbackTarget::Unknown(String::new());
            callback_end_pos = 0;
        }
    } else if after_comma.starts_with("function") || after_comma.starts_with("fn") {
        callback = CallbackTarget::Closure;
        callback_end_pos = after_comma.find(',').unwrap_or(after_comma.len());
    } else {
        let first_arg_end = after_comma
            .find(',')
            .unwrap_or_else(|| after_comma.find(')').unwrap_or(after_comma.len()));
        let raw = after_comma[..first_arg_end].trim();
        callback = CallbackTarget::Unknown(raw.to_string());
        callback_end_pos = first_arg_end;
    }

    let remainder = after_comma[callback_end_pos..].trim_start();
    if let Some(stripped) = remainder.strip_prefix(',') {
        let after_prio = stripped.trim_start();
        let prio_end = after_prio
            .find(|c: char| c == ',' || c == ')' || c.is_whitespace())
            .unwrap_or(after_prio.len());
        if let Ok(p) = after_prio[..prio_end].parse::<i32>() {
            priority = p;
        }

        let rem_args = after_prio[prio_end..].trim_start();
        if let Some(stripped_args) = rem_args.strip_prefix(',') {
            let after_args = stripped_args.trim_start();
            let args_end = after_args
                .find(|c: char| c == ',' || c == ')' || c.is_whitespace())
                .unwrap_or(after_args.len());
            if let Ok(a) = after_args[..args_end].parse::<usize>() {
                accepted_args = a;
            }
        }
    }

    (callback, priority, accepted_args)
}

fn parse_array_callback(inside: &str) -> CallbackTarget {
    let parts: Vec<&str> = inside.split(',').map(|s| s.trim()).collect();
    if parts.len() < 2 {
        return CallbackTarget::Unknown(inside.to_string());
    }

    let class_part = parts[0];
    let method_part = parts[1];

    let method_name = if let Some((name, _)) = extract_first_string_literal(method_part) {
        name
    } else {
        method_part
            .trim_matches(|c| c == '\'' || c == '"')
            .to_string()
    };

    if class_part == "$this" {
        CallbackTarget::Method {
            class_name: None,
            method_name,
            is_static: false,
        }
    } else {
        let class_name = if class_part.ends_with("::class") {
            class_part.trim_end_matches("::class").trim().to_string()
        } else if let Some((name, _)) = extract_first_string_literal(class_part) {
            name
        } else {
            class_part.to_string()
        };

        CallbackTarget::Method {
            class_name: Some(class_name),
            method_name,
            is_static: true,
        }
    }
}

/// Reads the docblock starting at `start_index`. It runs while lines keep starting with `*` (or
/// open with `/**`), ends at the line with `*/` or after [`MAX_DOCBLOCK_LINES`], and never reads
/// past the first line that is not comment. Returns the docblock and the index of its last line.
fn parse_docblock_stream(
    lines: &[&str],
    start_index: usize,
) -> ((String, Option<String>, Vec<HookParameter>), usize) {
    let mut desc_lines = Vec::new();
    let mut desc_len = 0;
    let mut since = None;
    let mut parameters = Vec::new();

    let mut end = start_index;
    let mut i = start_index;
    while i < lines.len() && i - start_index < MAX_DOCBLOCK_LINES {
        let trimmed = lines[i].trim();
        if i > start_index && !trimmed.starts_with('*') {
            // Not part of the comment: this line is code, leave it to the caller.
            break;
        }
        end = i;
        // The closing `*/` goes first, or a bare ` */` line would leave a stray `/`.
        let cleaned = prefix(trimmed, MAX_DOCBLOCK_LINE_BYTES)
            .trim_start_matches("/**")
            .trim_end_matches("*/")
            .trim_start_matches('*')
            .trim();

        if cleaned.starts_with("@since") {
            since = cleaned.strip_prefix("@since").map(|s| s.trim().to_string());
        } else if cleaned.starts_with("@param") {
            if let Some(param_spec) = cleaned.strip_prefix("@param") {
                let parts: Vec<&str> = param_spec.trim().split_whitespace().collect();
                if parts.len() >= 2 && parameters.len() < MAX_DOCBLOCK_PARAMS {
                    let type_hint = parts[0].to_string();
                    let name = parts[1].trim_start_matches('$').to_string();
                    let description = if parts.len() > 2 {
                        Some(parts[2..].join(" "))
                    } else {
                        None
                    };
                    parameters.push(HookParameter {
                        name,
                        type_hint: Some(type_hint),
                        description,
                    });
                }
            }
        } else if !cleaned.is_empty()
            && !cleaned.starts_with('@')
            && desc_len < MAX_DOCBLOCK_DESCRIPTION
        {
            // Only what still fits: one huge line must not be kept whole.
            let piece = prefix(cleaned, MAX_DOCBLOCK_DESCRIPTION - desc_len);
            desc_len += piece.len();
            desc_lines.push(piece);
        }

        if trimmed.contains("*/") {
            break;
        }
        i += 1;
    }

    ((desc_lines.join(" "), since, parameters), end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_hooks_completion() {
        let index = WordPressIndex::new();
        let completions = index.completions("admin_");
        let names: Vec<&str> = completions.iter().map(|c| c.name.as_str()).collect();

        assert!(names.contains(&"admin_init"));
        assert!(names.contains(&"admin_menu"));
        assert!(names.contains(&"admin_enqueue_scripts"));
    }

    #[test]
    fn test_core_hooks_hover() {
        let index = WordPressIndex::new();
        let hover = index
            .hover("save_post")
            .expect("save_post hover should exist");

        assert_eq!(hover.name, "save_post");
        assert_eq!(hover.kind, HookKind::Action);
        assert_eq!(hover.since.as_deref(), Some("1.5.0"));
        assert_eq!(hover.parameters.len(), 3);
        assert_eq!(hover.parameters[0].name, "post_ID");
        assert_eq!(hover.parameters[1].type_hint.as_deref(), Some("WP_Post"));
    }

    #[test]
    fn test_scan_definitions_with_docblock() {
        let source = r#"<?php
/**
 * Fires before the post is rendered.
 *
 * @since 2.0.0
 * @param int $post_id Post ID number.
 * @param string $view Current template view.
 */
do_action('my_plugin_before_render', $post_id, 'compact');
"#;
        let mut index = WordPressIndex::new();
        index.index_source(PathBuf::from("plugin.php"), source);

        let defs = index.find_definitions("my_plugin_before_render");
        assert_eq!(defs.len(), 1);
        let def = defs[0];
        assert_eq!(def.name, "my_plugin_before_render");
        assert_eq!(def.kind, HookKind::Action);
        assert_eq!(def.since.as_deref(), Some("2.0.0"));
        assert_eq!(def.parameters.len(), 2);
        assert_eq!(def.parameters[0].name, "post_id");
        assert_eq!(def.parameters[0].type_hint.as_deref(), Some("int"));
    }

    #[test]
    fn test_scan_listeners_callbacks() {
        let source = r#"<?php
add_action('init', 'my_custom_init', 20, 2);
add_filter('the_content', [$this, 'filter_content']);
add_action('save_post', [MyHandler::class, 'handle_save'], 5);
add_action('wp_footer', function() { echo 'footer'; });
"#;
        let mut index = WordPressIndex::new();
        index.index_source(PathBuf::from("handlers.php"), source);

        let init_listeners = index.find_listeners("init");
        assert_eq!(init_listeners.len(), 1);
        assert_eq!(
            init_listeners[0].callback,
            CallbackTarget::Function("my_custom_init".to_string())
        );
        assert_eq!(init_listeners[0].priority, 20);
        assert_eq!(init_listeners[0].accepted_args, 2);

        let content_listeners = index.find_listeners("the_content");
        assert_eq!(content_listeners.len(), 1);
        assert_eq!(
            content_listeners[0].callback,
            CallbackTarget::Method {
                class_name: None,
                method_name: "filter_content".to_string(),
                is_static: false,
            }
        );

        let save_listeners = index.find_listeners("save_post");
        assert_eq!(save_listeners.len(), 1);
        assert_eq!(
            save_listeners[0].callback,
            CallbackTarget::Method {
                class_name: Some("MyHandler".to_string()),
                method_name: "handle_save".to_string(),
                is_static: true,
            }
        );
        assert_eq!(save_listeners[0].priority, 5);

        let footer_listeners = index.find_listeners("wp_footer");
        assert_eq!(footer_listeners.len(), 1);
        assert_eq!(footer_listeners[0].callback, CallbackTarget::Closure);
    }

    #[test]
    fn test_hooks_past_the_total_cap_are_not_indexed() {
        let hooks = |names: &[&str]| {
            format!(
                "<?php\n{}",
                names
                    .iter()
                    .map(|n| format!("do_action('{n}');\n"))
                    .collect::<String>()
            )
        };
        let mut index = WordPressIndex::with_hook_cap(3);
        index.index_source(PathBuf::from("a.php"), &hooks(&["a1", "a2"]));
        // Two more would make four: the whole file is left out, not cut.
        index.index_source(PathBuf::from("b.php"), &hooks(&["b1", "b2"]));
        assert!(index.contains_file(Path::new("a.php")));
        assert!(!index.contains_file(Path::new("b.php")));
        assert!(index.find_definitions("b1").is_empty());
        // One more still fits.
        index.index_source(PathBuf::from("c.php"), &hooks(&["c1"]));
        assert!(index.contains_file(Path::new("c.php")));
        // Re-indexing a file counts its old hooks out first, and removal gives room back.
        index.index_source(PathBuf::from("a.php"), &hooks(&["a1", "a2"]));
        assert!(index.contains_file(Path::new("a.php")));
        index.remove_dir(Path::new(""));
        index.index_source(PathBuf::from("b.php"), &hooks(&["b1", "b2", "b3"]));
        assert!(index.contains_file(Path::new("b.php")));
    }

    #[test]
    fn test_summaries_match_completions() {
        let mut index = WordPressIndex::new();
        index.index_source(
            PathBuf::from("p.php"),
            "<?php\n/** Docs. */\ndo_action('mine');\nadd_filter('mine', 'cb');\nadd_action('init', 'boot');\napply_filters('zz', 1);\n",
        );
        let summaries = index.hook_summaries();
        let completions = index.completions("");
        assert_eq!(
            summaries
                .iter()
                .map(|s| (&s.name, s.kind))
                .collect::<Vec<_>>(),
            completions
                .iter()
                .map(|c| (&c.name, c.kind))
                .collect::<Vec<_>>()
        );
        let mine = summaries.iter().find(|s| s.name == "mine").unwrap();
        assert_eq!((mine.definitions, mine.listeners), (1, 1));
        let init = summaries.iter().find(|s| s.name == "init").unwrap();
        assert_eq!((init.definitions, init.listeners), (0, 1));
    }

    #[test]
    fn test_file_removal() {
        let source = "<?php do_action('temp_hook');";
        let path = PathBuf::from("temp.php");
        let mut index = WordPressIndex::new();

        index.index_source(path.clone(), source);
        assert_eq!(index.find_definitions("temp_hook").len(), 1);

        index.remove_file(&path);
        assert_eq!(index.find_definitions("temp_hook").len(), 0);
    }

    #[test]
    fn test_docblock_scan_is_linear_on_unterminated_comment_lines() {
        // Every `*` line used to rescan the rest of the file looking for `*/`.
        let source = format!("<?php\n{}", "*\ndo_action('a');\n".repeat(20_000));
        let started = std::time::Instant::now();
        let mut index = WordPressIndex::new();
        index.index_source(PathBuf::from("big.php"), &source);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            index.find_definitions("a").len(),
            MAX_HOOKS_PER_FILE,
            "scanning reaches the per-file cap, not just the first lines"
        );
    }

    #[test]
    fn test_docblock_stops_at_code_and_keeps_following_calls() {
        let source = "<?php\n/**\n * One.\n * @since 1.0\n*/\ndo_action('one');\n/**\n * Two.\ndo_action('two');\n";
        let (defs, _) = scan_php_source(Path::new("a.php"), source);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].docblock.as_deref(), Some("One."));
        assert_eq!(defs[0].since.as_deref(), Some("1.0"));
        assert_eq!(
            defs[1].docblock.as_deref(),
            Some("Two."),
            "an unterminated docblock ends at the code line"
        );
    }

    #[test]
    fn test_huge_docblock_is_capped_without_losing_the_call() {
        let comment = format!(
            "/**\n{}",
            " * lots of words here\n".repeat(MAX_DOCBLOCK_LINES * 4)
        );
        let source = format!("<?php\n{comment}do_action('late');\n");
        let (defs, _) = scan_php_source(Path::new("a.php"), &source);
        assert_eq!(defs.len(), 1);
        let description = defs[0].docblock.as_deref().unwrap_or("");
        assert!(
            description.len() <= MAX_DOCBLOCK_DESCRIPTION + 64,
            "{}",
            description.len()
        );
    }

    #[test]
    fn test_oversized_files_are_not_indexed() {
        let mut index = WordPressIndex::new();
        let filler = " ".repeat(MAX_INDEXED_FILE_BYTES);
        index.index_source(
            PathBuf::from("min.php"),
            &format!("<?php do_action('big');{filler}"),
        );
        assert!(index.find_definitions("big").is_empty());
        index.index_source(PathBuf::from("ok.php"), "<?php do_action('small');");
        assert_eq!(index.find_definitions("small").len(), 1);
        // A file that grew past the cap drops its stale entries.
        index.index_source(
            PathBuf::from("ok.php"),
            &format!("<?php do_action('small');{filler}"),
        );
        assert!(index.find_definitions("small").is_empty());
    }

    #[test]
    fn test_long_docblock_description_is_capped() {
        let line = format!(" * {}\n", "x".repeat(200));
        let source = format!("<?php\n/**\n{}*/\ndo_action('long');\n", line.repeat(100));
        let (defs, _) = scan_php_source(Path::new("a.php"), &source);
        let description = defs[0].docblock.as_deref().unwrap_or("");
        assert!(!description.is_empty());
        assert!(
            description.len() <= MAX_DOCBLOCK_DESCRIPTION + 256,
            "{}",
            description.len()
        );
    }

    /// Bytes of docblock text an index keeps alive.
    fn retained_docblock_bytes(defs: &[HookDefinition]) -> usize {
        defs.iter()
            .map(|def| {
                def.docblock.as_deref().map_or(0, str::len)
                    + def
                        .parameters
                        .iter()
                        .map(|p| p.name.len() + p.description.as_deref().map_or(0, str::len))
                        .sum::<usize>()
            })
            .sum()
    }

    #[test]
    fn test_huge_docblock_line_is_not_multiplied_across_many_calls() {
        // A 1 MB docblock line, then one line with 80,000 calls: the old code kept the line
        // whole and cloned it into every definition.
        let source = format!(
            "<?php\n/**\n * {}\n * @param int $x {}\n */\n{}\n",
            "w".repeat(1_000_000),
            "d".repeat(1_000_000),
            "do_action('a');".repeat(80_000)
        );
        let (defs, _) = scan_php_source(Path::new("a.php"), &source);
        assert!(defs.len() <= MAX_HOOKS_PER_LINE, "{}", defs.len());
        assert!(
            retained_docblock_bytes(&defs)
                <= MAX_DOCBLOCK_DESCRIPTION + 4 * MAX_DOCBLOCK_LINE_BYTES,
            "{}",
            retained_docblock_bytes(&defs)
        );
        assert!(defs[0].docblock.is_some(), "the first call keeps the docs");
    }

    #[test]
    fn test_definitions_are_capped_per_file() {
        let source = "<?php\n".to_string() + &"do_action('a');\n".repeat(MAX_HOOKS_PER_FILE * 3);
        let (defs, listeners) = scan_php_source(Path::new("a.php"), &source);
        assert_eq!(defs.len() + listeners.len(), MAX_HOOKS_PER_FILE);
    }

    #[test]
    fn test_long_line_of_unfinished_listeners_is_bounded_work() {
        // `[` never closes: every call used to search the rest of the 2 MiB line.
        let unit = "add_action('a', [";
        let line = unit.repeat(2 * 1024 * 1024 / unit.len());
        let started = std::time::Instant::now();
        let (_, listeners) = scan_php_source(Path::new("a.php"), &format!("<?php\n{line}\n"));
        assert!(listeners.len() <= MAX_HOOKS_PER_LINE, "{}", listeners.len());
        // Quadratic work here is minutes; linear is milliseconds.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_array_callback_detection_is_case_insensitive_and_bounded() {
        let (_, listeners) = scan_php_source(
            Path::new("a.php"),
            "<?php add_action('a', ARRAY('Foo', 'bar'));",
        );
        assert_eq!(
            listeners[0].callback,
            CallbackTarget::Method {
                class_name: Some("Foo".into()),
                method_name: "bar".into(),
                is_static: true
            }
        );
    }

    #[test]
    fn test_description_stops_exactly_at_its_budget() {
        let line = format!(" * {}\n", "x".repeat(MAX_DOCBLOCK_LINE_BYTES - 8));
        let source = format!("<?php\n/**\n{}*/\ndo_action('a');\n", line.repeat(40));
        let (defs, _) = scan_php_source(Path::new("a.php"), &source);
        let description = defs[0].docblock.as_deref().unwrap_or("");
        // The budget plus the spaces that join the pieces.
        assert!(
            description.len() <= MAX_DOCBLOCK_DESCRIPTION + 16,
            "{}",
            description.len()
        );
    }
}
