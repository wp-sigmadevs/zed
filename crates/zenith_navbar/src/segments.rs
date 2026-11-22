//! Pure logic of the navigation bar: path → segments, truncation, folder order, symbol path.

use std::{ops::Range, sync::Arc};

use util::rel_path::RelPath;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// A folder; the worktree root has an empty path.
    Dir,
    File,
    /// The n-th symbol (0 = outermost) of the caret's symbol path.
    Symbol(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub label: String,
    /// The folder or file path in its worktree (the file's for symbols).
    pub path: Arc<RelPath>,
}

/// `root › folder › … › file › symbol › …` for `path` in a worktree named `root_name`.
pub fn path_segments(
    root_name: &str,
    path: &RelPath,
    is_dir: bool,
    symbols: impl IntoIterator<Item = String>,
) -> Vec<Segment> {
    // A single-file worktree: its root is the file.
    if path.is_empty() && !is_dir {
        return vec![Segment {
            kind: SegmentKind::File,
            label: root_name.to_string(),
            path: path.into_arc(),
        }];
    }
    let mut segments = vec![Segment {
        kind: SegmentKind::Dir,
        label: root_name.to_string(),
        path: RelPath::empty_arc(),
    }];
    segments.extend(
        path.ancestors()
            .rev()
            .filter(|dir| !dir.is_empty())
            .map(|ancestor| {
                let kind = if ancestor != path || is_dir {
                    SegmentKind::Dir
                } else {
                    SegmentKind::File
                };
                Segment {
                    kind,
                    label: ancestor.file_name().unwrap_or_default().to_string(),
                    path: ancestor.into_arc(),
                }
            }),
    );
    if !is_dir {
        segments.extend(
            symbols
                .into_iter()
                .enumerate()
                .map(|(level, label)| Segment {
                    kind: SegmentKind::Symbol(level),
                    label,
                    path: path.into_arc(),
                }),
        );
    }
    segments
}

/// Characters a separator (` › `) takes.
const SEPARATOR: usize = 3;

/// Which segments fit in `budget` characters: `Some(index)` is shown, `None` is one `…` standing
/// for the hidden run. The first and last segments always show; middle ones hide from the second
/// on, so the folders nearest the file stay.
pub fn fit(label_lens: &[usize], budget: usize) -> Vec<Option<usize>> {
    let mut total: usize = label_lens.iter().map(|len| len + SEPARATOR).sum();
    let mut hidden = 0;
    if label_lens.len() > 2 && total > budget {
        total += 1 + SEPARATOR;
        while hidden < label_lens.len() - 2 && total > budget {
            hidden += 1;
            total -= label_lens[hidden] + SEPARATOR;
        }
    }
    let mut slots: Vec<Option<usize>> = (0..label_lens.len()).map(Some).collect();
    if hidden > 0 {
        slots.splice(1..=hidden, [None]);
    }
    slots
}

/// Folders first, then files, each by name ignoring case, as PhpStorm lists them.
pub fn sort_children(children: &mut [(Arc<RelPath>, bool)]) {
    children.sort_by_cached_key(|(path, is_dir)| {
        (!is_dir, path.file_name().unwrap_or_default().to_lowercase())
    });
}

/// Indices of the outline items (`(depth, range)`, in document order) containing `cursor`,
/// outermost first, as the editor's breadcrumbs pick them.
pub fn symbol_path<T: Ord>(items: &[(usize, Range<T>)], cursor: &T) -> Vec<usize> {
    let mut path: Vec<usize> = Vec::new();
    for (ix, (depth, range)) in items.iter().enumerate() {
        let deeper = path.last().is_none_or(|&last| *depth > items[last].0);
        if deeper && range.start <= *cursor && *cursor <= range.end {
            path.push(ix);
        }
    }
    path
}

