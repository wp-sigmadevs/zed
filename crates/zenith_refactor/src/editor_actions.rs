//! Hooks the refactorings into the workspace for the active PHP file: Move Class, Safe Delete
//! and Change Signature ask for their input in a small prompt, build a plan and open the
//! preview; Refactor This lists them. Extract Method and Extract Variable have no engine yet,
//! so their actions are not handled.

use crate::{
    ChangeSignature, MoveClass, RefactorPlan, RefactorThis, SafeDelete, SignatureParam,
    derive_psr4_namespace, open_refactor_preview, plan_change_signature, plan_move_class,
    plan_safe_delete,
};
use anyhow::{Result, anyhow, bail};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Task, WeakEntity, Window, div, rems,
};
use language::Point;
use picker::{Picker, PickerDelegate};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ui::{Color, Label, ListItem, ListItemSpacing, SharedString, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, notifications::DetachAndPromptErr as _};

type WorkspaceFn = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &RefactorThis, window, cx| {
            open_refactor_this(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &MoveClass, window, cx| {
            move_class(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &SafeDelete, window, cx| {
            safe_delete(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &ChangeSignature, window, cx| {
            change_signature(workspace, window, cx)
        });
    })
    .detach();
}

struct ActiveFile {
    path: PathBuf,
    /// The selected text, or the identifier under the cursor; empty when there is neither.
    word: String,
}

/// The active editor's file when it is a local `.php` file; otherwise says why nothing
/// happens, so the shortcut doesn't look broken.
fn active_php_file(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Option<ActiveFile> {
    let file = php_file_in(workspace, cx);
    if file.is_none() {
        toast(
            workspace,
            "Refactorings work on a PHP file saved in the project",
            cx,
        );
    }
    file
}

fn php_file_in(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<ActiveFile> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    editor.update(cx, |editor, cx| {
        let buffer = editor.buffer().read(cx).as_singleton()?;
        let path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
        if path.extension().is_none_or(|ext| ext != "php") {
            return None;
        }
        let selection = editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx));
        let snapshot = buffer.read(cx).snapshot();
        let word = if selection.is_empty() {
            let row = selection.head().row;
            let line: String = snapshot
                .text_for_range(Point::new(row, 0)..Point::new(row + 1, 0))
                .collect();
            word_at(&line, selection.head().column as usize)
        } else {
            snapshot.text_for_range(selection.range()).collect()
        };
        Some(ActiveFile { path, word })
    })
}

/// The identifier (`[A-Za-z0-9_]`) touching byte `column` of `line`.
fn word_at(line: &str, column: usize) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let column = column.min(line.len());
    let Some(before) = line.get(..column) else {
        return String::new();
    };
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(column, |(ix, _)| ix);
    let end = line
        .get(column..)
        .and_then(|rest| rest.find(|c: char| !is_word(c)))
        .map_or(line.len(), |ix| column + ix);
    line.get(start..end).unwrap_or_default().to_string()
}

/// Same bound as the WordPress and Laravel indexers.
const MAX_PHP_FILES: usize = 20_000;

fn toast(workspace: &mut Workspace, message: &str, cx: &mut Context<Workspace>) {
    workspace.show_error(anyhow!(message.to_string()), cx);
}

