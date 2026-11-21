//! PhpStorm-style bulb (plan M5): the code-actions menu always ends with
//! "AI Actions…", which opens the inline assistant for the selection. The
//! lightbulb icon itself is `IconName::ZenithLightbulb`.

use agent_settings::AgentSettings;
use anyhow::Result;
use editor::CodeActionProvider;
use gpui::{Action as _, App, Entity, Task, Window};
use language::Buffer;
use project::{CodeAction, LspAction, ProjectTransaction};
use settings::Settings as _;
use std::{ops::Range, rc::Rc, sync::Arc};

const PROVIDER_ID: &str = "zenith_look_ai_actions";

/// Not a language server (they count up from 0); marks the action as ours.
const SERVER_ID: lsp::LanguageServerId = lsp::LanguageServerId(usize::MAX - 3);

struct AiActionsProvider;

impl CodeActionProvider for AiActionsProvider {
    fn id(&self) -> Arc<str> {
        PROVIDER_ID.into()
    }

    fn code_actions(
        &self,
        _: &Entity<Buffer>,
        range: Range<text::Anchor>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        if !AgentSettings::get_global(cx).enabled(cx) {
            return Task::ready(Ok(Vec::new()));
        }
        Task::ready(Ok(vec![CodeAction {
            server_id: SERVER_ID,
            range,
            lsp_action: LspAction::Action(Box::new(lsp::CodeAction {
                title: "AI Actions…".into(),
                kind: Some(lsp::CodeActionKind::EMPTY),
                ..lsp::CodeAction::default()
            })),
            resolved: true,
        }]))
    }

    fn apply_code_action(
        &self,
        _: Entity<Buffer>,
        _: CodeAction,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<ProjectTransaction>> {
        // The editor is still focused, so the existing inline assist action
        // reaches the workspace handler and works on its selection.
        let inline_assist = zed_actions::assistant::InlineAssist::default().boxed_clone();
        window.dispatch_action(inline_assist, cx);
        Task::ready(Ok(ProjectTransaction::default()))
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut editor::Editor, _, _| {
        if editor.mode().is_full() && editor.project().is_some() {
            editor.add_code_action_provider(Rc::new(AiActionsProvider));
        }
    })
    .detach();
}
