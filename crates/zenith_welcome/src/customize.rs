//! Customize page: upstream onboarding sections, minus telemetry (plan 32 req. 3–5).

use fs::Fs;
use gpui::{App, IntoElement};
use onboarding::basics_page::{
    FEATURED_AGENT_IDS, render_import_settings_section, render_registry_agent_button,
    render_theme_section, render_vim_mode_switch, render_worktree_auto_trust_switch,
};
use project::{AgentRegistryStore, agent_server_store::AllAgentServersSettings};
use settings::{BaseKeymap, Settings as _, SettingsStore, update_settings_file};
use ui::{
    ToggleButtonGroup, ToggleButtonGroupSize, ToggleButtonGroupStyle, ToggleButtonWithIcon,
    prelude::*,
};

/// Upstream lists Claude and Codex first; the rest go under "More agents".
const PRIMARY_AGENTS: usize = 2;

pub fn render_customize(cx: &mut App) -> impl IntoElement {
    let mut tab_index = 0;
    v_flex()
        .gap_6()
        .child(render_theme_section(&mut tab_index, cx))
        .child(render_base_keymap_section(&mut tab_index, cx))
        .child(render_agents(cx))
        .child(render_import_settings_section(&mut tab_index, cx))
        .child(render_vim_mode_switch(&mut tab_index, cx))
        .child(render_worktree_auto_trust_switch(&mut tab_index, cx))
}

fn render_agents(cx: &mut App) -> impl IntoElement {
    let registry = AgentRegistryStore::try_global(cx)
        .map(|store| store.read(cx).agents().to_vec())
        .unwrap_or_default();
    let installed = cx
        .global::<SettingsStore>()
        .get::<AllAgentServersSettings>(None)
        .clone();
    let (primary, more) = FEATURED_AGENT_IDS.split_at(PRIMARY_AGENTS.min(FEATURED_AGENT_IDS.len()));

    let grid = |ids: &[&str], cx: &mut App| {
        ids.iter().fold(
            div().w_full().grid().grid_cols(2).gap_2(),
            |grid, id| match registry.iter().find(|agent| agent.id().as_ref() == *id) {
                Some(agent) => grid.child(render_registry_agent_button(
                    agent,
                    installed.contains_key(*id),
                    cx,
                )),
                None => grid,
            },
        )
    };
    let primary = grid(primary, cx);
    let more = grid(more, cx);

    v_flex()
        .gap_1p5()
        .child(Label::new("Agents"))
        .child(Label::new("Install an agent and start your first thread.").color(Color::Muted))
        .child(primary)
        .child(
            Label::new("More agents")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(more)
}

/// Upstream's keymap picker with Zed's default keymap labelled as Zenith's own; no telemetry.
fn render_base_keymap_section(tab_index: &mut isize, cx: &mut App) -> impl IntoElement {
    const KEYMAPS: [(&str, IconName, BaseKeymap); 8] = [
        ("Zenith (default)", IconName::Sparkle, BaseKeymap::Zed),
        ("VS Code", IconName::EditorVsCode, BaseKeymap::VSCode),
        (
            "JetBrains",
            IconName::EditorJetBrains,
            BaseKeymap::JetBrains,
        ),
        (
            "Sublime Text",
            IconName::EditorSublime,
            BaseKeymap::SublimeText,
        ),
        ("Atom", IconName::EditorAtom, BaseKeymap::Atom),
        ("Emacs", IconName::EditorEmacs, BaseKeymap::Emacs),
        ("Cursor", IconName::EditorCursor, BaseKeymap::Cursor),
        ("TextMate", IconName::Keyboard, BaseKeymap::TextMate),
    ];
    let current = *BaseKeymap::get_global(cx);
    let button = |index: usize| {
        let (label, icon, keymap) = KEYMAPS[index];
        ToggleButtonWithIcon::new(label, icon, move |_, _, cx| {
            update_settings_file(<dyn Fs>::global(cx), cx, move |setting, _| {
                setting.base_keymap = Some(keymap.into());
            });
        })
    };

    v_flex().gap_2().child(Label::new("Base Keymap")).child(
        ToggleButtonGroup::two_rows(
            "base_keymap_selection",
            [button(0), button(1), button(2), button(3)],
            [button(4), button(5), button(6), button(7)],
        )
        .when_some(
            KEYMAPS.iter().position(|(_, _, keymap)| *keymap == current),
            |this, index| this.selected_index(index),
        )
        .full_width()
        .tab_index(tab_index)
        .size(ToggleButtonGroupSize::Medium)
        .style(ToggleButtonGroupStyle::Outlined),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_lead_the_agent_list() {
        assert_eq!(
            &FEATURED_AGENT_IDS[..PRIMARY_AGENTS],
            &["claude-acp", "codex-acp"],
            "upstream reordered FEATURED_AGENT_IDS; update PRIMARY_AGENTS",
        );
    }
}
