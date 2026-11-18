//! Inspections (plan 21, milestone 1): PHPCS and PHPStan run on the saved PHP
//! file and their findings appear as ordinary diagnostics, so the gutter, inline
//! diagnostics and the project diagnostics view show them with no language server.

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Entity};
use language::{Buffer, BufferEvent, Diagnostic, DiagnosticEntry, DiagnosticSourceKind};
use lsp::{DiagnosticSeverity, LanguageServerId, NumberOrString};
use project::{Project, WorktreeId, buffer_store::BufferStoreEvent};
use serde::Deserialize;
use settings::{RegisterSetting, Settings, SettingsContent};
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

/// PHPCS counts a tab as up to `tab_width` columns; the editor counts characters.
pub fn char_column(line: &str, column: u32, tab_width: u32) -> u32 {
    let mut expanded = 1;
    let mut chars = 1;
    for character in line.chars() {
        if expanded >= column {
            break;
        }
        expanded += if character == '\t' {
            tab_width - (expanded - 1) % tab_width
        } else {
            1
        };
        chars += 1;
    }
    chars
}

/// The project ruleset's `<arg name="tab-width" value="N"/>`, else PHPCS's default.
pub fn ruleset_tab_width(root: &Path) -> u32 {
    PHPCS_CONFIGS
        .iter()
        .filter_map(|name| std::fs::read_to_string(root.join(name)).ok())
        .find_map(|xml| tab_width_in(&xml))
        .unwrap_or(PHPCS_TAB_WIDTH)
}

fn tab_width_in(xml: &str) -> Option<u32> {
    let after = &xml[xml.find("name=\"tab-width\"")?..];
    let tag = &after[..after.find('>')?];
    let value = &tag[tag.find("value=\"")? + 7..];
    value[..value.find('"')?]
        .parse()
        .ok()
        .filter(|width| *width > 0)
}

fn fix_tab_columns(problems: &mut [Problem], text: &str, tab_width: u32) {
    let lines: Vec<&str> = text.lines().collect();
    for problem in problems {
        if let Some(line) = lines.get(problem.line.saturating_sub(1) as usize) {
            problem.column = char_column(line, problem.column, tab_width);
        }
    }
}

/// PHPCS's default, and the WordPress ruleset's.
const PHPCS_TAB_WIDTH: u32 = 4;

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
        let mut problems = analyzer
            .parse(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| String::from_utf8_lossy(&output.stderr).trim().to_string())?;
        if analyzer == Analyzer::Phpcs
            && let Ok(text) = std::fs::read_to_string(file)
        {
            fix_tab_columns(&mut problems, &text, ruleset_tab_width(root));
        }
        Ok(problems)
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct InspectionSettings {
    pub phpcs: bool,
    pub phpstan: bool,
    pub run_on_save: bool,
    pub max_parallel: usize,
}

impl Settings for InspectionSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let inspections = content
            .zenith
            .as_ref()
            .and_then(|zenith| zenith.inspections.as_ref());
        let flag = |value: Option<bool>| value.unwrap_or(true);
        Self {
            phpcs: flag(inspections.and_then(|settings| settings.phpcs)),
            phpstan: flag(inspections.and_then(|settings| settings.phpstan)),
            run_on_save: flag(inspections.and_then(|settings| settings.run_on_save)),
            max_parallel: inspections
                .and_then(|settings| settings.max_parallel)
                .map_or(2, |count| count.max(1) as usize),
        }
    }
}

impl InspectionSettings {
    pub fn analyzers(&self) -> Vec<Analyzer> {
        [
            (Analyzer::Phpcs, self.phpcs),
            (Analyzer::Phpstan, self.phpstan),
        ]
        .into_iter()
        .filter_map(|(analyzer, enabled)| enabled.then_some(analyzer))
        .collect()
    }
}

gpui::actions!(
    zenith_inspections,
    [
        /// Runs PHPCS and PHPStan on every PHP file git reports as changed.
        InspectChangedFiles
    ]
);

