use super::*;
use gpui::TestAppContext;
use std::{fs, os::unix::fs::PermissionsExt as _};

const PHPCS: &str = include_str!("../fixtures/phpcs-wordpress.json");
const PHPSTAN: &str = include_str!("../fixtures/phpstan.json");

// Real PHPStan 2.1.40 runs over src/a.php (two undefined variables, one in
// phpstan-baseline.neon) with paths rewritten to /project.
const PHPSTAN_BASELINE_SINGLE: &str = include_str!("../fixtures/phpstan-baseline-single-file.json");
const PHPSTAN_BASELINE_WHOLE: &str =
    include_str!("../fixtures/phpstan-baseline-whole-project.json");

#[test]
fn test_phpcs_report_after_php_deprecation_notices() {
    let problems = parse_phpcs(PHPCS).unwrap();
    assert_eq!(problems.len(), 19);
    assert_eq!(
        problems[3],
        Problem {
            path: PathBuf::from("/project/a.php"),
            line: 2,
            column: 19,
            severity: Severity::Error,
            message: "Expected 1 space before opening brace; found 0".into(),
            rule: Some(
                "Generic.Functions.OpeningFunctionBraceKernighanRitchie.SpaceBeforeBrace".into()
            ),
        }
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.severity == Severity::Warning)
    );
}

#[test]
fn test_phpstan_report_with_trait_context_and_tip() {
    let problems = parse_phpstan(PHPSTAN).unwrap();
    let summary: Vec<_> = problems
        .iter()
        .map(|problem| {
            (
                problem.path.to_str().unwrap(),
                problem.line,
                problem.rule.as_deref().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("/project/a.php", 2, "missingType.return"),
            ("/project/a.php", 2, "missingType.parameter"),
            ("/project/a.php", 7, "function.notFound"),
            ("/project/a.php", 5, "return.type"),
        ]
    );
    assert!(problems[2].message.ends_with("discovering-symbols"));
}

#[test]
fn test_phpstan_baseline_hides_ignored_findings() {
    // The baseline entry for `$undefined_old` is gone from the report; only the
    // finding outside the baseline reaches the editor.
    let problems = parse_phpstan(PHPSTAN_BASELINE_SINGLE).unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].message, "Undefined variable: $undefined_new");
    assert_eq!(problems[0].line, 6);
    // A fully baselined file is an empty report.
    let clean = r#"{"totals":{"errors":0,"file_errors":0},"files":{},"errors":[]}"#;
    assert_eq!(parse_phpstan(clean).unwrap(), Vec::new());
}

#[test]
fn test_phpstan_baseline_unmatched_patterns() {
    // Whole-project runs put a path-scoped stale entry in the file's messages
    // (`ignore.unmatched`, no line) and a global one in top-level `errors`.
    // Neither becomes a problem; single-file runs report neither.
    let problems = parse_phpstan(PHPSTAN_BASELINE_WHOLE).unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].rule.as_deref(), Some("variable.undefined"));
}

#[test]
fn test_clean_phpstan_runs() {
    let note = "Note: Using configuration file /p/phpstan.neon.\n";
    for files in ["{}", "[]"] {
        let output = format!(
            r#"{note}{{"totals":{{"errors":0,"file_errors":0}},"files":{files},"errors":[]}}"#
        );
        assert_eq!(parse_phpstan(&output).unwrap(), Vec::new());
    }
    assert!(parse_phpstan("Fatal error: out of memory").is_err());
}

#[test]
fn test_arguments_follow_the_project_config() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("a.php");
    let phpcs = |kind| {
        Analyzer::Phpcs
            .args(root.path(), kind, &file)
            .unwrap()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect::<Vec<_>>()
    };
    let wordpress = phpcs(ProjectKind::WordPress);
    assert!(wordpress.contains(&"--standard=WordPress".to_string()));
    assert_eq!(wordpress.last(), Some(&file.to_str().unwrap().to_string()));
    assert!(phpcs(ProjectKind::Laravel).contains(&"--standard=PSR12".to_string()));
    assert_eq!(
        Analyzer::Phpstan.args(root.path(), ProjectKind::Php, &file),
        None
    );

    fs::write(root.path().join("phpcs.xml.dist"), "").unwrap();
    fs::write(root.path().join("phpstan.neon"), "").unwrap();
    assert!(
        !phpcs(ProjectKind::WordPress)
            .iter()
            .any(|arg| arg.starts_with("--standard"))
    );
    assert!(
        Analyzer::Phpstan
            .args(root.path(), ProjectKind::Php, &file)
            .is_some()
    );
}

#[test]
fn test_php_scripts_run_through_php() {
    let dir = tempfile::tempdir().unwrap();
    let php = Path::new("/herd/php");
    let script = dir.path().join("phpcs");
    fs::write(&script, "#!/usr/bin/env php\n<?php\n").unwrap();
    assert_eq!(
        launch(php, &script),
        (php.to_path_buf(), vec![script.clone().into()])
    );
    let native = dir.path().join("native");
    fs::write(&native, "#!/bin/sh\n").unwrap();
    assert_eq!(launch(php, &native), (native.clone(), Vec::new()));
}

#[gpui::test]
async fn test_run_reads_the_report_from_the_project_tool(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("phpcs");
    // Prints the real report and exits 2, as phpcs does when it finds errors.
    fs::write(
        &tool,
        format!("#!/bin/sh\ncat <<'REPORT'\n{PHPCS}\nREPORT\nexit 2\n"),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let file = root.path().join("a.php");
    cx.executor().allow_parking();
    let problems = cx
        .background_spawn(async move {
            run(
                Analyzer::Phpcs,
                root.path(),
                &file,
                Path::new("/no/php"),
                None,
                &[],
            )
            .await
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(problems.len(), 19);
}

#[gpui::test]
async fn test_each_analyzer_replaces_only_its_own_diagnostics(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
    });
    let fs = project::FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({ "a.php": "<?php\nfunction foo( $a ){\n  return $a+1;\n}\n" }),
    )
    .await;
    let project = Project::test(fs, [Path::new("/project")], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer("/project/a.php", cx)
        })
        .await
        .unwrap();
    let push = |analyzer: Analyzer, problems: Vec<Problem>, cx: &mut TestAppContext| {
        project.update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                push_diagnostics(
                    lsp_store,
                    analyzer,
                    &PathBuf::from("/project/a.php"),
                    &problems,
                    cx,
                )
                .unwrap();
            })
        });
    };
    let problem = |line, column, message: &str| Problem {
        path: PathBuf::from("/project/a.php"),
        line,
        column,
        severity: Severity::Error,
        message: message.into(),
        rule: None,
    };
    push(Analyzer::Phpcs, vec![problem(2, 19, "brace")], cx);
    push(Analyzer::Phpstan, vec![problem(2, 1, "no return type")], cx);
    push(Analyzer::Phpcs, vec![problem(3, 3, "indent")], cx);
    cx.run_until_parked();

    let diagnostics = buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut diagnostics: Vec<_> = snapshot
            .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
            .map(|entry| {
                (
                    entry.range.start,
                    entry.diagnostic.source.clone().unwrap(),
                    entry.diagnostic.message.to_string(),
                )
            })
            .collect();
        diagnostics.sort();
        diagnostics
    });
    assert_eq!(
        diagnostics,
        [
            (
                text::Point::new(1, 0),
                "phpstan".into(),
                "no return type".into()
            ),
            (text::Point::new(2, 2), "phpcs".into(), "indent".into()),
        ]
    );
}

