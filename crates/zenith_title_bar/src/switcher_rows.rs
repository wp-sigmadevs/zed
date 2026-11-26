//! Pure row logic of the project switcher: badge text and colour, display paths, the
//! entry list.

use std::path::Path;

use gpui::Hsla;
use project::ProjectGroupKey;
use util::paths::PathExt as _;
use workspace::RecentWorkspace;

/// Badge colours, FNV-1a indexed. White text on each has a WCAG contrast of at least 4.5.
const PALETTE: [u32; 8] = [
    0x4a5b8c, 0x2f7d6b, 0x8a4f9e, 0xb0573a, 0x3670a4, 0x66742a, 0xa8436b, 0x5f6b7a,
];

/// Words of a project name: split on anything that is not alphanumeric and on
/// lower-to-upper camelCase boundaries (`WooCommerce` is two words, `WPForms` one).
fn words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            words.push(std::mem::take(&mut word));
        }
        prev_lower = c.is_lowercase();
        word.push(c);
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn first_letter(word: &str) -> String {
    word.chars().take(1).flat_map(char::to_uppercase).collect()
}

/// Badge letters. One name: first letter of its first and last word (`easy-demo-importer`
/// is `EI`), one word gives one letter (`ocdi` is `O`). Several names (multi-root):
/// first letter of the first two names (`EE`). `None` when no name has a letter.
pub fn initials(names: &[&str]) -> Option<String> {
    let named: Vec<Vec<String>> = names
        .iter()
        .map(|name| words(name))
        .filter(|words| !words.is_empty())
        .collect();
    match named.as_slice() {
        [] => None,
        [only] => {
            let first = first_letter(&only[0]);
            Some(match only.len() {
                1 => first,
                n => first + &first_letter(&only[n - 1]),
            })
        }
        [first, second, ..] => Some(first_letter(&first[0]) + &first_letter(&second[0])),
    }
}

/// FNV-1a 32 over the lowercase bytes: stable across runs and versions, unlike `DefaultHasher`.
fn badge_index(name: &str) -> usize {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.to_lowercase().bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as usize % PALETTE.len()
}

pub fn badge_color(name: &str) -> Hsla {
    gpui::rgb(PALETTE[badge_index(name)]).into()
}

/// Letters and colour of a project's badge, from the same input everywhere (title bar
/// and switcher rows). `None` when no name has a letter or digit (emoji-only): no badge.
pub fn badge_for(names: &[&str]) -> Option<(String, Hsla)> {
    Some((initials(names)?, badge_color(&names.join(", "))))
}

/// `~/code/app` for paths under the home directory.
pub fn display_path(path: &Path) -> String {
    path.compact().to_string_lossy().into_owned()
}

