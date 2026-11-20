//! Suppress actions (plan 21, milestone 3): ⌘. on a PHPCS or PHPStan finding
//! offers "Suppress <rule> for this line", which writes the tool's own ignore
//! comment (`// phpcs:ignore <sniff>`, `// @phpstan-ignore <identifier>`).

use crate::Analyzer;
use anyhow::{Context as _, Result};
use gpui::{App, Entity, Task, Window};
use language::Buffer;
use project::{CodeAction, LspAction, ProjectTransaction};
use std::ops::Range;
use text::ToPoint as _;

/// Where the comment goes and what it says: replace `range` of the buffer text
/// with the string. `None` when the line can't take a comment safely.
pub fn suppression_edit(
    analyzer: Analyzer,
    text: &str,
    row: u32,
    column: u32,
    rule: Option<&str>,
) -> Option<(Range<usize>, String)> {
    let lines = line_ranges(text);
    let line_range = lines.get(row as usize)?.clone();
    let line = &text[line_range.clone()];
    let trimmed = line.trim_start();
    // Inside a docblock or block comment a `//` comment would become comment text.
    if trimmed.starts_with('*') {
        return None;
    }
    let comment = match (analyzer, rule) {
        (Analyzer::Phpcs, Some(rule)) => format!("// phpcs:ignore {rule}"),
        (Analyzer::Phpcs, None) => "// phpcs:ignore".to_string(),
        (Analyzer::Phpstan, Some(rule)) => format!("// @phpstan-ignore {rule}"),
        (Analyzer::Phpstan, None) => "// @phpstan-ignore-next-line".to_string(),
    };

    // Template line (`<li><?php echo $x; ?></li>`): the line above may be HTML,
    // so comment inside the PHP block. A `//` comment ends at `?>`.
    let column = (column as usize).min(line.len());
    if let Some(close) = line[column..].find("?>") {
        let at = line_range.start + column + close;
        let comment = comment.replace("-next-line", "-line");
        return Some((at..at, format!("{comment} ")));
    }
    // The PHP block opens on this line, so the line above is not PHP.
    if line.contains("<?php") || line.contains("<?=") {
        let at = line_range.start + line.trim_end().len();
        let comment = comment.replace("-next-line", "-line");
        return Some((at..at, format!(" {comment}")));
    }

    // Add to an ignore comment already on the line above.
    if let (Some(rule), Some(above)) = (rule, row.checked_sub(1)) {
        let above_range = lines[above as usize].clone();
        let above_line = &text[above_range.clone()];
        let prefix = match analyzer {
            Analyzer::Phpcs => "// phpcs:ignore ",
            Analyzer::Phpstan => "// @phpstan-ignore ",
        };
        if let Some(rules) = above_line.trim().strip_prefix(prefix) {
            // A reason follows ` -- ` (PHPCS) or sits in parentheses (PHPStan).
            let end = match analyzer {
                Analyzer::Phpcs => rules.find(" --"),
                Analyzer::Phpstan => rules.find(" ("),
            }
            .unwrap_or(rules.len());
            if rules[..end]
                .split(',')
                .any(|existing| existing.trim() == rule)
            {
                return None;
            }
            let at = above_range.start + above_line.find(prefix)? + prefix.len() + end;
            return Some((at..at, format!(", {rule}")));
        }
    }

    let indent = &line[..line.len() - trimmed.len()];
    let at = line_range.start;
    Some((at..at, format!("{indent}{comment}\n")))
}

/// Byte range of each line, without its line break.
fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut start = 0;
    let mut lines = Vec::new();
    for line in text.split('\n') {
        lines.push(start..start + line.len());
        start += line.len() + 1;
    }
    lines
}

const PROVIDER_ID: &str = "zenith_inspections";

pub struct SuppressProvider;

