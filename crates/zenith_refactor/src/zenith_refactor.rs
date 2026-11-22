use anyhow::{Context as _, Result, bail, ensure};
use fs::{Fs, RemoveOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

pub mod editor_actions;
pub mod preview_modal;
pub use editor_actions::init;
pub use preview_modal::*;

gpui::actions!(
    zenith_refactor,
    [
        RefactorThis,
        MoveClass,
        SafeDelete,
        ChangeSignature,
        ExtractMethod,
        ExtractVariable,
    ]
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefactorKind {
    MoveClass {
        class_name: String,
        source_path: PathBuf,
        target_path: PathBuf,
        old_namespace: String,
        new_namespace: String,
    },
    SafeDelete {
        symbol_name: String,
        file_path: PathBuf,
    },
    ChangeSignature {
        function_name: String,
        file_path: PathBuf,
        reordered_indices: Vec<usize>,
        added_params: Vec<SignatureParam>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureParam {
    pub name: String,
    pub type_hint: Option<String>,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReplacement {
    pub line: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub file_path: PathBuf,
    pub original_content: String,
    pub modified_content: String,
    pub replacements: Vec<TextReplacement>,
    pub is_included: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorPlan {
    pub kind: RefactorKind,
    pub description: String,
    pub file_diffs: Vec<FileDiff>,
    pub can_apply_safely: bool,
    pub warnings: Vec<String>,
}

impl RefactorPlan {
    pub fn apply(&self) -> HashMap<PathBuf, String> {
        let mut results = HashMap::new();
        for diff in &self.file_diffs {
            if diff.is_included {
                results.insert(diff.file_path.clone(), diff.modified_content.clone());
            }
        }
        results
    }

    /// Every file the ticked rows write or remove.
    pub fn touched_paths(&self) -> Vec<&Path> {
        let mut paths: Vec<&Path> = self
            .file_diffs
            .iter()
            .filter(|diff| diff.is_included)
            .map(|diff| diff.file_path.as_path())
            .collect();
        if let RefactorKind::MoveClass {
            source_path,
            target_path,
            ..
        } = &self.kind
            && paths.contains(&target_path.as_path())
        {
            paths.push(source_path);
        }
        paths
    }

    /// Writes every ticked row, then (for a move) removes the old file.
    ///
    /// Every path is checked before anything is written: it must lie inside one of `roots`
    /// (the project's worktrees) with no `..` and no symlink on the way. The source of a move
    /// is only removed when the target row is ticked, every write succeeded and the file still
    /// holds what the preview showed. Before the first write, every ticked file must still hold
    /// what the preview showed and a move's new file must not exist yet; otherwise nothing is
    /// written. Files are written atomically (temp file and rename).
    pub async fn apply_to_disk(&self, fs: &dyn Fs, roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let mut checked_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = fs
                .canonicalize(root)
                .await
                .with_context(|| format!("cannot resolve project folder {}", root.display()))?;
            checked_roots.push((root.clone(), canonical));
        }

        let writes: Vec<&FileDiff> = self
            .file_diffs
            .iter()
            .filter(|diff| diff.is_included)
            .collect();
        // The old file goes only when its replacement is ticked: (path, content the preview showed).
        let removal = match &self.kind {
            RefactorKind::MoveClass {
                source_path,
                target_path,
                ..
            } if source_path != target_path => writes
                .iter()
                .find(|diff| &diff.file_path == target_path)
                .map(|diff| (source_path, diff.original_content.as_str())),
            _ => None,
        };
        let removal = match removal {
            Some((path, shown)) if fs.metadata(path).await?.is_some() => Some((path, shown)),
            _ => None,
        };

        for path in writes
            .iter()
            .map(|diff| &diff.file_path)
            .chain(removal.map(|(path, _)| path))
        {
            check_inside_project(fs, &checked_roots, path).await?;
        }

        // Check the disk against the preview for every file before the first write.
        let new_file = match &self.kind {
            RefactorKind::MoveClass {
                source_path,
                target_path,
                ..
            } if source_path != target_path => Some(target_path),
            _ => None,
        };
        for diff in &writes {
            let path = &diff.file_path;
            if new_file == Some(path) {
                // A case-only rename on a case-insensitive disk "exists" as the source itself.
                let is_the_source = match &self.kind {
                    RefactorKind::MoveClass { source_path, .. } => {
                        is_same_file(fs, source_path, path).await?
                    }
                    _ => false,
                };
                ensure!(
                    is_the_source || fs.metadata(path).await?.is_none(),
                    "{} already exists; nothing was written",
                    path.display()
                );
            } else {
                let on_disk = fs.load(path).await.ok();
                ensure!(
                    on_disk.as_deref() == Some(diff.original_content.as_str()),
                    "{} changed since the preview (or is gone); nothing was written",
                    path.display()
                );
            }
        }
        if let Some((source, shown)) = removal {
            ensure!(
                fs.load(source).await.ok().as_deref() == Some(shown),
                "{} changed since the preview; nothing was written",
                source.display()
            );
        }

        let mut affected = Vec::new();
        for diff in &writes {
            if let Some(parent) = diff.file_path.parent() {
                fs.create_dir(parent)
                    .await
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            fs.atomic_write(diff.file_path.clone(), diff.modified_content.clone())
                .await
                .with_context(|| format!("cannot write {}", diff.file_path.display()))?;
            affected.push(diff.file_path.clone());
        }

        if let Some((source, shown)) = removal {
            // After a case-only rename on a case-insensitive disk the "old" path is the file
            // just written; removing it would delete the result.
            if let Some(target) = new_file
                && is_same_file(fs, source, target).await?
            {
                return Ok(affected);
            }
            let on_disk = fs.load(source).await.ok();
            ensure!(
                on_disk.as_deref() == Some(shown),
                "{} changed since the preview; it was left in place (the new file was written)",
                source.display()
            );
            fs.remove_file(
                source,
                RemoveOptions {
                    recursive: false,
                    ignore_if_not_exists: true,
                },
            )
            .await
            .with_context(|| format!("cannot remove {}", source.display()))?;
            affected.push(source.clone());
        }

        Ok(affected)
    }
}

/// Whether two paths name the same file: the same inode when the disk reports one, otherwise
/// (inode unavailable) the same path ignoring case. Paths that do not both exist are different.
async fn is_same_file(fs: &dyn Fs, a: &Path, b: &Path) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    Ok(match (fs.metadata(a).await?, fs.metadata(b).await?) {
        (Some(x), Some(y)) if x.inode != 0 && y.inode != 0 => x.inode == y.inode,
        (Some(_), Some(_)) => {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        }
        _ => false,
    })
}

/// Refuses paths that leave the project: relative paths, `..`, anything outside `roots`
/// (given path and its canonical form), and any symlink below a root.
async fn check_inside_project(
    fs: &dyn Fs,
    roots: &[(PathBuf, PathBuf)],
    path: &Path,
) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "{} is not an absolute path",
        path.display()
    );
    ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir)),
        "{} contains a relative component",
        path.display()
    );
    let Some((given, canonical)) = roots.iter().find(|(given, _)| path.starts_with(given)) else {
        bail!("{} is outside the project", path.display());
    };

    let mut current = given.clone();
    for component in path.strip_prefix(given)?.components() {
        current.push(component);
        match fs.metadata(&current).await? {
            Some(metadata) if metadata.is_symlink => {
                bail!("{} goes through a symlink", path.display())
            }
            Some(_) => {}
            None => break,
        }
    }

    // The deepest part that exists must also resolve inside the project.
    let mut existing = path;
    while fs.metadata(existing).await?.is_none() {
        existing = existing.parent().context("path has no existing ancestor")?;
    }
    let resolved = fs.canonicalize(existing).await?;
    ensure!(
        resolved.starts_with(canonical),
        "{} is outside the project",
        path.display()
    );
    Ok(())
}

