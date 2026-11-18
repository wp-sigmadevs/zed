//! Inspections (plan 21, milestone 1): PHPCS and PHPStan run on the saved PHP
//! file and their findings appear as ordinary diagnostics, so the gutter, inline
//! diagnostics and the project diagnostics view show them with no language server.

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Entity};
use language::{Buffer, BufferEvent, Diagnostic, DiagnosticEntry, DiagnosticSourceKind};
use lsp::{DiagnosticSeverity, LanguageServerId, NumberOrString};
use project::{Project, buffer_store::BufferStoreEvent};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
use text::{PointUtf16, Unclipped};
use util::ResultExt as _;
use workspace::Workspace;
use zenith_php_tools::{ProjectKind, Tool};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One finding. `line` and `column` are 1-based, as the tools print them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    pub severity: Severity,
    pub message: String,
    pub rule: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Analyzer {
    Phpcs,
    Phpstan,
}

impl Analyzer {
    pub fn name(self) -> &'static str {
        match self {
            Analyzer::Phpcs => "phpcs",
            Analyzer::Phpstan => "phpstan",
        }
    }

    fn tool(self) -> Tool {
        match self {
            Analyzer::Phpcs => Tool::Phpcs,
            Analyzer::Phpstan => Tool::Phpstan,
        }
    }

    /// Ids no language server gets (they count up from 0), so each analyzer's
    /// diagnostics replace only its own.
    pub fn server_id(self) -> LanguageServerId {
        match self {
            Analyzer::Phpcs => LanguageServerId(usize::MAX - 1),
            Analyzer::Phpstan => LanguageServerId(usize::MAX - 2),
        }
    }

    /// shortcut: group ids share one space per buffer with the language servers',
    /// which count up from 0; starting far above keeps them apart. Upgrade if upstream
    /// exposes its counter.
    fn first_group_id(self) -> usize {
        match self {
            Analyzer::Phpcs => 1 << 60,
            Analyzer::Phpstan => (1 << 60) + (1 << 40),
        }
    }

    pub fn parse(self, output: &str) -> Result<Vec<Problem>> {
        match self {
            Analyzer::Phpcs => parse_phpcs(output),
            Analyzer::Phpstan => parse_phpstan(output),
        }
    }

    /// Arguments after the tool, or `None` when it should not run here.
    pub fn args(self, root: &Path, kind: ProjectKind, file: &Path) -> Option<Vec<OsString>> {
        let mut args: Vec<OsString> = match self {
            Analyzer::Phpcs => {
                let mut args = vec!["-q".into(), "--report=json".into(), "--no-colors".into()];
                if !has_any(root, PHPCS_CONFIGS) {
                    let standard = match kind {
                        ProjectKind::WordPress => "WordPress",
                        ProjectKind::Laravel | ProjectKind::Php => "PSR12",
                    };
                    args.push(format!("--standard={standard}").into());
                }
                args
            }
            // Without the project's config PHPStan reports every WordPress or
            // framework function as unknown, so it only runs where one exists.
            Analyzer::Phpstan => {
                if !has_any(root, PHPSTAN_CONFIGS) {
                    return None;
                }
                vec![
                    "analyse".into(),
                    "--error-format=json".into(),
                    "--no-progress".into(),
                    "--no-interaction".into(),
                ]
            }
        };
        args.push(file.into());
        Some(args)
    }
}

const PHPCS_CONFIGS: &[&str] = &[
    ".phpcs.xml",
    "phpcs.xml",
    ".phpcs.xml.dist",
    "phpcs.xml.dist",
];
const PHPSTAN_CONFIGS: &[&str] = &["phpstan.neon", "phpstan.neon.dist", "phpstan.dist.neon"];

fn has_any(root: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| root.join(name).is_file())
}

/// PHP prints deprecation notices and PHPStan its config note to stdout ahead of
/// the report; the report is the first line that opens a JSON object.
fn json_part(output: &str) -> &str {
    if output.starts_with('{') {
        return output;
    }
    output
        .find("\n{")
        .map_or(output, |start| &output[start + 1..])
}

#[derive(Deserialize)]
struct PhpcsReport {
    files: BTreeMap<PathBuf, PhpcsFile>,
}

#[derive(Deserialize)]
struct PhpcsFile {
    messages: Vec<PhpcsMessage>,
}

