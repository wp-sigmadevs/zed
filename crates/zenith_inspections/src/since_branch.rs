//! "New since branch" (plan 21): which lines of a file differ from the merge
//! base with a base branch, so inspections can hide problems that were already
//! there. Git does the work; this module only runs it and reads hunk headers.
//!
//! Running git in a project folder runs whatever its `.git/config` says
//! (`core.fsmonitor`, hooks, `protocol.ext`), so the B2 caller (the "new since
//! branch" filter) MUST gate on `zenith_php_tools::worktree_trusted` first, as
//! `changed_php_files`/`collect_*` in `zenith_inspections.rs` do. The flags in
//! `GIT_HARDENING` are defence in depth, not a substitute for that check.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// Lines of a file that count as new.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewLines {
    /// Untracked file, or git could not say: every line is new.
    All,
    /// 1-based, half-open line ranges added or changed since the merge base.
    Ranges(Vec<Range<u32>>),
}

/// New-side ranges of `git diff -U0` hunk headers (`@@ -a,b +c,d @@`). `d`
/// omitted means 1; `d` of 0 (a pure deletion) adds no lines. Body lines never
/// start with `@@` at -U0 (they start with `+`, `-` or `\`), so the
/// `\ No newline at end of file` marker needs no handling.
pub fn parse_new_lines(diff: &str) -> Vec<Range<u32>> {
    diff.lines()
        .filter_map(|line| {
            let header = line.strip_prefix("@@ ")?;
            let new_side = header.split(' ').find_map(|part| part.strip_prefix('+'))?;
            let (start, count) = match new_side.split_once(',') {
                Some((start, count)) => (start, count),
                None => (new_side, "1"),
            };
            let (start, count): (u32, u32) = (start.parse().ok()?, count.parse().ok()?);
            (count > 0).then(|| start..start.saturating_add(count))
        })
        .collect()
}

/// Global git options for every call on a project folder: no fsmonitor command,
/// no `ext::` transports, no hooks, no index lock, no pager, paths taken
/// literally (a file named `[slug].php` is not a glob).
pub(crate) const GIT_HARDENING: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.hooksPath=/dev/null",
    "--no-optional-locks",
    "--no-pager",
    "--literal-pathspecs",
];

async fn git(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    util::command::new_command("git")
        .args(GIT_HARDENING)
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Could not run git: {error}"))
}

fn git_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("git failed: {}", stderr.trim())
}

/// A base starting with `-` would be read as an option by older git.
fn check_base(base: &str) -> Result<(), String> {
    if base.is_empty() || base.starts_with('-') {
        return Err(format!("Invalid base branch {base:?}."));
    }
    Ok(())
}

/// New lines of `file` against the merge base with `base`, plus a message when
/// git failed and every line is reported as new instead (the caller shows it
/// once). Working-tree edits count, so unsaved-to-git changes are new too.
pub async fn new_lines(root: &Path, base: &str, file: &Path) -> (NewLines, Option<String>) {
    match new_lines_inner(root, base, file).await {
        Ok(lines) => (lines, None),
        Err(message) => (NewLines::All, Some(message)),
    }
}

async fn new_lines_inner(root: &Path, base: &str, file: &Path) -> Result<NewLines, String> {
    check_base(base)?;
    let file = file.to_string_lossy();
    let tracked = git(root, &["ls-files", "--error-unmatch", "--", &file]).await?;
    match tracked.status.code() {
        Some(0) => {}
        // `--error-unmatch` exits 1 when git does not know the path.
        Some(1) => return Ok(NewLines::All),
        _ => return Err(git_error(&tracked)),
    }
    let diff = git(
        root,
        &[
            "diff",
            "-U0",
            "--text",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--merge-base",
            "--end-of-options",
            base,
            "--",
            &file,
        ],
    )
    .await?;
    if !diff.status.success() {
        return Err(git_error(&diff));
    }
    Ok(NewLines::Ranges(parse_new_lines(&String::from_utf8_lossy(
        &diff.stdout,
    ))))
}

