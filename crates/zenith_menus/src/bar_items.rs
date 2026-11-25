//! Menu-bar items PhpStorm has and upstream's bar lacks: most of Code, all of Refactor and
//! Git, and the head of Run (plan 44 item 14, chunk M2).
//!
//! Each menu is a `Spec` resolved by action name, as the editor menu is: an action whose
//! feature is compiled out is skipped, a submenu left empty is dropped, and a submenu holds
//! items only (one level deep). Not built, so not listed: Generate, Override / Implement
//! Methods, Start SSH Session, Run Command.

use gpui::{Action, App, Menu, MenuItem};
use project::DisableAiSettings;
use settings::Settings as _;

use crate::editor_menu::{Entry, Spec, entries, spec_names};

/// Menu name and the items to put at its head.
pub(crate) type Extras = Vec<(&'static str, Vec<MenuItem>)>;

const FOLDING: [(&str, &str); 8] = [
    ("Fold", "editor::Fold"),
    ("Unfold", "editor::UnfoldLines"),
    ("Toggle Fold", "editor::ToggleFold"),
    ("Fold Recursively", "editor::FoldRecursive"),
    ("Unfold Recursively", "editor::UnfoldRecursive"),
    ("Fold Selection", "editor::FoldSelectedRanges"),
    ("Fold All", "editor::FoldAll"),
    ("Unfold All", "editor::UnfoldAll"),
];

fn code() -> Vec<Spec> {
    use Spec::{Item, Menu, Separator};
    vec![
        Menu(
            "Code Completion",
            vec![
                ("Basic", "editor::ShowCompletions"),
                ("Word Completion", "editor::ShowWordCompletions"),
                ("Parameter Info", "editor::ShowSignatureHelp"),
            ],
        ),
        Item("Show Context Actions", "editor::ToggleCodeActions"),
        Separator,
        Item("Surround With…", "zenith_postfix::SurroundWith"),
        Item("Insert Live Template…", "zenith_postfix::LiveTemplates"),
        Menu("Folding", FOLDING.to_vec()),
        Separator,
        Item("Comment with Line Comment", "editor::ToggleComments"),
        Item("Comment with Block Comment", "editor::ToggleBlockComments"),
        Separator,
        Item("Reformat Code", "editor::Format"),
        Item("Reformat Selection", "editor::FormatSelections"),
        Item("Auto-Indent Lines", "editor::AutoIndent"),
        Item("Optimize Imports", "editor::OrganizeImports"),
        Separator,
        Item("Move Line Up", "editor::MoveLineUp"),
        Item("Move Line Down", "editor::MoveLineDown"),
        Separator,
        Menu(
            "Inspect Code",
            vec![
                (
                    "Inspect Current File",
                    "zenith_inspections::InspectCurrentFile",
                ),
                (
                    "Inspect Changed Files",
                    "zenith_inspections::InspectChangedFiles",
                ),
                ("Inspect Project", "zenith_inspections::InspectProject"),
                (
                    "Fix File with PHPCBF",
                    "zenith_inspections::FixFileWithPhpcbf",
                ),
                ("Problems in File", "zenith_problems::ShowFileProblems"),
                ("Project Errors", "zenith_problems::ShowProjectErrors"),
            ],
        ),
    ]
}

fn refactor() -> Vec<Spec> {
    use Spec::{Item, Menu};
    vec![
        Item("Refactor This…", "zenith_refactor::RefactorThis"),
        Item("Rename…", "editor::Rename"),
        Item("Change Signature…", "zenith_refactor::ChangeSignature"),
        Item("Move…", "zenith_refactor::MoveClass"),
        Item("Safe Delete…", "zenith_refactor::SafeDelete"),
        Menu(
            "Extract / Introduce",
            vec![
                ("Extract Variable…", "zenith_refactor::ExtractVariable"),
                ("Extract Method…", "zenith_refactor::ExtractMethod"),
            ],
        ),
    ]
}

/// The name of the one action hidden while AI is disabled.
pub(crate) const COMMIT_MESSAGE: &str = "git::GenerateCommitMessage";

fn git() -> Vec<Spec> {
    use Spec::{Item, Menu, Separator};
    vec![
        Item("Commit…", "git::Commit"),
        Item("Amend Commit", "git::Amend"),
        Item("Push…", "git::Push"),
        Item("Force Push", "git::ForcePush"),
        Item("Pull", "git::Pull"),
        Item("Fetch", "git::Fetch"),
        Separator,
        Menu(
            "Branches",
            vec![
                ("Branches…", "git::Branch"),
                ("Switch Branch…", "git::Switch"),
                ("Compare with Default Branch", "git::DiffBranch"),
                ("Create Pull Request", "git::CreatePullRequest"),
            ],
        ),
        Item("Git Tool Window", "git_panel::ToggleFocus"),
        Separator,
        Menu(
            "Uncommitted Changes",
            vec![
                ("Stash Changes", "git::StashAll"),
                ("Pop Stash", "git::StashPop"),
                ("View Stashes…", "git::ViewStash"),
                ("Open Modified Files", "git::OpenModifiedFiles"),
            ],
        ),
        Menu(
            "Current File",
            vec![
                ("Annotate with Git Blame", "git::Blame"),
                ("Toggle Inline Blame", "editor::ToggleGitBlameInline"),
                ("Show History", "git::FileHistory"),
                ("Rollback…", "git::RestoreFile"),
                ("Next Change", "editor::GoToHunk"),
                ("Previous Change", "editor::GoToPreviousHunk"),
                ("Copy Permalink to Line", "editor::CopyPermalinkToLine"),
            ],
        ),
        Separator,
        Item("Initialize Repository", "git::Init"),
        Item("Clone Repository…", "git::Clone"),
        Item("Generate Commit Message", COMMIT_MESSAGE),
    ]
}

fn run() -> Vec<Spec> {
    use Spec::Item;
    vec![
        Item("Run", "zenith_run::RunSelected"),
        Item("Debug", "zenith_run::DebugSelected"),
        Item("Stop", "zenith_run::StopRun"),
        Item("Rerun Last", "zenith_run::RerunLast"),
        Spec::Separator,
        Item("Run Scratch File", "zenith_scratches::RunScratch"),
        Item("Run HTTP Request", "zenith_http::RunRequest"),
        Item("Run All HTTP Requests", "zenith_http::RunAllRequests"),
    ]
}

/// The four menus' items, in bar order. `resolve` turns an action name into an action.
pub(crate) fn build(
    mut resolve: impl FnMut(&str) -> Option<Box<dyn Action>>,
    ai_disabled: bool,
) -> Extras {
    let mut resolve = |name: &str| {
        if ai_disabled && name == COMMIT_MESSAGE {
            return None;
        }
        resolve(name)
    };
    [
        ("Code", code()),
        ("Refactor", refactor()),
        ("Run", run()),
        ("Git", git()),
    ]
    .into_iter()
    .map(|(menu, spec)| {
        let rows = entries(&spec, &mut resolve);
        (menu, rows.into_iter().map(item).collect())
    })
    .collect()
}

/// Every action name the Code, Refactor, Run and Git menus look up, for tests that check they
/// exist.
pub fn bar_action_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for spec in [code(), refactor(), run(), git()] {
        names.extend(spec_names(spec));
    }
    names
}

/// `build` with real actions and the live `disable_ai` setting.
pub(crate) fn resolve(cx: &App) -> Extras {
    let ai_disabled = DisableAiSettings::get_global(cx).disable_ai;
    build(|name| cx.build_action(name, None).ok(), ai_disabled)
}

pub(crate) fn action(label: &'static str, action: Box<dyn Action>) -> MenuItem {
    MenuItem::Action {
        name: label.into(),
        action,
        os_action: None,
        checked: false,
        disabled: false,
    }
}

fn item(entry: Entry<Box<dyn Action>>) -> MenuItem {
    match entry {
        Entry::Separator => MenuItem::separator(),
        Entry::Item(label, resolved) => action(label, resolved),
        Entry::Menu(label, items) => MenuItem::submenu(
            Menu::new(label).items(
                items
                    .into_iter()
                    .map(|(label, resolved)| action(label, resolved)),
            ),
        ),
    }
}
