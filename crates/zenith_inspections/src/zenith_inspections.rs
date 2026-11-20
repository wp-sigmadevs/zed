//! Inspections (plan 21, milestone 1): PHPCS and PHPStan run on the saved PHP
//! file and their findings appear as ordinary diagnostics, so the gutter, inline
//! diagnostics and the project diagnostics view show them with no language server.

use anyhow::{Context as _, Result};
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Entity};
use language::{Buffer, BufferEvent, DiagnosticSourceKind};
use lsp::{DiagnosticSeverity, LanguageServerId, NumberOrString};
use project::{Project, ProjectPath, WorktreeId, buffer_store::BufferStoreEvent};
use serde::Deserialize;
use settings::{RegisterSetting, Settings, SettingsContent};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
use util::ResultExt as _;
use workspace::Workspace;
use zenith_php_tools::{ProjectKind, Tool};

mod new_since_branch;
pub mod since_branch;
mod suppress;
pub use suppress::{SuppressProvider, suppression_edit};

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
                args.push(phpcs_standard(root, kind));
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

/// Always given, so PHPCS never goes looking for a ruleset itself (it climbs
/// from the working folder into its parents, outside the trusted root): the
/// root's own ruleset, else the standard for the project's kind. The ruleset goes
/// by file name, relative to the working folder (the root): PHPCS splits
/// `--standard` at commas, so a root named `acme,inc` would break an absolute path.
fn phpcs_standard(root: &Path, kind: ProjectKind) -> OsString {
    let mut arg = OsString::from("--standard=");
    match find_phpcs_ruleset(root) {
        Some(ruleset) => arg.push(ruleset.file_name().unwrap_or(ruleset.as_os_str())),
        None => arg.push(match kind {
            ProjectKind::WordPress => "WordPress",
            ProjectKind::Laravel | ProjectKind::Php => "PSR12",
        }),
    }
    arg
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixOutcome {
    /// Nothing PHPCBF could fix.
    Nothing,
    Fixed,
    /// Fixed what it could; some findings need a hand.
    FixedSome,
}

/// PHPCBF exits 0 with nothing to fix, 1 when it fixed all, 2 when some remain.
pub fn fix_outcome(code: Option<i32>) -> Result<FixOutcome> {
    match code {
        Some(0) => Ok(FixOutcome::Nothing),
        Some(1) => Ok(FixOutcome::Fixed),
        Some(2) => Ok(FixOutcome::FixedSome),
        other => anyhow::bail!("phpcbf failed (exit code {other:?})"),
    }
}

/// Fixes `file` in place with PHPCBF; `None` when there is no phpcbf to run.
pub async fn run_fix(
    root: &Path,
    file: &Path,
    php: &Path,
    managed_bin: Option<&Path>,
    path_dirs: &[PathBuf],
) -> Option<Result<FixOutcome>> {
    let kind = zenith_php_tools::detect_project(root);
    let tool = zenith_php_tools::resolve(root, Tool::Phpcbf, managed_bin, path_dirs)?;
    let (program, mut args) = launch(php, &tool.path);
    args.extend(["-q".into(), "--no-colors".into()]);
    args.push(phpcs_standard(root, kind));
    args.push(file.into());
    let output = util::command::new_command(&program)
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await
        .context("running phpcbf");
    Some(output.and_then(|output| fix_outcome(output.status.code())))
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

/// The ruleset in the worktree root, in PHPCS's own order. Parents are not
/// searched: a ruleset can load PHP code, and a parent is not the trusted root.
fn find_phpcs_ruleset(root: &Path) -> Option<PathBuf> {
    PHPCS_CONFIGS
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
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
    // Top-level errors are config or bootstrap failures, or notes such as an
    // unmatched ignore pattern. With findings beside them, keep the findings.
    let top_level: Vec<String> = json
        .get("errors")
        .and_then(|errors| errors.as_array())
        .into_iter()
        .flatten()
        .map(|error| match error.as_str() {
            Some(text) => text.to_string(),
            None => error.to_string(),
        })
        .collect();
    let problems = if json.get("files").is_some_and(|files| files.is_array()) {
        Vec::new()
    } else {
        phpstan_files(serde_json::from_value(json).context("phpstan output")?)
    };
    if problems.is_empty() && !top_level.is_empty() {
        anyhow::bail!("{}", top_level.join("; "));
    }
    for error in &top_level {
        log::warn!("phpstan: {error}");
    }
    Ok(problems)
}

/// PHPStan's identifier for "Ignored error pattern ... was not matched".
const IGNORE_UNMATCHED: &str = "ignore.unmatched";

fn phpstan_files(report: PhpstanReport) -> Vec<Problem> {
    report
        .files
        .into_iter()
        .flat_map(|(path, file)| {
            // Trait errors arrive as "/x/T.php (in context of class C)".
            let path = PathBuf::from(path.split(" (in context of").next().unwrap_or(&path));
            // A stale baseline entry is housekeeping, not a problem in the code
            // (whole-project runs report it as a line-less file message).
            let messages = file
                .messages
                .into_iter()
                .filter(|message| message.identifier.as_deref() != Some(IGNORE_UNMATCHED));
            messages.map(move |message| Problem {
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
        .collect()
}

/// PHPCS counts a tab as up to `tab_width` columns; the editor counts characters.
/// Returns the editor's UTF-16 column, so characters outside the BMP count twice.
pub fn char_column(line: &str, column: u32, tab_width: u32) -> u32 {
    let mut expanded = 1;
    let mut utf16 = 1;
    for character in line.chars() {
        if expanded >= column {
            break;
        }
        expanded += if character == '\t' {
            tab_width - (expanded - 1) % tab_width
        } else {
            1
        };
        utf16 += character.len_utf16() as u32;
    }
    utf16
}

/// A ruleset is small; a bigger one (or a device) is not read.
const RULESET_MAX: u64 = 1024 * 1024;
/// The most of a PHP file read to map tab columns; a bigger one keeps PHPCS's columns.
const PHP_FILE_MAX: u64 = 16 * 1024 * 1024;

/// The project ruleset's `<arg name="tab-width" value="N"/>`, else PHPCS's default.
pub fn ruleset_tab_width(root: &Path) -> u32 {
    find_phpcs_ruleset(root)
        .and_then(|ruleset| zenith_php_tools::read_regular(&ruleset, RULESET_MAX))
        .and_then(|xml| tab_width_in(&String::from_utf8_lossy(&xml)))
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

/// Problems as an LSP publish for `path`. `update_diagnostic_entries` would be
/// simpler but only exists in test builds; this is the path real servers take,
/// and it numbers the diagnostic groups from Zed's own counter.
pub fn publish_params(
    analyzer: Analyzer,
    path: &Path,
    problems: &[Problem],
) -> Option<lsp::PublishDiagnosticsParams> {
    let uri = lsp::Uri::from_file_path(path).ok()?;
    let diagnostics = problems
        .iter()
        .map(|problem| {
            let position = lsp::Position::new(
                problem.line.saturating_sub(1),
                problem.column.saturating_sub(1),
            );
            lsp::Diagnostic {
                range: lsp::Range::new(position, position),
                severity: Some(match problem.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                code: problem.rule.clone().map(NumberOrString::String),
                source: Some(analyzer.name().to_string()),
                message: lsp::DiagnosticMessage::String(problem.message.clone()),
                ..lsp::Diagnostic::default()
            }
        })
        .collect();
    Some(lsp::PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    })
}

/// Replaces `analyzer`'s diagnostics for `path`. Disk-based, so they follow
/// edits made after the save.
pub fn push_diagnostics(
    lsp_store: &mut project::LspStore,
    analyzer: Analyzer,
    path: &Path,
    problems: &[Problem],
    cx: &mut gpui::Context<project::LspStore>,
) -> Result<()> {
    let params = publish_params(analyzer, path, problems).context("diagnostic path")?;
    let sources = [analyzer.name().to_string()];
    lsp_store.merge_lsp_diagnostics(
        DiagnosticSourceKind::Other,
        vec![project::lsp_store::DocumentDiagnosticsUpdate {
            diagnostics: params,
            result_id: None,
            registration_id: None,
            server_id: analyzer.server_id(),
            disk_based_sources: std::borrow::Cow::Borrowed(&sources),
        }],
        |_, _, _| false,
        cx,
    )
}

/// `vendor/bin` tools are PHP scripts: run them with the PHP Zenith found, since
/// an app started from the Dock may not have it on `PATH`.
pub fn launch(php: &Path, tool: &Path) -> (PathBuf, Vec<OsString>) {
    // A .phar can be tens of megabytes; the first line is all that matters.
    let mut head = Vec::new();
    std::fs::File::open(tool)
        .and_then(|file| std::io::Read::read_to_end(&mut std::io::Read::take(file, 256), &mut head))
        .log_err();
    let end = head
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(head.len());
    let first_line = String::from_utf8_lossy(&head[..end]);
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
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {}", analyzer.name()));
    Some(result.and_then(|output| {
        // Both tools exit non-zero when they find something; the JSON says what.
        let mut problems = analyzer
            .parse(&String::from_utf8_lossy(&output.stdout))
            .with_context(|| String::from_utf8_lossy(&output.stderr).trim().to_string())?;
        if analyzer == Analyzer::Phpcs
            && let Some(text) = zenith_php_tools::read_regular(file, PHP_FILE_MAX + 1)
                .filter(|bytes| bytes.len() as u64 <= PHP_FILE_MAX)
                .and_then(|bytes| String::from_utf8(bytes).ok())
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
        InspectChangedFiles,
        /// Fixes the formatting problems in the current PHP file with PHPCBF.
        FixFileWithPhpcbf,
        /// Runs PHPCS and PHPStan on the current PHP file now (PhpStorm's
        /// Inspect Code on a file), whatever `run_on_save` says.
        InspectCurrentFile,
        /// Runs PHPCS and PHPStan on every PHP file in the project's trusted folders.
        InspectProject,
        /// Switches this project between all PHPCS/PHPStan findings and only those
        /// on lines changed since the default branch.
        ToggleNewSinceBranch
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

/// Runs at most `limit` jobs at once, starting the next as each one finishes.
async fn run_limited<Job, Fut, Output>(jobs: Vec<Job>, limit: usize) -> Vec<Output>
where
    Job: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Output>,
{
    use futures::StreamExt as _;
    futures::stream::iter(jobs.into_iter().map(|job| job()))
        .buffer_unordered(limit.max(1))
        .collect()
        .await
}

/// Git's toplevel and porcelain status for a folder, or a message why not.
async fn git_changes(root: &Path) -> Result<(PathBuf, String), String> {
    let git = |args: &'static [&'static str]| {
        let root = root.to_path_buf();
        async move {
            util::command::new_command("git")
                .args(since_branch::GIT_HARDENING)
                .args(args)
                .current_dir(&root)
                .output()
                .await
        }
    };
    let toplevel = match git(&["rev-parse", "--show-toplevel"]).await {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
        Ok(_) => return Err("This folder is not a git repository.".into()),
        Err(error) => return Err(format!("Could not run git: {error}")),
    };
    match git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await {
        Ok(output) if output.status.success() => Ok((
            toplevel,
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(error) => Err(format!("Could not run git: {error}")),
    }
}

/// Changed PHP files inside `root`; git reports them from the repository's top.
pub fn changed_php_files_in(root: &Path, toplevel: &Path, status: &str) -> Vec<PathBuf> {
    // Git prints the resolved toplevel; compare in the same form.
    let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let toplevel = std::fs::canonicalize(toplevel).unwrap_or_else(|_| toplevel.to_path_buf());
    changed_php_files(&toplevel, status)
        .into_iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(&real_root).ok()?;
            Some(root.join(relative))
        })
        .collect()
}

/// Where the tools run: the worktree root, or its folder when a file was opened
/// on its own (the worktree root is then the file itself).
pub fn working_folder(root: &Path, single_file: bool) -> PathBuf {
    match root.parent() {
        Some(parent) if single_file => parent.to_path_buf(),
        _ => root.to_path_buf(),
    }
}

fn worktree_folder(worktree: &Entity<project::Worktree>, cx: &App) -> PathBuf {
    let worktree = worktree.read(cx);
    working_folder(&worktree.abs_path(), worktree.is_single_file())
}

fn inspect_changed_files(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    let worktrees: Vec<(WorktreeId, PathBuf)> = project
        .read(cx)
        .visible_worktrees(cx)
        .filter(|worktree| !worktree.read(cx).is_single_file())
        .map(|worktree| {
            (
                worktree.read(cx).id(),
                worktree.read(cx).abs_path().to_path_buf(),
            )
        })
        .collect();
    let trusted: Vec<(WorktreeId, PathBuf)> = worktrees
        .iter()
        .filter(|(id, _)| zenith_php_tools::worktree_trusted(&project, *id, cx))
        .cloned()
        .collect();
    // Trusted folders only; `base_branch` checks trust again before any git.
    let trusted: Vec<_> = trusted
        .into_iter()
        .map(|(id, root)| {
            let base = since_base_for(&project, id, cx);
            (id, root, base)
        })
        .collect();
    if trusted.is_empty() {
        let message = if worktrees.is_empty() {
            "Open a folder first."
        } else {
            "Trust this project to run its PHP tools."
        };
        return notify(workspace, message.into(), cx);
    }
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        let mut files = Vec::new();
        let mut problems = Vec::new();
        let mut branch_note = None;
        let mut bases = std::collections::HashMap::new();
        for (worktree_id, root, base) in trusted {
            if let Some(base) = base {
                bases.insert(worktree_id, base.clone());
                let branch_files = match base.await {
                    Ok(base) => {
                        cx.background_spawn({
                            let root = root.clone();
                            async move { since_branch::branch_php_files(&root, &base).await }
                        })
                        .await
                    }
                    Err(message) => Err(message),
                };
                match branch_files {
                    Ok(branch_files) => {
                        files.extend(branch_files.into_iter().map(|file| (worktree_id, file)))
                    }
                    Err(message) => {
                        branch_note = Some(new_since_branch::listing_fallback_message(&message))
                    }
                }
            }
            let changes = cx
                .background_spawn({
                    let root = root.clone();
                    async move { git_changes(&root).await }
                })
                .await;
            match changes {
                Ok((toplevel, status)) => files.extend(
                    changed_php_files_in(&root, &toplevel, &status)
                        .into_iter()
                        .map(|file| (worktree_id, file)),
                ),
                Err(message) => problems.push(message),
            }
        }
        let mut seen = std::collections::HashSet::new();
        files.retain(|file| seen.insert(file.clone()));
        let message = changed_files_message(files.len(), problems.first(), branch_note);
        inspect_files(project, files, message, max_parallel, bases, workspace, cx).await;
    })
    .detach();
}

/// The notice that starts "inspect changed files". A failed branch listing is
/// added only when there is nothing to inspect: otherwise each run reports it.
fn changed_files_message(
    count: usize,
    problem: Option<&String>,
    branch_note: Option<String>,
) -> String {
    let message = match (count, problem) {
        (0, Some(problem)) => problem.clone(),
        (0, None) => "No changed PHP files.".to_string(),
        (1, _) => "Inspecting 1 changed PHP file.".to_string(),
        (count, _) => format!("Inspecting {count} changed PHP files."),
    };
    match (count, branch_note) {
        (0, Some(note)) => format!("{message} {note}"),
        _ => message,
    }
}

/// Announces the run, inspects `files` at most `max_parallel` at a time, then shows
/// any tool failures.
async fn inspect_files(
    project: Entity<Project>,
    files: Vec<(WorktreeId, PathBuf)>,
    message: String,
    max_parallel: usize,
    mut bases: std::collections::HashMap<WorktreeId, new_since_branch::SharedBase>,
    workspace: gpui::WeakEntity<Workspace>,
    cx: &mut gpui::AsyncWindowContext,
) {
    workspace
        .update(cx, |workspace, cx| notify(workspace, message, cx))
        .log_err();
    // One base lookup per folder for the whole batch, not one per file.
    let folders: std::collections::HashSet<WorktreeId> = files.iter().map(|(id, _)| *id).collect();
    for worktree_id in folders {
        if bases.contains_key(&worktree_id) {
            continue;
        }
        if let Ok(Some(base)) = cx.update(|_, cx| since_base_for(&project, worktree_id, cx)) {
            bases.insert(worktree_id, base);
        }
    }
    let mut report = Report::default();
    for each in run_limited(
        files
            .into_iter()
            .map(|(worktree_id, file)| {
                let (project, mut cx) = (project.clone(), cx.clone());
                let base = bases.get(&worktree_id).cloned();
                move || async move {
                    match cx
                        .update(|_, cx| inspect_path_with(&project, worktree_id, file, base, cx))
                    {
                        Ok(task) => task.await,
                        Err(_) => Report::default(),
                    }
                }
            })
            .collect(),
        max_parallel,
    )
    .await
    {
        report.errors.extend(each.errors);
        report.notes.extend(each.notes);
    }
    workspace
        .update(cx, |workspace, cx| report_run(workspace, report, cx))
        .log_err();
}

/// PHP files in a folder of the project, skipping git-ignored ones (`vendor`).
pub fn folder_php_files(
    project: &Entity<Project>,
    folder: &ProjectPath,
    cx: &App,
) -> Vec<(WorktreeId, PathBuf)> {
    let Some(worktree) = project.read(cx).worktree_for_id(folder.worktree_id, cx) else {
        return Vec::new();
    };
    let worktree = worktree.read(cx);
    worktree
        .traverse_from_path(true, false, false, &folder.path)
        .take_while(|entry| entry.path.starts_with(&folder.path))
        .filter(|entry| entry.path.extension() == Some("php"))
        .map(|entry| (folder.worktree_id, worktree.absolutize(&entry.path)))
        .collect()
}

fn inspect_folder(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some(folder) = workspace
        .panel::<project_panel::ProjectPanel>(cx)
        .and_then(|panel| panel.read(cx).selected_entry_project_path(cx))
    else {
        return;
    };
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    if !zenith_php_tools::worktree_trusted(&project, folder.worktree_id, cx) {
        return notify(
            workspace,
            "Trust this project to run its PHP tools.".into(),
            cx,
        );
    }
    let files = folder_php_files(&project, &folder, cx);
    let message = match files.len() {
        0 => "No PHP files in this folder.".to_string(),
        1 => "Inspecting 1 PHP file.".to_string(),
        count => format!("Inspecting {count} PHP files."),
    };
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        inspect_files(
            project,
            files,
            message,
            max_parallel,
            Default::default(),
            workspace,
            cx,
        )
        .await;
    })
    .detach();
}

fn inspect_current_file(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    let Some(item) = workspace.active_item(cx) else {
        return;
    };
    let Some(project_path) = item.project_path(cx) else {
        return;
    };
    if project_path.path.extension() != Some("php") {
        return notify(workspace, "PHPCS and PHPStan inspect PHP files.".into(), cx);
    }
    if item.is_dirty(cx) {
        return notify(
            workspace,
            "Save the file first; the tools read the saved file.".into(),
            cx,
        );
    }
    if let Some(message) = untrusted_message(&project, project_path.worktree_id, cx) {
        return notify(workspace, message.into(), cx);
    }
    let Some(abs_path) = project.read(cx).absolute_path(&project_path, cx) else {
        return;
    };
    let name = abs_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = vec![(project_path.worktree_id, abs_path)];
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        inspect_files(
            project,
            files,
            format!("Inspecting {name}."),
            max_parallel,
            Default::default(),
            workspace,
            cx,
        )
        .await;
    })
    .detach();
}

/// Every PHP file in the project's trusted local folders.
pub fn project_php_files(
    project: &Entity<Project>,
    cx: &mut App,
) -> (Vec<(WorktreeId, PathBuf)>, usize) {
    let mut files = Vec::new();
    let mut untrusted = 0;
    let worktree_ids = project
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).id())
        .collect::<Vec<_>>();
    for worktree_id in worktree_ids {
        if !zenith_php_tools::worktree_trusted(project, worktree_id, cx) {
            untrusted += 1;
            continue;
        }
        let root = ProjectPath {
            worktree_id,
            path: util::rel_path::RelPath::empty().into(),
        };
        files.extend(folder_php_files(project, &root, cx));
    }
    (files, untrusted)
}

fn inspect_project(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    let (files, untrusted) = project_php_files(&project, cx);
    if files.is_empty() && untrusted > 0 {
        return notify(
            workspace,
            "Trust this project to run its PHP tools.".into(),
            cx,
        );
    }
    let message = match files.len() {
        0 => "No PHP files in this project.".to_string(),
        1 => "Inspecting 1 PHP file.".to_string(),
        count => format!("Inspecting {count} PHP files."),
    };
    let max_parallel = InspectionSettings::get_global(cx).max_parallel;
    cx.spawn_in(window, async move |workspace, cx| {
        inspect_files(
            project,
            files,
            message,
            max_parallel,
            Default::default(),
            workspace,
            cx,
        )
        .await;
    })
    .detach();
}

/// What a run tells the user: tool failures, and notices that are not failures.
#[derive(Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

/// Shows each distinct tool failure and notice once per session, so a broken
/// ruleset does not raise a notification on every save.
fn report_run(workspace: &mut Workspace, report: Report, cx: &mut gpui::Context<Workspace>) {
    let notes = unseen_errors(&mut cx.default_global::<ShownErrors>().0, report.notes);
    if !notes.is_empty() {
        notify_once::<ShownNotes>(workspace, notes.join("\n"), cx);
    }
    let new = unseen_errors(&mut cx.default_global::<ShownErrors>().0, report.errors);
    if !new.is_empty() {
        workspace.show_notification(
            workspace::notifications::NotificationId::unique::<ShownErrors>(),
            cx,
            |cx| {
                cx.new(|cx| {
                    workspace::notifications::simple_message_notification::MessageNotification::new(
                        format!("PHP inspections failed: {}", new.join("\n")),
                        cx,
                    )
                })
            },
        );
    }
}

#[derive(Default)]
struct ShownErrors(std::collections::HashSet<String>);

/// Id of the notice notification, apart from the failures one.
struct ShownNotes;

fn notify_once<Id: 'static>(
    workspace: &mut Workspace,
    message: String,
    cx: &mut gpui::Context<Workspace>,
) {
    workspace.show_notification(
        workspace::notifications::NotificationId::unique::<Id>(),
        cx,
        |cx| {
            cx.new(|cx| {
                workspace::notifications::simple_message_notification::MessageNotification::new(
                    message, cx,
                )
            })
        },
    );
}

/// Why the tools do not run on a file in this worktree; `None` when they may.
fn untrusted_message(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> Option<&'static str> {
    trust_message(zenith_php_tools::worktree_trust(project, worktree_id, cx))
}

const OUTSIDE_FILE_MESSAGE: &str =
    "This file is outside your trusted folders. Open its folder to run its PHP tools.";

fn trust_message(trust: zenith_php_tools::Trust) -> Option<&'static str> {
    match trust {
        zenith_php_tools::Trust::Trusted => None,
        zenith_php_tools::Trust::Untrusted => Some("Trust this project to run its PHP tools."),
        zenith_php_tools::Trust::OutsideTrustedFolders => Some(OUTSIDE_FILE_MESSAGE),
    }
}

