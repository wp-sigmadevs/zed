//! PhpStorm-style code lens (plan 44 #4): UI font, "N usages" and the author of the
//! lens line after the lens, through the hooks in `editor::code_lens`.

use editor::{Anchor, Editor, RowInfo, ToPoint as _};
use gpui::{AnyElement, App, SharedString, TextStyle, WeakEntity};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::prelude::*;

pub fn init() {
    editor::set_code_lens_style(text_style, title, trailing);
}

/// The UI font; the editor scales the size by 0.9 as it does for its own lens.
fn text_style(base: &TextStyle, cx: &App) -> TextStyle {
    let settings = ThemeSettings::get_global(cx);
    TextStyle {
        font_family: settings.ui_font.family.clone(),
        font_features: settings.ui_font.features.clone(),
        font_fallbacks: settings.ui_font.fallbacks.clone(),
        font_size: settings.ui_font_size(cx).into(),
        ..base.clone()
    }
}

/// "N reference(s)" reads "N usage(s)"; any other title is left alone.
fn title(title: SharedString) -> SharedString {
    match title.split_once(' ') {
        Some((count, noun @ ("reference" | "references"))) if count.parse::<u32>().is_ok() => {
            let plural = if noun == "references" { "s" } else { "" };
            format!("{count} usage{plural}").into()
        }
        _ => title,
    }
}

/// The user icon and the blame author of the lens line. `None` without blame data.
fn trailing(position: Anchor, editor: &WeakEntity<Editor>, cx: &mut App) -> Option<AnyElement> {
    let editor = editor.upgrade()?;
    let (blame, snapshot) = {
        let editor = editor.read(cx);
        (
            editor.blame()?.clone(),
            editor.buffer().read(cx).snapshot(cx),
        )
    };
    let (buffer, point) = snapshot.point_to_buffer_point(position.to_point(&snapshot))?;
    let row = RowInfo {
        buffer_id: Some(buffer.remote_id()),
        buffer_row: Some(point.row),
        ..Default::default()
    };
    let (_, entry) = blame.update(cx, |blame, cx| blame.blame_for_rows(&[row], cx).next())??;
    let author = entry.author.filter(|author| !author.is_empty())?;

    let settings = ThemeSettings::get_global(cx);
    let size = settings.ui_font_size(cx) * 0.9;
    let font = settings.ui_font.clone();
    let colors = cx.theme().colors();
    let color = colors
        .editor_code_lens_foreground
        .unwrap_or(colors.text_muted);
    Some(
        h_flex()
            .ml(size)
            .gap(size * 0.3)
            .font(font)
            .text_size(size)
            .text_color(color)
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(color)),
            )
            .child(author)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::{
        EditorMode, MultiBuffer,
        display_map::{BlockPlacement, BlockProperties, BlockStyle},
    };
    use git::blame::{Blame, BlameEntry};
    use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle};
    use std::sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    };
    use text::Point;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    #[test]
    fn test_reference_titles_become_usages() {
        for (input, expected) in [
            ("1 reference", "1 usage"),
            ("0 references", "0 usages"),
            ("3 references", "3 usages"),
            ("2 implementations", "2 implementations"),
            ("3 References", "3 References"),
            ("references", "references"),
            ("x references", "x references"),
            ("3 references ", "3 references "),
            ("", ""),
        ] {
            assert_eq!(title(input.into()), expected, "{input:?}");
        }
    }

    #[gpui::test]
    fn test_text_style_uses_the_ui_font(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let base = TextStyle {
                font_family: "Not The UI Font".into(),
                font_size: gpui::px(99.).into(),
                color: gpui::red(),
                ..Default::default()
            };
            let style = text_style(&base, cx);
            let settings = ThemeSettings::get_global(cx);
            assert_eq!(style.font_family, settings.ui_font.family);
            assert_eq!(style.font_size, settings.ui_font_size(cx).into());
            assert_eq!(style.color, base.color);
        });
    }

    async fn editor_on(with_blame: bool, cx: &mut TestAppContext) -> WindowHandle<Editor> {
        init_test(cx);
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            serde_json::json!({ ".git": {}, "file.php": "<?php\nfunction a() {}\n" }),
        )
        .await;
        fs.set_blame_for_repo(
            std::path::Path::new("/repo/.git"),
            vec![(
                git::repository::repo_path("file.php"),
                Blame {
                    entries: vec![BlameEntry {
                        sha: "1b1b1b".parse().unwrap(),
                        range: 1..2,
                        original_line_number: 0,
                        author: Some("Ada Lovelace".into()),
                        author_mail: None,
                        author_time: None,
                        author_tz: None,
                        committer_name: None,
                        committer_email: None,
                        committer_time: None,
                        committer_tz: None,
                        summary: None,
                        previous: None,
                        filename: String::new(),
                        boundary: false,
                    }],
                    ..Default::default()
                },
            )],
        );
        let project = project::Project::test(fs, ["/repo".as_ref()], cx).await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer("/repo/file.php", cx)
            })
            .await
            .unwrap();
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let window = cx.add_window(|window, cx| {
            let project = with_blame.then_some(project);
            Editor::new(EditorMode::full(), buffer, project, window, cx)
        });
        window
    }

    fn anchor_at_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) -> Anchor {
        editor.update(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_before(Point::new(row, 0))
        })
    }

    #[gpui::test]
    async fn test_trailing_needs_blame(cx: &mut TestAppContext) {
        let window = editor_on(false, cx).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let editor = window.root(cx).unwrap();
        let anchor = anchor_at_row(&editor, 1, cx);
        cx.update(|_, cx| {
            assert!(editor.read(cx).blame().is_none());
            assert!(trailing(anchor, &editor.downgrade(), cx).is_none());
        });
    }

    /// The lens renders inside the editor's paint, so draw one: 1 = shown, 2 = none.
    #[gpui::test]
    async fn test_trailing_shows_the_author_while_the_editor_paints(cx: &mut TestAppContext) {
        let window = editor_on(true, cx).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let editor = window.root(cx).unwrap();
        cx.update(|window, cx| window.focus(&editor.read(cx).focus_handle(cx), cx));
        editor.update(cx, |editor, cx| {
            let blame = editor
                .blame()
                .expect("inline blame is on by default")
                .clone();
            blame.update(cx, |blame, cx| blame.focus(cx));
        });
        cx.executor().run_until_parked();
        let anchor = anchor_at_row(&editor, 1, cx);
        let rendered = Arc::new(AtomicU8::new(0));
        editor.update(cx, |editor, cx| {
            let weak = cx.entity().downgrade();
            let rendered = rendered.clone();
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(anchor),
                    height: Some(1),
                    style: BlockStyle::Fixed,
                    render: Arc::new(move |block| {
                        let shown = trailing(anchor, &weak, block.app).is_some();
                        rendered.store(1 + u8::from(!shown), Ordering::SeqCst);
                        gpui::div().into_any_element()
                    }),
                    priority: 0,
                }],
                None,
                cx,
            );
        });
        cx.run_until_parked();
        // Blame is generated in the background; paint again once it has landed.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            rendered.load(Ordering::SeqCst),
            1,
            "0 = block never rendered, 2 = no author"
        );
    }
}
