//! "PHP tools: status" (plan 44): the PHP version and where each tool resolves from, as a popover
//! anchored to the status bar item, or as a modal when that item is not drawn (a non-PHP project,
//! or the palette action).

use crate::{
    ProjectKind, Reinstall, Remove, ShowInstallLog, StatusReport, Update, collect, detect_php,
    display_path, environment, managed_bin,
};
use gpui::{
    Action as _, Anchor, App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Role, SharedString, Task, WeakEntity, Window,
};
use std::path::{Path, PathBuf};
use ui::{
    ContextMenu, PopoverMenu, PopoverMenuHandle, StatefulInteractiveElement as _, Tooltip,
    prelude::*,
};
use workspace::ModalView;

pub struct PhpStatusView {
    /// The first button, "PHP Settings…": where focus lands on open.
    settings_focus: FocusHandle,
    root: PathBuf,
    home: PathBuf,
    /// `None` while the lookup runs.
    report: Option<StatusReport>,
    more: PopoverMenuHandle<ContextMenu>,
    _load: Task<()>,
}

impl PhpStatusView {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings_focus = cx.focus_handle();
        settings_focus.focus(window, cx);
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        #[cfg(test)]
        let test_candidates = crate::TEST_CANDIDATES.with(|candidates| candidates.borrow().clone());
        let load = cx.spawn({
            let (root, home) = (root.clone(), home.clone());
            async move |this, cx| {
                let executor = cx.background_executor().clone();
                let report = cx
                    .background_spawn(async move {
                        #[cfg(test)]
                        let (candidates, path_dirs, managed) = match test_candidates {
                            // A test must not see this machine's tools.
                            Some(candidates) => (candidates, Vec::new(), None),
                            None => lookup_environment(&home),
                        };
                        #[cfg(not(test))]
                        let (candidates, path_dirs, managed) = lookup_environment(&home);
                        let info = detect_php(&executor, &root, &candidates).await;
                        let managed = managed.map(|home: PathBuf| {
                            managed_bin(&home, crate::Profile::for_project(info.kind))
                        });
                        collect(&root, &info, managed.as_deref(), &path_dirs)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.report = Some(report);
                    cx.notify();
                })
                .ok();
            }
        });
        Self {
            settings_focus,
            root,
            home,
            report: None,
            more: PopoverMenuHandle::default(),
            _load: load,
        }
    }

    fn short(&self, path: &Path) -> String {
        display_path(path, &self.root, &self.home)
    }

    fn row(
        &self,
        label: &str,
        text: &str,
        aria: String,
        value: impl IntoElement,
    ) -> impl IntoElement {
        let selector = format!("zenith-php-row {label} = {text}");
        h_flex()
            .id(SharedString::from(format!("zenith-php-status-row-{label}")))
            .debug_selector(move || selector)
            .role(Role::ListItem)
            .aria_label(aria)
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(rems(5.5))
                    .flex_none()
                    .child(Label::new(label.to_string()).color(Color::Muted)),
            )
            .child(value)
    }

    /// A path cut to the column, the full path in a tooltip, then the source as a text chip.
    fn found(
        &self,
        id: &str,
        path: &Path,
        source: Option<&'static str>,
        lead: Option<String>,
        cx: &App,
    ) -> impl IntoElement {
        let full = SharedString::from(path.display().to_string());
        let short = self.short(path);
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .when_some(lead, |this, lead| this.child(Label::new(lead).flex_none()))
            .child(
                div()
                    .id(SharedString::from(format!("zenith-php-status-path-{id}")))
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(short).size(LabelSize::Small).truncate())
                    .tooltip(Tooltip::text(full)),
            )
            .when_some(source, |this, source| {
                this.child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new(source)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
    }

    fn render_rows(&self, report: &StatusReport, cx: &App) -> Vec<gpui::AnyElement> {
        let mut rows = Vec::new();
        match &report.php {
            Some((version, path)) => rows.push(
                self.row(
                    "PHP",
                    &self.short(path),
                    format!("PHP {version}: {}", self.short(path)),
                    self.found("php", path, None, Some(version.clone()), cx),
                )
                .into_any_element(),
            ),
            None => rows.push(
                self.row(
                    "PHP",
                    "Not found",
                    "PHP: not found, install Herd or Homebrew PHP".into(),
                    h_flex()
                        .flex_1()
                        .gap_1()
                        .child(Icon::new(IconName::Warning).color(Color::Warning))
                        .child(Label::new("Not found").color(Color::Warning))
                        .child(
                            Label::new("Install Herd or Homebrew PHP")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
            ),
        }
        for (tool, resolved) in &report.tools {
            let name = tool.binary();
            rows.push(match resolved {
                Some(resolved) => self
                    .row(
                        name,
                        &self.short(&resolved.path),
                        format!(
                            "{name}: {}, {}",
                            resolved.source.label(),
                            self.short(&resolved.path)
                        ),
                        self.found(
                            name,
                            &resolved.path,
                            Some(resolved.source.label()),
                            None,
                            cx,
                        ),
                    )
                    .into_any_element(),
                None => self
                    .row(
                        name,
                        "Not found",
                        format!("{name}: not found"),
                        Label::new("Not found").color(Color::Muted),
                    )
                    .into_any_element(),
            });
        }
        rows
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
        window.dispatch_action(
            zenith_external_libraries::ConfigurePhpIncludePaths.boxed_clone(),
            cx,
        );
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(report) = &self.report {
            cx.write_to_clipboard(ClipboardItem::new_string(report.to_string()));
        }
    }
}

/// The "⋯" menu: the install actions of `zenith_php_tools`. Each closes the status first, so the
/// action runs from the editor's focus; `focus` is the status view's, which is in the workspace.
fn more_menu(
    this: WeakEntity<PhpStatusView>,
    focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, _| {
        let item = |menu: ContextMenu, label: &'static str, action: fn() -> Box<dyn gpui::Action>| {
            let this = this.clone();
            menu.entry(label, None, move |window, cx| {
                this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                window.dispatch_action(action(), cx);
            })
        };
        let menu = menu.context(focus.clone());
        let menu = item(menu, "Update PHP Tools", || Update.boxed_clone());
        let menu = item(menu, "Reinstall PHP Tools", || Reinstall.boxed_clone());
        let menu = item(menu, "Remove PHP Tools", || Remove.boxed_clone());
        item(menu, "Show Install Log", || ShowInstallLog.boxed_clone())
    })
}