const PHP_NOT_FOUND: &str = "PHP was not found.";

/// The errors not shown yet, now marked shown.
pub fn unseen_errors(
    shown: &mut std::collections::HashSet<String>,
    errors: Vec<String>,
) -> Vec<String> {
    errors
        .into_iter()
        .filter(|error| shown.insert(error.clone()))
        .collect()
}

/// A run that worked: its old failures may be shown again if they come back.
pub fn forget(shown: &mut std::collections::HashSet<String>, prefix: &str) {
    shown.retain(|error| !error.starts_with(prefix));
}

fn forget_errors(prefix: &str, cx: &mut App) {
    if cx.has_global::<ShownErrors>() {
        forget(&mut cx.global_mut::<ShownErrors>().0, prefix);
    }
}

impl gpui::Global for ShownErrors {}

/// Where to look for PHP and tools; tests set it instead of changing `PATH`.
#[derive(Clone)]
pub struct ToolEnvironment {
    pub php_candidates: Vec<PathBuf>,
    pub path_dirs: Vec<PathBuf>,
}

impl gpui::Global for ToolEnvironment {}

fn tool_environment(cx: &App) -> ToolEnvironment {
    cx.try_global::<ToolEnvironment>()
        .cloned()
        .unwrap_or_else(|| {
            let (php_candidates, path_dirs) = zenith_php_tools::environment();
            ToolEnvironment {
                php_candidates,
                path_dirs,
            }
        })
}

