//! PhpStorm's Find in Files / Replace in Files popup (⌘⇧F / ⌘⇧R): a header with the
//! live match count and the file mask, a query box with case, word and regex toggles,
//! an In Project / Directory scope, the matching lines, a code preview of the picked
//! one, and a hand-off to the project search tab ("Open in Find Window") that does
//! the replacing.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use editor::{Editor, EditorEvent, SelectionEffects, scroll::Autoscroll};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Div, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, IntoElement, ParentElement, Render, Role, ScrollStrategy, SharedString, Styled,
    StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, div, prelude::*,
    px, uniform_list,
};
use language::{BufferSnapshot, HighlightId, LanguageAwareStyling};
use picker::{MatchLocation, PreviewBackend, PreviewLayout, PreviewUpdate};
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use settings::SeedQuerySetting;
use text::{Point, ToOffset as _, ToPoint as _};
use ui::{
    Button, ButtonSize, ButtonStyle, Checkbox, ContextMenu, IconButton, IconPosition, Label,
    PopoverMenu, PopoverMenuHandle, ScrollAxes, Scrollbars, TintColor, Tooltip, WithScrollbar,
    prelude::*,
};
use util::paths::PathMatcher;
use workspace::{
    DismissDecision, ModalView, Workspace, pane::DeploySearch, searchable::SearchableItem,
};

use super::{
    CycleFindContext, CycleFindScope, ToggleFileMaskHistory, ToggleFindCase, ToggleFindMask,
    ToggleFindRegex, ToggleFindWord, TogglePinFindPopup, mask_history,
    popup::{Frame, PopupKind},
    style::{
        FIELD_HEIGHT, FOOTER_HEIGHT, HEADER_GAP, HEADER_HEIGHT, MASK_WIDTH, PADDING, POPUP_BAND,
        POPUP_FOOTER, PREVIEW_HEIGHT, ROW_HEIGHT, SEARCH_BOX_HEIGHT, VISIBLE_ROWS,
    },
};

/// More hits than this say "refine the query", not "scroll".
const MAX_HITS: usize = 300;

/// Where the search looks: PhpStorm's In Project | Module | Directory | Scope tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    Project,
    /// The worktree of the file that was active when the popup opened.
    Module,
    Directory,
    /// PhpStorm's named scopes; "Open Files" is the one this popup offers.
    OpenFiles,
}

impl Scope {
    const ALL: [Self; 4] = [
        Self::Project,
        Self::Module,
        Self::Directory,
        Self::OpenFiles,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Project => "In Project",
            Self::Module => "Module",
            Self::Directory => "Directory",
            Self::OpenFiles => "Scope",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Project => "zenith-find-in-project",
            Self::Module => "zenith-find-in-module",
            Self::Directory => "zenith-find-in-directory",
            Self::OpenFiles => "zenith-find-in-scope",
        }
    }
}

/// What a match sits in, by the line heuristic of [`span_at`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Span {
    Code,
    Comment,
    String,
}

/// Which matches count, from PhpStorm's funnel menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SearchContext {
    #[default]
    Anywhere,
    InComments,
    InStringLiterals,
    ExceptComments,
    ExceptStringLiterals,
    ExceptCommentsAndStringLiterals,
}

impl SearchContext {
    const ALL: [Self; 6] = [
        Self::Anywhere,
        Self::InComments,
        Self::InStringLiterals,
        Self::ExceptComments,
        Self::ExceptStringLiterals,
        Self::ExceptCommentsAndStringLiterals,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Anywhere => "Anywhere",
            Self::InComments => "In Comments",
            Self::InStringLiterals => "In String Literals",
            Self::ExceptComments => "Except Comments",
            Self::ExceptStringLiterals => "Except String Literals",
            Self::ExceptCommentsAndStringLiterals => "Except Comments and String Literals",
        }
    }

    fn allows(self, span: Span) -> bool {
        match self {
            Self::Anywhere => true,
            Self::InComments => span == Span::Comment,
            Self::InStringLiterals => span == Span::String,
            Self::ExceptComments => span != Span::Comment,
            Self::ExceptStringLiterals => span != Span::String,
            Self::ExceptCommentsAndStringLiterals => span == Span::Code,
        }
    }

    fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }
}

pub struct FindInFilesModal {
    query: Entity<Editor>,
    mask: Entity<Editor>,
    directory: Entity<Editor>,
    replace: Option<Entity<Editor>>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    previous_focus: FocusHandle,
    /// The picked hit's code, drawn under the list.
    preview: Arc<dyn PreviewBackend>,
    scroll: UniformListScrollHandle,
    hits: Vec<Hit>,
    selected: usize,
    /// The search stopped at [`MAX_HITS`].
    capped: bool,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    /// The file mask only narrows the search while its box is ticked.
    mask_enabled: bool,
    /// The dropdown of recently used masks.
    mask_menu: PopoverMenuHandle<ContextMenu>,
    scope: Scope,
    /// The funnel: which kinds of match are listed.
    context: SearchContext,
    /// Pinned, the popup stays open when focus leaves it or the pointer clicks outside.
    pinned: bool,
    /// Set when the popup is closed on purpose (Esc, opening a hit), which pinning
    /// does not hold back.
    closing: bool,
    /// The worktree the Module tab searches: that of the file active at open.
    module_root: Option<String>,
    /// The tabs open at that moment: what the Scope tab searches.
    open_buffers: Vec<Entity<language::Buffer>>,
    /// Enter sends the results to a project search tab instead of opening one hit.
    open_in_tab: bool,
    search: Task<()>,
    /// The card: dragging the header moves it, the corner grip resizes it.
    frame: Frame,
    #[cfg(test)]
    previewed: Option<(ProjectPath, u32)>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for FindInFilesModal {
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        DismissDecision::Dismiss(!self.pinned || self.closing)
    }
}
impl EventEmitter<DismissEvent> for FindInFilesModal {}

impl Focusable for FindInFilesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