/// PHP files in `git status --porcelain=v1 -z` output that still exist.
pub fn changed_php_files(root: &Path, status: &str) -> Vec<PathBuf> {
    let mut entries = status.split('\0').filter(|entry| !entry.is_empty());
    let mut files = Vec::new();
    while let Some(entry) = entries.next() {
        let (Some(code), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        // A rename lists its old path as a second entry.
        if code.contains('R') || code.contains('C') {
            entries.next();
        }
        if code.contains('D') || !path.ends_with(".php") {
            continue;
        }
        files.push(root.join(path));
    }
    files
}

/// Starts each job only when fewer than `limit` of its batch are running.
async fn run_limited<Job, Fut>(jobs: Vec<Job>, limit: usize)
where
    Job: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut jobs = jobs.into_iter().peekable();
    while jobs.peek().is_some() {
        let batch: Vec<Fut> = jobs.by_ref().take(limit).map(|job| job()).collect();
        futures::future::join_all(batch).await;
    }
}

fn inspect_changed_files(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some(worktree) = project.read(cx).visible_worktrees(cx).next() else {
        return;
    };
    let worktree_id = worktree.read(cx).id();
    let root = worktree.read(cx).abs_path().to_path_buf();
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        let status = cx
            .background_spawn({
                let root = root.clone();
                async move {
                    util::command::new_command("git")
                        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
                        .current_dir(&root)
                        .output()
                        .await
                }
            })
            .await;
        let (message, files) = match status {
            Ok(output) if output.status.success() => {
                let files = changed_php_files(&root, &String::from_utf8_lossy(&output.stdout));
                let message = match files.len() {
                    0 => "No changed PHP files.".to_string(),
                    1 => "Inspecting 1 changed PHP file.".to_string(),
                    count => format!("Inspecting {count} changed PHP files."),
                };
                (message, files)
            }
            Ok(_) => ("This folder is not a git repository.".to_string(), Vec::new()),
            Err(error) => (format!("Could not run git: {error}"), Vec::new()),
        };
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_notification(
                    workspace::notifications::NotificationId::unique::<InspectChangedFiles>(),
                    cx,
                    |cx| cx.new(|cx| workspace::notifications::simple_message_notification::MessageNotification::new(message, cx)),
                );
            })
            .log_err();
        run_limited(
            files
                .into_iter()
                .map(|file| {
                    let (project, mut cx) = (project.clone(), cx.clone());
                    move || async move {
                        if let Ok(task) =
                            cx.update(|_, cx| inspect_path(&project, worktree_id, file, cx))
                        {
                            task.await;
                        }
                    }
                })
                .collect(),
            max_parallel,
        )
        .await;
    })
    .detach();
}

pub fn init(cx: &mut App) {
    InspectionSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|workspace, _: &InspectChangedFiles, window, cx| {
            inspect_changed_files(workspace, window, cx)
        });
        let project = workspace.project().clone();
        if !project.read(cx).is_local() {
            return;
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, move |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                let project = project.clone();
                cx.subscribe(buffer, move |_, buffer, event, cx| {
                    on_buffer_event(&project, &buffer, event, cx)
                })
                .detach();
            }
        })
        .detach();
    })
    .detach();
}

/// Saving a PHP file runs the analyzers, unless `run_on_save` is off.
fn on_buffer_event(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    event: &BufferEvent,
    cx: &mut App,
) {
    if matches!(event, BufferEvent::Saved) && InspectionSettings::get_global(cx).run_on_save {
        inspect(project, buffer, cx);
    }
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
    let root = worktree.read(cx).abs_path().to_path_buf();
    inspect_path(
        project,
        worktree_id,
        root.join(file.path().as_std_path()),
        cx,
    )
    .detach();
}

/// Runs the analyzers on one file under a worktree root, open or not.
pub fn inspect_path(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    abs_path: PathBuf,
    cx: &mut App,
) -> gpui::Task<()> {
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return gpui::Task::ready(());
    };
    if !zenith_php_tools::worktree_trusted(project, worktree_id, cx) {
        return gpui::Task::ready(());
    }
    let root = worktree.read(cx).abs_path().to_path_buf();
    let lsp_store = project.read(cx).lsp_store();
    let analyzers = InspectionSettings::get_global(cx).analyzers();
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
                for analyzer in analyzers {
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
}

#[cfg(test)]
mod tests;