/// PHP candidates, `PATH` directories and `HOME` (for the managed tools).
fn lookup_environment(home: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, Option<PathBuf>) {
    let (candidates, path_dirs) = environment();
    (
        candidates,
        path_dirs,
        (!home.as_os_str().is_empty()).then(|| home.to_path_buf()),
    )
}

impl ModalView for PhpStatusView {}

impl EventEmitter<DismissEvent> for PhpStatusView {}

impl Focusable for PhpStatusView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.settings_focus.clone()
    }
}

impl Render for PhpStatusView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let kind = self.report.as_ref().map(|report| report.kind);
        v_flex()
            .id("zenith-php-status-view")
            .key_context("ZenithPhpStatus")
            .role(Role::Dialog)
            .aria_label("PHP tools status")
            .tab_group()
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            // The base keymap's Tab is `menu::SelectNext`; nothing here selects, so it moves focus.
            .on_action(cx.listener(|_, _: &menu::SelectNext, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &menu::SelectPrevious, window, cx| window.focus_prev(cx)))
            .elevation_3(cx)
            .w(rems(30.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(Headline::new("PHP tools").size(HeadlineSize::Small))
                    .when_some(kind, |this, kind: ProjectKind| {
                        this.child(
                            Label::new(format!("{} project", kind.label()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(match &self.report {
                Some(report) => v_flex()
                    .id("zenith-php-status-rows")
                    .role(Role::List)
                    .aria_label("PHP and tool locations")
                    .gap_1()
                    .children(self.render_rows(report, cx))
                    .into_any_element(),
                None => Label::new("Looking up PHP and its tools…")
                    .color(Color::Muted)
                    .into_any_element(),
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("zenith-php-status-settings", "PHP Settings…")
                            .track_focus(&self.settings_focus)
                            .tab_index(0isize)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().debug_selector(|| "zenith-php-status-copy".into()).child(
                                IconButton::new("zenith-php-status-copy", IconName::Copy)
                                    .icon_size(IconSize::Small)
                                    .tab_index(1isize)
                                    .aria_label("Copy status")
                                    .disabled(self.report.is_none())
                                    .tooltip(Tooltip::text("Copy status"))
                                    .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                            ))
                            .child(
                                PopoverMenu::new("zenith-php-status-more")
                                    .with_handle(self.more.clone())
                                    .anchor(Anchor::BottomRight)
                                    .menu({
                                        let focus = self.settings_focus.clone();
                                        move |window, cx| {
                                            Some(more_menu(this.clone(), focus.clone(), window, cx))
                                        }
                                    })
                                    .trigger_with_tooltip(
                                        IconButton::new(
                                            "zenith-php-status-more-button",
                                            IconName::Ellipsis,
                                        )
                                        .icon_size(IconSize::Small)
                                        .tab_index(2isize)
                                        .aria_label("More PHP tools actions"),
                                        Tooltip::text("More PHP tools actions"),
                                    ),
                            ),
                    ),
            )
    }
}