#[derive(Deserialize)]
struct PhpcsMessage {
    message: String,
    source: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    line: u32,
    column: u32,
}

/// `phpcs --report=json`.
pub fn parse_phpcs(output: &str) -> Result<Vec<Problem>> {
    let report: PhpcsReport = serde_json::from_str(json_part(output)).context("phpcs output")?;
    Ok(report
        .files
        .into_iter()
        .flat_map(|(path, file)| {
            file.messages.into_iter().map(move |message| Problem {
                path: path.clone(),
                line: message.line,
                column: message.column,
                severity: if message.kind == "ERROR" {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                message: message.message,
                rule: message.source,
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct PhpstanReport {
    files: BTreeMap<String, PhpstanFile>,
}

#[derive(Deserialize)]
struct PhpstanFile {
    messages: Vec<PhpstanMessage>,
}

#[derive(Deserialize)]
struct PhpstanMessage {
    message: String,
    line: Option<u32>,
    identifier: Option<String>,
    tip: Option<String>,
}

/// `phpstan analyse --error-format=json`. An empty `files` is a JSON array.
pub fn parse_phpstan(output: &str) -> Result<Vec<Problem>> {
    let json: serde_json::Value =
        serde_json::from_str(json_part(output)).context("phpstan output")?;
    if json.get("files").is_some_and(|files| files.is_array()) {
        return Ok(Vec::new());
    }
    let report: PhpstanReport = serde_json::from_value(json).context("phpstan output")?;
    Ok(report
        .files
        .into_iter()
        .flat_map(|(path, file)| {
            // Trait errors arrive as "/x/T.php (in context of class C)".
            let path = PathBuf::from(path.split(" (in context of").next().unwrap_or(&path));
            file.messages.into_iter().map(move |message| Problem {
                path: path.clone(),
                line: message.line.unwrap_or(1),
                column: 1,
                severity: Severity::Error,
                message: match message.tip {
                    Some(tip) => format!("{}\n{tip}", message.message),
                    None => message.message,
                },
                rule: message.identifier,
            })
        })
        .collect())
}

/// Problems as diagnostics. Disk-based, so they follow edits made after the save.
pub fn diagnostic_entries(
    analyzer: Analyzer,
    problems: &[Problem],
) -> Vec<DiagnosticEntry<Unclipped<PointUtf16>>> {
    problems
        .iter()
        .enumerate()
        .map(|(ix, problem)| {
            let point = Unclipped(PointUtf16::new(
                problem.line.saturating_sub(1),
                problem.column.saturating_sub(1),
            ));
            DiagnosticEntry::new(
                point..point,
                Diagnostic {
                    source: Some(analyzer.name().to_string()),
                    code: problem.rule.clone().map(NumberOrString::String),
                    severity: match problem.severity {
                        Severity::Error => DiagnosticSeverity::ERROR,
                        Severity::Warning => DiagnosticSeverity::WARNING,
                    },
                    message: problem.message.clone().into(),
                    group_id: analyzer.first_group_id() + ix,
                    is_primary: true,
                    is_disk_based: true,
                    source_kind: DiagnosticSourceKind::Other,
                    underline: true,
                    ..Diagnostic::default()
                },
            )
        })
        .collect()
}

/// `vendor/bin` tools are PHP scripts: run them with the PHP Zenith found, since
/// an app started from the Dock may not have it on `PATH`.
pub fn launch(php: &Path, tool: &Path) -> (PathBuf, Vec<OsString>) {
    let first_line = std::fs::read(tool)
        .ok()
        .and_then(|bytes| {
            let end = bytes
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(bytes.len());
            String::from_utf8(bytes[..end.min(256)].to_vec()).ok()
        })
        .unwrap_or_default();
    if (first_line.starts_with("#!") && first_line.contains("php"))
        || first_line.starts_with("<?php")
    {
        (php.to_path_buf(), vec![tool.into()])
    } else {
        (tool.to_path_buf(), Vec::new())
    }
}

/// Runs the analyzer on `file`; `None` when it has no tool or should not run.
pub async fn run(
    analyzer: Analyzer,
    root: &Path,
    file: &Path,
    php: &Path,
    managed_bin: Option<&Path>,
    path_dirs: &[PathBuf],
) -> Option<Result<Vec<Problem>>> {
    let kind = zenith_php_tools::detect_project(root);
    let args = analyzer.args(root, kind, file)?;
    let tool = zenith_php_tools::resolve(root, analyzer.tool(), managed_bin, path_dirs)?;
    let (program, mut full_args) = launch(php, &tool.path);
    full_args.extend(args);
    let result = util::command::new_command(&program)
        .args(full_args)
        .current_dir(root)
        .output()
        .await
        .with_context(|| format!("running {}", analyzer.name()));
    Some(result.and_then(|output| {
        // Both tools exit non-zero when they find something; the JSON says what.
        analyzer
            .parse(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| String::from_utf8_lossy(&output.stderr).trim().to_string())
    }))
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let project = workspace.project().clone();
        if !project.read(cx).is_local() {
            return;
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, move |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                let project = project.clone();
                cx.subscribe(buffer, move |_, buffer, event, cx| {
                    if matches!(event, BufferEvent::Saved) {
                        inspect(&project, &buffer, cx);
                    }
                })
                .detach();
            }
        })
        .detach();
    })
    .detach();
}

/// Save counter per file, so a slow older run cannot overwrite a newer one.
#[derive(Default)]
struct Generations(std::collections::HashMap<PathBuf, u64>);

impl gpui::Global for Generations {}

fn next_generation(path: &Path, cx: &mut App) -> u64 {
    let count = cx
        .default_global::<Generations>()
        .0
        .entry(path.to_path_buf())
        .or_default();
    *count += 1;
    *count
}

fn current_generation(path: &Path, cx: &App) -> u64 {
    cx.try_global::<Generations>()
        .and_then(|generations| generations.0.get(path).copied())
        .unwrap_or(0)
}

/// Runs the analyzers on a saved PHP file in a trusted local worktree.
pub fn inspect(project: &Entity<Project>, buffer: &Entity<Buffer>, cx: &mut App) {
    let Some(file) = buffer.read(cx).file().cloned() else {
        return;
    };
    if file.path().extension() != Some("php") {
        return;
    }
    let worktree_id = file.worktree_id(cx);
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return;
    };
    if !zenith_php_tools::worktree_trusted(project, worktree_id, cx) {
        return;
    }
    let root = worktree.read(cx).abs_path().to_path_buf();
    let abs_path = root.join(file.path().as_std_path());
    let lsp_store = project.read(cx).lsp_store();
    let generation = next_generation(&abs_path, cx);
    cx.spawn(async move |cx| {
        let results = cx
            .background_spawn(async move {
                let (candidates, path_dirs) = zenith_php_tools::environment();
                let (php, _) = zenith_php_tools::find_php(&candidates).await?;
                let managed = std::env::var_os("HOME").map(|home| {
                    let kind = zenith_php_tools::detect_project(&root);
                    zenith_php_tools::managed_bin(
                        Path::new(&home),
                        zenith_php_tools::Profile::for_project(kind),
                    )
                });
                let mut results = Vec::new();
                for analyzer in [Analyzer::Phpcs, Analyzer::Phpstan] {
                    if let Some(result) = run(
                        analyzer,
                        &root,
                        &abs_path,
                        &php,
                        managed.as_deref(),
                        &path_dirs,
                    )
                    .await
                    {
                        results.push((analyzer, result));
                    }
                }
                Some((abs_path, results))
            })
            .await;
        let Some((abs_path, results)) = results else {
            return;
        };
        lsp_store.update(cx, |lsp_store, cx| {
            // A newer save of this file is running; its results win.
            if current_generation(&abs_path, cx) != generation {
                return;
            }
            for (analyzer, result) in results {
                let problems = match result {
                    Ok(problems) => problems,
                    Err(error) => {
                        log::warn!("{}: {error:#}", analyzer.name());
                        continue;
                    }
                };
                let mine: Vec<Problem> = problems
                    .into_iter()
                    .filter(|problem| problem.path == abs_path)
                    .collect();
                lsp_store
                    .update_diagnostic_entries(
                        analyzer.server_id(),
                        abs_path.clone(),
                        None,
                        None,
                        diagnostic_entries(analyzer, &mine),
                        cx,
                    )
                    .log_err();
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests;
