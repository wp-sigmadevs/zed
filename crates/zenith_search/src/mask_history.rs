//! The file masks Find in Files was last confirmed with, most recent first, for the
//! dropdown next to the "File mask:" box. Kept in the app's key-value store, so they
//! survive a restart.

use db::kvp::KeyValueStore;
use gpui::{App, Global};
use util::ResultExt as _;

/// How many masks the dropdown remembers.
const MAX_MASKS: usize = 10;
/// What an empty history offers, as PhpStorm's mask box does.
const DEFAULT_MASKS: [&str; 4] = ["*.php", "*.js", "*.ts", "*.blade.php"];
const KEY: &str = "zenith_search.file_mask_history";

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct MaskHistory {
    /// Most recent first, without repeats, at most [`MAX_MASKS`].
    masks: Vec<String>,
}

impl Global for MaskHistory {}

impl MaskHistory {
    /// The saved form: one mask per line, a mask being a single line already.
    fn parse(saved: &str) -> Self {
        let mut history = Self::default();
        // Saved most recent first; recording walks oldest first.
        for mask in saved.lines().rev() {
            history.record(mask);
        }
        history
    }

    fn serialize(&self) -> String {
        self.masks.join("\n")
    }

    /// Moves `mask` to the front. Returns whether the list changed.
    fn record(&mut self, mask: &str) -> bool {
        let mask = mask.trim();
        if mask.is_empty() || self.masks.first().is_some_and(|first| first == mask) {
            return false;
        }
        self.masks.retain(|known| known != mask);
        self.masks.insert(0, mask.to_string());
        self.masks.truncate(MAX_MASKS);
        true
    }

    /// What the dropdown lists: the history, or the defaults while there is none.
    fn entries(&self) -> Vec<String> {
        if self.masks.is_empty() {
            DEFAULT_MASKS.map(str::to_string).to_vec()
        } else {
            self.masks.clone()
        }
    }
}

/// The history, read from the store the first time it is needed.
fn history(cx: &mut App) -> &mut MaskHistory {
    if !cx.has_global::<MaskHistory>() {
        let saved = KeyValueStore::global(cx)
            .read_kvp(KEY)
            .log_err()
            .flatten()
            .unwrap_or_default();
        cx.set_global(MaskHistory::parse(&saved));
    }
    cx.global_mut::<MaskHistory>()
}

/// Loads the history so the dropdown can list it while only reading the app.
pub(crate) fn load(cx: &mut App) {
    history(cx);
}

/// The masks the dropdown lists.
pub(crate) fn entries(cx: &App) -> Vec<String> {
    cx.try_global::<MaskHistory>()
        .map_or_else(|| MaskHistory::default().entries(), MaskHistory::entries)
}

/// Remembers a mask a search was confirmed with.
pub(crate) fn remember(mask: &str, cx: &mut App) {
    let history = history(cx);
    if !history.record(mask) {
        return;
    }
    let saved = history.serialize();
    let store = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        store.write_kvp(KEY.to_string(), saved).await
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(masks: &[&str]) -> MaskHistory {
        let mut history = MaskHistory::default();
        for mask in masks {
            history.record(mask);
        }
        history
    }

    #[test]
    fn test_most_recent_mask_is_first_and_repeats_move_up() {
        let mut history = recorded(&["*.php", "*.js", "*.ts"]);
        assert_eq!(history.entries(), ["*.ts", "*.js", "*.php"]);
        assert!(history.record("*.php"));
        assert_eq!(history.entries(), ["*.php", "*.ts", "*.js"]);
        // Already first, nothing to save.
        assert!(!history.record("*.php"));
    }

    #[test]
    fn test_masks_are_trimmed_and_blank_ones_ignored() {
        let mut history = recorded(&["  *.php, *.js  "]);
        assert_eq!(history.entries(), ["*.php, *.js"]);
        assert!(!history.record("   "));
        assert!(!history.record("*.php, *.js"));
        assert_eq!(history.entries().len(), 1);
    }

    #[test]
    fn test_history_is_capped_dropping_the_oldest() {
        let masks: Vec<String> = (0..MAX_MASKS + 3).map(|ix| format!("*.m{ix}")).collect();
        let history = recorded(&masks.iter().map(String::as_str).collect::<Vec<_>>());
        let entries = history.entries();
        assert_eq!(entries.len(), MAX_MASKS);
        assert_eq!(entries[0], format!("*.m{}", MAX_MASKS + 2));
        assert_eq!(entries[MAX_MASKS - 1], "*.m3");
    }

    #[test]
    fn test_empty_history_offers_the_defaults() {
        assert_eq!(
            MaskHistory::default().entries(),
            ["*.php", "*.js", "*.ts", "*.blade.php"]
        );
        // Once something is recorded the defaults go away.
        assert_eq!(recorded(&["*.rs"]).entries(), ["*.rs"]);
    }

    #[test]
    fn test_saved_form_round_trips_in_order() {
        let history = recorded(&["*.php", "*.js", "*.ts"]);
        let loaded = MaskHistory::parse(&history.serialize());
        assert_eq!(loaded, history);
        // A hand-edited or over-long saved value still loads capped and deduped.
        let long = (0..20).map(|ix| format!("*.m{ix}")).collect::<Vec<_>>();
        let loaded = MaskHistory::parse(&format!("{}\n*.m0", long.join("\n")));
        assert_eq!(loaded.entries().len(), MAX_MASKS);
        assert_eq!(loaded.entries()[0], "*.m0");
    }
}
