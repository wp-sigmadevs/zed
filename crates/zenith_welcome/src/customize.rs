//! Customize page: upstream onboarding sections, minus telemetry (plan 32 req. 3–5).

use client::UserStore;
use gpui::{App, Entity, IntoElement};
use onboarding::basics_page::{
    FEATURED_AGENT_IDS, render_base_keymap_section, render_import_settings_section,
    render_registry_agent_button, render_theme_section, render_vim_mode_switch,
    render_worktree_auto_trust_switch, render_zed_agent_button,
};
use project::{AgentRegistryStore, agent_server_store::AllAgentServersSettings};
use settings::SettingsStore;
use ui::prelude::*;

/// Upstream lists Claude and Codex first; the rest go under "More agents".
const PRIMARY_AGENTS: usize = 2;

pub fn render_customize(user_store: &Entity<UserStore>, cx: &mut App) -> impl IntoElement {
    let mut tab_index = 0;
    v_flex()
        .gap_6()
        .child(render_theme_section(&mut tab_index, cx))
        .child(render_base_keymap_section(&mut tab_index, cx))
        .child(render_agents(user_store, cx))
        .child(render_import_settings_section(&mut tab_index, cx))
        .child(render_vim_mode_switch(&mut tab_index, cx))
        .child(render_worktree_auto_trust_switch(&mut tab_index, cx))
}

fn render_agents(user_store: &Entity<UserStore>, cx: &mut App) -> impl IntoElement {
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
        .child(
            Label::new("Zed Agent")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(div().w_1_2().child(render_zed_agent_button(user_store, cx)))
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