#[gpui::test]
fn test_older_run_is_stale_after_a_newer_save(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let first = next_generation(Path::new("/a.php"), cx);
        let second = next_generation(Path::new("/a.php"), cx);
        let other = next_generation(Path::new("/b.php"), cx);
        assert_ne!(current_generation(Path::new("/a.php"), cx), first);
        assert_eq!(current_generation(Path::new("/a.php"), cx), second);
        assert_eq!(current_generation(Path::new("/b.php"), cx), other);
    });
}

#[test]
fn test_phpcs_tab_columns_become_character_columns() {
    // Two tabs then `$x`: PHPCS says column 9, the editor's third character.
    assert_eq!(char_column("\t\t$x  = 1;", 9, 4), 3);
    assert_eq!(char_column("\t\t$x  = 1;", 1, 4), 1);
    // Real phpcs: the alignment warning on `=` is column 13 by default, 7 with --tab-width=0.
    assert_eq!(char_column("\t\t$x  = 1;", 13, 4), 7);
    // A tab after two characters only reaches the next stop.
    assert_eq!(char_column("ab\tc", 5, 4), 4);
    assert_eq!(char_column("no tabs", 4, 4), 4);
    // Past the end of the line stays on the line.
    assert_eq!(char_column("\t", 99, 4), 2);

    let mut problems = vec![Problem {
        path: PathBuf::from("/p/a.php"),
        line: 2,
        column: 9,
        severity: Severity::Error,
        message: String::new(),
        rule: None,
    }];
    fix_tab_columns(&mut problems, "<?php\n\t\t$x = 1;\n", 4);
    assert_eq!(problems[0].column, 3);
}

#[test]
fn test_tab_width_comes_from_the_project_ruleset() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(ruleset_tab_width(root.path()), 4);
    fs::write(
        root.path().join("phpcs.xml"),
        r#"<ruleset><arg name="tab-width" value="2"/><rule ref="PSR12"/></ruleset>"#,
    )
    .unwrap();
    assert_eq!(ruleset_tab_width(root.path()), 2);
    assert_eq!(tab_width_in(r#"<arg name="tab-width" value="0"/>"#), None);
    assert_eq!(tab_width_in("<ruleset/>"), None);
    assert_eq!(char_column("\t$x", 3, 2), 2);
}

#[test]
fn test_changed_php_files_from_git_status() {
    let status =
        " M src/a.php\0?? new/b.php\0 D gone.php\0R  moved.php\0old.php\0A  readme.md\0AM c.php\0";
    assert_eq!(
        changed_php_files(Path::new("/p"), status),
        [
            PathBuf::from("/p/src/a.php"),
            PathBuf::from("/p/new/b.php"),
            PathBuf::from("/p/moved.php"),
            PathBuf::from("/p/c.php"),
        ]
    );
    assert_eq!(
        changed_php_files(Path::new("/p"), ""),
        Vec::<PathBuf>::new()
    );
}

#[gpui::test]
async fn test_run_limited_never_exceeds_the_limit(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let running = std::sync::Arc::new(AtomicUsize::new(0));
    let peak = std::sync::Arc::new(AtomicUsize::new(0));
    let finished = std::sync::Arc::new(AtomicUsize::new(0));
    let executor = cx.executor();
    let jobs: Vec<_> = (0..5)
        .map(|_| {
            let (running, peak, finished, executor) = (
                running.clone(),
                peak.clone(),
                finished.clone(),
                executor.clone(),
            );
            move || async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                executor.timer(std::time::Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                finished.fetch_add(1, Ordering::SeqCst);
            }
        })
        .collect();
    run_limited(jobs, 2).await;
    assert_eq!(finished.load(Ordering::SeqCst), 5);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

/// Saves a PHP file against a project whose `phpcs` is a script, with a stub `php`
/// on PATH, and checks the finding reaches the buffer as a diagnostic.
#[gpui::test]
async fn test_saving_a_php_file_shows_phpcs_findings(cx: &mut TestAppContext) {
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({})).await,
        (
            vec![(text::Point::new(1, 0), "Missing doc comment".to_string())],
            true
        )
    );
}

#[gpui::test]
async fn test_switched_off_analyzers_and_saving_show_nothing(cx: &mut TestAppContext) {
    // Switched off: its old findings are cleared.
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({ "phpcs": false })).await,
        (Vec::new(), false)
    );
    // Not run at all: the old findings stay.
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({ "run_on_save": false })).await,
        (vec![(text::Point::new(0, 0), "stale".to_string())], false)
    );
}