pub fn derive_psr4_namespace(base_path: &Path, file_path: &Path, vendor_prefix: &str) -> String {
    let parent = file_path.parent().unwrap_or(Path::new(""));
    let relative = parent.strip_prefix(base_path).unwrap_or(parent);

    let mut parts = Vec::new();
    if !vendor_prefix.is_empty() {
        parts.push(vendor_prefix.trim_end_matches('\\').to_string());
    }

    for component in relative.components() {
        let comp_str = component.as_os_str().to_string_lossy();
        if comp_str == "src" || comp_str == "app" {
            continue;
        }
        parts.push(comp_str.to_string());
    }

    parts.join("\\")
}

pub fn plan_move_class(
    class_name: &str,
    source_path: &Path,
    target_path: &Path,
    old_namespace: &str,
    new_namespace: &str,
    source_content: &str,
    project_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut file_diffs = Vec::new();

    let mut moved_replacements = Vec::new();
    let old_ns_decl = format!("namespace {old_namespace};");
    let new_ns_decl = format!("namespace {new_namespace};");

    let mut modified_source = source_content.to_string();
    if let Some(pos) = source_content.find(&old_ns_decl) {
        let line_num = source_content[..pos].lines().count() + 1;
        moved_replacements.push(TextReplacement {
            line: line_num,
            old_text: old_ns_decl.clone(),
            new_text: new_ns_decl.clone(),
        });
        modified_source = modified_source.replacen(&old_ns_decl, &new_ns_decl, 1);
    }

    file_diffs.push(FileDiff {
        file_path: target_path.to_path_buf(),
        original_content: source_content.to_string(),
        modified_content: modified_source,
        replacements: moved_replacements,
        is_included: true,
    });

    let old_import = format!("use {old_namespace}\\{class_name};");
    let new_import = format!("use {new_namespace}\\{class_name};");

    let old_qualified = format!("\\{old_namespace}\\{class_name}");
    let new_qualified = format!("\\{new_namespace}\\{class_name}");

    for (file_path, content) in project_files {
        if file_path == source_path || file_path == target_path {
            continue;
        }

        let mut replacements = Vec::new();
        let mut new_content = content.clone();

        if content.contains(&old_import) {
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(&old_import) {
                    replacements.push(TextReplacement {
                        line: line_idx + 1,
                        old_text: old_import.clone(),
                        new_text: new_import.clone(),
                    });
                }
            }
            new_content = new_content.replace(&old_import, &new_import);
        }

        if content.contains(&old_qualified) {
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(&old_qualified) {
                    replacements.push(TextReplacement {
                        line: line_idx + 1,
                        old_text: old_qualified.clone(),
                        new_text: new_qualified.clone(),
                    });
                }
            }
            new_content = new_content.replace(&old_qualified, &new_qualified);
        }

        if !replacements.is_empty() {
            file_diffs.push(FileDiff {
                file_path: file_path.clone(),
                original_content: content.clone(),
                modified_content: new_content,
                replacements,
                is_included: true,
            });
        }
    }

    RefactorPlan {
        kind: RefactorKind::MoveClass {
            class_name: class_name.to_string(),
            source_path: source_path.to_path_buf(),
            target_path: target_path.to_path_buf(),
            old_namespace: old_namespace.to_string(),
            new_namespace: new_namespace.to_string(),
        },
        description: format!("Move class {class_name} from {old_namespace} to {new_namespace}"),
        file_diffs,
        can_apply_safely: true,
        warnings: Vec::new(),
    }
}