fn notify(workspace: &mut Workspace, message: String, cx: &mut gpui::Context<Workspace>) {
    workspace.show_notification(
        workspace::notifications::NotificationId::unique::<FixFileWithPhpcbf>(),
        cx,
        |cx| {
            cx.new(|cx| {
                workspace::notifications::simple_message_notification::MessageNotification::new(
                    message, cx,
                )
            })
        },
    );
}

fn fix_current_file(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return notify(
            workspace,
            "PHP tools run on local projects only.".into(),
            cx,
        );
    }
    let Some(item) = workspace.active_item(cx) else {
        return;
    };
    let Some(project_path) = item.project_path(cx) else {
        return;
    };
    if project_path.path.extension() != Some("php") {
        return notify(workspace, "PHPCBF fixes PHP files.".into(), cx);
    }
    if item.is_dirty(cx) {
        return notify(
            workspace,
            "Save the file first; PHPCBF fixes the saved file.".into(),
            cx,
        );
    }
    let worktree_id = project_path.worktree_id;
    if let Some(message) = untrusted_message(&project, worktree_id, cx) {
        return notify(workspace, message.into(), cx);
    }
    let (Some(abs_path), Some(worktree)) = (
        project.read(cx).absolute_path(&project_path, cx),
        project.read(cx).worktree_for_id(worktree_id, cx),
    ) else {
        return;
    };
    let root = worktree_folder(&worktree, cx);
    let environment = tool_environment(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = cx
            .background_spawn({
                let (root, abs_path) = (root.clone(), abs_path.clone());
                async move {
                    let path_dirs = environment.path_dirs;
                    let Some((php, _)) =
                        zenith_php_tools::find_php(&environment.php_candidates).await
                    else {
                        anyhow::bail!("PHP was not found");
                    };
                    let managed = std::env::var_os("HOME").map(|home| {
                        let kind = zenith_php_tools::detect_project(&root);
                        zenith_php_tools::managed_bin(
                            Path::new(&home),
                            zenith_php_tools::Profile::for_project(kind),
                        )
                    });
                    run_fix(&root, &abs_path, &php, managed.as_deref(), &path_dirs)
                        .await
                        .unwrap_or_else(|| {
                            anyhow::bail!("phpcbf is not installed for this project")
                        })
                }
            })
            .await;
        let message = match &result {
            Ok(FixOutcome::Nothing) => "PHPCBF found nothing it can fix.".to_string(),
            Ok(FixOutcome::Fixed) => "PHPCBF fixed the file.".to_string(),
            Ok(FixOutcome::FixedSome) => {
                "PHPCBF fixed what it could; some problems need a hand.".to_string()
            }
            Err(error) => format!("{error:#}"),
        };
        workspace
            .update(cx, |workspace, cx| {
                notify(workspace, message, cx);
                if matches!(result, Ok(FixOutcome::Fixed | FixOutcome::FixedSome)) {
                    let run = inspect_path(&project, worktree_id, abs_path, cx);
                    cx.spawn(async move |workspace, cx| {
                        let report = run.await;
                        workspace
                            .update(cx, |workspace, cx| report_run(workspace, report, cx))
                            .ok();
                    })
                    .detach();
                }
            })
            .log_err();
    })
    .detach();
}