async fn saved_php_diagnostics(
    cx: &mut TestAppContext,
    inspections: serde_json::Value,
) -> (Vec<(text::Point, String)>, bool) {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().to_str().unwrap().to_string();
    let bin = root.path().join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let report = r#"{"totals":{"errors":1,"warnings":0,"fixable":0},"files":{"ROOT/a.php":{"errors":1,"warnings":0,"messages":[{"message":"Missing doc comment","source":"Squiz.Commenting.FunctionComment.Missing","severity":5,"fixable":false,"type":"ERROR","line":2,"column":1}]}}}"#
        .replace(
            "ROOT",
            fs::canonicalize(root.path()).unwrap().to_str().unwrap(),
        );
    let phpcs = bin.join("phpcs");
    // Leaves a marker, so a test can tell "not run" from "ran and found nothing".
    let marker = root.path().join("phpcs-ran");
    fs::write(
        &phpcs,
        format!(
            "#!/bin/sh\n: > '{}'\necho '{report}'\nexit 2\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&phpcs, fs::Permissions::from_mode(0o755)).unwrap();
    let path_dir = tempfile::tempdir().unwrap();
    let stub_php = path_dir.path().join("php");
    fs::write(&stub_php, "#!/bin/sh\necho 'PHP 8.3.0 (cli)'\n").unwrap();
    fs::set_permissions(&stub_php, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.path().join("a.php"), "<?php\nfunction foo() {}\n").unwrap();

    cx.update(|cx| {
        let mut settings = settings::SettingsStore::test(cx);
        settings
            .set_user_settings(
                &serde_json::json!({ "zenith": { "inspections": inspections } }).to_string(),
                cx,
            )
            .unwrap();
        cx.set_global(settings);
        cx.set_global(ToolEnvironment {
            php_candidates: vec![stub_php.clone()],
            path_dirs: Vec::new(),
        });
    });
    let fake = project::FakeFs::new(cx.executor());
    fake.insert_tree(
        &root_path,
        serde_json::json!({ "a.php": "<?php\nfunction foo() {}\n" }),
    )
    .await;
    let project = Project::test(fake, [Path::new(&root_path)], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(root.path().join("a.php"), cx)
        })
        .await
        .unwrap();
    // A finding from an earlier run, which this run must replace or clear.
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let stale = Problem {
                path: root.path().join("a.php"),
                line: 1,
                column: 1,
                severity: Severity::Error,
                message: "stale".into(),
                rule: None,
            };
            push_diagnostics(
                lsp_store,
                Analyzer::Phpcs,
                &root.path().join("a.php"),
                &[stale],
                cx,
            )
            .unwrap();
        })
    });
    cx.executor().allow_parking();
    cx.update(|cx| {
        on_buffer_event(
            &project,
            &buffer,
            &BufferEvent::Saved,
            gpui::WeakEntity::new_invalid(),
            cx,
        )
    });
    let read = |cx: &mut TestAppContext| {
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
                .map(|entry| (entry.range.start, entry.diagnostic.message.to_string()))
                .collect::<Vec<_>>()
        })
    };
    // The run spawns real processes; wait until it has finished.
    for _ in 0..250 {
        cx.run_until_parked();
        let idle = cx.update(|cx| {
            cx.try_global::<Running>()
                .is_none_or(|running| running.tasks.is_empty())
        });
        if idle {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    (read(cx), marker.exists())
}

fn settings_from(json: serde_json::Value) -> InspectionSettings {
    let content: SettingsContent =
        serde_json::from_value(serde_json::json!({ "zenith": { "inspections": json } })).unwrap();
    InspectionSettings::from_settings(&content)
}

#[test]
fn test_inspection_settings_defaults_and_overrides() {
    let defaults = InspectionSettings::from_settings(&SettingsContent::default());
    assert_eq!(
        defaults,
        InspectionSettings {
            phpcs: true,
            phpstan: true,
            run_on_save: true,
            max_parallel: 2
        }
    );
    assert_eq!(defaults.analyzers(), [Analyzer::Phpcs, Analyzer::Phpstan]);

    let custom = settings_from(
        serde_json::json!({ "phpcs": false, "run_on_save": false, "max_parallel": 5 }),
    );
    assert_eq!(custom.analyzers(), [Analyzer::Phpstan]);
    assert!(!custom.run_on_save);
    assert_eq!(custom.max_parallel, 5);

    // Zero would never start a job.
    assert_eq!(
        settings_from(serde_json::json!({ "max_parallel": 0 })).max_parallel,
        1
    );
    assert!(
        settings_from(serde_json::json!({ "phpcs": false, "phpstan": false }))
            .analyzers()
            .is_empty()
    );
}

#[test]
fn test_default_settings_file_matches_the_built_in_defaults() {
    let defaults: SettingsContent =
        settings::parse_json_with_comments(&settings::default_settings()).unwrap();
    assert_eq!(
        InspectionSettings::from_settings(&defaults),
        InspectionSettings::from_settings(&SettingsContent::default())
    );
    assert!(defaults.zenith.unwrap().inspections.is_some());
}

#[test]
fn test_phpcbf_exit_codes() {
    assert_eq!(fix_outcome(Some(0)).unwrap(), FixOutcome::Nothing);
    assert_eq!(fix_outcome(Some(1)).unwrap(), FixOutcome::Fixed);
    assert_eq!(fix_outcome(Some(2)).unwrap(), FixOutcome::FixedSome);
    assert!(fix_outcome(Some(3)).is_err());
    assert!(fix_outcome(None).is_err());
}

#[gpui::test]
async fn test_run_fix_rewrites_the_file_through_the_project_phpcbf(cx: &mut TestAppContext) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let phpcbf = bin.join("phpcbf");
    // Rewrites the file it is given (the last argument) and exits 1, as phpcbf does.
    fs::write(
        &phpcbf,
        "#!/bin/sh\nfor last; do :; done\nprintf 'fixed\\n' > \"$last\"\necho \"$@\" > \"$last.args\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&phpcbf, fs::Permissions::from_mode(0o755)).unwrap();
    let file = root.path().join("a.php");
    fs::write(&file, "broken\n").unwrap();
    cx.executor().allow_parking();
    let (root_path, file_path) = (root.path().to_path_buf(), file.clone());
    let outcome = cx
        .background_spawn(async move {
            run_fix(&root_path, &file_path, Path::new("/no/php"), None, &[]).await
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, FixOutcome::Fixed);
    assert_eq!(fs::read_to_string(&file).unwrap(), "fixed\n");
    let args = fs::read_to_string(root.path().join("a.php.args")).unwrap();
    assert!(args.contains("--standard=PSR12"), "{args}");
    let other = tempfile::tempdir().unwrap();
    let (path, file) = (other.path().to_path_buf(), file.clone());
    assert!(
        cx.background_spawn(async move {
            run_fix(&path, &file, Path::new("/no/php"), None, &[])
                .await
                .is_none()
        })
        .await
    );
}

#[test]
fn test_char_column_counts_utf16() {
    // PHPCS counts the emoji as one column; the editor's UTF-16 column counts two.
    assert_eq!(char_column("\u{1F600}x", 2, 4), 3);
    assert_eq!(char_column("\u{e9}x", 2, 4), 2);
}

#[test]
fn test_phpstan_top_level_errors() {
    let alone =
        r#"{"totals":{"errors":1,"file_errors":0},"files":[],"errors":["Invalid configuration"]}"#;
    let error = parse_phpstan(alone).unwrap_err();
    assert_eq!(error.to_string(), "Invalid configuration");
    let beside = r#"{"totals":{"errors":1,"file_errors":1},"files":{"/p/a.php":{"errors":1,"messages":[{"message":"Bad","line":3,"ignorable":true}]}},"errors":["Ignored error pattern #x# was not matched"]}"#;
    let problems = parse_phpstan(beside).unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].message, "Bad");
    let not_text =
        r#"{"totals":{"errors":1,"file_errors":0},"files":[],"errors":[{"message":"x"}]}"#;
    assert_eq!(
        parse_phpstan(not_text).unwrap_err().to_string(),
        r#"{"message":"x"}"#
    );
}