/// Every `.php` file of the project plus `extra`, read from disk off the main thread, then
/// `planner` runs on them in the background and the preview opens.
fn plan_and_preview(
    workspace: &mut Workspace,
    extra: PathBuf,
    failure: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    planner: impl FnOnce(&[(PathBuf, String)]) -> Result<RefactorPlan> + Send + 'static,
) {
    let project = workspace.project().read(cx);
    let fs = project.fs().clone();
    let mut seen = std::collections::HashSet::new();
    seen.insert(extra.clone());
    let mut paths = vec![extra];
    for worktree in project.visible_worktrees(cx) {
        let snapshot = worktree.read(cx).snapshot();
        for entry in snapshot.files(false, 0) {
            if entry.path.extension() == Some("php") {
                let abs = snapshot.absolutize(&entry.path);
                if seen.insert(abs.clone()) {
                    paths.push(abs);
                }
            }
        }
    }
    // A partial scan would miss call sites silently, so refuse instead of truncating.
    if paths.len() > MAX_PHP_FILES {
        toast(
            workspace,
            &format!(
                "{failure}: the project has more than {MAX_PHP_FILES} PHP files outside ignored folders"
            ),
            cx,
        );
        return;
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let plan = cx
            .background_spawn(async move {
                let mut files = Vec::with_capacity(paths.len());
                for path in paths {
                    if let Some(text) = fs.load(&path).await.log_err() {
                        files.push((path, text));
                    }
                }
                planner(&files)
            })
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            open_refactor_preview(workspace, plan, window, cx)
        })
    })
    .detach_and_prompt_err(failure, window, cx, |_, _, _| None);
}

fn content_of<'a>(files: &'a [(PathBuf, String)], path: &Path) -> Result<&'a str> {
    files
        .iter()
        .find(|(candidate, _)| candidate == path)
        .map(|(_, text)| text.as_str())
        .ok_or_else(|| anyhow!("cannot read {}", path.display()))
}

fn move_class(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(file) = active_php_file(workspace, cx) else {
        return;
    };
    let root = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .find(|root| file.path.starts_with(root));
    let Some(root) = root else {
        toast(
            workspace,
            "Move Class works on files inside a project folder",
            cx,
        );
        return;
    };
    let relative = file
        .path
        .strip_prefix(&root)
        .unwrap_or(&file.path)
        .to_string_lossy()
        .into_owned();
    let source = file.path;
    PromptModal::open(
        workspace,
        "Move Class",
        "New path, relative to the project folder",
        relative,
        window,
        cx,
        move |workspace, text, window, cx| {
            let text = text.trim().to_string();
            if text.is_empty() || Path::new(&text).is_absolute() {
                toast(workspace, "Enter a path inside the project folder", cx);
                return;
            }
            let target = root.join(&text);
            let class_name = source
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (plan_source, plan_root) = (source.clone(), root.clone());
            plan_and_preview(
                workspace,
                source.clone(),
                "Failed to plan Move Class",
                window,
                cx,
                move |files| {
                    let content = content_of(files, &plan_source)?;
                    let old_namespace = namespace_of(content).ok_or_else(|| {
                        anyhow!("{} has no namespace declaration", plan_source.display())
                    })?;
                    // ponytail: the vendor prefix is the old namespace's first segment; read
                    // composer.json psr-4 when a project maps several roots.
                    let prefix = old_namespace.split('\\').next().unwrap_or_default();
                    let new_namespace = derive_psr4_namespace(&plan_root, &target, prefix);
                    Ok(plan_move_class(
                        &class_name,
                        &plan_source,
                        &target,
                        &old_namespace,
                        &new_namespace,
                        content,
                        files,
                    ))
                },
            );
        },
    );
}

