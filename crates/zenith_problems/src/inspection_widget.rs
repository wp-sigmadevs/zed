//! PhpStorm's inspection widget: the active file's error, warning and weak warning counts at the
//! end of the tab bar, with previous/next problem arrows. It is a toolbar item that never takes
//! a toolbar row (location `Hidden`); the rails draw it through `zenith_rails::tab_bar_end`.

use editor::{
    Editor,
    actions::{GoToDiagnostic, GoToPreviousDiagnostic},
};
use gpui::{
    Action, Context, Entity, EventEmitter, Focusable as _, IntoElement, Render, Role, Subscription,
    WeakEntity, Window,
};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectItem as _, ProjectPath};
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use ui::{ButtonLike, KeyBinding, Tooltip, prelude::*};
use workspace::{
    ItemHandle, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace, pane,
};

use crate::{Problem, Severity, ShowFileProblems, Tab, collect, show_tab};

/// `zenith.inspection_widget.enabled`; unset means on. Read at every draw, so it applies live.
#[derive(Clone, Copy, RegisterSetting)]
struct InspectionWidgetSettings {
    enabled: bool,
}

impl Settings for InspectionWidgetSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            enabled: content
                .zenith
                .as_ref()
                .and_then(|zenith| zenith.inspection_widget.as_ref()?.enabled)
                .unwrap_or(true),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Counts {
    errors: usize,
    warnings: usize,
    /// Hints, minus the greyed-out "unnecessary code" ones.
    weak: usize,
}

impl Counts {
    fn of(problems: &[Problem]) -> Self {
        let mut counts = Self::default();
        for problem in problems {
            match problem.severity {
                Severity::Error => counts.errors += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Info if !problem.unnecessary => counts.weak += 1,
                Severity::Info => {}
            }
        }
        counts
    }

    fn is_clean(self) -> bool {
        self == Self::default()
    }

    /// The button's accessible label; the icons and colours say the same visually.
    fn label(self) -> String {
        let plural = |count: usize, one: &str, many: &str| {
            format!("{count} {}", if count == 1 { one } else { many })
        };
        let parts: Vec<String> = [
            (self.errors, "error", "errors"),
            (self.warnings, "warning", "warnings"),
            (self.weak, "weak warning", "weak warnings"),
        ]
        .into_iter()
        .filter(|(count, ..)| *count > 0)
        .map(|(count, one, many)| plural(count, one, many))
        .collect();
        if parts.is_empty() {
            "Current file: no problems".to_string()
        } else {
            format!("Current file: {}", parts.join(", "))
        }
    }
}

pub(crate) struct InspectionWidget {
    workspace: WeakEntity<Workspace>,
    pane: WeakEntity<Pane>,
    project: Entity<Project>,
    active_item: Option<Box<dyn ItemHandle>>,
    editor: Option<WeakEntity<Editor>>,
    /// Kept even while `path` is `None` (an untitled tab), so a Save As finds its way back.
    buffer: Option<WeakEntity<Buffer>>,
    path: Option<ProjectPath>,
    /// Cached; only the active-tab change and diagnostics updates of `path` recount it.
    counts: Option<Counts>,
    has_server: bool,
    /// Test seam: behave as a project without local language servers.
    #[cfg(test)]
    pretend_remote: bool,
    /// Follows `buffer`: a Save As, a rename or a new language retargets the widget.
    _buffer_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// Puts one widget at the tab-bar end of `pane`.
pub(crate) fn attach(
    pane: &Entity<Pane>,
    project: &Entity<Project>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace = cx.weak_entity();
    let weak_pane = pane.downgrade();
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let widget =
                cx.new(|cx| InspectionWidget::new(workspace, weak_pane, project.clone(), cx));
            let weak = widget.downgrade();
            zenith_rails::tab_bar_end(
                widget.clone(),
                -1,
                move |_, cx| {
                    weak.upgrade()
                        .is_some_and(|widget| widget.read(cx).shown(cx))
                },
                cx,
            );
            toolbar.add_item(widget, window, cx);
        });
    });
}