/// Indices of the items at the depth of `path[level]` inside the same parent.
pub fn sibling_symbols<T: Ord>(
    items: &[(usize, Range<T>)],
    path: &[usize],
    level: usize,
) -> Vec<usize> {
    let Some(&own) = path.get(level) else {
        return Vec::new();
    };
    let depth = items[own].0;
    let parent = level.checked_sub(1).map(|parent| &items[path[parent]].1);
    items
        .iter()
        .enumerate()
        .filter(|(_, (item_depth, range))| {
            *item_depth == depth
                && parent
                    .is_none_or(|parent| parent.start <= range.start && range.end <= parent.end)
        })
        .map(|(ix, _)| ix)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> &RelPath {
        RelPath::from_unix_str(path).unwrap()
    }

    fn labels(segments: &[Segment]) -> Vec<&str> {
        segments
            .iter()
            .map(|segment| segment.label.as_str())
            .collect()
    }

    #[test]
    fn file_path_becomes_root_folders_file() {
        let segments = path_segments(
            "project",
            rel(".github/workflows/unit-tests.yml"),
            false,
            Vec::new(),
        );
        assert_eq!(
            labels(&segments),
            ["project", ".github", "workflows", "unit-tests.yml"]
        );
        let kinds: Vec<_> = segments.iter().map(|s| s.kind.clone()).collect();
        assert_eq!(
            kinds,
            [
                SegmentKind::Dir,
                SegmentKind::Dir,
                SegmentKind::Dir,
                SegmentKind::File
            ]
        );
        assert!(segments[0].path.is_empty());
        assert_eq!(segments[2].path.as_unix_str(), ".github/workflows");
        assert_eq!(
            segments[3].path.as_unix_str(),
            ".github/workflows/unit-tests.yml"
        );
    }

    #[test]
    fn symbols_follow_the_file() {
        let segments = path_segments(
            "app",
            rel("src/User.php"),
            false,
            ["class User".to_string(), "function save".to_string()],
        );
        assert_eq!(
            labels(&segments),
            ["app", "src", "User.php", "class User", "function save"]
        );
        assert_eq!(segments[4].kind, SegmentKind::Symbol(1));
        assert_eq!(segments[4].path.as_unix_str(), "src/User.php");
    }

    #[test]
    fn root_file_and_folder_and_single_file_worktree() {
        assert_eq!(
            labels(&path_segments("p", rel("a.txt"), false, Vec::new())),
            ["p", "a.txt"]
        );
        let dir = path_segments("p", rel("src/lib"), true, ["ignored".to_string()]);
        assert_eq!(labels(&dir), ["p", "src", "lib"]);
        assert_eq!(dir[2].kind, SegmentKind::Dir);
        let single = path_segments("notes.md", rel(""), false, Vec::new());
        assert_eq!(labels(&single), ["notes.md"]);
        assert_eq!(single[0].kind, SegmentKind::File);
    }

    #[test]
    fn fit_keeps_everything_that_fits() {
        assert_eq!(fit(&[5, 5, 5], 100), [Some(0), Some(1), Some(2)]);
        assert_eq!(fit(&[50, 50], 10), [Some(0), Some(1)]);
        assert_eq!(fit(&[], 10), Vec::<Option<usize>>::new());
    }

    #[test]
    fn fit_hides_middle_segments_from_the_left() {
        // 4 segments of 10: 52 chars in full.
        let lens = [10, 10, 10, 10];
        // Hiding #1 leaves 10+10+10 + 4 separators... 39 + 4 (…) = 43.
        assert_eq!(fit(&lens, 45), [Some(0), None, Some(2), Some(3)]);
        assert_eq!(fit(&lens, 30), [Some(0), None, Some(3)]);
        // Never hides the first or last, even over budget.
        assert_eq!(fit(&lens, 1), [Some(0), None, Some(3)]);
    }

    #[test]
    fn children_list_folders_first_then_files_ignoring_case() {
        let mut children: Vec<(Arc<RelPath>, bool)> = [
            ("d/b.txt", false),
            ("d/Zeta", true),
            ("d/A.txt", false),
            ("d/alpha", true),
            ("d/.github", true),
        ]
        .into_iter()
        .map(|(path, is_dir)| (rel(path).into_arc(), is_dir))
        .collect();
        sort_children(&mut children);
        let names: Vec<_> = children
            .iter()
            .map(|(path, _)| path.file_name().unwrap())
            .collect();
        assert_eq!(names, [".github", "alpha", "Zeta", "A.txt", "b.txt"]);
    }

    #[test]
    fn symbol_path_and_siblings() {
        // class A { fn a1 {} fn a2 {} }  class B { fn b1 {} }
        let items = vec![
            (0, 0..50),  // 0 class A
            (1, 5..20),  // 1 a1
            (1, 25..45), // 2 a2
            (0, 60..90), // 3 class B
            (1, 65..80), // 4 b1
        ];
        assert_eq!(symbol_path(&items, &30), [0, 2]);
        assert_eq!(symbol_path(&items, &55), Vec::<usize>::new());
        assert_eq!(symbol_path(&items, &70), [3, 4]);

        let path = symbol_path(&items, &30);
        assert_eq!(sibling_symbols(&items, &path, 0), [0, 3]);
        assert_eq!(sibling_symbols(&items, &path, 1), [1, 2]);
        assert_eq!(sibling_symbols(&items, &path, 2), Vec::<usize>::new());
    }
}
