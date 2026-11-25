//! Zenith branding for text that ships inside the app (default settings,
//! keymaps) and is shown to the user.

use std::borrow::Cow;

/// Product phrases that keep their upstream name.
const KEEP_AFTER: [&str; 5] = [" Agent", " Industries", " Plex", " Mono", " Sans"];

/// Swaps the capitalised whole word "Zed" for "Zenith" ("Zed's" becomes
/// "Zenith's"). Returns the input untouched, without allocating, when
/// nothing matches.
///
/// Kept as is: "Zed Agent", "Zed Industries", the Zed Plex/Mono/Sans font
/// names, "ZED", lowercase `zed` (paths, URLs, crate names), and any "Zed"
/// glued to identifier or path characters (`ZedMono`, `"Zed"`, `{zed,Zed}`).
pub fn rebrand_text(text: Cow<'static, str>) -> Cow<'static, str> {
    let mut out: Option<String> = None;
    let mut copied = 0;
    for (start, word) in text.match_indices("Zed") {
        let end = start + word.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let glued_before = before.is_some_and(|c| c.is_alphanumeric() || "_-./:\"{,'".contains(c));
        let glued_after = after.is_some_and(|c| c.is_alphanumeric() || "_-\"}/".contains(c));
        if glued_before || glued_after || KEEP_AFTER.iter().any(|k| text[end..].starts_with(k)) {
            continue;
        }
        let out = out.get_or_insert_with(|| String::with_capacity(text.len() + 16));
        out.push_str(&text[copied..start]);
        out.push_str("Zenith");
        copied = end;
    }
    match out {
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &'static str) -> String {
        rebrand_text(Cow::Borrowed(text)).into_owned()
    }

    #[test]
    fn swaps_the_word() {
        assert_eq!(run("Zed settings"), "Zenith settings");
        assert_eq!(run("Zed's window"), "Zenith's window");
        assert_eq!(run("restart Zed."), "restart Zenith.");
        assert_eq!(run("Zed and Zed"), "Zenith and Zenith");
        assert_eq!(run("(Zed)"), "(Zenith)");
    }

    #[test]
    fn keeps_product_phrases_and_identifiers() {
        for keep in [
            "Zed Agent threads",
            "by Zed Industries",
            "font Zed Plex Mono, Zed Mono, Zed Sans",
            "https://zed.dev/docs and github.com/zed-industries/zed",
            "use zed::Foo, .zed/settings.json, zed_actions, /zed/ dir",
            "ZED stays, zed stays",
            "\"base_keymap\": \"Zed\"",
            "\"**/{zed,Zed}/{settings,keymap}.json\"",
            "\".ZedMono\" ZedPreview",
        ] {
            assert_eq!(run(keep), keep);
        }
    }

    #[test]
    fn swaps_next_to_a_kept_phrase() {
        assert_eq!(
            run("Zed uses Zed Agent in Zed"),
            "Zenith uses Zed Agent in Zenith"
        );
    }

    #[test]
    fn no_match_borrows() {
        let text: Cow<'static, str> = Cow::Borrowed("nothing to see, Zed Agent only");
        let ptr = text.as_ptr();
        let out = rebrand_text(text);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ptr(), ptr);
    }

    #[test]
    fn idempotent() {
        let once = rebrand_text(Cow::Borrowed("Zed's Zed Agent: restart Zed"));
        let twice = rebrand_text(once.clone());
        assert_eq!(once, twice);
    }

    #[test]
    fn settings_and_keymap_comment_lines() {
        assert_eq!(
            run("  // Whether to confirm before quitting Zed.\n"),
            "  // Whether to confirm before quitting Zenith.\n"
        );
        assert_eq!(
            run("    // Whether to automatically generate a title for Zed Agent threads using the"),
            "    // Whether to automatically generate a title for Zed Agent threads using the"
        );
        assert_eq!(
            run("// Zed's keymap: use \"Zed\" as base_keymap"),
            "// Zenith's keymap: use \"Zed\" as base_keymap"
        );
    }

    #[test]
    fn bundled_default_settings_have_no_unkept_zed_word() {
        let shown = rebrand_text(Cow::Borrowed(include_str!(
            "../../../assets/settings/default.json"
        )));
        for (n, line) in shown.lines().enumerate() {
            let rest = line
                .replace("Zed Agent", "")
                .replace("\"Zed\"", "")
                .replace("{zed,Zed}", "");
            assert!(
                rest.match_indices("Zed").all(|(i, _)| {
                    rest[i + 3..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphanumeric())
                }),
                "line {}: {line}",
                n + 1
            );
        }
    }
}
