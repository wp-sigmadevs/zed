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
