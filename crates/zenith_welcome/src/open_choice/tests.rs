use super::*;
use fs::{FakeFs, Fs as _};
use gpui::{Entity, Modifiers, TestAppContext, UpdateGlobal as _, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use std::path::Path;
use workspace::MultiWorkspace;

#[test]
fn decide_table() {
    use OpenPathPromptMode::{CurrentWindow, NewWindow};
    let cases = [
        // No project in this window: it opens there, without asking.
        (
            false,
            true,
            CurrentWindow,
            None,
            Decision::Do(Choice::ThisWindow),
        ),
        (true, true, CurrentWindow, None, Decision::Ask),
        // Only files: upstream's behaviour.
        (true, false, CurrentWindow, None, Decision::Upstream),
        (false, false, CurrentWindow, None, Decision::Upstream),
        // The caller already asked for a new window.
        (true, true, NewWindow, None, Decision::Do(Choice::NewWindow)),
        (
            true,
            true,
            CurrentWindow,
            Some(Choice::Attach),
            Decision::Do(Choice::Attach),
        ),
        (
            true,
            true,
            NewWindow,
            Some(Choice::Attach),
            Decision::Do(Choice::NewWindow),
        ),
        (
            false,
            true,
            CurrentWindow,
            Some(Choice::Attach),
            Decision::Do(Choice::ThisWindow),
        ),
    ];
    for (has_project, any_dir, mode, remembered, expected) in cases {
        assert_eq!(
            decide(has_project, any_dir, mode, remembered),
            expected,
            "has_project={has_project} any_dir={any_dir} remembered={remembered:?}"
        );
    }
}

struct Fixture {
    multi: Entity<MultiWorkspace>,
    workspace: Entity<Workspace>,
    fs: Arc<FakeFs>,
}

async fn setup<'a>(
    cx: &'a mut TestAppContext,
    open: &[&str],
) -> (Fixture, &'a mut VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        workspace::init(app_state.clone(), cx);
        editor::init(cx);
        <dyn fs::Fs>::set_global(app_state.fs.clone(), cx);
        init(cx);
        app_state
    });
    let fs = app_state.fs.as_fake();
    fs.insert_tree(
        "/root",
        json!({ "a": { "one.txt": "1" }, "b": { "two.txt": "2" }, "notes.txt": "n" }),
    )
    .await;
    let roots: Vec<&Path> = open.iter().map(Path::new).collect();
    let project = Project::test(app_state.fs.clone(), roots, cx).await;
    let (multi, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    cx.run_until_parked();
    (
        Fixture {
            multi,
            workspace,
            fs,
        },
        cx,
    )
}

fn open_paths_choice(fixture: &Fixture, paths: &[&str], cx: &mut VisualTestContext) {
    let paths = paths.iter().map(PathBuf::from).collect();
    fixture.workspace.update_in(cx, |workspace, window, cx| {
        open_with_choice(paths, workspace, window, cx)
    });
    cx.run_until_parked();
}

fn modal(fixture: &Fixture, cx: &mut VisualTestContext) -> Option<Entity<OpenProjectDialog>> {
    fixture.workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<OpenProjectDialog>(cx)
    })
}

fn click(cx: &mut VisualTestContext, id: &'static str) {
    let bounds = cx
        .debug_bounds(id)
        .unwrap_or_else(|| panic!("{id} is not shown"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// The folders of the active project of `multi`.
fn active_roots(multi: &Entity<MultiWorkspace>, cx: &mut VisualTestContext) -> Vec<String> {
    multi.read_with(cx, |multi, cx| {
        multi
            .workspace()
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|tree| tree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect()
    })
}

fn window_count(cx: &mut VisualTestContext) -> usize {
    cx.cx.update(|cx| {
        cx.windows()
            .into_iter()
            .filter(|window| window.downcast::<MultiWorkspace>().is_some())
            .count()
    })
}

fn settings_text(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
    cx.run_until_parked();
    futures::executor::block_on(fixture.fs.load(paths::settings_file())).unwrap_or_default()
}

fn remember(cx: &mut VisualTestContext, window: ZenithOpenProjectWindow) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .zenith
                    .get_or_insert_default()
                    .open_project
                    .get_or_insert_default()
                    .window = Some(window);
            });
        })
    });
}

