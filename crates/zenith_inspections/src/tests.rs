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
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({})).await,
        [(text::Point::new(1, 0), "Missing doc comment".to_string())]
    );
}

#[gpui::test]
async fn test_switched_off_analyzers_and_saving_show_nothing(cx: &mut TestAppContext) {
    // Switched off: its old findings are cleared.
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({ "phpcs": false })).await,
        []
    );
    // Not run at all: the old findings stay.
    assert_eq!(
        saved_php_diagnostics(cx, serde_json::json!({ "run_on_save": false })).await,
        [(text::Point::new(0, 0), "stale".to_string())]
    );
}

async fn saved_php_diagnostics(
    cx: &mut TestAppContext,
    inspections: serde_json::Value,
) -> Vec<(text::Point, String)> {
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
    fs::write(&phpcs, format!("#!/bin/sh\necho '{report}'\nexit 2\n")).unwrap();
    fs::set_permissions(&phpcs, fs::Permissions::from_mode(0o755)).unwrap();
    let path_dir = tempfile::tempdir().unwrap();
    let stub_php = path_dir.path().join("php");
    fs::write(&stub_php, "#!/bin/sh\necho 'PHP 8.3.0 (cli)'\n").unwrap();
    fs::set_permissions(&stub_php, fs::Permissions::from_mode(0o755)).unwrap();
    // The stub first, then the system directories other tests' shell scripts need.
    // SAFETY: nothing else in this crate's tests reads PATH for its result.
    let _restore = RestoreEnv("PATH", std::env::var_os("PATH"));
    let path = format!("{}:/usr/bin:/bin", path_dir.path().display());
    unsafe { std::env::set_var("PATH", path) };
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
            lsp_store
                .update_diagnostic_entries(
                    Analyzer::Phpcs.server_id(),
                    root.path().join("a.php"),
                    None,
                    None,
                    diagnostic_entries(Analyzer::Phpcs, &[stale]),
                    cx,
                )
                .unwrap();
        })
    });
    cx.executor().allow_parking();
    cx.update(|cx| on_buffer_event(&project, &buffer, &BufferEvent::Saved, cx));
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
        if messages.iter().all(|(_, message)| message != "stale") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    messages
}

/// Puts an environment variable back when dropped, even if the test fails.
struct RestoreEnv(&'static str, Option<std::ffi::OsString>);

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        // SAFETY: as where the tests set it.
        unsafe {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

#[test]
fn test_restore_env_puts_the_variable_back() {
    let name = "ZENITH_INSPECTIONS_TEST_VARIABLE";
    // SAFETY: a variable no other test touches.
    unsafe { std::env::set_var(name, "before") };
    {
        let _restore = RestoreEnv(name, std::env::var_os(name));
        unsafe { std::env::set_var(name, "during") };
        assert_eq!(std::env::var(name).unwrap(), "during");
    }
    assert_eq!(std::env::var(name).unwrap(), "before");
    {
        let _restore = RestoreEnv(name, None);
        unsafe { std::env::set_var(name, "during") };
    }
    assert!(std::env::var_os(name).is_none());
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