/// Scans PHP text from `start` (just after an opening bracket) to the bracket that closes it.
/// Returns the byte index of that closing bracket and the byte ranges of the top-level
/// comma-separated pieces. Strings and comments are skipped; `None` when unbalanced.
fn scan_balanced(text: &str, start: usize) -> Option<(usize, Vec<(usize, usize)>)> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut pieces = Vec::new();
    let mut piece_start = start;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map(|end| i + 2 + end + 1)?;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = text[i..].find('\n').map_or(bytes.len(), |end| i + end);
            }
            b'#' if bytes.get(i + 1) != Some(&b'[') => {
                i = text[i..].find('\n').map_or(bytes.len(), |end| i + end);
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    pieces.push((piece_start, i));
                    return Some((i, pieces));
                }
            }
            b',' if depth == 1 => {
                pieces.push((piece_start, i));
                piece_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn pieces_of(text: &str, ranges: &[(usize, usize)]) -> Vec<String> {
    ranges
        .iter()
        .filter_map(|&(start, end)| text.get(start..end))
        .map(|piece| piece.trim().to_string())
        .filter(|piece| !piece.is_empty())
        .collect()
}

fn line_of(text: &str, byte: usize) -> usize {
    text.get(..byte).map_or(1, |head| head.lines().count() + 1)
}

/// The byte range of the top-level definition of `name` (class, interface, trait, enum or
/// function), from the start of its line through the closing brace and the rest of that line.
fn symbol_definition_range(content: &str, name: &str) -> Option<(usize, usize)> {
    let pattern = format!(
        r"(?m)^([ \t]*(?:<\?php[ \t]+)?)(?:(?:abstract|final|readonly)[ \t]+)*(?:class|interface|trait|enum|function)[ \t]+&?{}\b",
        regex::escape(name)
    );
    let captures = regex::Regex::new(&pattern).ok()?.captures(content)?;
    let found = captures.get(0)?;
    // A one-line file may open with `<?php class X {}`: keep the opening tag.
    let prefix = captures.get(1)?;
    let start = if prefix.as_str().contains("<?php") {
        prefix.end()
    } else {
        found.start()
    };
    let after_header = content.get(found.end()..)?;
    let open = found.end() + after_header.find('{')?;
    // A `;` first means a declaration without a body.
    if content.get(found.end()..open)?.contains(';') {
        return None;
    }
    let (close, _) = scan_balanced(content, open + 1)?;
    let mut end = close + 1;
    let rest = content.get(end..)?;
    let blank = rest
        .find('\n')
        .filter(|&newline| rest[..newline].trim().is_empty());
    if let Some(newline) = blank {
        end += newline + 1;
    }
    Some((start, end))
}

pub fn plan_safe_delete(
    symbol_name: &str,
    file_path: &Path,
    file_content: &str,
    project_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut warnings = Vec::new();
    let mut file_diffs = Vec::new();

    let pattern = format!(r"\b{}\b", regex::escape(symbol_name));
    let reg = regex::Regex::new(&pattern).ok();

    for (p, content) in project_files {
        if p == file_path {
            continue;
        }

        let mut count = 0;
        let mut replacements = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let matches = if let Some(ref r) = reg {
                r.is_match(line)
            } else {
                line.contains(symbol_name)
            };

            if matches {
                count += 1;
                replacements.push(TextReplacement {
                    line: line_idx + 1,
                    old_text: line.trim().to_string(),
                    new_text: String::new(),
                });
            }
        }

        if count > 0 {
            warnings.push(format!(
                "Symbol '{symbol_name}' referenced {count} time(s) in {}",
                p.display()
            ));
            file_diffs.push(FileDiff {
                file_path: p.clone(),
                original_content: content.clone(),
                modified_content: content.clone(),
                replacements,
                is_included: false,
            });
        }
    }

    let has_usages = !warnings.is_empty();

    // Only the symbol's own definition is removed; the rest of its file stays.
    match symbol_definition_range(file_content, symbol_name) {
        Some((start, end)) => {
            let mut modified = file_content.to_string();
            modified.replace_range(start..end, "");
            file_diffs.insert(
                0,
                FileDiff {
                    file_path: file_path.to_path_buf(),
                    original_content: file_content.to_string(),
                    modified_content: modified,
                    replacements: vec![TextReplacement {
                        line: line_of(file_content, start),
                        old_text: file_content[start..end].trim().to_string(),
                        new_text: String::new(),
                    }],
                    is_included: !has_usages,
                },
            );
        }
        None => warnings.push(format!(
            "Could not find the definition of '{symbol_name}' in {}; nothing would be deleted",
            file_path.display()
        )),
    }

    let can_apply_safely = warnings.is_empty();

    RefactorPlan {
        kind: RefactorKind::SafeDelete {
            symbol_name: symbol_name.to_string(),
            file_path: file_path.to_path_buf(),
        },
        description: format!("Safe delete symbol '{symbol_name}'"),
        file_diffs,
        can_apply_safely,
        warnings,
    }
}

/// A call of the function found in a file.
struct CallSite {
    start: usize,
    end: usize,
    args: Vec<String>,
}

/// Calls of the plain function `name(`. Method calls (`->name(`, `::name(`), other namespaces'
/// functions, `new name(` and definitions are not calls of it; they are counted in `skipped`.
/// Calls whose parentheses cannot be matched, or that sit inside another call, are counted in
/// `unclear`.
fn find_calls(content: &str, name: &str) -> (Vec<CallSite>, usize, usize) {
    let mut calls: Vec<CallSite> = Vec::new();
    let (mut skipped, mut unclear) = (0, 0);
    let Ok(reg) = regex::Regex::new(&format!(r"\b{}\s*\(", regex::escape(name))) else {
        return (calls, skipped, unclear);
    };
    for found in reg.find_iter(content) {
        let before = content[..found.start()].trim_end();
        let qualified_elsewhere = before
            .strip_suffix('\\')
            .is_some_and(|rest| rest.ends_with(|c: char| c.is_alphanumeric() || c == '_'));
        let word_before = before
            .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("");
        if before.ends_with("->") || before.ends_with("::") || qualified_elsewhere {
            skipped += 1;
            continue;
        }
        if matches!(word_before, "function" | "new" | "fn") || before.ends_with("function &") {
            continue;
        }
        if calls.last().is_some_and(|call| found.start() < call.end) {
            unclear += 1;
            continue;
        }
        match scan_balanced(content, found.end()) {
            Some((close, pieces)) => calls.push(CallSite {
                start: found.start(),
                end: close + 1,
                args: pieces_of(content, &pieces),
            }),
            None => unclear += 1,
        }
    }
    (calls, skipped, unclear)
}