impl SuppressProvider {
    /// One action per finding on the rows `range` touches.
    pub fn actions(
        buffer: &Entity<Buffer>,
        range: Range<text::Anchor>,
        cx: &App,
    ) -> Vec<CodeAction> {
        let snapshot = buffer.read(cx).snapshot();
        let rows = range.start.to_point(&snapshot).row..=range.end.to_point(&snapshot).row;
        let mut actions = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for analyzer in [Analyzer::Phpcs, Analyzer::Phpstan] {
            for (_, group) in snapshot.diagnostic_groups(Some(analyzer.server_id())) {
                for entry in group.entries {
                    let row = entry.range.start.to_point(&snapshot).row;
                    if !rows.contains(&row) {
                        continue;
                    }
                    let rule = match &entry.diagnostic.code {
                        Some(lsp::NumberOrString::String(rule)) => Some(rule.clone()),
                        Some(lsp::NumberOrString::Number(rule)) => Some(rule.to_string()),
                        None => None,
                    };
                    if !seen.insert((analyzer.name(), row, rule.clone())) {
                        continue;
                    }
                    let label = match analyzer {
                        Analyzer::Phpcs => "PHPCS",
                        Analyzer::Phpstan => "PHPStan",
                    };
                    let title = match &rule {
                        Some(rule) => format!("Suppress {rule} for this line ({label})"),
                        None => format!("Suppress {label} for this line"),
                    };
                    actions.push(CodeAction {
                        server_id: analyzer.server_id(),
                        range: entry.range.clone(),
                        lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                            title,
                            kind: Some(lsp::CodeActionKind::QUICKFIX),
                            data: Some(serde_json::json!({ "rule": rule })),
                            ..lsp::CodeAction::default()
                        })),
                        resolved: true,
                    });
                }
            }
        }
        actions
    }

    /// Writes the comment for `action`, located from its anchor so edits made
    /// since the menu opened don't move it to the wrong line.
    pub fn apply(
        buffer: &Entity<Buffer>,
        action: &CodeAction,
        push_to_history: bool,
        cx: &mut App,
    ) -> Result<ProjectTransaction> {
        let analyzer = [Analyzer::Phpcs, Analyzer::Phpstan]
            .into_iter()
            .find(|analyzer| analyzer.server_id() == action.server_id)
            .context("not a suppress action")?;
        let LspAction::Action(lsp_action) = &action.lsp_action else {
            anyhow::bail!("not a suppress action");
        };
        let rule = lsp_action
            .data
            .as_ref()
            .and_then(|data| data.get("rule"))
            .and_then(|rule| rule.as_str())
            .map(str::to_string);
        let snapshot = buffer.read(cx).snapshot();
        let point = action.range.start.to_point(&snapshot);
        let text = snapshot.text();
        let (range, comment) =
            suppression_edit(analyzer, &text, point.row, point.column, rule.as_deref())
                .context("this line can't take an ignore comment")?;
        let mut transaction = ProjectTransaction::default();
        let edited = buffer.update(cx, |buffer, cx| {
            buffer.finalize_last_transaction();
            buffer.start_transaction();
            buffer.edit([(range, comment)], None, cx);
            buffer.end_transaction(cx).and_then(|id| {
                if push_to_history {
                    buffer.finalize_last_transaction();
                    buffer.get_transaction(id).cloned()
                } else {
                    buffer.forget_transaction(id)
                }
            })
        });
        if let Some(edited) = edited {
            transaction.0.insert(buffer.clone(), edited);
        }
        Ok(transaction)
    }
}

impl editor::CodeActionProvider for SuppressProvider {
    fn id(&self) -> std::sync::Arc<str> {
        PROVIDER_ID.into()
    }

    fn code_actions(
        &self,
        buffer: &Entity<Buffer>,
        range: Range<text::Anchor>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        Task::ready(Ok(Self::actions(buffer, range, cx)))
    }

    fn apply_code_action(
        &self,
        buffer: Entity<Buffer>,
        action: CodeAction,
        push_to_history: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<ProjectTransaction>> {
        Task::ready(Self::apply(&buffer, &action, push_to_history, cx))
    }
}