#[test]
fn test_phpcs_ruleset_in_a_parent_folder() {
    let top = tempfile::tempdir().unwrap();
    fs::write(
        top.path().join("phpcs.xml"),
        r#"<ruleset><arg name="tab-width" value="2"/></ruleset>"#,
    )
    .unwrap();
    let project = top.path().join("wp-content/plugins/demo");
    fs::create_dir_all(&project).unwrap();
    assert_eq!(phpcs_standard(&project, ProjectKind::WordPress), None);
    assert_eq!(ruleset_tab_width(&project), 2);
}

#[test]
fn test_changed_files_in_a_repo_subfolder() {
    let repo = tempfile::tempdir().unwrap();
    let project = repo.path().join("plugin");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(repo.path().join("other")).unwrap();
    fs::write(project.join("src/a.php"), "<?php").unwrap();
    fs::write(repo.path().join("other/b.php"), "<?php").unwrap();
    // Git prints the resolved toplevel (/private/var on macOS), paths from there.
    let toplevel = fs::canonicalize(repo.path()).unwrap();
    let status = " M plugin/src/a.php\0 M other/b.php\0";
    assert_eq!(
        changed_php_files_in(&project, &toplevel, status),
        [project.join("src/a.php")]
    );
}

#[test]
fn test_single_file_runs_in_its_folder() {
    assert_eq!(
        working_folder(Path::new("/x/a.php"), true),
        PathBuf::from("/x")
    );
    assert_eq!(
        working_folder(Path::new("/x/project"), false),
        PathBuf::from("/x/project")
    );
}

#[test]
fn test_failures_show_once_until_a_run_works() {
    let mut shown = std::collections::HashSet::new();
    let failure = || vec!["phpcs: bad ruleset".to_string()];
    assert_eq!(unseen_errors(&mut shown, failure()), failure());
    assert_eq!(unseen_errors(&mut shown, failure()), Vec::<String>::new());
    forget(&mut shown, "phpstan");
    assert_eq!(unseen_errors(&mut shown, failure()), Vec::<String>::new());
    forget(&mut shown, "phpcs");
    assert_eq!(unseen_errors(&mut shown, failure()), failure());
}

#[gpui::test]
async fn test_folder_php_files_skip_other_folders_and_ignored(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
    });
    let fake = project::FakeFs::new(cx.executor());
    fake.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {},
            ".gitignore": "vendor/\n*.cache.php\n",
            "src": {
                "a.php": "<?php",
                "routes.cache.php": "<?php",
                "notes.txt": "",
                "sub": { "b.php": "<?php" },
            },
            "src-other": { "c.php": "<?php" },
            "vendor": { "d.php": "<?php" },
        }),
    )
    .await;
    let project = Project::test(fake, [Path::new("/project")], cx).await;
    cx.run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let paths = |path: &str, cx: &mut TestAppContext| {
        let folder = ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path(path).into(),
        };
        cx.update(|cx| folder_php_files(&project, &folder, cx))
            .into_iter()
            .map(|(_, path)| path)
            .collect::<Vec<_>>()
    };
    let files = (paths("src", cx), paths("", cx));
    assert_eq!(
        files.0,
        [
            PathBuf::from("/project/src/a.php"),
            PathBuf::from("/project/src/sub/b.php")
        ]
    );
    assert_eq!(
        files.1,
        [
            PathBuf::from("/project/src/a.php"),
            PathBuf::from("/project/src/sub/b.php"),
            PathBuf::from("/project/src-other/c.php")
        ]
    );
}

fn suppressed(analyzer: Analyzer, text: &str, row: u32, column: u32, rule: Option<&str>) -> Option<String> {
    suppression_edit(analyzer, text, row, column, rule).map(|(range, comment)| {
        let mut text = text.to_string();
        text.replace_range(range, &comment);
        text
    })
}

#[test]
fn test_suppress_comment_goes_on_the_line_above_with_its_indent() {
    let text = "<?php\nfunction f() {\n\techo $x;\n}\n";
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 2, 1, Some("WordPress.Security.EscapeOutput.OutputNotEscaped")).unwrap(),
        "<?php\nfunction f() {\n\t// phpcs:ignore WordPress.Security.EscapeOutput.OutputNotEscaped\n\techo $x;\n}\n"
    );
    assert_eq!(
        suppressed(Analyzer::Phpstan, text, 2, 0, Some("variable.undefined")).unwrap(),
        "<?php\nfunction f() {\n\t// @phpstan-ignore variable.undefined\n\techo $x;\n}\n"
    );
    assert_eq!(
        suppressed(Analyzer::Phpstan, text, 2, 0, None).unwrap(),
        "<?php\nfunction f() {\n\t// @phpstan-ignore-next-line\n\techo $x;\n}\n"
    );
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 2, 0, None).unwrap(),
        "<?php\nfunction f() {\n\t// phpcs:ignore\n\techo $x;\n}\n"
    );
}

#[test]
fn test_suppress_adds_to_an_ignore_comment_above() {
    let text = "<?php\n  // phpcs:ignore A.B.C\n  echo $x;\n";
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 2, 2, Some("D.E.F")).unwrap(),
        "<?php\n  // phpcs:ignore A.B.C, D.E.F\n  echo $x;\n"
    );
    // Rules go before a reason.
    let text = "<?php\n// phpcs:ignore A.B.C -- legacy\necho $x;\n";
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 2, 0, Some("D.E.F")).unwrap(),
        "<?php\n// phpcs:ignore A.B.C, D.E.F -- legacy\necho $x;\n"
    );
    let text = "<?php\n// @phpstan-ignore a.b (old code)\nfoo();\n";
    assert_eq!(
        suppressed(Analyzer::Phpstan, text, 2, 0, Some("c.d")).unwrap(),
        "<?php\n// @phpstan-ignore a.b, c.d (old code)\nfoo();\n"
    );
    // Already there: nothing to do.
    let text = "<?php\n// phpcs:ignore A.B.C, D.E.F\necho $x;\n";
    assert_eq!(suppression_edit(Analyzer::Phpcs, text, 2, 0, Some("D.E.F")), None);
    // Another tool's comment above is left alone.
    let text = "<?php\n// phpcs:ignore A.B.C\nfoo();\n";
    assert_eq!(
        suppressed(Analyzer::Phpstan, text, 2, 0, Some("c.d")).unwrap(),
        "<?php\n// phpcs:ignore A.B.C\n// @phpstan-ignore c.d\nfoo();\n"
    );
}