impl InspectionWidget {
    fn new(
        workspace: WeakEntity<Workspace>,
        pane: WeakEntity<Pane>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![
            cx.subscribe(&project, |this, _, event, cx| match event {
                project::Event::DiagnosticsUpdated { paths, .. }
                    if this.path.as_ref().is_some_and(|path| paths.contains(path)) =>
                {
                    this.changed(cx, |this, cx| {
                        // Remote projects have no local server list; their diagnostics prove one.
                        if this.is_remote(cx) {
                            this.has_server = true;
                        }
                        this.recount(cx);
                    });
                }
                project::Event::LanguageServerAdded(..)
                | project::Event::LanguageServerRemoved(_) => {
                    this.changed(cx, Self::recheck_server)
                }
                _ => {}
            }),
            // The setting decides at draw time, which the pane does, not this view.
            cx.observe_global::<SettingsStore>(|this, cx| this.changed(cx, |_, _| {})),
        ];
        // The pane is updated when it removes an item and `update_toolbar` does not always follow
        // (a pane that stays open when emptied), so the widget must not keep that tab alive.
        if let Some(pane) = pane.upgrade() {
            subscriptions.push(cx.subscribe(&pane, |this, _, event, cx| {
                if let pane::Event::RemovedItem { item } = event
                    && this
                        .active_item
                        .as_ref()
                        .is_some_and(|active| active.item_id() == item.item_id())
                {
                    this.active_item = None;
                    this.changed(cx, Self::retarget);
                }
            }));
        }
        Self {
            workspace,
            pane,
            project,
            active_item: None,
            editor: None,
            buffer: None,
            path: None,
            counts: None,
            has_server: false,
            #[cfg(test)]
            pretend_remote: false,
            _buffer_subscription: None,
            _subscriptions: subscriptions,
        }
    }

    fn shown(&self, cx: &App) -> bool {
        InspectionWidgetSettings::get_global(cx).enabled && self.has_server && self.counts.is_some()
    }

    /// Runs `update`, then redraws; the pane too, because it decides at its own draw whether
    /// this view is on the tab bar. Not for use while the pane is being updated.
    fn changed(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Self, &mut Context<Self>),
    ) {
        update(self, cx);
        cx.notify();
        if let Some(pane) = self.pane.upgrade() {
            pane.update(cx, |_, cx| cx.notify());
        }
    }

    fn buffer(&self) -> Option<Entity<Buffer>> {
        self.buffer.as_ref()?.upgrade()
    }

    fn is_remote(&self, cx: &App) -> bool {
        #[cfg(test)]
        if self.pretend_remote {
            return true;
        }
        !self.project.read(cx).is_local()
    }

    fn recount(&mut self, cx: &mut Context<Self>) {
        // A buffer without a file in the project has no diagnostics to count.
        self.counts = self.path.as_ref().and_then(|_| {
            self.buffer()
                .map(|buffer| Counts::of(&collect(&buffer.read(cx).snapshot(), false)))
        });
    }

    fn recheck_server(&mut self, cx: &mut Context<Self>) {
        let lsp_store = self.project.read(cx).lsp_store();
        // Only local projects list their servers; elsewhere a seen diagnostics update is sticky.
        let remote_seen = self.has_server && self.is_remote(cx);
        self.has_server = remote_seen
            || self.buffer().is_some_and(|buffer| {
                buffer.update(cx, |buffer, cx| {
                    lsp_store.update(cx, |store, cx| {
                        store
                            .running_language_servers_for_local_buffer(buffer, cx)
                            .next()
                            .is_some()
                    })
                })
            });
    }

    /// The active tab changed: everything is looked up again.
    fn retarget(&mut self, cx: &mut Context<Self>) {
        let editor = self
            .active_item
            .as_ref()
            .and_then(|item| item.act_as::<Editor>(cx));
        let buffer = editor
            .as_ref()
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
        self.path = buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).project_path(cx));
        if buffer.as_ref().map(Entity::entity_id) != self.buffer().map(|buffer| buffer.entity_id())
        {
            self.has_server = false;
            self._buffer_subscription = buffer.as_ref().map(|buffer| {
                cx.subscribe(buffer, |this, _, event: &BufferEvent, cx| {
                    if matches!(
                        event,
                        BufferEvent::FileHandleChanged | BufferEvent::LanguageChanged(_)
                    ) {
                        this.changed(cx, Self::retarget);
                    }
                })
            });
        }
        self.buffer = buffer.as_ref().map(Entity::downgrade);
        self.editor = editor.map(|editor| editor.downgrade());
        self.recount(cx);
        self.recheck_server(cx);
    }

    fn go(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        editor.update(cx, |editor, cx| {
            if forward {
                editor.go_to_diagnostic(&GoToDiagnostic::default(), window, cx);
            } else {
                editor.go_to_prev_diagnostic(&GoToPreviousDiagnostic::default(), window, cx);
            }
        });
        let focus = editor.focus_handle(cx);
        window.focus(&focus, cx);
    }

    fn show_file_problems(&self, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        // The click may come while the workspace is being updated.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                show_tab(workspace, Tab::File, window, cx)
            });
        });
    }
}