/// The name in the file's `namespace X;` declaration.
pub fn namespace_of(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let name = line.trim().strip_prefix("namespace ")?;
        let name = name.trim_end().trim_end_matches([';', '{']).trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

fn safe_delete(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(file) = active_php_file(workspace, cx) else {
        return;
    };
    let initial = if file.word.is_empty() {
        file.path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        file.word
    };
    let path = file.path;
    PromptModal::open(
        workspace,
        "Safe Delete",
        "Class, function or method to delete",
        initial,
        window,
        cx,
        move |workspace, text, window, cx| {
            let symbol = text.trim().to_string();
            if symbol.is_empty() {
                return;
            }
            let plan_path = path.clone();
            plan_and_preview(
                workspace,
                path.clone(),
                "Failed to plan Safe Delete",
                window,
                cx,
                move |files| {
                    let content = content_of(files, &plan_path)?;
                    Ok(plan_safe_delete(&symbol, &plan_path, content, files))
                },
            );
        },
    );
}

fn change_signature(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(file) = active_php_file(workspace, cx) else {
        return;
    };
    let function = file.word;
    if function.is_empty() {
        toast(workspace, "Put the cursor on a function name first", cx);
        return;
    }
    let path = file.path;
    PromptModal::open(
        workspace,
        &format!("Change Signature of {function}"),
        "New order: kept parameters by 0-based index, then +[type] $name[=default]. Example: 1, 0, +int $limit=10",
        String::new(),
        window,
        cx,
        move |workspace, text, window, cx| {
            let (order, added) = match parse_signature_spec(&text) {
                Ok(spec) => spec,
                Err(error) => {
                    toast(workspace, &format!("{error}"), cx);
                    return;
                }
            };
            let plan_path = path.clone();
            let function = function.clone();
            plan_and_preview(
                workspace,
                path.clone(),
                "Failed to plan Change Signature",
                window,
                cx,
                move |files| {
                    let content = content_of(files, &plan_path)?;
                    Ok(plan_change_signature(
                        &function, &plan_path, content, &order, &added, files,
                    ))
                },
            );
        },
    );
}

/// Parses `1, 0, +int $limit=10` into kept indices and added parameters.
// ponytail: splits on every comma, so a default value containing one (`[1, 2]`) is not
// supported; add a bracket-aware split when that matters.
fn parse_signature_spec(spec: &str) -> Result<(Vec<usize>, Vec<SignatureParam>)> {
    let mut order = Vec::new();
    let mut added = Vec::new();
    for piece in spec
        .split(',')
        .map(str::trim)
        .filter(|piece| !piece.is_empty())
    {
        if let Some(declaration) = piece.strip_prefix('+') {
            let (declaration, default_value) = match declaration.split_once('=') {
                Some((left, right)) => (left.trim(), Some(right.trim().to_string())),
                None => (declaration.trim(), None),
            };
            let (type_hint, name) = match declaration.rsplit_once(char::is_whitespace) {
                Some((type_hint, name)) => (Some(type_hint.trim().to_string()), name),
                None => (None, declaration),
            };
            if name.trim_start_matches('$').is_empty() {
                bail!("'{piece}' has no parameter name");
            }
            added.push(SignatureParam {
                name: name.to_string(),
                type_hint,
                default_value,
            });
        } else {
            order.push(
                piece
                    .parse()
                    .map_err(|_| anyhow!("'{piece}' is not a parameter index"))?,
            );
        }
    }
    if order.is_empty() && added.is_empty() {
        bail!("Enter the new parameter order");
    }
    Ok((order, added))
}

// ---- Refactor This ----

const ENTRIES: [(&str, WorkspaceFn); 3] = [
    ("Move Class", move_class),
    ("Safe Delete", safe_delete),
    ("Change Signature", change_signature),
];

fn open_refactor_this(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if active_php_file(workspace, cx).is_none() {
        return;
    }
    let weak = workspace.weak_handle();
    workspace.toggle_modal(window, cx, move |window, cx| {
        RefactorThisPicker::new(weak, window, cx)
    });
}

struct RefactorThisPicker {
    picker: Entity<Picker<EntryDelegate>>,
}

impl RefactorThisPicker {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = EntryDelegate {
            owner: cx.entity().downgrade(),
            workspace,
            matches: (0..ENTRIES.len()).collect(),
            selected: 0,
        };
        Self {
            picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
        }
    }
}

impl Render for RefactorThisPicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RefactorThisPicker")
            .child(self.picker.clone())
    }
}

impl Focusable for RefactorThisPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RefactorThisPicker {}
impl ModalView for RefactorThisPicker {}

struct EntryDelegate {
    owner: WeakEntity<RefactorThisPicker>,
    workspace: WeakEntity<Workspace>,
    /// Indices into `ENTRIES` that match the query.
    matches: Vec<usize>,
    selected: usize,
}