#[test]
fn test_suppress_in_templates_stays_inside_the_php_block() {
    let text = "<ul>\n  <li><?php echo $title; ?></li>\n</ul>\n";
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 1, 17, Some("X.Y")).unwrap(),
        "<ul>\n  <li><?php echo $title; // phpcs:ignore X.Y ?></li>\n</ul>\n"
    );
    // Two blocks: the one holding the finding.
    let text = "<?= $a ?> and <?= $b ?>\n";
    assert_eq!(
        suppressed(Analyzer::Phpcs, text, 0, 18, Some("X.Y")).unwrap(),
        "<?= $a ?> and <?= $b // phpcs:ignore X.Y ?>\n"
    );
    // PHPStan without an identifier needs the same-line form here.
    assert_eq!(
        suppressed(Analyzer::Phpstan, "<p><?= $a ?></p>\n", 0, 0, None).unwrap(),
        "<p><?= $a // @phpstan-ignore-line ?></p>\n"
    );
    // The block opens on this line and stays open: comment at the end.
    assert_eq!(
        suppressed(Analyzer::Phpcs, "<?php echo $a;  \n", 0, 6, Some("X.Y")).unwrap(),
        "<?php echo $a; // phpcs:ignore X.Y  \n"
    );
}

#[test]
fn test_no_suppress_inside_a_docblock() {
    let text = "<?php\n/**\n * Summary\n */\nfunction f() {}\n";
    assert_eq!(suppression_edit(Analyzer::Phpcs, text, 2, 3, Some("X.Y")), None);
    assert_eq!(suppression_edit(Analyzer::Phpcs, text, 9, 0, Some("X.Y")), None);
}

#[gpui::test]
async fn test_suppress_actions_for_findings_on_the_cursor_line(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
    });
    let fs = project::FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({ "a.php": "<?php\necho $a;\necho $b;\n" }),
    )
    .await;
    let project = Project::test(fs, [Path::new("/project")], cx).await;
    let buffer = project
        .update(cx, |project, cx| project.open_local_buffer("/project/a.php", cx))
        .await
        .unwrap();
    let problem = |line, rule: Option<&str>| Problem {
        path: PathBuf::from("/project/a.php"),
        line,
        column: 6,
        severity: Severity::Error,
        message: "m".into(),
        rule: rule.map(Into::into),
    };
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            let path = PathBuf::from("/project/a.php");
            push_diagnostics(lsp_store, Analyzer::Phpcs, &path, &[problem(2, Some("A.B")), problem(3, Some("C.D"))], cx).unwrap();
            push_diagnostics(lsp_store, Analyzer::Phpstan, &path, &[problem(2, None)], cx).unwrap();
        })
    });
    cx.run_until_parked();

    let actions = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        let at = snapshot.anchor_before(text::Point::new(1, 2));
        SuppressProvider::actions(&buffer, at..at, cx)
    });
    let titles: Vec<_> = actions.iter().map(|action| action.lsp_action.title().to_string()).collect();
    assert_eq!(titles, ["Suppress A.B for this line (PHPCS)", "Suppress PHPStan for this line"]);

    // A line typed above after the menu opened: the comment still lands on the finding.
    buffer.update(cx, |buffer, cx| buffer.edit([(6..6, "// new\n")], None, cx));
    cx.update(|cx| SuppressProvider::apply(&buffer, &actions[0], true, cx)).unwrap();
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "<?php\n// new\n// phpcs:ignore A.B\necho $a;\necho $b;\n"
    );
}

#[gpui::test]
async fn test_project_php_files_cover_every_folder(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
    });
    let fake = project::FakeFs::new(cx.executor());
    fake.insert_tree(
        "/one",
        serde_json::json!({
            ".git": {},
            ".gitignore": "*.cache.php\n",
            "a.php": "<?php",
            "x.cache.php": "<?php",
            "src": { "b.php": "<?php", "c.js": "" },
        }),
    )
    .await;
    fake.insert_tree("/two", serde_json::json!({ "d.php": "<?php" })).await;
    let project = Project::test(fake, [Path::new("/one"), Path::new("/two")], cx).await;
    cx.run_until_parked();
    let (files, untrusted) = cx.update(|cx| project_php_files(&project, cx));
    let mut paths: Vec<_> = files.into_iter().map(|(_, path)| path).collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            PathBuf::from("/one/a.php"),
            PathBuf::from("/one/src/b.php"),
            PathBuf::from("/two/d.php"),
        ]
    );
    assert_eq!(untrusted, 0);
}

/// Opens `a.php` in a workspace with a fake project PHPCS, dispatches `action`
/// and waits for findings. `run_on_save` is off, so only the action can run it.
async fn dispatched_inspection(
    cx: &mut TestAppContext,
    action: Box<dyn gpui::Action>,
) -> (Vec<(text::Point, String)>, bool) {
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.keep()).unwrap();
    let root_path = root.to_str().unwrap().to_string();
    let bin = root.join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let report = r#"{"totals":{"errors":1,"warnings":0,"fixable":0},"files":{"ROOT/a.php":{"errors":1,"warnings":0,"messages":[{"message":"Missing doc comment","source":"Squiz.Commenting.FunctionComment.Missing","severity":5,"fixable":false,"type":"ERROR","line":2,"column":1}]}}}"#
        .replace("ROOT", &root_path);
    let marker = root.join("phpcs-ran");
    fs::write(
        bin.join("phpcs"),
        format!("#!/bin/sh\n: > '{}'\necho '{report}'\nexit 2\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(bin.join("phpcs"), fs::Permissions::from_mode(0o755)).unwrap();
    let stub_php = root.join("php-stub");
    fs::write(&stub_php, "#!/bin/sh\necho 'PHP 8.3.0 (cli)'\n").unwrap();
    fs::set_permissions(&stub_php, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("a.php"), "<?php\nfunction foo() {}\n").unwrap();

    use gpui::BorrowAppContext as _;
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        init(cx);
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    &serde_json::json!({ "zenith": { "inspections": { "run_on_save": false, "phpstan": false } } })
                        .to_string(),
                    cx,
                )
                .unwrap();
        });
        cx.set_global(ToolEnvironment {
            php_candidates: vec![stub_php.clone()],
            path_dirs: Vec::new(),
        });
        app_state
    });
    app_state
        .fs
        .as_fake()
        .insert_tree(&root_path, serde_json::json!({ "a.php": "<?php\nfunction foo() {}\n" }))
        .await;
    let project = Project::test(app_state.fs.clone(), [Path::new(&root_path)], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let item = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(root.join("a.php"), workspace::OpenOptions::default(), window, cx)
        })
        .await
        .unwrap();
    let editor = item.downcast::<editor::Editor>().unwrap();
    cx.run_until_parked();

    cx.executor().allow_parking();
    cx.update(|window, cx| window.dispatch_action(action, cx));
    let read = |cx: &mut gpui::VisualTestContext| {
        editor.read_with(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton().unwrap();
            let snapshot = buffer.read(cx).snapshot();
            snapshot
                .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
                .map(|entry| (entry.range.start, entry.diagnostic.message.to_string()))
                .collect::<Vec<_>>()
        })
    };
    // Real processes run; wait for the finding (or give up after ~5 s).
    for _ in 0..250 {
        cx.run_until_parked();
        if !read(cx).is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let result = (read(cx), marker.exists());
    fs::remove_dir_all(&root).ok();
    result
}

