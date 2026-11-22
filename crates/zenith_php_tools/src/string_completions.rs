//! Adds completions inside a string literal (a route name, a URL path) to an editor's existing
//! completion provider. The language server and snippet completions stay as they were; these
//! come after them in the same menu.

use std::{cell::RefCell, ops::Range, rc::Rc};

use anyhow::Result;
use editor::{CompletionProvider, Editor};
use fuzzy::StringMatch;
use gpui::{App, Context, Entity, Subscription, Task, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Project, WorktreeId,
};
use util::ResultExt as _;

/// Only this much of the line before the cursor is looked at.
/// shortcut: a call and its string fit in far less; raise it if a long line loses completions.
const MAX_LINE_CHARS: usize = 1024;

/// Most completions one source adds per request; the request is marked incomplete so typing
/// more asks again.
/// shortcut: plenty for a menu; raise it if a name past the cut is wanted with a short prefix.
const MAX_COMPLETIONS: usize = 1_000;

/// What a zenith crate completes inside a string.
pub trait StringCompletionSource: 'static {
    /// The text typed so far in the string the cursor is in, when this source completes that
    /// string. `line` is the buffer line up to the cursor; `path` is the file's path in its
    /// worktree, with `/` separators.
    fn prefix<'a>(
        &self,
        worktree_id: WorktreeId,
        path: &str,
        line: &'a str,
        cx: &mut App,
    ) -> Option<&'a str>;

    /// The values that start with `prefix`.
    fn candidates(&self, prefix: &str) -> Vec<String>;
}

/// From now on, wraps the provider of every full editor of `project` so `source` adds to it.
/// Editors without a provider are left alone. Dropping the subscription stops the wrapping of
/// new editors.
pub fn add_string_completions(
    project: &Entity<Project>,
    source: Rc<dyn StringCompletionSource>,
    cx: &App,
) -> Subscription {
    let project_id = project.entity_id();
    // Only the editor handed in is touched; no entity is read while it is being created.
    cx.observe_new(move |editor: &mut Editor, _, _| {
        if !editor.mode().is_full()
            || editor
                .project()
                .is_none_or(|project| project.entity_id() != project_id)
        {
            return;
        }
        let Some(inner) = editor.zenith_completion_provider() else {
            return;
        };
        editor.set_completion_provider(Some(Rc::new(StringCompletionProvider {
            inner,
            source: source.clone(),
        })));
    })
}

struct StringCompletionProvider {
    inner: Rc<dyn CompletionProvider>,
    source: Rc<dyn StringCompletionSource>,
}

impl StringCompletionProvider {
    /// Where the string being completed starts, with the text typed in it so far.
    fn string_at(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        cx: &mut App,
    ) -> Option<(language::Anchor, String)> {
        let (snapshot, file) = {
            let buffer = buffer.read(cx);
            (buffer.text_snapshot(), buffer.file()?.clone())
        };
        let worktree_id = file.worktree_id(cx);
        let offset = position.to_offset(&snapshot);
        let mut line: Vec<char> = snapshot
            .reversed_chars_at(offset)
            .take_while(|character| *character != '\n')
            .take(MAX_LINE_CHARS)
            .collect();
        line.reverse();
        let line: String = line.into_iter().collect();
        let prefix = self
            .source
            .prefix(worktree_id, file.path().as_unix_str(), &line, cx)?;
        let start = snapshot.anchor_before(offset - prefix.len());
        Some((start, prefix.to_string()))
    }

    fn response(
        &self,
        start: language::Anchor,
        end: language::Anchor,
        prefix: &str,
    ) -> CompletionResponse {
        let completions = self
            .source
            .candidates(prefix)
            .into_iter()
            .take(MAX_COMPLETIONS)
            .map(|value| Completion {
                replace_range: start..end,
                new_text: value.clone(),
                label: CodeLabel::plain(value, None),
                documentation: None,
                source: CompletionSource::Custom,
                icon_path: None,
                icon_color: None,
                // Filter on the whole string typed so far: `users.ind`, `/blog/po`.
                match_start: Some(start),
                snippet_deduplication_key: None,
                insert_text_mode: None,
                confirm: None,
                group: None,
            })
            .collect();
        CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions::default(),
            is_incomplete: true,
        }
    }
}

impl CompletionProvider for StringCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        trigger: editor::CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let inner = self
            .inner
            .completions(buffer, buffer_position, trigger, window, cx);
        let Some((start, prefix)) = self.string_at(buffer, buffer_position, cx) else {
            return inner;
        };
        let extra = self.response(start, buffer_position, &prefix);
        cx.spawn(async move |_, _| {
            // The added ones still show when the language server fails.
            let mut responses = inner.await.log_err().unwrap_or_default();
            responses.push(extra);
            Ok(responses)
        })
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        self.inner
            .resolve_completions(buffer, completion_indices, completions, cx)
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        self.inner.apply_additional_edits_for_completion(
            buffer,
            completions,
            completion_index,
            push_to_history,
            all_commit_ranges,
            cx,
        )
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        self.inner
            .is_completion_trigger(buffer, position, text, trigger_in_words, cx)
            || self.string_at(buffer, position, cx).is_some()
    }

    fn selection_changed(&self, mat: Option<&StringMatch>, window: &mut Window, cx: &mut App) {
        self.inner.selection_changed(mat, window, cx)
    }

    fn sort_completions(&self) -> bool {
        self.inner.sort_completions()
    }

    fn filter_completions(&self) -> bool {
        self.inner.filter_completions()
    }

    fn show_snippets(&self) -> bool {
        self.inner.show_snippets()
    }
}