pub fn plan_change_signature(
    function_name: &str,
    def_file: &Path,
    def_content: &str,
    reordered_indices: &[usize],
    added_params: &[SignatureParam],
    caller_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut file_diffs = Vec::new();
    let mut warnings = Vec::new();
    let mut can_apply_safely = true;

    let definition = regex::Regex::new(&format!(
        r"function\s+&?\s*{}\s*\(",
        regex::escape(function_name)
    ))
    .ok()
    .and_then(|reg| reg.find(def_content))
    .and_then(|found| {
        let by_ref = found.as_str().contains('&');
        let (close, pieces) = scan_balanced(def_content, found.end())?;
        Some((
            found.start(),
            close + 1,
            by_ref,
            pieces_of(def_content, &pieces),
        ))
    });

    let plan = |file_diffs, can_apply_safely, warnings| RefactorPlan {
        kind: RefactorKind::ChangeSignature {
            function_name: function_name.to_string(),
            file_path: def_file.to_path_buf(),
            reordered_indices: reordered_indices.to_vec(),
            added_params: added_params.to_vec(),
        },
        description: format!("Change signature for {function_name}"),
        file_diffs,
        can_apply_safely,
        warnings,
    };

    let Some((start, end, by_ref, raw_params)) = definition else {
        warnings.push(format!(
            "Could not read the signature of '{function_name}' in {}; nothing was changed",
            def_file.display()
        ));
        return plan(file_diffs, false, warnings);
    };

    let mut seen = HashSet::new();
    if let Some(&bad) = reordered_indices
        .iter()
        .find(|&&idx| idx >= raw_params.len() || !seen.insert(idx))
    {
        warnings.push(format!(
            "Parameter index {bad} is out of range or repeated ('{function_name}' has {} parameter(s), numbered from 0); nothing was changed",
            raw_params.len()
        ));
        return plan(file_diffs, false, warnings);
    }
    for added in added_params
        .iter()
        .filter(|added| added.default_value.is_none())
    {
        can_apply_safely = false;
        warnings.push(format!(
            "New parameter '{}' has no default value; existing calls will pass null",
            added.name
        ));
    }

    let mut new_params: Vec<String> = reordered_indices
        .iter()
        .map(|&idx| raw_params[idx].clone())
        .collect();
    for added in added_params {
        let mut param_str = String::new();
        if let Some(ref th) = added.type_hint {
            param_str.push_str(th);
            param_str.push(' ');
        }
        if !added.name.starts_with('$') {
            param_str.push('$');
        }
        param_str.push_str(&added.name);
        if let Some(ref def_val) = added.default_value {
            param_str.push_str(" = ");
            param_str.push_str(def_val);
        }
        new_params.push(param_str);
    }
    let amp = if by_ref { "&" } else { "" };
    let new_full = format!("function {amp}{function_name}({})", new_params.join(", "));

    // The defining file is rewritten once: its signature plus any calls inside it
    // (recursion, helpers used further down).
    let mut def_edits = vec![(start, end, new_full)];
    def_edits.extend(
        rewrite_calls(
            def_content,
            def_file,
            function_name,
            reordered_indices,
            added_params,
            raw_params.len(),
            &mut warnings,
            &mut can_apply_safely,
        )
        .into_iter()
        .filter(|&(call_start, call_end, _)| call_end <= start || call_start >= end),
    );
    file_diffs.push(file_diff(def_file, def_content, def_edits));

    for (caller_path, caller_content) in caller_files {
        if caller_path == def_file {
            continue;
        }
        let edits = rewrite_calls(
            caller_content,
            caller_path,
            function_name,
            reordered_indices,
            added_params,
            raw_params.len(),
            &mut warnings,
            &mut can_apply_safely,
        );
        if !edits.is_empty() {
            file_diffs.push(file_diff(caller_path, caller_content, edits));
        }
    }

    plan(file_diffs, can_apply_safely, warnings)
}

/// Applies non-overlapping `(start, end, new_text)` edits to `content`.
fn file_diff(path: &Path, content: &str, mut edits: Vec<(usize, usize, String)>) -> FileDiff {
    edits.sort_by_key(|&(start, _, _)| start);
    let mut modified = content.to_string();
    // Later edits first, so earlier byte offsets stay valid.
    for (start, end, new_text) in edits.iter().rev() {
        modified.replace_range(*start..*end, new_text);
    }
    FileDiff {
        file_path: path.to_path_buf(),
        original_content: content.to_string(),
        modified_content: modified,
        replacements: edits
            .into_iter()
            .map(|(start, end, new_text)| TextReplacement {
                line: line_of(content, start),
                old_text: content[start..end].to_string(),
                new_text,
            })
            .collect(),
        is_included: true,
    }
}