/// PHP files under `root` that changed since the merge base with `base`
/// (deleted ones skipped, renames listed by their new name) or are untracked
/// and not ignored, as sorted paths in `root`.
pub async fn branch_php_files(root: &Path, base: &str) -> Result<Vec<PathBuf>, String> {
    check_base(base)?;
    let changed = git(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=d",
            "--relative",
            "--merge-base",
            "--end-of-options",
            base,
        ],
    )
    .await?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut files = std::collections::BTreeSet::new();
    for output in [&changed, &untracked] {
        if !output.status.success() {
            return Err(git_error(output));
        }
        files.extend(
            String::from_utf8_lossy(&output.stdout)
                .split('\0')
                .filter(|path| path.ends_with(".php"))
                .map(|path| root.join(path)),
        );
    }
    Ok(files.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_new_lines_from_hunk_headers() {
        let diff = "\
diff --git a/f.php b/f.php
index de98044..a7bc997 100644
--- a/f.php
+++ b/f.php
@@ -2 +2 @@ function a()
-b
+B
@@ -5,2 +6,0 @@ c
-x
-y
@@ -9,0 +10,3 @@
+one
+two
+three
@@ -20,2 +25 @@
-gone
-gone
+kept
\\ No newline at end of file
";
        // Modify, delete-only (skipped), add, and a header with no count (1).
        assert_eq!(parse_new_lines(diff), [2..3, 10..13, 25..26]);
        assert_eq!(parse_new_lines(""), Vec::<Range<u32>>::new());
        assert_eq!(parse_new_lines("@@ -0,0 +1,2 @@\n+a\n+b\n"), [1..3]);
    }

    #[allow(clippy::disallowed_methods, reason = "blocking git in a test helper")]
    fn run_git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A repo on branch `feat` forked from `main`, which holds `f.php` (a, b, c).
    fn repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path();
        run_git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("f.php"), "a\nb\nc\n").expect("write");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-qm", "base"]);
        run_git(root, &["checkout", "-qb", "feat"]);
        repo
    }

    fn lines(root: &Path, base: &str, file: &str) -> (NewLines, Option<String>) {
        futures::executor::block_on(new_lines(root, base, &root.join(file)))
    }

    #[test]
    fn test_new_lines_after_a_branch_commit_and_an_uncommitted_edit() {
        let repo = repo();
        let root = repo.path();
        assert_eq!(
            lines(root, "main", "f.php"),
            (NewLines::Ranges(vec![]), None)
        );
        fs::write(root.join("f.php"), "a\nB\nc\nd\n").expect("write");
        run_git(root, &["commit", "-qam", "edit"]);
        assert_eq!(
            lines(root, "main", "f.php"),
            (NewLines::Ranges(vec![2..3, 4..5]), None)
        );
        // An edit not yet committed counts too.
        fs::write(root.join("f.php"), "a\nB\nc\nd\ne\n").expect("write");
        assert_eq!(
            lines(root, "main", "f.php"),
            (NewLines::Ranges(vec![2..3, 4..6]), None)
        );
    }

    #[test]
    fn test_new_lines_ignore_changes_made_on_the_base_branch() {
        let repo = repo();
        let root = repo.path();
        run_git(root, &["checkout", "-q", "main"]);
        fs::write(root.join("f.php"), "z\nb\nc\n").expect("write");
        run_git(root, &["commit", "-qam", "main moves on"]);
        run_git(root, &["checkout", "-q", "feat"]);
        // Merge base is the fork point, so main's later edit is not ours.
        assert_eq!(
            lines(root, "main", "f.php"),
            (NewLines::Ranges(vec![]), None)
        );
    }

    #[test]
    fn test_untracked_file_is_all_new() {
        let repo = repo();
        fs::write(repo.path().join("u.php"), "x\n").expect("write");
        assert_eq!(lines(repo.path(), "main", "u.php"), (NewLines::All, None));
    }

    #[test]
    fn test_file_added_on_the_branch_is_all_new() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join("n.php"), "x\ny\n").expect("write");
        run_git(root, &["add", "n.php"]);
        // Staged only, then committed: both are tracked, both show every line.
        assert_eq!(
            lines(root, "main", "n.php"),
            (NewLines::Ranges(vec![1..3]), None)
        );
        run_git(root, &["commit", "-qm", "add"]);
        assert_eq!(
            lines(root, "main", "n.php"),
            (NewLines::Ranges(vec![1..3]), None)
        );
    }

    #[test]
    fn test_bad_bases_fall_back_to_all_with_a_message() {
        let repo = repo();
        let root = repo.path();
        let (all, message) = lines(root, "-x", "f.php");
        assert_eq!(all, NewLines::All);
        assert!(message.is_some_and(|message| message.contains("Invalid base")));
        let (all, message) = lines(root, "no-such-branch", "f.php");
        assert_eq!(all, NewLines::All);
        assert!(message.is_some_and(|message| message.contains("git failed")));
        // Not a repository at all.
        let plain = tempfile::tempdir().expect("tempdir");
        fs::write(plain.path().join("f.php"), "x").expect("write");
        let (all, message) = lines(plain.path(), "main", "f.php");
        assert_eq!(all, NewLines::All);
        assert!(message.is_some());
    }

    #[test]
    fn test_branch_php_files() {
        let repo = repo();
        let root = repo.path();
        fs::create_dir(root.join("sub")).expect("dir");
        fs::write(root.join("sub/n.php"), "x\n").expect("write");
        fs::write(root.join("sub/readme.md"), "x\n").expect("write");
        fs::write(root.join("f.php"), "a\nB\nc\n").expect("write");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-qm", "work"]);
        let files = futures::executor::block_on(branch_php_files(root, "main"));
        assert_eq!(files, Ok(vec![root.join("f.php"), root.join("sub/n.php")]));
        // Deleted files are skipped.
        run_git(root, &["rm", "-q", "f.php"]);
        let files = futures::executor::block_on(branch_php_files(root, "main"));
        assert_eq!(files, Ok(vec![root.join("sub/n.php")]));
        assert!(futures::executor::block_on(branch_php_files(root, "-x")).is_err());
        // Inside a subfolder only that folder's files are listed.
        let files = futures::executor::block_on(branch_php_files(&root.join("sub"), "main"));
        assert_eq!(files, Ok(vec![root.join("sub/n.php")]));
    }

    #[test]
    fn test_branch_php_files_include_untracked_but_not_ignored() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join(".gitignore"), "ignored.php\n").expect("write");
        run_git(root, &["add", ".gitignore"]);
        run_git(root, &["commit", "-qm", "ignore"]);
        fs::create_dir(root.join("sub")).expect("dir");
        fs::write(root.join("new.php"), "x\n").expect("write");
        fs::write(root.join("sub/deep.php"), "x\n").expect("write");
        fs::write(root.join("ignored.php"), "x\n").expect("write");
        fs::write(root.join("notes.txt"), "x\n").expect("write");
        let files = futures::executor::block_on(branch_php_files(root, "main"));
        assert_eq!(
            files,
            Ok(vec![root.join("new.php"), root.join("sub/deep.php")])
        );
        // Same folder rule as the diff list: only files under `sub`.
        let files = futures::executor::block_on(branch_php_files(&root.join("sub"), "main"));
        assert_eq!(files, Ok(vec![root.join("sub/deep.php")]));
    }

    #[test]
    fn test_glob_looking_file_names_are_literal() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join("s.php"), "1\n2\n").expect("write");
        run_git(root, &["add", "s.php"]);
        run_git(root, &["commit", "-qm", "s"]);
        // Untracked `[slug].php` must not be mistaken for tracked `s.php`.
        fs::write(root.join("[slug].php"), "x\n").expect("write");
        assert_eq!(lines(root, "main", "[slug].php"), (NewLines::All, None));
        // Tracked, both edited: each file reports only its own lines.
        run_git(root, &["add", "[slug].php"]);
        fs::write(root.join("s.php"), "1\n2\n3\n").expect("write");
        assert_eq!(
            lines(root, "main", "[slug].php"),
            (NewLines::Ranges(vec![1..2]), None)
        );
    }

    #[test]
    fn test_file_with_a_nul_byte_still_gets_hunks() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join("bin.php"), b"a\0b\nc\n").expect("write");
        run_git(root, &["add", "bin.php"]);
        run_git(root, &["commit", "-qm", "bin"]);
        fs::write(root.join("bin.php"), b"a\0b\nC\nd\n").expect("write");
        assert_eq!(
            lines(root, "main", "bin.php"),
            (NewLines::Ranges(vec![1..4]), None)
        );
    }

    #[test]
    fn test_repo_config_cannot_run_a_command_through_fsmonitor() {
        let repo = repo();
        let root = repo.path();
        let marker = root.join("fsmonitor-ran");
        // Git appends its arguments, so `#` turns them into a shell comment.
        let hook = format!("touch {} #", marker.display());
        run_git(root, &["config", "core.fsmonitor", &hook]);
        fs::write(root.join("f.php"), "a\nB\nc\n").expect("write");
        run_git(root, &["add", "f.php"]);
        // Setting up the repo above may itself have run the hook.
        if marker.exists() {
            fs::remove_file(&marker).expect("remove marker");
        }
        let (found, message) = lines(root, "main", "f.php");
        assert_eq!((found, message), (NewLines::Ranges(vec![2..3]), None));
        assert!(futures::executor::block_on(branch_php_files(root, "main")).is_ok());
        assert!(!marker.exists(), "git ran the repo's fsmonitor command");
    }
}
