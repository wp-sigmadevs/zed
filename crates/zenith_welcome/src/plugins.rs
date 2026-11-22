//! Plugins page: the extensions zed-kit installs for PHP/web work, with their
//! install state read from the extension store.

use std::{collections::HashMap, sync::Arc};

use extension_host::{ExtensionStatus, ExtensionStore};
use gpui::{Action as _, App, IntoElement, TaskExt as _};
use ui::{ListItem, ListItemSpacing, TintColor, prelude::*};
use zed_actions::Extensions;

/// Mirrors `auto_install_extensions` in zed-kit's `settings.global.jsonc`.
pub const RECOMMENDED: &[(&str, &str, &str)] = &[
    ("php", "PHP", "Language server, syntax and snippets"),
    ("blade", "Blade", "Laravel Blade templates"),
    ("twig", "Twig", "Twig templates"),
    ("scss", "SCSS", "Sass stylesheets"),
    ("dockerfile", "Dockerfile", "Docker build files"),
    ("make", "Make", "Makefiles"),
    ("http", "HTTP", "Send requests from .http files"),
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PluginAction {
    Install,
    Busy(&'static str),
    Installed,
    /// Newer version on the extension server.
    Update(Arc<str>),
}

/// `update` is the newest version the server offers, if any.
pub(crate) fn plugin_action(status: &ExtensionStatus, update: Option<&Arc<str>>) -> PluginAction {
    match status {
        ExtensionStatus::NotInstalled => PluginAction::Install,
        ExtensionStatus::Installing => PluginAction::Busy("Installing…"),
        ExtensionStatus::Upgrading => PluginAction::Busy("Updating…"),
        ExtensionStatus::Removing => PluginAction::Busy("Removing…"),
        ExtensionStatus::Installed(installed) => match update {
            Some(version) if version != installed => PluginAction::Update(version.clone()),
            _ => PluginAction::Installed,
        },
    }
}

/// `updates`: extension id → newer version, from `fetch_extensions_with_update_available`.
pub fn render_plugins(updates: &HashMap<Arc<str>, Arc<str>>, cx: &mut App) -> impl IntoElement {
    let store = ExtensionStore::try_global(cx);
    v_flex()
        .gap_2()
        .max_w(rems(40.))
        .child(Label::new("Recommended for PHP and web projects").color(Color::Muted))
        .children(
            RECOMMENDED
                .iter()
                .enumerate()
                .map(|(ix, (id, name, description))| {
                    let status = store
                        .as_ref()
                        .map(|store| store.read(cx).extension_status(id))
                        .unwrap_or(ExtensionStatus::NotInstalled);
                    let action = plugin_action(&status, updates.get(*id));
                    ListItem::new(*id)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .child(
                            v_flex().child(Label::new(*name)).child(
                                Label::new(*description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                        .end_slot(render_action_button(ix, id, name, action))
                        .on_click(move |_, window, cx| {
                            let action = Extensions {
                                id: Some(id.to_string()),
                                ..Default::default()
                            };
                            window.dispatch_action(action.boxed_clone(), cx);
                        })
                }),
        )
        .child(h_flex().pt_2().child(
            Button::new("browse-extensions", "Browse All Extensions").on_click(|_, window, cx| {
                window.dispatch_action(Extensions::default().boxed_clone(), cx)
            }),
        ))
}

fn render_action_button(
    ix: usize,
    id: &'static str,
    name: &'static str,
    action: PluginAction,
) -> Button {
    let element_id = ("plugin-action", ix);
    match action {
        // Same store calls as extensions_ui's extension card.
        PluginAction::Install => Button::new(element_id, "Install")
            .style(ButtonStyle::Tinted(TintColor::Accent))
            .aria_label(format!("Install {name}"))
            .on_click(move |_, _, cx| {
                if let Some(store) = ExtensionStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store.install_latest_extension(Arc::from(id), cx)
                    });
                }
            }),
        PluginAction::Update(version) => Button::new(element_id, "Update")
            .style(ButtonStyle::Tinted(TintColor::Accent))
            .aria_label(format!("Update {name} to {version}"))
            .on_click(move |_, _, cx| {
                if let Some(store) = ExtensionStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store
                            .upgrade_extension(Arc::from(id), version.clone(), cx)
                            .detach_and_log_err(cx)
                    });
                }
            }),
        PluginAction::Busy(label) => Button::new(element_id, label).disabled(true),
        PluginAction::Installed => Button::new(element_id, "Installed")
            .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
            .aria_label(format!("{name} installed"))
            .disabled(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_action_follows_store_status() {
        let v1: Arc<str> = "1.0.0".into();
        let v2: Arc<str> = "1.1.0".into();
        assert_eq!(
            plugin_action(&ExtensionStatus::NotInstalled, None),
            PluginAction::Install
        );
        assert_eq!(
            plugin_action(&ExtensionStatus::Installed(v1.clone()), None),
            PluginAction::Installed
        );
        assert_eq!(
            plugin_action(&ExtensionStatus::Installed(v1.clone()), Some(&v1)),
            PluginAction::Installed
        );
        assert_eq!(
            plugin_action(&ExtensionStatus::Installed(v1), Some(&v2)),
            PluginAction::Update(v2)
        );
        assert!(matches!(
            plugin_action(&ExtensionStatus::Installing, None),
            PluginAction::Busy(_)
        ));
    }
}