#[gpui::test]
async fn test_inspect_current_file_action(cx: &mut TestAppContext) {
    assert_eq!(
        dispatched_inspection(cx, Box::new(InspectCurrentFile)).await,
        (vec![(text::Point::new(1, 0), "Missing doc comment".to_string())], true)
    );
}

#[gpui::test]
async fn test_inspect_project_action(cx: &mut TestAppContext) {
    assert_eq!(
        dispatched_inspection(cx, Box::new(InspectProject)).await,
        (vec![(text::Point::new(1, 0), "Missing doc comment".to_string())], true)
    );
}

// "New since branch" (B2). A real git repo holds `a.php` on `main`; branch `feat`
// changes line 4 and leaves an uncommitted line 5, and adds `b.php`. The fake
// PHPCS reports lines 2 (old) and 4 (new) of `a.php` and logs the file it is
// asked about. A clean filter on `*.php` touches `git_marker` whenever git
// reads a PHP file, so "git ran" is observable.
const BASE_PHP: &str = "<?php\nfunction one() {}\nfunction two() {}\nfunction three() {}\n";

struct BranchEnv {
    root: PathBuf,
    project: Entity<Project>,
    worktree_id: WorktreeId,
    git_marker: PathBuf,
    phpcs_log: PathBuf,
}

#[allow(clippy::disallowed_methods, reason = "blocking git in a test helper")]
fn test_git(root: &Path, args: &[&str]) {
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
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `repo`: the folder is a git repository (real, and a fake one for the project
/// so it finds a default branch). `trusted`: the worktree is trusted.
async fn branch_env(cx: &mut TestAppContext, repo: bool, trusted: bool) -> BranchEnv {
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.keep()).unwrap();
    let root_path = root.to_str().unwrap().to_string();
    let git_marker = root.join("git-ran");
    let phpcs_log = root.join("phpcs-log");
    let bin = root.join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let report = r#"{"totals":{"errors":2,"warnings":0,"fixable":0},"files":{"ROOT/a.php":{"errors":2,"warnings":0,"messages":[{"message":"old","source":"S.Old","severity":5,"fixable":false,"type":"ERROR","line":2,"column":1},{"message":"new","source":"S.New","severity":5,"fixable":false,"type":"ERROR","line":4,"column":1}]}}}"#
        .replace("ROOT", &root_path);
    fs::write(
        bin.join("phpcs"),
        format!(
            "#!/bin/sh\nfor last; do :; done\necho \"$last\" >> '{}'\necho '{report}'\nexit 2\n",
            phpcs_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("phpcs"), fs::Permissions::from_mode(0o755)).unwrap();
    let stub_php = root.join("php-stub");
    fs::write(&stub_php, "#!/bin/sh\necho 'PHP 8.3.0 (cli)'\n").unwrap();
    fs::set_permissions(&stub_php, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("a.php"), BASE_PHP).unwrap();
    if repo {
        test_git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join(".gitattributes"), "*.php filter=probe\n").unwrap();
        test_git(&root, &["add", "."]);
        test_git(&root, &["commit", "-qm", "base"]);
        test_git(&root, &["checkout", "-qb", "feat"]);
        fs::write(
            root.join("a.php"),
            BASE_PHP.replace("three()", "three( $a )"),
        )
        .unwrap();
        fs::write(root.join("b.php"), "<?php\nfunction b() {}\n").unwrap();
        test_git(&root, &["add", "."]);
        test_git(&root, &["commit", "-qm", "work"]);
        // Uncommitted, so git has to read the file (and run the filter) to diff it.
        let mut text = fs::read_to_string(root.join("a.php")).unwrap();
        text.push_str("function four() {}\n");
        fs::write(root.join("a.php"), text).unwrap();
        test_git(
            &root,
            &[
                "config",
                "filter.probe.clean",
                &format!("touch {}; cat", git_marker.display()),
            ],
        );
    }

    use gpui::BorrowAppContext as _;
    let app_state = cx.update(|cx| {
        cx.set_global(db::AppDatabase::test_new());
        let app_state = workspace::AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        init(cx);
        if !trusted {
            project::trusted_worktrees::init(Default::default(), cx);
        }
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    &serde_json::json!({ "zenith": { "inspections": { "run_on_save": false, "phpstan": false } } })
                        .to_string(),
                    cx,
                )
                .unwrap();
        });
        cx.set_global(ToolEnvironment {
            php_candidates: vec![stub_php.clone()],
            path_dirs: Vec::new(),
        });
        app_state
    });
    let fake = app_state.fs.as_fake();
    let mut tree = serde_json::json!({ "a.php": BASE_PHP, "b.php": "<?php\n" });
    if repo {
        tree["vendor"] = serde_json::json!({});
        tree[".git"] = serde_json::json!({});
    }
    fake.insert_tree(&root_path, tree).await;
    if repo {
        fake.with_git_state(&root.join(".git"), false, |state| {
            state.default_branch_ref = Some("refs/heads/main".into());
        })
        .unwrap();
    }
    let project = if trusted {
        Project::test(app_state.fs.clone(), [Path::new(&root_path)], cx).await
    } else {
        Project::test_with_worktree_trust(app_state.fs.clone(), [Path::new(&root_path)], cx).await
    };
    cx.run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    BranchEnv {
        root,
        project,
        worktree_id,
        git_marker,
        phpcs_log,
    }
}

fn diagnostic_rows(buffer: &Entity<Buffer>, cx: &impl gpui::AppContext) -> Vec<u32> {
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut rows: Vec<u32> = snapshot
            .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
            .map(|entry| entry.range.start.row)
            .collect();
        rows.sort();
        rows
    })
}