pub fn init(cx: &mut App) {
    InspectionSettings::register(cx);
    cx.observe_new(|editor: &mut editor::Editor, _, _| {
        if editor.mode().is_full() && editor.project().is_some() {
            editor.add_code_action_provider(std::rc::Rc::new(SuppressProvider));
        }
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|workspace, _: &FixFileWithPhpcbf, window, cx| {
            fix_current_file(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &InspectChangedFiles, window, cx| {
            inspect_changed_files(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &InspectCurrentFile, window, cx| {
            inspect_current_file(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &InspectProject, window, cx| {
            inspect_project(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &ToggleNewSinceBranch, window, cx| {
            new_since_branch::toggle(workspace, window, cx)
        });
        workspace.register_action(
            |workspace, _: &zed_actions::zenith_inspections::InspectFolder, window, cx| {
                inspect_folder(workspace, window, cx)
            },
        );
        let project = workspace.project().clone();
        if !project.read(cx).is_local() {
            return;
        }
        let buffer_store = project.read(cx).buffer_store().clone();
        cx.subscribe(&buffer_store, move |_, _, event, cx| {
            if let BufferStoreEvent::BufferAdded(buffer) = event {
                let project = project.clone();
                cx.subscribe(buffer, move |_, buffer, event, cx| {
                    on_buffer_event(&project, &buffer, event, cx.entity().downgrade(), cx)
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
    workspace: gpui::WeakEntity<Workspace>,
    cx: &mut App,
) {
    if matches!(event, BufferEvent::Saved) && InspectionSettings::get_global(cx).run_on_save {
        inspect(project, buffer, workspace, cx);
    }
}

/// The latest run per file, numbered from one counter for all files so a number is
/// never reused; a slow older run sees a different number and drops its results.
#[derive(Default)]
struct Generations {
    next: u64,
    latest: std::collections::HashMap<PathBuf, u64>,
}

impl gpui::Global for Generations {}

fn next_generation(path: &Path, cx: &mut App) -> u64 {
    let generations = cx.default_global::<Generations>();
    generations.next += 1;
    let generation = generations.next;
    generations.latest.insert(path.to_path_buf(), generation);
    generation
}

fn current_generation(path: &Path, cx: &App) -> u64 {
    cx.try_global::<Generations>()
        .and_then(|generations| generations.latest.get(path).copied())
        .unwrap_or(0)
}

/// Forgets a file once its latest run has finished.
fn finish_generation(path: &Path, generation: u64, cx: &mut App) {
    let latest = &mut cx.default_global::<Generations>().latest;
    if latest.get(path) == Some(&generation) {
        latest.remove(path);
    }
}

/// Runs the analyzers on a saved PHP file in a trusted local worktree.
pub fn inspect(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    workspace: gpui::WeakEntity<Workspace>,
    cx: &mut App,
) {
    let Some(file) = buffer.read(cx).file().cloned() else {
        return;
    };
    let worktree_id = file.worktree_id(cx);
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return;
    };
    // A file opened on its own is its worktree's root, with an empty relative path.
    let abs_path = worktree.read(cx).absolutize(file.path());
    if abs_path
        .extension()
        .is_none_or(|extension| extension != "php")
    {
        return;
    }
    let run = inspect_path(project, worktree_id, abs_path.clone(), cx);
    let key = (project.entity_id(), abs_path);
    let running = cx.default_global::<Running>();
    running.next += 1;
    let id = running.next;
    let task = cx.spawn({
        let key = key.clone();
        async move |cx| {
            let report = run.await;
            workspace
                .update(cx, |workspace, cx| report_run(workspace, report, cx))
                .ok();
            cx.update(|cx| {
                let tasks = &mut cx.default_global::<Running>().tasks;
                if tasks.get(&key).is_some_and(|(running, _)| *running == id) {
                    tasks.remove(&key);
                }
            });
        }
    });
    // Replacing the previous run drops it, which kills its tool processes.
    cx.default_global::<Running>().tasks.insert(key, (id, task));
}

/// The in-flight on-save run per project and file.
#[derive(Default)]
struct Running {
    next: u64,
    tasks: std::collections::HashMap<(gpui::EntityId, PathBuf), (u64, gpui::Task<()>)>,
}

impl gpui::Global for Running {}

/// The default-branch lookup for a folder, when "new since branch" is on there.
fn since_base_for(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> Option<new_since_branch::SharedBase> {
    let worktree = project.read(cx).worktree_for_id(worktree_id, cx)?;
    let root = worktree_folder(&worktree, cx);
    new_since_branch::is_on(&root, cx)
        .then(|| new_since_branch::base_branch(project, worktree_id, cx).shared())
}

/// Runs the analyzers on one file under a worktree root, open or not.
pub fn inspect_path(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    abs_path: PathBuf,
    cx: &mut App,
) -> gpui::Task<Report> {
    let since_base = since_base_for(project, worktree_id, cx);
    inspect_path_with(project, worktree_id, abs_path, since_base, cx)
}

/// `inspect_path` with the base already looked up (`Some` when "new since branch"
/// is on), so a batch looks it up once.
fn inspect_path_with(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    abs_path: PathBuf,
    since_base: Option<new_since_branch::SharedBase>,
    cx: &mut App,
) -> gpui::Task<Report> {
    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return gpui::Task::ready(Report::default());
    };
    // Remote paths are not local files, and local tools must not run on them.
    if !project.read(cx).is_local() {
        return gpui::Task::ready(Report::default());
    }
    match zenith_php_tools::worktree_trust(project, worktree_id, cx) {
        zenith_php_tools::Trust::Trusted => {}
        zenith_php_tools::Trust::Untrusted => return gpui::Task::ready(Report::default()),
        // Said once per file and session: the note names the file.
        zenith_php_tools::Trust::OutsideTrustedFolders => {
            let name = abs_path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            return gpui::Task::ready(Report {
                errors: Vec::new(),
                notes: vec![format!(
                    "{name} is outside your trusted folders. Open its folder to run its PHP tools."
                )],
            });
        }
    }
    let root = worktree_folder(&worktree, cx);
    let environment = tool_environment(cx);
    let lsp_store = project.read(cx).lsp_store();
    let analyzers = InspectionSettings::get_global(cx).analyzers();
    let generation = next_generation(&abs_path, cx);
    cx.spawn(async move |cx| {
        let results = cx
            .background_spawn(async move {
                let path_dirs = environment.path_dirs;
                let Some((php, _)) = zenith_php_tools::find_php(&environment.php_candidates).await
                else {
                    return Err(abs_path);
                };
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
                        // Tools report real paths; a symlinked root (macOS /var) differs.
                        let real = std::fs::canonicalize(&abs_path).ok();
                        let result = result.map(|problems| {
                            problems
                                .into_iter()
                                .filter(|problem| {
                                    problem.path == abs_path
                                        || real.is_some()
                                            && std::fs::canonicalize(&problem.path).ok() == real
                                })
                                .collect::<Vec<_>>()
                        });
                        results.push((analyzer, result));
                    }
                }
                // The base lookup has been running beside the tools. Git only runs
                // with a base, which `base_branch` gives trusted folders alone.
                let mut base_note = None;
                let new_lines = match since_base {
                    Some(base) if !results.is_empty() => match base.await {
                        Ok(base) => Some(since_branch::new_lines(&root, &base, &abs_path).await),
                        Err(message) => {
                            base_note = Some(new_since_branch::fallback_message(&message));
                            None
                        }
                    },
                    _ => None,
                };
                Ok((abs_path, results, new_lines, base_note))
            })
            .await;
        let (abs_path, results, new_lines, base_note) = match results {
            Ok(found) => found,
            Err(abs_path) => {
                cx.update(|cx| finish_generation(&abs_path, generation, cx));
                log::warn!("inspections: PHP was not found");
                return Report {
                    errors: vec![PHP_NOT_FOUND.to_string()],
                    notes: Vec::new(),
                };
            }
        };
        let mut report = Report {
            errors: Vec::new(),
            notes: base_note.into_iter().collect(),
        };
        let new_lines = match new_lines {
            Some((new_lines, message)) => {
                report
                    .notes
                    .extend(message.map(|message| new_since_branch::fallback_message(&message)));
                new_lines
            }
            None => since_branch::NewLines::All,
        };
        lsp_store.update(cx, |lsp_store, cx| {
            // A newer save of this file is running; its results win.
            if current_generation(&abs_path, cx) != generation {
                return;
            }
            finish_generation(&abs_path, generation, cx);
            forget_errors(PHP_NOT_FOUND, cx);
            let mut hidden = false;
            for analyzer in [Analyzer::Phpcs, Analyzer::Phpstan] {
                // An analyzer that did not run (switched off, no tool, no config)
                // clears its old findings; a failed run keeps them.
                let mine = match results.iter().find(|(ran, _)| *ran == analyzer) {
                    None => Vec::new(),
                    Some((_, Ok(problems))) => {
                        forget_errors(analyzer.name(), cx);
                        let shown = new_since_branch::keep_new(problems.clone(), &new_lines);
                        hidden |= shown.len() < problems.len();
                        shown
                    }
                    Some((_, Err(error))) => {
                        log::warn!("{}: {error:#}", analyzer.name());
                        report
                            .errors
                            .push(format!("{}: {error:#}", analyzer.name()));
                        continue;
                    }
                };
                push_diagnostics(lsp_store, analyzer, &abs_path, &mine, cx).log_err();
            }
            new_since_branch::mark_hidden(&abs_path, hidden, cx);
        });
        report
    })
}

#[cfg(test)]
mod tests;