impl PickerDelegate for EntryDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "refactor this picker"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Refactor This…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = ENTRIES
            .iter()
            .enumerate()
            .filter(|(_, (label, _))| label.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect();
        self.selected = 0;
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(&(_, run)) = self
            .matches
            .get(self.selected)
            .and_then(|&ix| ENTRIES.get(ix))
        {
            let workspace = self.workspace.clone();
            // After the picker closes, so the prompt or preview opens in the free modal slot.
            window.defer(cx, move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| run(workspace, window, cx))
                    .log_err();
            });
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.owner.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let (label, _) = ENTRIES.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(*label)),
        )
    }
}

// ---- one-line prompt ----

type Submit = Box<dyn FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>;

struct PromptModal {
    title: SharedString,
    hint: SharedString,
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    on_submit: Option<Submit>,
}

impl PromptModal {
    fn open(
        workspace: &mut Workspace,
        title: &str,
        hint: &str,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        on_submit: impl FnOnce(&mut Workspace, String, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let weak = workspace.weak_handle();
        let (title, hint) = (title.to_string(), hint.to_string());
        workspace.toggle_modal(window, cx, move |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(initial, window, cx);
                editor
            });
            Self {
                title: title.into(),
                hint: hint.into(),
                editor,
                workspace: weak,
                on_submit: Some(Box::new(on_submit)),
            }
        });
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let (Some(submit), Some(workspace)) = (self.on_submit.take(), self.workspace.upgrade())
        else {
            return;
        };
        cx.emit(DismissEvent);
        // After the prompt closes, so the preview opens in the free modal slot.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| submit(workspace, text, window, cx));
        });
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for PromptModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RefactorPrompt")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(32.))
            .p_3()
            .gap_2()
            .child(Label::new(self.title.clone()).weight(gpui::FontWeight::BOLD))
            .child(
                div()
                    .px_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(self.editor.clone()),
            )
            .child(Label::new(self.hint.clone()).color(Color::Muted))
    }
}

impl Focusable for PromptModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for PromptModal {}
impl ModalView for PromptModal {}

#[cfg(test)]
mod tests {
    use super::{namespace_of, parse_signature_spec, word_at};

    #[test]
    fn test_word_at_finds_the_identifier_under_the_cursor() {
        let line = "$total = calc_tax($sub, 2);";
        assert_eq!(word_at(line, 12), "calc_tax");
        assert_eq!(word_at(line, 9), "calc_tax", "at the start");
        assert_eq!(word_at(line, 17), "calc_tax", "just after the end");
        assert_eq!(word_at(line, 8), "");
        assert_eq!(word_at(line, 999), "");
        assert_eq!(word_at("é_x", 1), "", "not a char boundary");
    }

    #[test]
    fn test_namespace_of_reads_both_declaration_styles() {
        assert_eq!(
            namespace_of("<?php\n\nnamespace App\\Billing;\nclass A {}").as_deref(),
            Some("App\\Billing")
        );
        assert_eq!(
            namespace_of("<?php\nnamespace App {\n}").as_deref(),
            Some("App")
        );
        assert_eq!(namespace_of("<?php\nclass A {}"), None);
        assert_eq!(namespace_of("<?php\nnamespace ;"), None);
    }

    #[test]
    fn test_parse_signature_spec() {
        let (order, added) = parse_signature_spec(" 1, 0 , +?int $limit = 10, +$flag").unwrap();
        assert_eq!(order, [1, 0]);
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].type_hint.as_deref(), Some("?int"));
        assert_eq!(added[0].name, "$limit");
        assert_eq!(added[0].default_value.as_deref(), Some("10"));
        assert_eq!(added[1].type_hint, None);
        assert_eq!(added[1].default_value, None);

        assert!(parse_signature_spec("").is_err());
        assert!(parse_signature_spec("a, 1").is_err());
        assert!(parse_signature_spec("0, +$").is_err());
    }
}
