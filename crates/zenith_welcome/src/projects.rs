use std::{collections::HashMap, path::PathBuf, sync::Arc};

use crate::detect::ProjectInfo;
use chrono::{DateTime, Utc};
use fuzzy_nucleo::{
    MaxResults, StringMatch, StringMatchCandidate, StringMatchOptions, match_strings,
};
use gpui::{Action as _, App, Context, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use settings::Settings as _;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::{
    OpenMode, RecentWorkspace, SerializedWorkspaceLocation, Workspace, WorkspaceSettings,
};

pub struct ProjectsDelegate {
    workspace: WeakEntity<Workspace>,
    projects: Vec<RecentWorkspace>,
    info: HashMap<PathBuf, ProjectInfo>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl ProjectsDelegate {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            projects: Vec::new(),
            info: HashMap::new(),
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    pub fn set_projects(&mut self, projects: Vec<RecentWorkspace>) {
        self.projects = projects;
    }

    pub fn set_info(&mut self, info: HashMap<PathBuf, ProjectInfo>) {
        self.info = info;
    }

    #[cfg(test)]
    pub fn info(&self, root: &std::path::Path) -> Option<&ProjectInfo> {
        self.info.get(root)
    }

    #[cfg(test)]
    pub fn matched_paths(&self) -> Vec<String> {
        self.matches.iter().map(|m| m.string.to_string()).collect()
    }
}

fn search_text(project: &RecentWorkspace) -> String {
    project
        .identity_paths
        .ordered_paths()
        .map(|path| path.compact().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

fn project_name(project: &RecentWorkspace) -> String {
    project
        .identity_paths
        .ordered_paths()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where opened projects go, per the `default_open_behavior` setting.
pub fn open_mode(cx: &App) -> OpenMode {
    match WorkspaceSettings::get_global(cx).default_open_behavior {
        settings::DefaultOpenBehavior::ExistingWindow => OpenMode::Activate,
        settings::DefaultOpenBehavior::NewWindow => OpenMode::NewWindow,
    }
}

/// "just now", "5m ago", "2h ago", "yesterday", "3d ago", or the date.
pub fn relative_time(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    match delta.num_minutes() {
        m if m < 1 => "just now".into(),
        m if m < 60 => format!("{m}m ago"),
        m if m < 60 * 24 => format!("{}h ago", m / 60),
        m if m < 60 * 48 => "yesterday".into(),
        m if m < 60 * 24 * 30 => format!("{}d ago", m / (60 * 24)),
        _ => then.format("%Y-%m-%d").to_string(),
    }
}

impl PickerDelegate for ProjectsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "zenith welcome projects"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates: Vec<_> = self
            .projects
            .iter()
            .enumerate()
            .map(|(ix, project)| StringMatchCandidate::new(ix, &search_text(project)))
            .collect();
        let query = query.trim();
        self.matches = if query.is_empty() {
            candidates
                .into_iter()
                .map(|candidate| StringMatch {
                    candidate_id: candidate.id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: candidate.string,
                })
                .collect()
        } else {
            match_strings(
                &candidates,
                query,
                StringMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    ..Default::default()
                },
            )
        };
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(project) = self
            .matches
            .get(self.selected_index)
            .and_then(|hit| self.projects.get(hit.candidate_id))
        else {
            return;
        };
        if !matches!(project.location, SerializedWorkspaceLocation::Local) {
            // Remote projects need upstream's connection flow.
            window.dispatch_action(zed_actions::OpenRecent::default().boxed_clone(), cx);
            return;
        }
        let paths = project.paths.paths().to_vec();
        self.workspace
            .update(cx, |workspace, cx| {
                crate::open_choice::open_with_choice(paths, workspace, window, cx);
            })
            .log_err();
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(if self.projects.is_empty() {
            "Recently opened projects will show up here".into()
        } else {
            "No matches".into()
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let project = self.projects.get(hit.candidate_id)?;
        let info = project
            .paths
            .paths()
            .first()
            .and_then(|root| self.info.get(root));
        let mut meta = relative_time(project.timestamp, Utc::now());
        if let Some(branch) = info.and_then(|info| info.branch.as_deref()) {
            meta = format!("{branch} · {meta}");
        }
        if let SerializedWorkspaceLocation::Remote(options) = &project.location {
            meta = format!("{} · {meta}", options.display_name());
        }
        Some(
            ListItem::new(ix)
                .toggle_state(selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(project_name(project)))
                                .children(info.and_then(|info| info.framework).map(|framework| {
                                    Label::new(framework.label())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent)
                                })),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    HighlightedLabel::new(
                                        hit.string.clone(),
                                        hit.positions.clone(),
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                                )
                                .child(Label::new(meta).size(LabelSize::Small).color(Color::Muted)),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - Duration::minutes(5), now), "5m ago");
        assert_eq!(relative_time(now - Duration::hours(2), now), "2h ago");
        assert_eq!(relative_time(now - Duration::hours(30), now), "yesterday");
        assert_eq!(relative_time(now - Duration::days(3), now), "3d ago");
        let old = now - Duration::days(90);
        assert_eq!(relative_time(old, now), old.format("%Y-%m-%d").to_string());
    }
}