/// Rewritten calls of `function_name` in `content`. A call whose arguments can't be
/// mapped onto the new order (extra variadic arguments, or omitted optional ones that
/// would leave a gap) is left as it is and the plan is marked unsafe.
#[allow(clippy::too_many_arguments)]
fn rewrite_calls(
    content: &str,
    path: &Path,
    function_name: &str,
    reordered_indices: &[usize],
    added_params: &[SignatureParam],
    param_count: usize,
    warnings: &mut Vec<String>,
    can_apply_safely: &mut bool,
) -> Vec<(usize, usize, String)> {
    let (calls, skipped, mut unclear) = find_calls(content, function_name);
    if skipped > 0 {
        warnings.push(format!(
            "{skipped} method-style or other-namespace call(s) of '{function_name}' in {} were left unchanged; check them by hand",
            path.display()
        ));
    }
    let mut edits = Vec::new();
    for call in calls {
        let given = call.args.len();
        let mut args = Vec::new();
        let mut gap = false;
        let mut mappable = given <= param_count;
        for &idx in reordered_indices {
            match call.args.get(idx) {
                Some(arg) if !gap => args.push(arg.clone()),
                Some(_) => mappable = false,
                None => gap = true,
            }
        }
        if gap && !added_params.is_empty() {
            mappable = false;
        }
        if !mappable {
            unclear += 1;
            continue;
        }
        for added in added_params {
            args.push(
                added
                    .default_value
                    .clone()
                    .unwrap_or_else(|| "null".to_string()),
            );
        }
        edits.push((
            call.start,
            call.end,
            format!("{function_name}({})", args.join(", ")),
        ));
    }
    if unclear > 0 {
        *can_apply_safely = false;
        warnings.push(format!(
            "{unclear} call(s) of '{function_name}' in {} are nested, could not be parsed, or pass a different number of arguments, and were left unchanged",
            path.display()
        ));
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[test]
    fn test_derive_psr4_namespace() {
        let base = Path::new("/var/www/my-project");
        let file = Path::new("/var/www/my-project/app/Services/Billing/PaymentService.php");
        let ns = derive_psr4_namespace(base, file, "App");
        assert_eq!(ns, "App\\Services\\Billing");
    }

    #[test]
    fn test_move_class_plan() {
        let source_path = PathBuf::from("app/Services/OldService.php");
        let target_path = PathBuf::from("app/Billing/OldService.php");
        let source_content = r#"<?php
namespace App\Services;

class OldService {
    public function execute() {}
}
"#;

        let caller_files = vec![(
            PathBuf::from("app/Http/Controllers/OrderController.php"),
            r#"<?php
namespace App\Http\Controllers;

use App\Services\OldService;

class OrderController {
    public function handle(OldService $service) {
        $service->execute();
    }
}
"#
            .to_string(),
        )];

        let plan = plan_move_class(
            "OldService",
            &source_path,
            &target_path,
            "App\\Services",
            "App\\Billing",
            source_content,
            &caller_files,
        );

        assert!(plan.can_apply_safely);
        assert_eq!(plan.file_diffs.len(), 2);

        let applied = plan.apply();
        let moved_file = applied.get(&target_path).expect("moved file should exist");
        assert!(moved_file.contains("namespace App\\Billing;"));

        let updated_caller = applied
            .get(&PathBuf::from("app/Http/Controllers/OrderController.php"))
            .expect("caller should exist");
        assert!(updated_caller.contains("use App\\Billing\\OldService;"));
    }

    #[test]
    fn test_safe_delete_with_usages_warning() {
        let file_path = PathBuf::from("app/Services/UserService.php");
        let content = "<?php class UserService {}";

        let caller_files = vec![(
            PathBuf::from("app/Controllers/UserController.php"),
            "<?php use App\\Services\\UserService;\nclass UserController { private UserService $s; }"
                .to_string(),
        )];

        let plan = plan_safe_delete("UserService", &file_path, content, &caller_files);
        assert!(!plan.can_apply_safely);
        assert_eq!(plan.warnings.len(), 1);
        // Row 0 is the definition (left unticked while usages exist), row 1 the usages.
        assert!(!plan.file_diffs[0].is_included);
        assert_eq!(plan.file_diffs[1].replacements.len(), 2);
        assert!(plan.warnings[0].contains("UserController.php"));
    }

    #[test]
    fn test_safe_delete_zero_usages() {
        let file_path = PathBuf::from("app/Services/DeadCode.php");
        let content = "<?php class DeadCode {}";
        let caller_files = vec![(
            PathBuf::from("app/Controllers/OtherController.php"),
            "<?php class OtherController {}".to_string(),
        )];

        let plan = plan_safe_delete("DeadCode", &file_path, content, &caller_files);
        assert!(plan.can_apply_safely);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn test_change_signature_reordering_and_defaults() {
        let def_path = PathBuf::from("app/functions.php");
        let def_content = r#"<?php
function calculate_tax($subtotal, $rate) {
    return $subtotal * $rate;
}
"#;

        let caller_files = vec![(
            PathBuf::from("app/cart.php"),
            "<?php $tax = calculate_tax($amount, 0.05);".to_string(),
        )];

        let reordered_indices = vec![1, 0];
        let added_params = vec![SignatureParam {
            name: "currency".to_string(),
            type_hint: Some("string".to_string()),
            default_value: Some("'USD'".to_string()),
        }];

        let plan = plan_change_signature(
            "calculate_tax",
            &def_path,
            def_content,
            &reordered_indices,
            &added_params,
            &caller_files,
        );

        assert!(plan.can_apply_safely);
        let applied = plan.apply();

        let modified_def = applied.get(&def_path).expect("def should exist");
        assert!(
            modified_def
                .contains("function calculate_tax($rate, $subtotal, string $currency = 'USD')")
        );

        let modified_caller = applied
            .get(&PathBuf::from("app/cart.php"))
            .expect("caller should exist");
        assert!(modified_caller.contains("calculate_tax(0.05, $amount, 'USD')"));
    }

    #[gpui::test]
    fn test_preview_modal_inclusion_toggle(cx: &mut gpui::App) {
        let source_path = PathBuf::from("app/Services/OldService.php");
        let target_path = PathBuf::from("app/Billing/OldService.php");
        let source_content = "<?php\nnamespace App\\Services;\nclass OldService {}";
        let caller_files = vec![(
            PathBuf::from("app/Controllers/OrderController.php"),
            "<?php\nuse App\\Services\\OldService;\nclass OrderController {}".to_string(),
        )];

        let plan = plan_move_class(
            "OldService",
            &source_path,
            &target_path,
            "App\\Services",
            "App\\Billing",
            source_content,
            &caller_files,
        );

        let modal = cx.new(|cx| RefactorPreviewModal {
            plan,
            selected_file_index: 0,
            focus_handle: cx.focus_handle(),
            workspace: gpui::WeakEntity::new_invalid(),
            is_applying: false,
            status_message: None,
        });

        modal.update(cx, |this, cx| {
            assert_eq!(this.plan.file_diffs.len(), 2);
            assert!(this.plan.file_diffs[0].is_included);
            assert!(this.plan.file_diffs[1].is_included);

            // Toggle inclusion of first file
            this.toggle_file_inclusion(0, cx);
            assert!(!this.plan.file_diffs[0].is_included);

            // Select None
            this.set_all_files_included(false, cx);
            assert!(!this.plan.file_diffs[0].is_included);
            assert!(!this.plan.file_diffs[1].is_included);

            // Select All
            this.set_all_files_included(true, cx);
            assert!(this.plan.file_diffs[0].is_included);
            assert!(this.plan.file_diffs[1].is_included);

            // Select file
            this.select_file(1, cx);
            assert_eq!(this.selected_file_index, 1);
        });
    }

    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use std::sync::Arc;

    const OLD_SERVICE: &str = "<?php\nnamespace App\\Services;\nclass OldService {}\n";
    const ORDER_CONTROLLER: &str =
        "<?php\nuse App\\Services\\OldService;\nclass OrderController {}\n";

    fn move_plan(root: &str) -> RefactorPlan {
        plan_move_class(
            "OldService",
            &PathBuf::from(format!("{root}/app/Services/OldService.php")),
            &PathBuf::from(format!("{root}/app/Billing/OldService.php")),
            "App\\Services",
            "App\\Billing",
            OLD_SERVICE,
            &[(
                PathBuf::from(format!("{root}/app/Controllers/OrderController.php")),
                ORDER_CONTROLLER.to_string(),
            )],
        )
    }

    async fn project_fs(cx: &TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({
                "app": {
                    "Services": { "OldService.php": OLD_SERVICE },
                    "Controllers": { "OrderController.php": ORDER_CONTROLLER },
                }
            }),
        )
        .await;
        fs
    }

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/project")]
    }

    #[gpui::test]
    async fn test_move_writes_then_removes_source_and_respects_unticked(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        let mut plan = move_plan("/project");
        assert_eq!(plan.file_diffs.len(), 2);
        plan.file_diffs[1].is_included = false;

        let affected = plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap();
        assert_eq!(
            affected,
            vec![
                PathBuf::from("/project/app/Billing/OldService.php"),
                PathBuf::from("/project/app/Services/OldService.php")
            ]
        );
        let target = fs
            .load(Path::new("/project/app/Billing/OldService.php"))
            .await
            .unwrap();
        assert!(target.contains("namespace App\\Billing;"));
        assert!(
            !fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
        let caller = fs
            .load(Path::new("/project/app/Controllers/OrderController.php"))
            .await
            .unwrap();
        assert_eq!(caller, ORDER_CONTROLLER, "an unticked row is not written");
    }

    #[gpui::test]
    async fn test_unticked_target_keeps_the_source(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        let mut plan = move_plan("/project");
        plan.file_diffs[0].is_included = false;

        plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap();
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
        assert!(
            !fs.is_file(Path::new("/project/app/Billing/OldService.php"))
                .await
        );
    }

    #[gpui::test]
    async fn test_unwritable_target_keeps_the_source(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        // `app/Billing` is a file, so the target cannot be created.
        fs.insert_file("/project/app/Billing", b"x".to_vec()).await;
        let plan = move_plan("/project");

        assert!(plan.apply_to_disk(fs.as_ref(), &roots()).await.is_err());
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
    }

    #[gpui::test]
    async fn test_failed_later_write_keeps_the_source(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        fs.insert_file("/project/blocked", b"x".to_vec()).await;
        let mut plan = move_plan("/project");
        plan.file_diffs[1].file_path = PathBuf::from("/project/blocked/OrderController.php");

        assert!(plan.apply_to_disk(fs.as_ref(), &roots()).await.is_err());
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await,
            "the old file stays when any write failed"
        );
    }

    #[gpui::test]
    async fn test_source_changed_since_preview_is_not_removed(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        let plan = move_plan("/project");
        fs.insert_file(
            "/project/app/Services/OldService.php",
            b"<?php // edited since".to_vec(),
        )
        .await;

        let error = plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap_err();
        assert!(
            error.to_string().contains("changed since the preview"),
            "{error:#}"
        );
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
        assert!(
            !fs.is_file(Path::new("/project/app/Billing/OldService.php"))
                .await
        );
    }

    #[gpui::test]
    async fn test_move_onto_an_existing_target_is_refused(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        fs.insert_tree(
            "/project/app/Billing",
            json!({ "OldService.php": "<?php // somebody else's class" }),
        )
        .await;
        let plan = move_plan("/project");

        let error = plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
        assert_eq!(
            fs.load(Path::new("/project/app/Billing/OldService.php"))
                .await
                .unwrap(),
            "<?php // somebody else's class"
        );
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
        assert_eq!(
            fs.load(Path::new("/project/app/Controllers/OrderController.php"))
                .await
                .unwrap(),
            ORDER_CONTROLLER,
            "nothing else was written either"
        );
    }

    #[gpui::test]
    async fn test_file_changed_after_the_preview_is_refused_and_untouched(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        let plan = move_plan("/project");
        let edited = "<?php // edited after the preview\nuse App\\Services\\OldService;";
        fs.insert_file(
            "/project/app/Controllers/OrderController.php",
            edited.as_bytes().to_vec(),
        )
        .await;

        let error = plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap_err();
        assert!(
            error.to_string().contains("changed since the preview"),
            "{error:#}"
        );
        assert_eq!(
            fs.load(Path::new("/project/app/Controllers/OrderController.php"))
                .await
                .unwrap(),
            edited
        );
        assert!(
            !fs.is_file(Path::new("/project/app/Billing/OldService.php"))
                .await,
            "the check runs before the first write"
        );
        assert!(
            fs.is_file(Path::new("/project/app/Services/OldService.php"))
                .await
        );
    }

    #[gpui::test]
    async fn test_paths_outside_the_project_or_through_symlinks_are_refused(
        cx: &mut TestAppContext,
    ) {
        let fs = project_fs(cx).await;
        fs.insert_tree("/outside", json!({})).await;
        fs.create_symlink(Path::new("/project/link"), PathBuf::from("/outside"))
            .await
            .unwrap();

        let one_file = |path: &str| RefactorPlan {
            kind: RefactorKind::ChangeSignature {
                function_name: "f".into(),
                file_path: PathBuf::from(path),
                reordered_indices: vec![],
                added_params: vec![],
            },
            description: String::new(),
            file_diffs: vec![FileDiff {
                file_path: PathBuf::from(path),
                original_content: String::new(),
                modified_content: "<?php // pwned".into(),
                replacements: vec![],
                is_included: true,
            }],
            can_apply_safely: true,
            warnings: vec![],
        };

        for path in [
            "/project/link/x.php",
            "/outside/x.php",
            "/project/../outside/x.php",
            "/project/app/../../outside/x.php",
            "relative.php",
        ] {
            let result = one_file(path).apply_to_disk(fs.as_ref(), &roots()).await;
            assert!(result.is_err(), "{path} must be refused");
        }
        assert!(!fs.is_file(Path::new("/outside/x.php")).await);
        // A file inside the project is fine.
        fs.insert_file("/project/app/ok.php", Vec::new()).await;
        one_file("/project/app/ok.php")
            .apply_to_disk(fs.as_ref(), &roots())
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn test_symlinked_source_is_not_followed_for_removal(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        fs.insert_tree("/outside", json!({ "Secret.php": OLD_SERVICE }))
            .await;
        fs.create_symlink(Path::new("/project/linked"), PathBuf::from("/outside"))
            .await
            .unwrap();
        let plan = plan_move_class(
            "OldService",
            Path::new("/project/linked/Secret.php"),
            Path::new("/project/app/Billing/OldService.php"),
            "App\\Services",
            "App\\Billing",
            OLD_SERVICE,
            &[],
        );
        assert!(plan.apply_to_disk(fs.as_ref(), &roots()).await.is_err());
        assert!(fs.is_file(Path::new("/outside/Secret.php")).await);
    }

    #[gpui::test]
    async fn test_safe_delete_removes_only_the_symbol(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let content = "<?php\nclass Dead {\n    function a() { return '}'; }\n}\n\nclass Live {}\n";
        fs.insert_tree("/project", json!({ "lib.php": content }))
            .await;

        let plan = plan_safe_delete("Dead", Path::new("/project/lib.php"), content, &[]);
        assert!(plan.can_apply_safely);
        plan.apply_to_disk(fs.as_ref(), &roots()).await.unwrap();
        let after = fs.load(Path::new("/project/lib.php")).await.unwrap();
        assert_eq!(after, "<?php\n\nclass Live {}\n");

        // A SafeDelete plan with no rows (the old whole-file case) touches nothing.
        let empty = RefactorPlan {
            kind: RefactorKind::SafeDelete {
                symbol_name: "Live".into(),
                file_path: PathBuf::from("/project/lib.php"),
            },
            description: String::new(),
            file_diffs: vec![],
            can_apply_safely: true,
            warnings: vec![],
        };
        empty.apply_to_disk(fs.as_ref(), &roots()).await.unwrap();
        assert!(
            fs.is_file(Path::new("/project/lib.php")).await,
            "the file is never removed"
        );
    }

    #[test]
    fn test_safe_delete_without_definition_warns() {
        let plan = plan_safe_delete("Nope", Path::new("/p/a.php"), "<?php class Other {}", &[]);
        assert!(!plan.can_apply_safely);
        assert!(plan.file_diffs.is_empty());
        assert!(plan.warnings[0].contains("Could not find the definition"));
    }

    #[test]
    fn test_scan_balanced_skips_strings_and_nesting() {
        let text = "f(a, g(b, c), 'x)', \"y,\", [1, 2]) tail";
        let open = text.find('(').unwrap() + 1;
        let (close, pieces) = scan_balanced(text, open).unwrap();
        assert_eq!(&text[close..], ") tail");
        assert_eq!(
            pieces_of(text, &pieces),
            vec!["a", "g(b, c)", "'x)'", "\"y,\"", "[1, 2]"]
        );
        assert!(scan_balanced("f(a, g(b", 2).is_none());
    }

    fn change(content: &str, callers: &str) -> RefactorPlan {
        plan_change_signature(
            "calc",
            Path::new("/p/def.php"),
            content,
            &[1, 0],
            &[],
            &[(PathBuf::from("/p/use.php"), callers.to_string())],
        )
    }

    #[test]
    fn test_change_signature_handles_nested_calls_and_defaults() {
        let def = "<?php\nfunction calc($a, $b = array(1, 2)) { return 1; }\n";
        let plan = change(def, "<?php $x = calc(round($v, 2), 5);\n$y = calc(1, 2);\n");
        let applied = plan.apply();
        assert!(applied[Path::new("/p/def.php")].contains("function calc($b = array(1, 2), $a)"));
        let caller = &applied[Path::new("/p/use.php")];
        assert_eq!(
            caller,
            "<?php $x = calc(5, round($v, 2));\n$y = calc(2, 1);\n"
        );
        assert!(plan.can_apply_safely);
    }

    #[test]
    fn test_change_signature_rewrites_every_call_even_when_lengths_change() {
        let def = "<?php\nfunction calc($a, $b) {}\n";
        let plan = plan_change_signature(
            "calc",
            Path::new("/p/def.php"),
            def,
            &[0],
            &[],
            &[(
                PathBuf::from("/p/use.php"),
                "<?php calc(1, 22222); calc(3, 4); calc(5, 6);".to_string(),
            )],
        );
        assert_eq!(
            plan.apply()[Path::new("/p/use.php")],
            "<?php calc(1); calc(3); calc(5);"
        );
    }

    #[test]
    fn test_change_signature_leaves_method_calls_and_says_so() {
        let def = "<?php\nfunction calc($a, $b) {}\n";
        let plan = change(
            def,
            "<?php $this->calc(1, 2); Foo::calc(3, 4); Other\\calc(5, 6); new calc(7, 8); calc(9, 10);",
        );
        let caller = &plan.apply()[Path::new("/p/use.php")];
        assert_eq!(
            caller,
            "<?php $this->calc(1, 2); Foo::calc(3, 4); Other\\calc(5, 6); new calc(7, 8); calc(10, 9);"
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("3 method-style or other-namespace call(s)")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn test_change_signature_refuses_what_it_cannot_parse() {
        let def = "<?php\nfunction calc($a, $b) {}\n";
        let plan = change(def, "<?php calc(1, 2;");
        assert!(!plan.can_apply_safely);
        assert_eq!(plan.file_diffs.len(), 1, "only the definition");
        let missing = change("<?php // nothing here", "<?php calc(1, 2);");
        assert!(!missing.can_apply_safely);
        assert!(missing.warnings[0].contains("Could not read the signature"));
        assert!(missing.file_diffs.is_empty());
    }

    #[test]
    fn test_change_signature_rewrites_calls_in_the_defining_file() {
        let def = "<?php\nfunction &calc($a, $b) { return calc($b, $a - 1); }\n$z = calc(1, 2);\n";
        let plan = change(def, "<?php calc(3, 4);");
        let applied = plan.apply();
        assert_eq!(
            applied[Path::new("/p/def.php")],
            "<?php\nfunction &calc($b, $a) { return calc($a - 1, $b); }\n$z = calc(2, 1);\n"
        );
        assert_eq!(applied[Path::new("/p/use.php")], "<?php calc(4, 3);");
        assert!(plan.can_apply_safely);
    }

    #[test]
    fn test_change_signature_refuses_bad_indices_and_flags_missing_defaults() {
        let def = "<?php\nfunction calc($a, $b) {}\n";
        for order in [&[0, 2][..], &[1, 1][..]] {
            let plan = plan_change_signature("calc", Path::new("/p/def.php"), def, order, &[], &[]);
            assert!(!plan.can_apply_safely);
            assert!(plan.file_diffs.is_empty(), "{order:?}");
        }
        let added = [SignatureParam {
            name: "limit".to_string(),
            type_hint: Some("int".to_string()),
            default_value: None,
        }];
        let plan = plan_change_signature(
            "calc",
            Path::new("/p/def.php"),
            def,
            &[0, 1],
            &added,
            &[(
                PathBuf::from("/p/use.php"),
                "<?php calc(1, 2); calc(1);".to_string(),
            )],
        );
        assert!(!plan.can_apply_safely);
        assert!(plan.warnings.iter().any(|w| w.contains("no default value")));
        // calc(1) omits $b, so the new argument would land in $b's slot: left alone.
        assert_eq!(
            plan.apply()[Path::new("/p/use.php")],
            "<?php calc(1, 2, null); calc(1);"
        );
    }

    #[gpui::test]
    async fn test_modal_apply_respects_unticked_rows_and_unsaved_editors(cx: &mut TestAppContext) {
        use workspace::MultiWorkspace;
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = project_fs(cx).await;
        let project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);

        let mut plan = move_plan("/project");
        plan.file_diffs[1].is_included = false;
        let modal = cx.new(|cx| RefactorPreviewModal {
            plan: plan.clone(),
            selected_file_index: 0,
            focus_handle: cx.focus_handle(),
            workspace: workspace.downgrade(),
            is_applying: false,
            status_message: None,
        });

        // An editor with unsaved changes on a ticked file blocks the apply.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/project/app/Services/OldService.php", cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "// local edit\n")], None, cx)
        });
        modal.update_in(cx, |modal, window, cx| modal.apply(window, cx));
        cx.run_until_parked();
        modal.read_with(cx, |modal, _| {
            assert!(
                modal
                    .status_message
                    .as_deref()
                    .is_some_and(|m| m.contains("unsaved")),
                "{:?}",
                modal.status_message
            );
            assert!(!modal.is_applying);
        });
        assert!(
            !fs.is_file(Path::new("/project/app/Billing/OldService.php"))
                .await
        );

        // Once saved, the apply runs off the main thread and skips the unticked row.
        buffer.update(cx, |buffer, cx| buffer.undo(cx));
        modal.update_in(cx, |modal, window, cx| modal.apply(window, cx));
        cx.run_until_parked();
        assert!(
            fs.is_file(Path::new("/project/app/Billing/OldService.php"))
                .await
        );
        assert_eq!(
            fs.load(Path::new("/project/app/Controllers/OrderController.php"))
                .await
                .unwrap(),
            ORDER_CONTROLLER
        );
    }

    /// `user.php` -> `User.php`: on a case-insensitive disk (default APFS) both names are one
    /// file, and writing the new one then removing the old one used to delete the result. The
    /// result must survive on either kind of disk.
    #[gpui::test]
    async fn test_case_only_move_keeps_the_file(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("user.php");
        let target = root.join("User.php");
        let content = "<?php\nnamespace App\\Models;\nclass User {}\n";
        std::fs::write(&source, content).unwrap();

        let plan = plan_move_class(
            "User",
            &source,
            &target,
            "App\\Models",
            "App\\Entities",
            content,
            &[],
        );
        let fs = fs::RealFs::new(None, cx.executor());
        plan.apply_to_disk(fs.as_ref(), std::slice::from_ref(&root))
            .await
            .unwrap();

        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains("namespace App\\Entities;"), "{written}");
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        // Case-sensitive disk: the old name was removed. Case-insensitive: it was the same
        // file and stays. Either way exactly one file is left, with the new content.
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[gpui::test]
    async fn test_is_same_file(cx: &mut TestAppContext) {
        let fs = project_fs(cx).await;
        let (a, b) = (
            Path::new("/project/app/Services/OldService.php"),
            Path::new("/project/app/Controllers/OrderController.php"),
        );
        assert!(is_same_file(fs.as_ref(), a, a).await.unwrap());
        assert!(!is_same_file(fs.as_ref(), a, b).await.unwrap());
        assert!(
            !is_same_file(fs.as_ref(), a, Path::new("/project/missing.php"))
                .await
                .unwrap()
        );
    }
}