impl FindInFilesModal {
    pub fn open(
        workspace: &mut Workspace,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let previous_focus = window
            .focused(cx)
            .unwrap_or_else(|| workspace.focus_handle(cx));
        let workspace_handle = cx.entity().downgrade();
        let project = workspace.project().clone();
        // Opening again closes it, as for every modal, pinned or not.
        if let Some(open) = workspace.active_modal::<Self>(cx) {
            open.update(cx, |open, _| open.closing = true);
        }
        mask_history::load(cx);
        let module_root = module_root(workspace, cx);
        let open_buffers: Vec<_> = workspace
            .items_of_type::<Editor>(cx)
            .filter_map(|editor| editor.read(cx).buffer().read(cx).as_singleton())
            .collect();
        // The selection, else the word at the caret, like PhpStorm's Find in Files.
        let seed = workspace
            .active_item_as::<Editor>(cx)
            .map(|editor| {
                editor.update(cx, |editor, cx| {
                    SearchableItem::query_suggestion(
                        editor,
                        Some(SeedQuerySetting::Always),
                        window,
                        cx,
                    )
                })
            })
            .filter(|text| !text.contains('\n'))
            .unwrap_or_default();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let mut modal = Self::new(
                workspace_handle,
                project,
                replace,
                previous_focus,
                seed,
                window,
                cx,
            );
            modal.module_root = module_root;
            modal.open_buffers = open_buffers;
            modal
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        replace: bool,
        previous_focus: FocusHandle,
        seed: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let query = field("Search all files…", window, cx);
        let mask = field("*.php, *.js", window, cx);
        let directory = field("Directory, relative to the project", window, cx);
        let replace_field = replace.then(|| field("Replace with", window, cx));
        let preview = picker_preview::editor_preview(project.clone(), window, cx);

        let mut subscriptions = Vec::new();
        for editor in [&query, &mask, &directory] {
            let is_mask = editor == &mask;
            subscriptions.push(cx.subscribe_in(
                editor,
                window,
                move |this, editor, event: &EditorEvent, window, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        // Typing a mask switches it on, as in PhpStorm.
                        if is_mask && !editor.read(cx).text(cx).trim().is_empty() {
                            this.mask_enabled = true;
                        }
                        this.refresh(window, cx);
                    }
                },
            ));
        }
        if !seed.is_empty() {
            query.update(cx, |editor, cx| {
                editor.set_text(seed, window, cx);
                // Selected, so typing replaces it.
                editor.select_all(&editor::actions::SelectAll, window, cx);
            });
        }
        Self {
            query,
            mask,
            directory,
            replace: replace_field,
            workspace,
            project,
            previous_focus,
            preview,
            scroll: UniformListScrollHandle::new(),
            hits: Vec::new(),
            selected: 0,
            capped: false,
            case_sensitive: false,
            whole_word: false,
            regex: false,
            mask_enabled: false,
            mask_menu: PopoverMenuHandle::default(),
            scope: Scope::Project,
            context: SearchContext::default(),
            pinned: false,
            closing: false,
            module_root: None,
            open_buffers: Vec::new(),
            open_in_tab: false,
            search: Task::ready(()),
            frame: Frame::default(),
            #[cfg(test)]
            previewed: None,
            _subscriptions: subscriptions,
        }
    }

    /// The Module tab's worktree name, when it has to be in the globs: with one
    /// worktree the module is the whole project.
    fn module_prefix(&self, cx: &App) -> Option<&str> {
        let root = self.module_root.as_deref()?;
        (self.scope == Scope::Module && self.project.read(cx).visible_worktrees(cx).count() > 1)
            .then_some(root)
    }

    /// The include globs: the mask when it is ticked, inside the directory or
    /// module when that scope is picked.
    fn include_globs(&self, cx: &App) -> Vec<String> {
        let mask = if self.mask_enabled {
            self.mask.read(cx).text(cx)
        } else {
            String::new()
        };
        let directory = if self.scope == Scope::Directory {
            let roots: Vec<PathBuf> = self
                .project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect();
            relative_directory(&self.directory.read(cx).text(cx), &roots)
        } else {
            // ponytail: the name goes in as a glob, so a worktree named with glob
            // characters ([ ] { } * ?) matches nothing; escape it if that shows up.
            self.module_prefix(cx).unwrap_or_default().to_string()
        };
        include_globs(&mask, &directory)
    }

    /// With several worktrees the Module tab matches globs against paths that start
    /// with the worktree's name, as the project search tab does.
    fn match_full_paths(&self, cx: &App) -> bool {
        self.module_prefix(cx).is_some()
    }

    /// The Scope tab searches the tabs that were open, nothing else.
    fn scope_buffers(&self) -> Option<Vec<Entity<language::Buffer>>> {
        (self.scope == Scope::OpenFiles).then(|| self.open_buffers.clone())
    }

    /// Searches again with the current query, options and scope.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text(cx);
        let path_style = self.project.read(cx).path_style(cx);
        let globs = self.include_globs(cx);
        let full_paths = self.match_full_paths(cx);
        let buffers = self.scope_buffers();
        let context = self.context;
        // The hits of the previous query stay until the new ones arrive.
        self.search = Task::ready(());
        let built = (!query.trim().is_empty())
            .then(|| PathMatcher::new(globs, path_style).ok())
            .flatten()
            .and_then(|include| {
                if self.regex {
                    SearchQuery::regex(
                        query,
                        self.whole_word,
                        self.case_sensitive,
                        false,
                        false,
                        include,
                        PathMatcher::default(),
                        full_paths,
                        buffers,
                    )
                } else {
                    SearchQuery::text(
                        query,
                        self.whole_word,
                        self.case_sensitive,
                        false,
                        include,
                        PathMatcher::default(),
                        full_paths,
                        buffers,
                    )
                }
                // An unfinished regex is not an error worth showing while typing.
                .ok()
            });
        let Some(built) = built else {
            self.set_hits(Vec::new(), false, window, cx);
            return;
        };
        let results = self
            .project
            .update(cx, |project, cx| project.search(built, cx));
        self.search = cx.spawn_in(window, async move |this, cx| {
            let _search = results.task_handle;
            let mut hits: Vec<Hit> = Vec::new();
            let mut capped = false;
            while let Ok(result) = results.rx.recv().await {
                let SearchResult::Buffer { buffer, ranges } = result else {
                    continue;
                };
                let source = cx.update(|_, cx| HitSource::new(&buffer, cx));
                let Ok(source) = source else { return };
                let Some(source) = source else { continue };
                // Lines and their syntax colours are read off the main thread.
                let found = cx
                    .background_spawn(async move { source.hits(&ranges, context) })
                    .await;
                hits.extend(found);
                if hits.len() >= MAX_HITS {
                    hits.truncate(MAX_HITS);
                    capped = true;
                    break;
                }
            }
            // Files arrive in any order; list them by path.
            hits.sort_by(|a, b| {
                a.project_path
                    .cmp(&b.project_path)
                    .then(a.point.row.cmp(&b.point.row))
            });
            this.update_in(cx, |this, window, cx| {
                this.set_hits(hits, capped, window, cx)
            })
            .ok();
        });
    }

    fn set_hits(
        &mut self,
        hits: Vec<Hit>,
        capped: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hits = hits;
        self.capped = capped;
        self.select(0, window, cx);
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = ix;
        if !self.hits.is_empty() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.update_preview(window, cx);
        cx.notify();
    }

    fn update_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(self.selected) else {
            self.preview.clear(cx);
            #[cfg(test)]
            {
                self.previewed = None;
            }
            return;
        };
        #[cfg(test)]
        {
            self.previewed = Some((hit.project_path.clone(), hit.point.row));
        }
        self.preview.update(
            PreviewUpdate::from_buffer(
                hit.buffer.clone(),
                MatchLocation {
                    anchor_range: hit.anchors.clone(),
                    range: hit.offsets.clone(),
                },
            ),
            window,
            cx,
        );
        // The editor knows its height only after it is drawn; then centre the match.
        let preview = self.preview.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| preview.adjust_to_new_size(window, cx));
        });
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        // In the mask box the arrows open its history, as in a combo box.
        if self.mask.read(cx).focus_handle(cx).is_focused(window) {
            self.mask_menu.toggle(window, cx);
            return;
        }
        if !self.hits.is_empty() {
            self.select((self.selected + 1) % self.hits.len(), window, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mask.read(cx).focus_handle(cx).is_focused(window) {
            self.mask_menu.toggle(window, cx);
            return;
        }
        if !self.hits.is_empty() {
            let len = self.hits.len();
            self.select((self.selected + len - 1) % len, window, cx);
        }
    }

    fn select_first(&mut self, _: &menu::SelectFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.select(0, window, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.hits.len().saturating_sub(1), window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }

    /// Closes the popup even when it is pinned.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.closing = true;
        cx.emit(DismissEvent);
    }

    fn toggle_pin(&mut self, _: &TogglePinFindPopup, _: &mut Window, cx: &mut Context<Self>) {
        self.pinned = !self.pinned;
        cx.notify();
    }

    fn cycle_scope(&mut self, _: &CycleFindScope, window: &mut Window, cx: &mut Context<Self>) {
        let usable = |scope: &Scope| *scope != Scope::Module || self.module_root.is_some();
        let tabs: Vec<Scope> = Scope::ALL.into_iter().filter(usable).collect();
        let ix = tabs
            .iter()
            .position(|scope| *scope == self.scope)
            .unwrap_or(0);
        self.set_scope(tabs[(ix + 1) % tabs.len()], window, cx);
    }

    fn set_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| this.scope = scope);
        if scope == Scope::Directory {
            window.focus(&self.directory.read(cx).focus_handle(cx), cx);
        }
    }

    fn cycle_context(&mut self, _: &CycleFindContext, window: &mut Window, cx: &mut Context<Self>) {
        self.set_context(self.context.next(), window, cx);
    }

    fn set_context(&mut self, context: SearchContext, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| this.context = context);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_in_find_window(window, cx);
    }

    /// Tab moves between the boxes, since the search box has no use for it.
    fn focus_next_field(
        &mut self,
        _: &picker::ConfirmCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut fields = vec![self.query.read(cx).focus_handle(cx)];
        fields.extend(self.replace.iter().map(|e| e.read(cx).focus_handle(cx)));
        if self.scope == Scope::Directory {
            fields.push(self.directory.read(cx).focus_handle(cx));
        }
        fields.push(self.mask.read(cx).focus_handle(cx));
        let current = fields.iter().position(|field| field.is_focused(window));
        let next = current.map_or(0, |ix| (ix + 1) % fields.len());
        window.focus(&fields[next], cx);
    }

    /// Enter opens the picked line; with "Open results in new tab" ticked it sends the
    /// whole result to a project search tab instead.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_in_tab {
            self.open_in_find_window(window, cx);
            return;
        }
        let Some(hit) = self.hits.get(self.selected).cloned() else {
            return;
        };
        self.remember_mask(cx);
        self.closing = true;
        open_hit(self.workspace.clone(), hit, window, cx).detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn toggle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        flip: impl FnOnce(&mut Self),
    ) {
        flip(self);
        self.refresh(window, cx);
        cx.notify();
    }

    /// A search that used the mask was confirmed: keep the mask for the dropdown.
    fn remember_mask(&self, cx: &mut Context<Self>) {
        if self.mask_enabled {
            let mask = self.mask.read(cx).text(cx);
            mask_history::remember(&mask, cx);
        }
    }

    /// Hands the query to the project search tab, which also does replacing.
    fn open_in_find_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_mask(cx);
        let included = self.include_globs(cx).join(",");
        let action = DeploySearch {
            replace_enabled: self.replace.is_some(),
            included_files: (!included.is_empty()).then_some(included),
            query: Some(self.query.read(cx).text(cx)),
            regex: Some(self.regex),
            case_sensitive: Some(self.case_sensitive),
            whole_word: Some(self.whole_word),
            ..Default::default()
        };
        let focus = self.previous_focus.clone();
        self.close(cx);
        window.focus(&focus, cx);
        window.dispatch_action(Box::new(action), cx);
    }

    /// "4 matches in 1 file"; empty until something is searched for.
    fn summary(&self, cx: &App) -> String {
        if self.query.read(cx).text(cx).trim().is_empty() {
            return String::new();
        }
        let matches = self.hits.iter().map(|hit| hit.matches.len()).sum();
        let mut files: Vec<&ProjectPath> = self.hits.iter().map(|hit| &hit.project_path).collect();
        files.dedup();
        summary(matches, files.len(), self.capped)
    }

    #[cfg(test)]
    pub(crate) fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query
            .update(cx, |editor, cx| editor.set_text(query, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn query_text(&self, cx: &App) -> String {
        self.query.read(cx).text(cx)
    }

    #[cfg(test)]
    pub(crate) fn set_mask(&mut self, mask: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.mask
            .update(cx, |editor, cx| editor.set_text(mask, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn set_directory(
        &mut self,
        directory: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scope = Scope::Directory;
        self.directory
            .update(cx, |editor, cx| editor.set_text(directory, window, cx));
        cx.notify();
    }

    /// A grip drag from `from` to `to`, which may be outside the window.
    #[cfg(test)]
    pub(crate) fn resize_between(
        &mut self,
        from: gpui::Point<Pixels>,
        to: gpui::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.frame.resize_to(PopupKind::Find, from, window, cx);
        self.frame.resize_to(PopupKind::Find, to, window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn dragged(&self) -> bool {
        self.frame.moved()
    }

    #[cfg(test)]
    pub(crate) fn set_open_in_tab(&mut self, on: bool) {
        self.open_in_tab = on;
    }

    #[cfg(test)]
    pub(crate) fn pick_scope(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        self.set_scope(scope, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn pick_context(
        &mut self,
        context: SearchContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_context(context, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn mask_text(&self, cx: &App) -> String {
        self.mask.read(cx).text(cx)
    }

    #[cfg(test)]
    pub(crate) fn mask_enabled(&self) -> bool {
        self.mask_enabled
    }

    /// What picking the `ix`th entry of the mask dropdown does.
    #[cfg(test)]
    pub(crate) fn pick_history_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mask = mask_history::entries(cx)[ix].clone();
        self.pick_mask(&mask, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn untick_mask(&mut self) {
        self.mask_enabled = false;
    }

    /// Match Case: the "Aa" chip and its action.
    fn toggle_case(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| {
            this.case_sensitive = !this.case_sensitive
        });
    }

    /// Words: the "W" chip and its action.
    fn toggle_word(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| this.whole_word = !this.whole_word);
    }

    /// Regex: the ".*" chip and its action.
    fn toggle_regex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| this.regex = !this.regex);
    }

    /// The "File mask:" checkbox and its action.
    fn toggle_mask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle(window, cx, |this| this.mask_enabled = !this.mask_enabled);
    }

    #[cfg(test)]
    pub(crate) fn focus_mask(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.mask.read(cx).focus_handle(cx), cx);
    }

    #[cfg(test)]
    pub(crate) fn mask_menu_open(&self) -> bool {
        self.mask_menu.is_deployed()
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    #[cfg(test)]
    pub(crate) fn context(&self) -> SearchContext {
        self.context
    }

    #[cfg(test)]
    pub(crate) fn is_pinned(&self) -> bool {
        self.pinned
    }

    #[cfg(test)]
    pub(crate) fn globs(&self, cx: &App) -> Vec<String> {
        self.include_globs(cx)
    }

    #[cfg(test)]
    pub(crate) fn match_count(&self) -> usize {
        self.hits.len()
    }

    /// The hit lines as "file:row", in list order.
    #[cfg(test)]
    pub(crate) fn hit_labels(&self) -> Vec<String> {
        self.hits
            .iter()
            .map(|hit| format!("{}:{}", hit.file_name, hit.point.row + 1))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(crate) fn summary_text(&self, cx: &App) -> String {
        self.summary(cx)
    }

    /// The file and row the preview was last pointed at.
    #[cfg(test)]
    pub(crate) fn previewed(&self) -> Option<(ProjectPath, u32)> {
        self.previewed.clone()
    }

    /// A rounded box round an editor whose border turns blue while it has focus.
    fn field(&self, editor: &Entity<Editor>, window: &Window, cx: &App) -> Div {
        let colors = cx.theme().colors();
        let focused = editor.read(cx).focus_handle(cx).is_focused(window);
        h_flex()
            .h(FIELD_HEIGHT)
            .px_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(if focused {
                colors.border_focused
            } else {
                colors.border
            })
            .bg(colors.editor_background)
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let title = if self.replace.is_some() {
            "Replace in Files"
        } else {
            "Find in Files"
        };
        let summary = self.summary(cx);
        let query_focus = self.query.read(cx).focus_handle(cx);
        let pinned = self.pinned;
        let pin_label = if pinned {
            "Unpin (the popup closes when focus leaves it)"
        } else {
            "Pin (keep the popup open when focus leaves it)"
        };
        let header = h_flex()
            .debug_selector(|| "zenith-popup-header".into())
            .h(HEADER_HEIGHT)
            .px(PADDING)
            .gap(HEADER_GAP)
            .justify_between()
            .bg(gpui::rgb(POPUP_BAND))
            .child(
                h_flex()
                    .gap(HEADER_GAP)
                    .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("zenith-find-mask-on", self.mask_enabled.into())
                            .label("File mask:")
                            .label_size(LabelSize::Small)
                            .label_color(Color::Default)
                            .tooltip(Tooltip::for_action_title_in(
                                "File mask on or off",
                                &ToggleFindMask,
                                &query_focus,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_mask(window, cx)),
                            ),
                    )
                    .child(
                        self.field(&self.mask, window, cx)
                            .w(MASK_WIDTH)
                            .child(div().flex_1().child(self.mask.clone()))
                            .child(self.render_mask_history(cx)),
                    )
                    .child(self.render_context_menu(cx))
                    .child(
                        IconButton::new(
                            "zenith-find-pin",
                            if pinned {
                                IconName::Pin
                            } else {
                                IconName::Unpin
                            },
                        )
                        .icon_size(IconSize::Small)
                        .toggle_state(pinned)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .tooltip(Tooltip::text(pin_label))
                        .aria_label(pin_label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_pin(&TogglePinFindPopup, window, cx)
                        })),
                    ),
            );
        header
    }

    /// The chevron in the mask box: the masks used before, most recent first.
    fn render_mask_history(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let modal = cx.entity().downgrade();
        let current = self.mask.read(cx).text(cx).trim().to_string();
        let label = "File mask history";
        PopoverMenu::new("zenith-find-mask-menu")
            .with_handle(self.mask_menu.clone())
            .trigger(
                IconButton::new("zenith-find-mask-history", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(label))
                    .aria_label(label),
            )
            .menu(move |window, cx| {
                let modal = modal.clone();
                let current = current.clone();
                let masks = mask_history::entries(cx);
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for mask in masks {
                        let modal = modal.clone();
                        let picked = mask.clone();
                        menu = menu.toggleable_entry(
                            mask.clone(),
                            mask == current,
                            IconPosition::Start,
                            None,
                            move |window, cx| {
                                modal
                                    .update(cx, |modal, cx| modal.pick_mask(&picked, window, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    /// Puts a mask from the dropdown in the box; editing the box ticks it and
    /// searches again.
    fn pick_mask(&mut self, mask: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.mask
            .update(cx, |editor, cx| editor.set_text(mask, window, cx));
    }

    fn toggle_mask_history(
        &mut self,
        _: &ToggleFileMaskHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mask_menu.toggle(window, cx);
    }

    /// The funnel: which kinds of match to list, as PhpStorm's menu.
    fn render_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let modal = cx.entity().downgrade();
        let current = self.context;
        let label = format!("Search context: {}", current.label());
        PopoverMenu::new("zenith-find-context-menu")
            .trigger(
                IconButton::new("zenith-find-context", IconName::Filter)
                    .icon_size(IconSize::Small)
                    .toggle_state(current != SearchContext::Anywhere)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .tooltip(Tooltip::text(label.clone()))
                    .aria_label(label),
            )
            .menu(move |window, cx| {
                let modal = modal.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for context in SearchContext::ALL {
                        let modal = modal.clone();
                        menu = menu.toggleable_entry(
                            context.label(),
                            context == current,
                            IconPosition::Start,
                            None,
                            move |window, cx| {
                                modal
                                    .update(cx, |modal, cx| modal.set_context(context, window, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_search_box(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let query_focus = self.query.read(cx).focus_handle(cx);
        let chip = |id: &'static str,
                    label: &'static str,
                    tip: &'static str,
                    action: &dyn gpui::Action,
                    on: bool| {
            Button::new(id, label)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .toggle_state(on)
                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                .tooltip(Tooltip::for_action_title_in(tip, action, &query_focus))
                .aria_label(tip)
        };
        let has_query = !self.query.read(cx).text(cx).is_empty();
        v_flex()
            .gap_2()
            .child(
                self.field(&self.query, window, cx)
                    .debug_selector(|| "zenith-find-query".into())
                    .h(SEARCH_BOX_HEIGHT)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().child(self.query.clone()))
                    .when(has_query, |this| {
                        this.child(
                            IconButton::new("zenith-find-clear", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Clear"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.query
                                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                                    window.focus(&this.query.read(cx).focus_handle(cx), cx);
                                })),
                        )
                    })
                    .child(
                        chip(
                            "zenith-find-case",
                            "Aa",
                            "Match Case",
                            &ToggleFindCase,
                            self.case_sensitive,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_case(window, cx))),
                    )
                    .child(
                        chip(
                            "zenith-find-word",
                            "W",
                            "Words",
                            &ToggleFindWord,
                            self.whole_word,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_word(window, cx))),
                    )
                    .child(
                        chip(
                            "zenith-find-regex",
                            ".*",
                            "Regex",
                            &ToggleFindRegex,
                            self.regex,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_regex(window, cx))),
                    ),
            )
            .children(self.replace.as_ref().map(|replace| {
                self.field(replace, window, cx)
                    .h(SEARCH_BOX_HEIGHT)
                    .child(div().flex_1().child(replace.clone()))
            }))
    }

    fn render_scope(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tab = |scope: Scope| {
            Button::new(scope.id(), scope.label())
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.scope == scope)
                .disabled(scope == Scope::Module && self.module_root.is_none())
                .aria_role(Role::Tab)
                .aria_label(scope.label())
                .on_click(cx.listener(move |this, _, window, cx| this.set_scope(scope, window, cx)))
        };
        h_flex()
            .gap_2()
            .children(Scope::ALL.map(tab))
            .when(self.scope == Scope::Directory, |this| {
                this.child(
                    self.field(&self.directory, window, cx)
                        .debug_selector(|| "zenith-find-directory".into())
                        .flex_1()
                        .child(div().flex_1().child(self.directory.clone())),
                )
            })
            .when(self.scope == Scope::OpenFiles, |this| {
                this.child(
                    h_flex()
                        .debug_selector(|| "zenith-find-scope-name".into())
                        .h(FIELD_HEIGHT)
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .child(Label::new("Open Files").size(LabelSize::Small)),
                )
            })
    }

    fn render_results(
        &self,
        sized: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Until the popup is resized the list shows a fixed number of rows;
        // after, it takes whatever height the body has left.
        let list = div()
            .id("zenith-find-results")
            .relative()
            .when(sized, |list| list.flex_1().min_h_0())
            .when(!sized, |list| list.h(px(ROW_HEIGHT * VISIBLE_ROWS)));
        if self.hits.is_empty() {
            let text = if self.query.read(cx).text(cx).trim().is_empty() {
                "Type search query to find in files"
            } else {
                "No matches"
            };
            return list
                .px_4()
                .py_1()
                .child(Label::new(text).color(Color::Muted))
                .into_any_element();
        }
        list.child(
            uniform_list(
                "zenith-find-hits",
                self.hits.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_hit(ix, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .size_full(),
        )
        .custom_scrollbars(
            Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.scroll),
            window,
            cx,
        )
        .into_any_element()
    }

    /// One matching line: the code with the match in a chip, the file and line on the right.
    fn render_hit(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hit = &self.hits[ix];
        let colors = cx.theme().colors();
        let chip = HighlightStyle {
            background_color: Some(colors.search_match_background),
            ..Default::default()
        };
        let syntax = cx.theme().syntax().resolve_runs(&hit.syntax);
        let highlights = row_highlights(&hit.line, syntax, &hit.matches, chip);
        let text_color = if is_comment_line(&hit.line) {
            colors.text_muted
        } else {
            colors.text
        };
        h_flex()
            .id(("zenith-find-hit", ix))
            .debug_selector(move || format!("zenith-find-hit-{ix}"))
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_4()
            .gap_3()
            .flex_none()
            .justify_between()
            .when(ix == self.selected, |row| {
                row.bg(colors.element_selection_background)
            })
            .hover(|row| row.bg(colors.ghost_element_hover))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(ix, window, cx);
                if event.click_count() > 1 {
                    this.confirm(&menu::Confirm, window, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ui_sm(cx)
                    .text_color(text_color)
                    .child(StyledText::new(hit.line.clone()).with_highlights(highlights)),
            )
            .child(
                Label::new(format!("{} {}", hit.file_name, hit.point.row + 1))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let border = cx.theme().colors().border_variant;
        let hit = self.hits.get(self.selected);
        v_flex()
            .border_t_1()
            .border_color(border)
            .child(h_flex().h_8().px_4().gap_2().when_some(hit, |this, hit| {
                this.child(Label::new(hit.file_name.clone())).child(
                    Label::new(hit.directory.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .child(
                div()
                    .debug_selector(|| "zenith-find-preview".into())
                    .h(px(PREVIEW_HEIGHT))
                    .overflow_hidden()
                    .child(self.preview.render(PreviewLayout::Below, cx)),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        h_flex()
            .debug_selector(|| "zenith-popup-footer".into())
            .h(FOOTER_HEIGHT)
            .px(PADDING)
            .gap_2()
            .justify_between()
            .bg(gpui::rgb(POPUP_FOOTER))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Checkbox::new("zenith-find-new-tab", self.open_in_tab.into())
                    .label("Open results in new tab")
                    .label_size(LabelSize::Small)
                    .label_color(Color::Default)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_in_tab = !this.open_in_tab;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(if cfg!(target_os = "macos") {
                            "⌘↵"
                        } else {
                            "Ctrl+Enter"
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Button::new("zenith-find-window", "Open in Find Window")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_find_window(window, cx)
                            })),
                    ),
            )
    }
}

impl Render for FindInFilesModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, height) = self.frame.size(PopupKind::Find, window, cx);
        let header = self.render_header(window, cx);
        let search = self.render_search_box(window, cx);
        let scope = self.render_scope(window, cx);
        let results = self.render_results(height.is_some(), window, cx);
        let preview = self.render_preview(cx);
        let footer = self.render_footer(cx);
        let body = v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_none()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .gap_2()
                    .child(search)
                    .child(scope),
            )
            .child(results)
            .child(v_flex().flex_none().child(preview))
            .child(v_flex().flex_none().child(footer));
        let root = v_flex()
            // "Picker" makes the keymap's up, down, escape and tab work in the search box.
            .key_context("FindInFiles Picker")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_next_field))
            .on_action(cx.listener(Self::toggle_pin))
            .on_action(cx.listener(Self::cycle_scope))
            .on_action(cx.listener(Self::cycle_context))
            .on_action(cx.listener(Self::toggle_mask_history))
            .on_action(
                cx.listener(|this, _: &ToggleFindCase, window, cx| this.toggle_case(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleFindWord, window, cx| this.toggle_word(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleFindRegex, window, cx| this.toggle_regex(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleFindMask, window, cx| this.toggle_mask(window, cx)),
            );
        self.frame.render(
            PopupKind::Find,
            root,
            header,
            body,
            |this: &mut Self| &mut this.frame,
            window,
            cx,
        )
    }
}

/// One matching line.
#[derive(Clone)]
pub struct Hit {
    pub(super) buffer: Entity<language::Buffer>,
    /// The first match, for the preview.
    pub(super) anchors: Range<text::Anchor>,
    pub(super) offsets: Range<usize>,
    pub(super) project_path: ProjectPath,
    pub(super) file_name: SharedString,
    pub(super) directory: SharedString,
    pub(super) point: Point,
    /// The line with leading space removed, and the byte ranges that matched in it.
    pub(super) line: String,
    pub(super) matches: Vec<Range<usize>>,
    /// The syntax colours of `line`, found once with the hit and turned into styles
    /// with the current theme when the row is drawn.
    pub(super) syntax: Vec<(Range<usize>, HighlightId)>,
}

/// What one buffer's hits are built from. Made on the main thread, which owns the
/// buffer; [`HitSource::hits`] then runs anywhere.
pub(super) struct HitSource {
    pub(super) buffer: Entity<language::Buffer>,
    pub(super) snapshot: BufferSnapshot,
    pub(super) project_path: ProjectPath,
    pub(super) file_name: SharedString,
    pub(super) directory: SharedString,
}

impl HitSource {
    /// `None` for a buffer without a file, which has no path to list.
    pub(super) fn new(buffer: &Entity<language::Buffer>, cx: &App) -> Option<Self> {
        let snapshot_source = buffer.read(cx);
        let file = snapshot_source.file()?;
        Some(Self {
            buffer: buffer.clone(),
            project_path: ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path().clone(),
            },
            file_name: file
                .path()
                .file_name()
                .unwrap_or_default()
                .to_string()
                .into(),
            directory: file
                .path()
                .parent()
                .map(|parent| parent.as_unix_str().to_string())
                .unwrap_or_default()
                .into(),
            snapshot: snapshot_source.snapshot(),
        })
    }

    pub(super) fn hits(&self, ranges: &[Range<text::Anchor>], context: SearchContext) -> Vec<Hit> {
        let snapshot = &self.snapshot;
        let mut hits: Vec<Hit> = Vec::new();
        for range in ranges {
            let start = range.start.to_point(snapshot);
            let end = range.end.to_point(snapshot);
            let line_len = snapshot.line_len(start.row);
            let text: String = snapshot
                .text_for_range(Point::new(start.row, 0)..Point::new(start.row, line_len))
                .collect();
            let lead = text.len() - text.trim_start().len();
            let column_end = if end.row == start.row {
                end.column
            } else {
                line_len
            };
            let matched = (start.column as usize).saturating_sub(lead)
                ..(column_end as usize).saturating_sub(lead).max(1);
            if context != SearchContext::Anywhere
                && !context.allows(span_at(&text[lead..], matched.start))
            {
                continue;
            }
            // Several matches on one line make one row.
            if let Some(last) = hits.last_mut()
                && last.point.row == start.row
            {
                last.matches.push(matched);
                continue;
            }
            hits.push(Hit {
                buffer: self.buffer.clone(),
                anchors: range.clone(),
                offsets: range.start.to_offset(snapshot)..range.end.to_offset(snapshot),
                project_path: self.project_path.clone(),
                file_name: self.file_name.clone(),
                directory: self.directory.clone(),
                point: start,
                line: text[lead..].to_string(),
                matches: vec![matched],
                syntax: line_syntax(snapshot, start.row, line_len, lead),
            });
        }
        hits
    }
}

/// Opens the hit's file and puts the cursor on its first match.
pub(super) fn open_hit<T: 'static>(
    workspace: WeakEntity<Workspace>,
    hit: Hit,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Task<anyhow::Result<()>> {
    cx.spawn_in(window, async move |_, cx| {
        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_path(hit.project_path.clone(), None, true, window, cx)
        })?;
        let item = open.await?;
        if let Some(editor) = item.downcast::<Editor>() {
            editor.update_in(cx, |editor, window, cx| {
                editor.change_selections(
                    SelectionEffects::scroll(Autoscroll::center()).unfold(),
                    window,
                    cx,
                    |selections| selections.select_ranges([hit.point..hit.point]),
                );
            })?;
        }
        anyhow::Ok(())
    })
}

/// The syntax colours along a line, past its `lead` bytes of leading space.
fn line_syntax(
    snapshot: &BufferSnapshot,
    row: u32,
    line_len: u32,
    lead: usize,
) -> Vec<(Range<usize>, HighlightId)> {
    let chunks = snapshot
        .chunks(
            Point::new(row, 0)..Point::new(row, line_len),
            LanguageAwareStyling {
                tree_sitter: true,
                diagnostics: false,
            },
        )
        .map(|chunk| (chunk.text.len(), chunk.syntax_highlight_id));
    runs_after_lead(chunks, lead)
}

/// The coloured runs of consecutive `(length, colour)` chunks, moved left by `lead`
/// bytes; what falls in the first `lead` bytes is dropped.
fn runs_after_lead<Id>(
    chunks: impl IntoIterator<Item = (usize, Option<Id>)>,
    lead: usize,
) -> Vec<(Range<usize>, Id)> {
    let mut runs = Vec::new();
    let mut at = 0;
    for (len, id) in chunks {
        let end = at + len;
        if let Some(id) = id
            && end > lead
        {
            runs.push((at.max(lead) - lead..end - lead, id));
        }
        at = end;
    }
    runs
}

/// What a row's text is drawn with: the syntax colours with the matches in a chip on
/// top, all cut back to character boundaries of `line`.
pub(super) fn row_highlights(
    line: &str,
    syntax: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    matches: &[Range<usize>],
    chip: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    gpui::combine_highlights(
        syntax
            .into_iter()
            .map(|(range, style)| (clamp_range(line, &range), style)),
        matches.iter().map(|range| (clamp_range(line, range), chip)),
    )
    .collect()
}

/// "4 matches in 1 file"; a capped search adds a plus, as there may be more.
fn summary(matches: usize, files: usize, capped: bool) -> String {
    let plus = if capped { "+" } else { "" };
    format!(
        "{matches}{plus} {} in {files}{plus} {}",
        if matches == 1 && !capped {
            "match"
        } else {
            "matches"
        },
        if files == 1 && !capped {
            "file"
        } else {
            "files"
        },
    )
}

/// The globs for the include side of the search: the mask's comma-separated globs,
/// inside `directory` when there is one.
fn include_globs(mask: &str, directory: &str) -> Vec<String> {
    let masks = mask
        .split(',')
        .map(str::trim)
        .filter(|glob| !glob.is_empty());
    if directory.is_empty() {
        return masks.map(str::to_string).collect();
    }
    let masks: Vec<&str> = masks.collect();
    if masks.is_empty() {
        vec![directory.to_string()]
    } else {
        masks
            .into_iter()
            .map(|glob| format!("{directory}/**/{glob}"))
            .collect()
    }
}

/// The directory as the search wants it: relative to a worktree root. An absolute
/// path inside a root loses the root; a root itself, or nothing, means everywhere.
fn relative_directory(input: &str, roots: &[PathBuf]) -> String {
    let input = input.trim();
    let relative = roots
        .iter()
        .find_map(|root| Path::new(input).strip_prefix(root).ok())
        .map_or(input, |rest| rest.to_str().unwrap_or(input));
    relative
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// The name of the worktree the Module tab searches: that of the active file, else
/// the first one.
fn module_root(workspace: &Workspace, cx: &App) -> Option<String> {
    let project = workspace.project().read(cx);
    let worktree = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
        .and_then(|path| project.worktree_for_id(path.worktree_id, cx))
        .or_else(|| project.visible_worktrees(cx).next())?;
    Some(worktree.read(cx).root_name_str().to_string())
}

/// What the byte `at` of `line` sits in: a line comment, a block comment or a
/// quoted string, found by scanning the line from its start.
// ponytail: one line at a time, `//`, `/* */` and quotes only, so a string or block
// comment opened on an earlier line reads as code and a lone apostrophe opens a
// string. Real spans need the syntax tree.
fn span_at(line: &str, at: usize) -> Span {
    if is_comment_line(line) {
        return Span::Comment;
    }
    let mut quote: Option<char> = None;
    let mut skip_to = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((ix, c)) = chars.next() {
        if ix > at {
            break;
        }
        if ix < skip_to {
            continue;
        }
        match quote {
            Some(_) if c == '\\' => {
                chars.next();
            }
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None => match (c, chars.peek().map(|&(_, next)| next)) {
                ('"' | '\'' | '`', _) => quote = Some(c),
                ('/', Some('/')) => return Span::Comment,
                ('/', Some('*')) => match line[ix + 2..].find("*/") {
                    Some(end) if ix + 2 + end + 2 <= at => skip_to = ix + 2 + end + 2,
                    _ => return Span::Comment,
                },
                _ => {}
            },
        }
    }
    if quote.is_some() {
        Span::String
    } else {
        Span::Code
    }
}

/// Whether the line looks like a comment, which the list dims.
// ponytail: language-agnostic prefixes only; real comment spans would need the syntax tree.
pub(super) fn is_comment_line(line: &str) -> bool {
    let line = line.trim_start();
    ["//", "/*", "*/", "* ", "<!--"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        || line == "*"
}

/// The range cut back to the line and to character boundaries.
fn clamp_range(line: &str, range: &Range<usize>) -> Range<usize> {
    let snap = |mut ix: usize| {
        ix = ix.min(line.len());
        while !line.is_char_boundary(ix) {
            ix -= 1;
        }
        ix
    };
    let start = snap(range.start);
    start..snap(range.end).max(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYWORD: u8 = 1;
    const STRING: u8 = 2;

    #[test]
    fn test_runs_after_lead_shift_left_and_drop_the_indent() {
        // "    echo 'x';": 4 blank, a keyword, a blank, a string, plain tail.
        let chunks = [
            (4, None),
            (4, Some(KEYWORD)),
            (1, None),
            (3, Some(STRING)),
            (1, None),
        ];
        assert_eq!(
            runs_after_lead(chunks, 4),
            [(0..4, KEYWORD), (5..8, STRING)]
        );
        // A run that starts inside the indent keeps only what is past it.
        assert_eq!(
            runs_after_lead([(6, Some(KEYWORD)), (2, None)], 4),
            [(0..2, KEYWORD)]
        );
        assert_eq!(runs_after_lead([(3, Some(KEYWORD))], 4), []);
        assert_eq!(runs_after_lead::<u8>([], 0), []);
    }

    #[test]
    fn test_row_highlights_lay_the_match_chip_over_the_syntax_colours() {
        let colour = |hsla| HighlightStyle {
            color: Some(hsla),
            ..Default::default()
        };
        let keyword = colour(gpui::red());
        let chip = HighlightStyle {
            background_color: Some(gpui::blue()),
            ..Default::default()
        };
        // "echo 'é';": the keyword is 0..4, the string 5..9 ('é' takes two bytes).
        let line = "echo 'é';";
        let string = colour(gpui::green());
        let runs = row_highlights(
            line,
            [(0..4, keyword), (5..9, string)],
            // The match covers `ho` of the keyword and runs into the string, ending
            // inside 'é' (bytes 6..8), which it must not split.
            &[2..7],
            chip,
        );
        let both = |style: HighlightStyle| style.highlight(chip);
        assert_eq!(
            runs,
            [
                (0..2, keyword),
                (2..4, both(keyword)),
                (4..5, chip),
                (5..6, both(string)),
                (6..9, string),
            ]
        );
    }

    #[test]
    fn test_row_highlights_cut_ranges_back_to_character_boundaries() {
        let chip = HighlightStyle {
            background_color: Some(gpui::blue()),
            ..Default::default()
        };
        // 'é' is bytes 1..3: a range ending or starting inside it must not split it,
        // and one past the end of the line is cut to the line.
        let line = "aéb";
        let runs = row_highlights(line, [], &[2..3, 3..40], chip);
        assert_eq!(runs, [(1..3, chip), (3..4, chip)]);
    }

    #[test]
    fn test_summary_is_singular_and_marks_a_capped_search() {
        assert_eq!(summary(4, 1, false), "4 matches in 1 file");
        assert_eq!(summary(1, 1, false), "1 match in 1 file");
        assert_eq!(summary(0, 0, false), "0 matches in 0 files");
        assert_eq!(summary(300, 12, true), "300+ matches in 12+ files");
    }

    #[test]
    fn test_include_globs_put_the_mask_inside_the_directory() {
        assert_eq!(include_globs("*.php, *.js", ""), ["*.php", "*.js"]);
        assert_eq!(include_globs("", "inc"), ["inc"]);
        assert_eq!(
            include_globs("*.php, ,*.js", "inc/App"),
            ["inc/App/**/*.php", "inc/App/**/*.js"]
        );
        assert!(include_globs(" , ", "").is_empty());
    }

    #[test]
    fn test_relative_directory_accepts_absolute_paths_inside_a_worktree() {
        let roots = [PathBuf::from("/work/site")];
        assert_eq!(relative_directory("/work/site/inc/", &roots), "inc");
        assert_eq!(relative_directory(" ./inc ", &roots), "inc");
        assert_eq!(relative_directory("/work/site", &roots), "");
        assert_eq!(relative_directory("inc/App", &roots), "inc/App");
    }

    #[test]
    fn test_comment_lines_are_told_apart() {
        assert!(is_comment_line("// note"));
        assert!(is_comment_line("* @param array $services"));
        assert!(is_comment_line("/** Docs */"));
        assert!(!is_comment_line("public $services = [];"));
        assert!(!is_comment_line("*ptr = 1;"));
    }

    #[test]
    fn test_span_at_tells_code_comments_and_strings_apart() {
        let line = r#"echo "a // b"; $x = 'it\'s'; /* c */ run(); // tail"#;
        let at = |needle: &str| span_at(line, line.find(needle).unwrap());
        assert_eq!(at("echo"), Span::Code);
        // A `//` inside quotes is text, not a comment.
        assert_eq!(at("a //"), Span::String);
        assert_eq!(at("b\""), Span::String);
        assert_eq!(at("s'"), Span::String, "an escaped quote stays inside");
        assert_eq!(at(" c "), Span::Comment);
        assert_eq!(at("run"), Span::Code, "after a closed block comment");
        assert_eq!(at("tail"), Span::Comment);
        assert_eq!(span_at("* @param $services", 3), Span::Comment);
        assert_eq!(span_at("", 0), Span::Code);
    }

    #[test]
    fn test_search_context_admits_the_right_spans() {
        use Span::{Code, Comment, String};
        let admitted =
            |context: SearchContext| [Code, Comment, String].map(|span| context.allows(span));
        assert_eq!(admitted(SearchContext::Anywhere), [true, true, true]);
        assert_eq!(admitted(SearchContext::InComments), [false, true, false]);
        assert_eq!(
            admitted(SearchContext::InStringLiterals),
            [false, false, true]
        );
        assert_eq!(admitted(SearchContext::ExceptComments), [true, false, true]);
        assert_eq!(
            admitted(SearchContext::ExceptStringLiterals),
            [true, true, false]
        );
        assert_eq!(
            admitted(SearchContext::ExceptCommentsAndStringLiterals),
            [true, false, false]
        );
        // The cycle visits every context once and comes back.
        let mut context = SearchContext::Anywhere;
        for _ in SearchContext::ALL {
            context = context.next();
        }
        assert_eq!(context, SearchContext::Anywhere);
    }

    #[test]
    fn test_clamp_range_stays_on_character_boundaries() {
        assert_eq!(clamp_range("héllo", &(1..2)), 1..1);
        assert_eq!(clamp_range("héllo", &(1..3)), 1..3);
        assert_eq!(clamp_range("abc", &(1..99)), 1..3);
        assert_eq!(clamp_range("abc", &(9..99)), 3..3);
    }
}
