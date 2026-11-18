use super::*;
use gpui::TestAppContext;
use std::{fs, os::unix::fs::PermissionsExt as _};

const PHPCS: &str = include_str!("../fixtures/phpcs-wordpress.json");
const PHPSTAN: &str = include_str!("../fixtures/phpstan.json");

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
                lsp_store
                    .update_diagnostic_entries(
                        analyzer.server_id(),
                        PathBuf::from("/project/a.php"),
                        None,
                        None,
                        diagnostic_entries(analyzer, &problems),
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
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().to_str().unwrap().to_string();
    let bin = root.path().join("vendor/bin");
    fs::create_dir_all(&bin).unwrap();
    let report = r#"{"totals":{"errors":1,"warnings":0,"fixable":0},"files":{"ROOT/a.php":{"errors":1,"warnings":0,"messages":[{"message":"Missing doc comment","source":"Squiz.Commenting.FunctionComment.Missing","severity":5,"fixable":false,"type":"ERROR","line":2,"column":1}]}}}"#
        .replace("ROOT", &root_path);
    let phpcs = bin.join("phpcs");
    fs::write(&phpcs, format!("#!/bin/sh\necho '{report}'\nexit 2\n")).unwrap();
    fs::set_permissions(&phpcs, fs::Permissions::from_mode(0o755)).unwrap();
    let path_dir = tempfile::tempdir().unwrap();
    let stub_php = path_dir.path().join("php");
    fs::write(&stub_php, "#!/bin/sh\necho 'PHP 8.3.0 (cli)'\n").unwrap();
    fs::set_permissions(&stub_php, fs::Permissions::from_mode(0o755)).unwrap();
    // The stub first, then the system directories other tests' shell scripts need.
    // SAFETY: nothing else in this crate's tests reads PATH for its result.
    let path = format!("{}:/usr/bin:/bin", path_dir.path().display());
    unsafe { std::env::set_var("PATH", path) };
    fs::write(root.path().join("a.php"), "<?php\nfunction foo() {}\n").unwrap();

    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
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
    cx.executor().allow_parking();
    cx.update(|cx| inspect(&project, &buffer, cx));
    let mut messages = Vec::new();
    for _ in 0..100 {
        cx.run_until_parked();
        messages = buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
                .map(|entry| (entry.range.start, entry.diagnostic.message.to_string()))
                .collect::<Vec<_>>()
        });
        if !messages.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        messages,
        [(text::Point::new(1, 0), "Missing doc comment".to_string())]
    );
}
