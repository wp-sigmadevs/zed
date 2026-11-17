//! Zenith additions to the title bar (plan 34, step 2).

use gpui::FontWeight;
use ui::{ButtonLike, Tooltip, prelude::*};

/// Two-letter badge shown before the project name, e.g. "LG" for `lumen-gallery`.
pub fn project_badge(project_name: &str, cx: &App) -> Option<impl IntoElement + use<>> {
    let initials = initials(project_name)?;
    let colors = cx.theme().colors();
    Some(
        div()
            .flex_none()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                Label::new(initials)
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::BOLD),
            ),
    )
}

/// "Search Everywhere" box for the middle of the title bar. Dispatches `action`
/// (the file finder until the real Search Everywhere lands, plan 34 step 4).
pub fn search_box(action: Box<dyn gpui::Action>) -> impl IntoElement {
    ButtonLike::new("zenith-search-everywhere")
        .style(ButtonStyle::Filled)
        .child(
            h_flex()
                .w_48()
                .min_w_0()
                .gap_1()
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Search Everywhere")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .tooltip({
            let action = action.boxed_clone();
            move |_window, cx| Tooltip::for_action("Search Everywhere", action.as_ref(), cx)
        })
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// First letter of the first two words, or the first two letters of a single word.
fn initials(name: &str) -> Option<SharedString> {
    let mut words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let first = words.next()?;
    let letters: String = match words.next() {
        Some(second) => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect(),
        None => first.chars().take(2).collect(),
    };
    Some(letters.to_uppercase().into())
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn test_initials() {
        assert_eq!(initials("lumen-gallery").as_deref(), Some("LG"));
        assert_eq!(initials("my_wp plugin").as_deref(), Some("MW"));
        assert_eq!(initials("zenith").as_deref(), Some("ZE"));
        assert_eq!(initials("a").as_deref(), Some("A"));
        assert_eq!(initials("ünïcode-tëst").as_deref(), Some("ÜT"));
        assert_eq!(initials("--"), None);
        assert_eq!(initials(""), None);
    }
}