#[gpui::test]
async fn attach_adds_the_folder_to_this_project(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    assert!(modal(&fixture, cx).is_some(), "a project is open: asks");
    click(cx, "zenith-open-attach");
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(window_count(cx), 1);
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a", "/root/b"]);
}

#[gpui::test]
async fn new_window_leaves_this_project_alone(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    click(cx, "zenith-open-new");
    assert_eq!(window_count(cx), 2);
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a"]);
}

#[gpui::test]
async fn this_window_switches_to_the_folder(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    click(cx, "zenith-open-this");
    assert_eq!(window_count(cx), 1);
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/b"]);
}

#[gpui::test]
async fn a_remembered_choice_skips_the_dialog(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    remember(cx, ZenithOpenProjectWindow::Attach);
    open_paths_choice(&fixture, &["/root/b"], cx);
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a", "/root/b"]);
}

#[gpui::test]
async fn an_empty_window_opens_the_folder_without_asking(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &[]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/b"]);
}

#[gpui::test]
async fn files_only_never_ask(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/notes.txt"], cx);
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(window_count(cx), 1);
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a"]);
}

#[gpui::test]
async fn ticking_the_box_saves_the_choice(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    click(cx, "zenith-open-remember");
    click(cx, "zenith-open-attach");
    assert!(
        settings_text(&fixture, cx).contains("\"window\": \"attach\""),
        "settings: {}",
        settings_text(&fixture, cx)
    );
}

#[gpui::test]
async fn cancel_saves_nothing_and_opens_nothing(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    click(cx, "zenith-open-remember");
    click(cx, "zenith-open-cancel");
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(settings_text(&fixture, cx), "");
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a"]);
}

#[gpui::test]
async fn escape_cancels(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    open_paths_choice(&fixture, &["/root/b"], cx);
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/a"]);
}

#[gpui::test]
async fn confirm_window_forgets_the_choice(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    fixture
        .fs
        .create_dir(paths::settings_file().parent().unwrap())
        .await
        .unwrap();
    let saved = r#"{ "zenith": { "open_project": { "window": "attach" } } }"#;
    fixture
        .fs
        .insert_file(paths::settings_file(), saved.as_bytes().to_vec())
        .await;
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(saved, cx).result().unwrap();
        })
    });
    assert_eq!(
        cx.update(|_, cx| ZenithOpenProjectSettings::get_global(cx).window),
        Some(Choice::Attach)
    );
    cx.dispatch_action(ConfirmWindow);
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| ZenithOpenProjectSettings::get_global(cx).window),
        None
    );
    open_paths_choice(&fixture, &["/root/b"], cx);
    assert!(modal(&fixture, cx).is_some(), "asks again");
}

#[gpui::test]
async fn enter_picks_the_default_button_and_tab_stays_inside(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx, &["/root/a"]).await;
    // The base keymap binds a context-less `tab` to `menu::SelectNext`; the fork's own keymap
    // file takes it over inside the dialog.
    cx.update(|_, cx| {
        for json in [
            include_str!("../../../../assets/keymaps/default-macos.json"),
            include_str!("../../../../assets/keymaps/zenith.json"),
        ] {
            let bindings = match settings::KeymapFile::load(json, cx) {
                settings::KeymapFileLoadResult::Success { key_bindings }
                | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => {
                    key_bindings
                }
                settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                    panic!("{error:#}")
                }
            };
            cx.bind_keys(bindings);
        }
    });
    open_paths_choice(&fixture, &["/root/b"], cx);
    let dialog = modal(&fixture, cx).expect("asks");
    let handle = dialog.read_with(cx, |dialog, _| dialog.focus_handle.clone());
    let default_focus = dialog.read_with(cx, |dialog, _| dialog.default_focus.clone());
    cx.update(|window, _| assert!(default_focus.is_focused(window), "This Window is focused"));
    for key in ["tab"; 5].into_iter().chain(["shift-tab"; 5]) {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(handle.contains_focused(window, cx), "{key} left the dialog");
        });
    }
    // Five Tabs and five Shift-Tabs lead back to the default button.
    cx.update(|window, _| assert!(default_focus.is_focused(window)));
    // A focused button is pressed on the release of Enter (the test keystroke is the press).
    cx.simulate_keystrokes("enter");
    cx.simulate_event(gpui::KeyUpEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
    });
    cx.run_until_parked();
    assert!(modal(&fixture, cx).is_none());
    assert_eq!(active_roots(&fixture.multi, cx), ["/root/b"]);
}
