//! Status bar indentation ("4 spaces" / "Tab"), as on the Main board and in PhpStorm.

use editor::Editor;
use gpui::{Subscription, WeakEntity};
use ui::prelude::*;
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

pub fn indent_label(tab_size: u32, hard_tabs: bool) -> String {
    if hard_tabs {
        "Tab".to_string()
    } else {
        format!("{tab_size} spaces")
    }
}

#[derive(Default)]
pub struct IndentIndicator {
    editor: Option<WeakEntity<Editor>>,
    _observe_editor: Option<Subscription>,
}

impl IndentIndicator {
    /// Read at render time, so a settings or language change shows at once.
    pub fn label(&self, cx: &App) -> Option<String> {
        let editor = self.editor.as_ref()?.upgrade()?;
        let buffer = editor.read(cx).active_buffer(cx)?;
        let snapshot = buffer.read(cx).snapshot();
        let settings = snapshot.settings_at(0, cx);
        Some(indent_label(settings.tab_size.get(), settings.hard_tabs))
    }
}

impl Render for IndentIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().debug_selector(|| "zenith-indent".into()).when_some(self.label(cx), |this, label| {
            this.child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        })
    }
}

impl StatusItemView for IndentIndicator {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = active_pane_item.and_then(|item| item.downcast::<Editor>());
        self._observe_editor = editor
            .as_ref()
            .map(|editor| cx.observe_in(editor, window, |_, _, _, cx| cx.notify()));
        self.editor = editor.map(|editor| editor.downgrade());
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