/// One row of the switcher. `O` is what activating an open project needs.
pub enum Entry<O> {
    Open,
    Clone,
    Header(&'static str),
    OpenProject(ProjectGroupKey, O),
    Recent(RecentWorkspace),
    /// Footer rows; the switcher adds them after [`entries`].
    OpenRemote,
    More,
}

impl<O> Entry<O> {
    /// Identity of the row, stable while the list around it changes (element ids, selection).
    pub fn id(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::hash::DefaultHasher::new();
        match self {
            Entry::Open => 0u8.hash(&mut hasher),
            Entry::Clone => 1u8.hash(&mut hasher),
            Entry::Header(title) => (2u8, title).hash(&mut hasher),
            Entry::OpenProject(key, _) => (3u8, key).hash(&mut hasher),
            Entry::Recent(project) => (4u8, project.project_group_key()).hash(&mut hasher),
            Entry::OpenRemote => 5u8.hash(&mut hasher),
            Entry::More => 6u8.hash(&mut hasher),
        }
        hasher.finish()
    }
}

/// Open…, Clone…, then the open projects, then recent projects without the open ones
/// (a section with no rows has no header).
pub fn entries<O>(
    open: Vec<(ProjectGroupKey, O)>,
    mut recent: Vec<RecentWorkspace>,
) -> Vec<Entry<O>> {
    recent.retain(|project| {
        !open
            .iter()
            .any(|(key, _)| *key == project.project_group_key())
    });
    let mut entries = vec![Entry::Open, Entry::Clone];
    if !open.is_empty() {
        entries.push(Entry::Header("Open Projects"));
        entries.extend(open.into_iter().map(|(key, o)| Entry::OpenProject(key, o)));
    }
    if !recent.is_empty() {
        entries.push(Entry::Header("Recent Projects"));
        entries.extend(recent.into_iter().map(Entry::Recent));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use util::path_list::PathList;
    use workspace::{SerializedWorkspaceLocation, WorkspaceId};

    #[test]
    fn test_initials() {
        let one = |name| initials(&[name]);
        assert_eq!(one("easy-demo-importer").as_deref(), Some("EI"));
        assert_eq!(
            one("neon-product-designer-for-woocommerce").as_deref(),
            Some("NW")
        );
        assert_eq!(one("ocdi").as_deref(), Some("O"));
        assert_eq!(one("zenith").as_deref(), Some("Z"));
        assert_eq!(one("lumen-gallery").as_deref(), Some("LG"));
        assert_eq!(one("my_wp plugin").as_deref(), Some("MP"));
        assert_eq!(one("WooCommerce").as_deref(), Some("WC"));
        assert_eq!(one("ünïcode-tëst").as_deref(), Some("ÜT"));
        assert_eq!(
            initials(&["easy-demo-importer", "easy-export-settings"]).as_deref(),
            Some("EE")
        );
        assert_eq!(initials(&["--"]), None);
        assert_eq!(initials(&[]), None);
    }

    #[test]
    fn test_badge_color_is_stable_and_spread() {
        // FNV-1a 32 mod 8, case-insensitive.
        let pinned = [
            ("zenith", 5),
            ("easy-demo-importer", 6),
            ("ocdi", 0),
            ("radius", 3),
            ("api", 7),
        ];
        for (name, index) in pinned {
            assert_eq!(badge_index(name), index, "{name}");
            assert_eq!(
                badge_color(name),
                gpui::rgb(PALETTE[index]).into(),
                "{name}"
            );
        }
        assert_eq!(badge_index("ZENITH"), 5);
        let names = [
            "zenith",
            "easy-demo-importer",
            "ocdi",
            "lumen-gallery",
            "neon-product-designer-for-woocommerce",
            "my_wp plugin",
            "radius",
            "api",
        ];
        let distinct: std::collections::HashSet<_> = names.iter().map(|n| badge_index(n)).collect();
        assert!(distinct.len() >= 4, "{distinct:?}");
    }

    /// WCAG relative luminance contrast of white on `rgb`.
    fn white_contrast(rgb: u32) -> f64 {
        let linear = |shift: u32| {
            let c = f64::from((rgb >> shift) & 0xff) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0);
        1.05 / (luminance + 0.05)
    }

    #[test]
    fn test_white_text_reads_on_every_badge_colour() {
        for rgb in PALETTE {
            assert!(
                white_contrast(rgb) >= 4.5,
                "{rgb:06x}: {}",
                white_contrast(rgb)
            );
        }
    }

    #[test]
    fn test_badge_for_is_one_function_for_letters_and_colour() {
        let (letters, color) = badge_for(&["easy-demo-importer"]).expect("badge");
        assert_eq!(letters, "EI");
        assert_eq!(color, badge_color("easy-demo-importer"));
        // Multi-root: the colour comes from the names joined.
        let (letters, color) = badge_for(&["alpha", "beta"]).expect("badge");
        assert_eq!(letters, "AB");
        assert_eq!(color, badge_color("alpha, beta"));
        // Emoji-only: no empty coloured box.
        assert!(badge_for(&["🚀"]).is_none());
        assert!(badge_for(&[]).is_none());
    }

    #[test]
    fn test_row_ids_follow_the_row_not_its_position() {
        let ids = |rows: &[Entry<u8>]| rows.iter().map(Entry::id).collect::<Vec<_>>();
        let few = entries(vec![(key("/code/a"), 1)], vec![recent(2, "/code/b")]);
        let more = entries(
            vec![(key("/code/a"), 1)],
            vec![recent(3, "/code/c"), recent(2, "/code/b")],
        );
        let (few, more) = (ids(&few), ids(&more));
        // `a` and `b` keep their ids when `c` lands above `b`.
        for id in &few {
            assert!(more.contains(id));
        }
        let mut unique = more.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), more.len());
    }

    #[test]
    fn test_display_path_compacts_home() {
        let home = util::paths::home_dir();
        assert_eq!(display_path(&home.join("code/app")), "~/code/app");
        assert_eq!(display_path(Path::new("/srv/app")), "/srv/app");
    }

    fn recent(id: i64, path: &str) -> RecentWorkspace {
        let paths = PathList::new(&[path]);
        RecentWorkspace {
            workspace_id: WorkspaceId::from_i64(id),
            location: SerializedWorkspaceLocation::Local,
            identity_paths: paths.clone(),
            paths,
            path_style: None,
            timestamp: Utc::now(),
        }
    }

    fn key(path: &str) -> ProjectGroupKey {
        ProjectGroupKey::new(None, PathList::new(&[path]))
    }

    fn shape(entries: &[Entry<u8>]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Entry::Open => "open".into(),
                Entry::Clone => "clone".into(),
                Entry::Header(title) => format!("# {title}"),
                Entry::OpenProject(key, o) => {
                    format!("open {} {o}", key.path_list().paths()[0].display())
                }
                Entry::Recent(project) => format!("recent {}", project.paths.paths()[0].display()),
                Entry::OpenRemote | Entry::More => "footer".into(),
            })
            .collect()
    }

    #[test]
    fn test_entries_drop_open_projects_from_recent() {
        let rows = entries(
            vec![(key("/code/a"), 1)],
            vec![recent(1, "/code/a"), recent(2, "/code/b")],
        );
        assert_eq!(
            shape(&rows),
            [
                "open",
                "clone",
                "# Open Projects",
                "open /code/a 1",
                "# Recent Projects",
                "recent /code/b"
            ]
        );
    }

    #[test]
    fn test_entries_without_recent_or_open_have_no_empty_header() {
        let rows = entries::<u8>(vec![], vec![recent(1, "/code/a")]);
        assert_eq!(
            shape(&rows),
            ["open", "clone", "# Recent Projects", "recent /code/a"]
        );
        let rows = entries(vec![(key("/code/a"), 1)], vec![recent(1, "/code/a")]);
        assert_eq!(
            shape(&rows),
            ["open", "clone", "# Open Projects", "open /code/a 1"]
        );
    }
}
