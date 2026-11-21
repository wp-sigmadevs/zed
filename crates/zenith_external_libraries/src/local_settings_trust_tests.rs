//! Project-level proof that local settings and tasks inside library folders are never loaded
//! (hook in `project::SettingsObserver::update_local_worktree_settings`).

use fs::FakeFs;
use gpui::TestAppContext;
use project::Project;
use serde_json::json;
use settings::SettingsStore;

fn task_file(label: &str) -> String {
    json!([{ "label": label, "command": "true" }]).to_string()
}

#[gpui::test]
async fn library_folders_never_load_local_files(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/root",
        json!({
            ".zed": {
                "settings.json": r#"{ "tab_size": 3 }"#,
                "tasks.json": task_file("root task"),
            },
            "vendor": { "acme": { ".zed": {
                "settings.json": r#"{ "tab_size": 7 }"#,
                "tasks.json": task_file("vendor task"),
            } } },
            "node_modules": { "x": { ".zed": { "tasks.json": task_file("node task") } } },
            "src": { "vendor_x": { ".zed": { "settings.json": r#"{ "tab_size": 5 }"# } } },
        }),
    )
    .await;

    let project = Project::test(fs, ["/root".as_ref()], cx).await;
    cx.run_until_parked();

    let (worktree_id, inventory) = project.read_with(cx, |project, cx| {
        let worktree_id = project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
            .expect("worktree");
        let inventory = project
            .task_store()
            .read(cx)
            .task_inventory()
            .cloned()
            .expect("task inventory");
        (worktree_id, inventory)
    });

    let loaded: Vec<String> = cx.read(|cx| {
        cx.global::<SettingsStore>()
            .local_settings(worktree_id)
            .map(|(path, _)| path.as_unix_str().to_string())
            .collect()
    });
    assert_eq!(loaded, ["", "src/vendor_x"]);

    let mut labels: Vec<String> = inventory
        .update(cx, |inventory, cx| {
            inventory.list_tasks(None, None, Some(worktree_id), cx)
        })
        .await
        .into_iter()
        .map(|(_, task)| task.label)
        .collect();
    labels.sort();
    assert_eq!(labels, ["root task"]);
}