/// Inspects `a.php`; the rows of its findings afterwards, the errors and the notes shown.
async fn inspect_a(
    cx: &mut TestAppContext,
    env: &BranchEnv,
) -> (Vec<u32>, Vec<String>, Vec<String>) {
    let buffer = env
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(env.root.join("a.php"), cx)
        })
        .await
        .unwrap();
    cx.executor().allow_parking();
    let report = cx
        .update(|cx| inspect_path(&env.project, env.worktree_id, env.root.join("a.php"), cx))
        .await;
    cx.run_until_parked();
    (diagnostic_rows(&buffer, cx), report.errors, report.notes)
}

fn switch(env: &BranchEnv, on: bool, cx: &mut TestAppContext) {
    cx.update(|cx| new_since_branch::set_on(&env.root, on, cx));
}

#[gpui::test]
async fn test_new_since_branch_shows_only_findings_on_new_lines(cx: &mut TestAppContext) {
    let env = branch_env(cx, true, true).await;
    switch(&env, true, cx);
    // Line 2 (row 1) is old, line 4 (row 3) was changed on the branch.
    assert_eq!(inspect_a(cx, &env).await, (vec![3], Vec::new(), Vec::new()));
    // Control for the untrusted test: here git really read the file.
    assert!(env.git_marker.exists());
}

#[gpui::test]
async fn test_new_since_branch_off_shows_all_findings(cx: &mut TestAppContext) {
    let env = branch_env(cx, true, true).await;
    assert_eq!(
        inspect_a(cx, &env).await,
        (vec![1, 3], Vec::new(), Vec::new())
    );
    assert!(!env.git_marker.exists());
}

#[gpui::test]
async fn test_new_since_branch_without_a_repository_shows_all_with_a_message(
    cx: &mut TestAppContext,
) {
    let env = branch_env(cx, false, true).await;
    switch(&env, true, cx);
    let (rows, errors, notes) = inspect_a(cx, &env).await;
    assert_eq!(rows, vec![1, 3]);
    // A notice, not a failure: it must not be shown as "PHP inspections failed".
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        notes,
        ["New since branch: No git repository was found. Showing all findings."]
    );
}

#[gpui::test]
async fn test_new_since_branch_never_runs_git_in_an_untrusted_project(cx: &mut TestAppContext) {
    let env = branch_env(cx, true, false).await;
    switch(&env, true, cx);
    // Nothing is inspected, so there is no base and no git.
    assert_eq!(
        inspect_a(cx, &env).await,
        (Vec::new(), Vec::new(), Vec::new())
    );
    let base = cx
        .update(|cx| new_since_branch::base_branch(&env.project, env.worktree_id, cx))
        .await;
    assert_eq!(base, Err("The project is not trusted.".to_string()));
    assert!(!env.git_marker.exists());
    // Once trusted, the same project does get a base and git runs.
    cx.update(|cx| {
        let trusted_worktrees =
            project::trusted_worktrees::TrustedWorktrees::try_get_global(cx).expect("trust store");
        let worktree_store = env.project.read(cx).worktree_store();
        trusted_worktrees.update(cx, |store, cx| {
            store.trust(
                &worktree_store,
                [project::trusted_worktrees::PathTrust::Worktree(
                    env.worktree_id,
                )]
                .into_iter()
                .collect(),
                cx,
            )
        });
    });
    let base = cx
        .update(|cx| new_since_branch::base_branch(&env.project, env.worktree_id, cx))
        .await;
    assert_eq!(base, Ok("main".to_string()));
    assert_eq!(inspect_a(cx, &env).await, (vec![3], Vec::new(), Vec::new()));
    assert!(env.git_marker.exists());
}