impl EventEmitter<ToolbarItemEvent> for InspectionWidget {}

impl ToolbarItemView for InspectionWidget {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_item = active_pane_item.map(ItemHandle::boxed_clone);
        // The pane is being updated here: it redraws on its own.
        self.retarget(cx);
        cx.notify();
        ToolbarItemLocation::Hidden
    }
}

impl Render for InspectionWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(counts) = self.counts else {
            return h_flex().into_any_element();
        };
        let count = |icon: IconName, color: Color, count: usize| {
            h_flex()
                .gap_0p5()
                .child(Icon::new(icon).size(IconSize::Small).color(color))
                .child(Label::new(count.to_string()).size(LabelSize::Small))
        };
        let summary = h_flex()
            .gap_1()
            .when(counts.is_clean(), |this| {
                this.child(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success),
                )
            })
            .when(counts.errors > 0, |this| {
                this.child(count(IconName::XCircle, Color::Error, counts.errors))
            })
            .when(counts.warnings > 0, |this| {
                this.child(count(IconName::Warning, Color::Warning, counts.warnings))
            })
            .when(counts.weak > 0, |this| {
                this.child(count(IconName::Info, Color::Info, counts.weak))
            });
        // The arrows act on the editor, so the shortcut is looked up in its key context.
        let editor_focus = self
            .editor
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .map(|editor| editor.focus_handle(cx));
        let clean = counts.is_clean();
        let arrow = |id: &'static str,
                     icon: IconName,
                     label: &'static str,
                     forward: bool,
                     window: &Window,
                     cx: &mut Context<Self>| {
            let action: &dyn Action = if forward {
                &GoToDiagnostic::default()
            } else {
                &GoToPreviousDiagnostic::default()
            };
            let binding = match &editor_focus {
                Some(focus) => KeyBinding::for_action_in(action, focus, cx),
                None => KeyBinding::for_action(action, cx),
            };
            let shortcut = binding.keyboard_shortcut_text(window, cx);
            let action = action.boxed_clone();
            let focus = editor_focus.clone();
            div().debug_selector(move || id.into()).child(
                ButtonLike::new(id)
                    .tab_index(0isize)
                    .disabled(clean)
                    .aria_label(label)
                    .when_some(shortcut, |this, shortcut| this.aria_keyshortcuts(shortcut))
                    .tooltip(move |_, cx| match &focus {
                        Some(focus) => Tooltip::for_action_in(label, action.as_ref(), focus, cx),
                        None => Tooltip::for_action(label, action.as_ref(), cx),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.go(forward, window, cx)))
                    .child(Icon::new(icon).size(IconSize::Small).color(if clean {
                        Color::Disabled
                    } else {
                        Color::Default
                    })),
            )
        };
        h_flex()
            .debug_selector(|| "zenith-inspection-widget".into())
            .id("zenith-inspection-widget")
            .role(Role::Group)
            .aria_label("Inspections")
            .gap_0p5()
            .child(
                div()
                    .debug_selector(|| "zenith-inspection-counts".into())
                    .child(
                        ButtonLike::new("zenith-inspection-counts")
                            .style(ButtonStyle::Subtle)
                            .tab_index(0isize)
                            .aria_label(counts.label())
                            .tooltip(|_, cx| {
                                Tooltip::for_action("File Problems", &ShowFileProblems, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_file_problems(window, cx)
                            }))
                            .child(summary),
                    ),
            )
            .child(arrow(
                "zenith-inspection-up",
                IconName::ArrowUp,
                "Previous Problem",
                false,
                window,
                cx,
            ))
            .child(arrow(
                "zenith-inspection-down",
                IconName::ArrowDown,
                "Next Problem",
                true,
                window,
                cx,
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProblemsPanel;
    use futures::StreamExt as _;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use language::{FakeLspAdapter, rust_lang};
    use lsp::{DiagnosticSeverity, DiagnosticTag, LanguageServerId};
    use project::Fs as _;
    use serde_json::json;
    use text::Point;
    use workspace::{AppState, MultiWorkspace, OpenOptions};

    struct Fixture {
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        server: LanguageServerId,
    }

    async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            crate::init(cx);
            zenith_rails::init(cx);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(
                "/p",
                json!({
                    "a.rs": "fn a() {}\nlet x = 1;\nlet y = 2;\nlet z = 3;\n",
                    "b.rs": "fn b() {}\nlet w = 4;\n",
                    "c.txt": "plain\n",
                }),
            )
            .await;
        let project = Project::test(app_state.fs.clone(), ["/p".as_ref()], cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        languages.add(rust_lang());
        let mut servers = languages.register_fake_lsp("Rust", FakeLspAdapter::default());
        let (multi, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let mut fixture = Fixture {
            project,
            workspace,
            server: LanguageServerId(0),
        };
        open(&fixture, cx, "/p/a.rs").await;
        fixture.server = servers
            .next()
            .await
            .expect("rust server starts")
            .server
            .server_id();
        cx.run_until_parked();
        (fixture, cx)
    }

    async fn open(fixture: &Fixture, cx: &mut VisualTestContext, path: &str) -> Entity<Editor> {
        let editor = fixture
            .workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path.into(), OpenOptions::default(), window, cx)
            })
            .await
            .expect("file opens")
            .downcast::<Editor>()
            .expect("an editor");
        cx.run_until_parked();
        editor
    }

    fn diagnostic(
        line: u32,
        column: u32,
        severity: DiagnosticSeverity,
        tags: Option<Vec<DiagnosticTag>>,
    ) -> lsp::Diagnostic {
        lsp::Diagnostic {
            range: lsp::Range::new(
                lsp::Position::new(line, column),
                lsp::Position::new(line, column + 1),
            ),
            severity: Some(severity),
            message: format!("problem at {line}:{column}").into(),
            tags,
            ..Default::default()
        }
    }

    fn publish(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
        path: &str,
        diagnostics: Vec<lsp::Diagnostic>,
    ) {
        let lsp_store = fixture
            .project
            .read_with(cx, |project, _| project.lsp_store());
        lsp_store.update(cx, |lsp_store, cx| {
            lsp_store
                .update_diagnostics(
                    fixture.server,
                    lsp::PublishDiagnosticsParams {
                        uri: lsp::Uri::from_file_path(path).unwrap(),
                        diagnostics,
                        version: None,
                    },
                    None,
                    language::DiagnosticSourceKind::Pushed,
                    &[],
                    cx,
                )
                .unwrap();
        });
        cx.run_until_parked();
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn widget(fixture: &Fixture, cx: &mut VisualTestContext) -> Entity<InspectionWidget> {
        fixture.workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            pane.toolbar()
                .read(cx)
                .item_of_type::<InspectionWidget>()
                .expect("every pane has a widget")
        })
    }

    fn counts(fixture: &Fixture, cx: &mut VisualTestContext) -> Option<Counts> {
        widget(fixture, cx).read_with(cx, |widget, _| widget.counts)
    }

    fn on_the_tab_bar(cx: &mut VisualTestContext) -> bool {
        draw(cx);
        cx.debug_bounds("zenith-tab-bar-end").is_some()
            && cx.debug_bounds("zenith-inspection-widget").is_some()
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn set_enabled(cx: &mut VisualTestContext, enabled: bool) {
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content
                        .zenith
                        .get_or_insert_default()
                        .inspection_widget
                        .get_or_insert_default()
                        .enabled = Some(enabled);
                });
            });
        });
        cx.run_until_parked();
    }

    fn head(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        })
    }

    #[gpui::test]
    async fn counts_follow_the_active_file_and_diagnostic_updates(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        assert_eq!(counts(&fixture, cx), Some(Counts::default()));
        assert!(on_the_tab_bar(cx));
        publish(
            &fixture,
            cx,
            "/p/a.rs",
            vec![
                diagnostic(1, 4, DiagnosticSeverity::ERROR, None),
                diagnostic(2, 0, DiagnosticSeverity::WARNING, None),
                diagnostic(3, 2, DiagnosticSeverity::HINT, None),
                // Greyed-out unused code is not a weak warning: up/down would not reach it.
                diagnostic(
                    3,
                    0,
                    DiagnosticSeverity::HINT,
                    Some(vec![DiagnosticTag::UNNECESSARY]),
                ),
            ],
        );
        let expected = Counts {
            errors: 1,
            warnings: 1,
            weak: 1,
        };
        assert_eq!(counts(&fixture, cx), Some(expected));
        assert!(on_the_tab_bar(cx));
        publish(
            &fixture,
            cx,
            "/p/b.rs",
            vec![
                diagnostic(0, 3, DiagnosticSeverity::ERROR, None),
                diagnostic(1, 0, DiagnosticSeverity::ERROR, None),
            ],
        );
        assert_eq!(
            counts(&fixture, cx),
            Some(expected),
            "b.rs is not the active file"
        );
        open(&fixture, cx, "/p/b.rs").await;
        assert_eq!(
            counts(&fixture, cx),
            Some(Counts {
                errors: 2,
                warnings: 0,
                weak: 0
            })
        );
        publish(&fixture, cx, "/p/b.rs", vec![]);
        assert_eq!(counts(&fixture, cx), Some(Counts::default()));
        // The widget never takes a toolbar row.
        let widget = widget(&fixture, cx);
        let item = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item(cx))
            .expect("an active tab");
        let location = widget.update_in(cx, |widget, window, cx| {
            widget.set_active_pane_item(Some(item.as_ref()), window, cx)
        });
        assert_eq!(location, ToolbarItemLocation::Hidden);
    }

    #[gpui::test]
    async fn a_file_without_a_language_server_shows_nothing(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        assert!(on_the_tab_bar(cx));
        open(&fixture, cx, "/p/c.txt").await;
        assert!(!on_the_tab_bar(cx));
        open(&fixture, cx, "/p/a.rs").await;
        assert!(on_the_tab_bar(cx));
    }

    #[gpui::test]
    async fn down_then_up_move_to_the_diagnostics_and_focus_the_editor(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        let editor = open(&fixture, cx, "/p/a.rs").await;
        publish(
            &fixture,
            cx,
            "/p/a.rs",
            vec![
                diagnostic(1, 4, DiagnosticSeverity::ERROR, None),
                diagnostic(2, 0, DiagnosticSeverity::WARNING, None),
            ],
        );
        // Focus lives elsewhere, so landing in the editor is the buttons' doing.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<ProblemsPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(!editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));
        click(cx, "zenith-inspection-down");
        assert_eq!(head(&editor, cx), Point::new(1, 4));
        assert!(editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));
        click(cx, "zenith-inspection-down");
        assert_eq!(head(&editor, cx), Point::new(2, 0));
        click(cx, "zenith-inspection-up");
        assert_eq!(head(&editor, cx), Point::new(1, 4));
    }

    #[gpui::test]
    async fn disabled_draws_nothing_and_keeps_the_stock_buttons(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        assert!(on_the_tab_bar(cx));
        set_enabled(cx, false);
        draw(cx);
        assert!(cx.debug_bounds("zenith-inspection-widget").is_none());
        assert!(
            cx.debug_bounds("zenith-tab-bar-end").is_none(),
            "no end slot of ours; the pane keeps its stock buttons"
        );
        set_enabled(cx, true);
        assert!(on_the_tab_bar(cx));
        drop(fixture);
    }

    #[gpui::test]
    async fn clicking_the_counts_shows_the_file_tab_of_the_problems_panel(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        publish(
            &fixture,
            cx,
            "/p/a.rs",
            vec![diagnostic(1, 4, DiagnosticSeverity::ERROR, None)],
        );
        let panel = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ProblemsPanel>(cx))
            .expect("panel is registered");
        panel.update(cx, |panel, cx| panel.set_tab(Tab::Project, cx));
        click(cx, "zenith-inspection-counts");
        assert!(panel.read_with(cx, |panel, _| panel.tab == Tab::File));
        let focused = panel.update_in(cx, |panel, window, cx| {
            panel.focus_handle(cx).contains_focused(window, cx)
        });
        assert!(focused, "the Problems panel has focus");
    }

    #[gpui::test]
    async fn save_as_shows_the_widget_without_switching_tabs(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            Editor::new_file(workspace, &workspace::NewFile, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(counts(&fixture, cx), None, "an untitled tab has no file");
        assert!(!on_the_tab_bar(cx));
        let buffer = fixture
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<Editor>(cx).expect("the new tab")
            })
            .read_with(cx, |editor, cx| {
                editor.buffer().read(cx).as_singleton().expect("one buffer")
            });
        let worktree_id = fixture.project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id()
        });
        let path = ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path("new.rs").into(),
        };
        fixture
            .project
            .update(cx, |project, cx| project.save_buffer_as(buffer, path, cx))
            .await
            .expect("saves");
        cx.run_until_parked();
        assert_eq!(counts(&fixture, cx), Some(Counts::default()));
        assert!(on_the_tab_bar(cx));
    }

    #[gpui::test]
    async fn a_renamed_file_keeps_counting(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        let fs = fixture
            .project
            .read_with(cx, |project, _| project.fs().clone());
        fs.as_fake()
            .rename("/p/a.rs".as_ref(), "/p/d.rs".as_ref(), Default::default())
            .await
            .expect("renames");
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        publish(
            &fixture,
            cx,
            "/p/d.rs",
            vec![diagnostic(1, 4, DiagnosticSeverity::ERROR, None)],
        );
        assert_eq!(
            counts(&fixture, cx),
            Some(Counts {
                errors: 1,
                warnings: 0,
                weak: 0
            })
        );
    }

    #[gpui::test]
    async fn a_pane_that_loses_its_last_item_without_closing_lets_go_of_it(
        cx: &mut TestAppContext,
    ) {
        let (fixture, cx) = fixture(cx).await;
        let pane = fixture
            .workspace
            .read_with(cx, |workspace, _| workspace.active_pane().clone());
        let item_id = fixture
            .workspace
            .read_with(cx, |workspace, cx| workspace.active_item(cx))
            .expect("an active tab")
            .item_id();
        pane.update_in(cx, |pane, window, cx| {
            pane.remove_item(item_id, false, false, window, cx)
        });
        cx.run_until_parked();
        let widget = widget(&fixture, cx);
        assert!(widget.read_with(cx, |widget, _| widget.active_item.is_none()));
        assert_eq!(counts(&fixture, cx), None);
    }

    #[gpui::test]
    async fn diagnostics_prove_a_server_where_none_is_listed(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        widget(&fixture, cx).update(cx, |widget, _| widget.pretend_remote = true);
        open(&fixture, cx, "/p/c.txt").await;
        assert!(!on_the_tab_bar(cx));
        publish(
            &fixture,
            cx,
            "/p/c.txt",
            vec![diagnostic(0, 0, DiagnosticSeverity::ERROR, None)],
        );
        assert!(on_the_tab_bar(cx));
        publish(&fixture, cx, "/p/c.txt", vec![]);
        assert!(on_the_tab_bar(cx), "a clean file keeps its widget");
    }

    #[gpui::test]
    async fn the_arrows_are_inert_while_the_file_is_clean(cx: &mut TestAppContext) {
        let (fixture, cx) = fixture(cx).await;
        let editor = open(&fixture, cx, "/p/a.rs").await;
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<ProblemsPanel>(window, cx);
        });
        cx.run_until_parked();
        click(cx, "zenith-inspection-down");
        assert!(!editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));
    }

    #[test]
    fn the_label_names_every_nonzero_count() {
        let label = |errors, warnings, weak| {
            Counts {
                errors,
                warnings,
                weak,
            }
            .label()
        };
        assert_eq!(
            label(2, 1, 3),
            "Current file: 2 errors, 1 warning, 3 weak warnings"
        );
        assert_eq!(label(1, 0, 1), "Current file: 1 error, 1 weak warning");
        assert_eq!(label(0, 0, 0), "Current file: no problems");
    }
}