#[gpui::test]
async fn test_new_since_branch_switch_persists_across_a_fresh_cache(cx: &mut TestAppContext) {
    let env = branch_env(cx, false, true).await;
    let other = env.root.join("other");
    assert!(!cx.update(|cx| new_since_branch::is_on(&env.root, cx)));
    switch(&env, true, cx);
    cx.executor().allow_parking();
    let key = env.root.to_string_lossy().into_owned();
    for _ in 0..250 {
        cx.run_until_parked();
        let stored = cx.update(|cx| {
            db::kvp::KeyValueStore::global(cx)
                .scoped("zenith_inspections")
                .read(&key)
                .unwrap()
        });
        if stored.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    cx.update(|cx| {
        cx.remove_global::<new_since_branch::NewSinceBranch>();
        assert!(new_since_branch::is_on(&env.root, cx));
        assert!(!new_since_branch::is_on(&other, cx));
    });
}

#[test]
fn test_keep_new_filters_by_line() {
    let problem = |line| Problem {
        path: PathBuf::from("/a.php"),
        line,
        column: 1,
        severity: Severity::Error,
        message: String::new(),
        rule: None,
    };
    let problems = vec![problem(1), problem(3), problem(4)];
    let ranges = since_branch::NewLines::Ranges(vec![3..4]);
    assert_eq!(
        new_since_branch::keep_new(problems.clone(), &ranges),
        [problem(3)]
    );
    assert_eq!(
        new_since_branch::keep_new(problems.clone(), &since_branch::NewLines::All),
        problems
    );
}

/// Files the fake PHPCS was asked about after dispatching `InspectChangedFiles`,
/// and how many base lookups that took.
async fn files_inspected_after(
    cx: &mut TestAppContext,
    on: bool,
    wait_for: usize,
) -> (Vec<String>, usize) {
    let env = branch_env(cx, true, true).await;
    switch(&env, on, cx);
    let (_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(env.project.clone(), window, cx)
    });
    cx.executor().allow_parking();
    cx.update(|window, cx| window.dispatch_action(Box::new(InspectChangedFiles), cx));
    let read = || {
        let mut names: Vec<String> = fs::read_to_string(&env.phpcs_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| Some(Path::new(line).file_name()?.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    };
    // Real processes run: wait for the expected files, then a little longer so a
    // wrongly listed one would show up too.
    for _ in 0..250 {
        cx.run_until_parked();
        if read().len() >= wait_for {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for _ in 0..15 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let lookups = cx.update(|_, cx| {
        cx.try_global::<new_since_branch::BaseLookups>()
            .map_or(0, |lookups| lookups.0)
    });
    (read(), lookups)
}

#[gpui::test]
async fn test_changed_files_with_new_since_branch_include_the_branch_files(
    cx: &mut TestAppContext,
) {
    // `a.php` is in git status and in the branch list: inspected once. `b.php`
    // is committed on the branch only.
    assert_eq!(
        files_inspected_after(cx, true, 2).await.0,
        ["a.php", "b.php"]
    );
}

#[gpui::test]
async fn test_changed_files_without_new_since_branch_use_git_status_only(cx: &mut TestAppContext) {
    assert_eq!(
        files_inspected_after(cx, false, 1).await,
        (vec!["a.php".to_string()], 0)
    );
}

#[gpui::test]
async fn test_toggle_action_reruns_findings_and_flips_the_mode(cx: &mut TestAppContext) {
    let env = branch_env(cx, true, true).await;
    assert_eq!(inspect_a(cx, &env).await.0, vec![1, 3]);
    let buffer = env
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(env.root.join("a.php"), cx)
        })
        .await
        .unwrap();
    let (_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(env.project.clone(), window, cx)
    });
    cx.executor().allow_parking();
    for (expected_on, expected_rows) in [(true, vec![3]), (false, vec![1, 3])] {
        cx.update(|window, cx| window.dispatch_action(Box::new(ToggleNewSinceBranch), cx));
        for _ in 0..250 {
            cx.run_until_parked();
            if diagnostic_rows(&buffer, cx) == expected_rows {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(diagnostic_rows(&buffer, cx), expected_rows);
        assert_eq!(
            cx.update(|_, cx| new_since_branch::is_on(&env.root, cx)),
            expected_on
        );
    }
}

#[gpui::test]
async fn test_files_to_rerun_include_files_whose_findings_were_all_hidden(cx: &mut TestAppContext) {
    let env = branch_env(cx, false, true).await;
    let file = env.root.join("a.php");
    let rerun = |cx: &mut TestAppContext| {
        cx.update(|cx| new_since_branch::files_to_rerun(&env.project, cx))
    };
    // No diagnostics shown, nothing hidden: nothing to re-run.
    assert_eq!(rerun(cx), Vec::new());
    cx.update(|cx| new_since_branch::mark_hidden(&file, true, cx));
    assert_eq!(rerun(cx), [(env.worktree_id, file.clone())]);
    cx.update(|cx| new_since_branch::mark_hidden(&file, false, cx));
    assert_eq!(rerun(cx), Vec::new());
}

#[gpui::test]
async fn test_clean_filter_marker_is_created_by_plain_git(cx: &mut TestAppContext) {
    // Positive control for the untrusted test. GIT_HARDENING does not stop
    // `filter.<name>.clean`, so plain git (and ours) running in a project folder
    // runs the repo's filter: the trust gate is the only guard, and this marker
    // shows git running at all.
    let env = branch_env(cx, true, true).await;
    assert!(!env.git_marker.exists());
    #[allow(clippy::disallowed_methods, reason = "blocking git in a test")]
    let output = std::process::Command::new("git")
        .args(["diff", "--merge-base", "main", "--", "a.php"])
        .current_dir(&env.root)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(env.git_marker.exists());
}

#[test]
fn test_changed_files_message_keeps_no_changed_files_wording() {
    let note = || Some("New since branch: nope.".to_string());
    assert_eq!(
        changed_files_message(0, None, None),
        "No changed PHP files."
    );
    assert_eq!(
        changed_files_message(0, None, note()),
        "No changed PHP files. New since branch: nope."
    );
    let problem = "Not a repository.".to_string();
    assert_eq!(
        changed_files_message(0, Some(&problem), note()),
        "Not a repository. New since branch: nope."
    );
    // With files to inspect the runs report the failed base themselves.
    assert_eq!(
        changed_files_message(2, None, note()),
        "Inspecting 2 changed PHP files."
    );
    assert_eq!(
        changed_files_message(1, None, None),
        "Inspecting 1 changed PHP file."
    );
}

#[gpui::test]
async fn test_inspect_path_uses_the_base_it_is_given(cx: &mut TestAppContext) {
    use futures::FutureExt as _;
    // The repo has a real default branch ("main"); a batch hands down its own
    // lookup instead of each file doing another, so the handed-down result wins.
    let env = branch_env(cx, true, true).await;
    switch(&env, true, cx);
    let buffer = env
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(env.root.join("a.php"), cx)
        })
        .await
        .unwrap();
    cx.executor().allow_parking();
    let given = gpui::Task::ready(Err("Given.".to_string())).shared();
    let report = cx
        .update(|cx| {
            inspect_path_with(
                &env.project,
                env.worktree_id,
                env.root.join("a.php"),
                Some(given),
                cx,
            )
        })
        .await;
    cx.run_until_parked();
    assert_eq!(
        report.notes,
        ["New since branch: Given. Showing all findings."]
    );
    assert_eq!(diagnostic_rows(&buffer, cx), vec![1, 3]);
    assert!(!env.git_marker.exists());
}

#[gpui::test]
async fn test_a_batch_looks_up_the_base_once_per_folder(cx: &mut TestAppContext) {
    // Two files (`a.php`, `b.php`) in one folder: one lookup, shared by the file
    // list and both runs.
    let (files, lookups) = files_inspected_after(cx, true, 2).await;
    assert_eq!(files, ["a.php", "b.php"]);
    assert_eq!(lookups, 1);
}

#[gpui::test]
async fn test_a_single_save_looks_up_the_base_beside_the_tools(cx: &mut TestAppContext) {
    use futures::FutureExt as _;
    let env = branch_env(cx, true, true).await;
    switch(&env, true, cx);
    let buffer = env
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(env.root.join("a.php"), cx)
        })
        .await
        .unwrap();
    cx.executor().allow_parking();
    // A lookup that only finishes once PHPCS has been started: it is awaited after
    // the tools, not before them (before them it would time out, ~3 s).
    let log = env.phpcs_log.clone();
    let base = cx
        .background_spawn(async move {
            // The test scheduler runs background tasks on one thread: yield, never
            // sleep, so the tools can start.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if log.exists() {
                    return Ok("main".to_string());
                }
                let mut yielded = false;
                std::future::poll_fn(|task| {
                    if yielded {
                        return std::task::Poll::Ready(());
                    }
                    yielded = true;
                    task.waker().wake_by_ref();
                    std::task::Poll::Pending
                })
                .await;
            }
            Err("The tools never started.".to_string())
        })
        .shared();
    let report = cx
        .update(|cx| {
            inspect_path_with(
                &env.project,
                env.worktree_id,
                env.root.join("a.php"),
                Some(base),
                cx,
            )
        })
        .await;
    cx.run_until_parked();
    assert_eq!(report.notes, Vec::<String>::new());
    assert_eq!(diagnostic_rows(&buffer, cx), vec![3]);
}
