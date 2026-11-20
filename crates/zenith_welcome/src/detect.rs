//! Cheap file-presence detection for the welcome screen's project rows.
//! Moves to `zenith_core` once a second crate needs it (plans 19/20/24/25).

use std::path::Path;

use fs::Fs;
use futures::StreamExt as _;

/// WordPress reads a plugin or theme header from the first 8 KB.
const HEADER_BYTES: u64 = 8192;
/// `.git/HEAD` and a worktree's `.git` pointer are a line long.
const GIT_FILE_BYTES: u64 = 4096;

/// Up to `max` bytes of a regular file as text. A project can plant a symlink
/// to `/dev/zero` or a pipe, so anything that is not a regular file is skipped
/// (a pipe would block the open) and the read never goes past `max`.
async fn load_head(fs: &dyn Fs, path: &Path, max: u64) -> Option<String> {
    use std::io::Read as _;
    if !fs.is_file(path).await {
        return None;
    }
    let mut reader = fs.open_sync(path).await.ok()?;
    let mut bytes = Vec::new();
    reader.by_ref().take(max).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Root `.php` files checked for a plugin header; bounds the scan on big roots.
const MAX_PHP_FILES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framework {
    Laravel,
    WordPress,
    Node,
}

impl Framework {
    pub fn label(self) -> &'static str {
        match self {
            Framework::Laravel => "Laravel",
            Framework::WordPress => "WordPress",
            Framework::Node => "Node",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectInfo {
    pub framework: Option<Framework>,
    pub branch: Option<String>,
}

pub async fn detect(fs: &dyn Fs, root: &Path) -> ProjectInfo {
    ProjectInfo {
        framework: framework(fs, root).await,
        branch: branch(fs, root).await,
    }
}

async fn framework(fs: &dyn Fs, root: &Path) -> Option<Framework> {
    if fs.is_file(&root.join("artisan")).await {
        return Some(Framework::Laravel);
    }
    if is_wordpress(fs, root).await {
        return Some(Framework::WordPress);
    }
    if fs.is_file(&root.join("package.json")).await {
        return Some(Framework::Node);
    }
    None
}

async fn is_wordpress(fs: &dyn Fs, root: &Path) -> bool {
    if fs.is_file(&root.join("wp-config.php")).await
        || fs.is_dir(&root.join("wp-includes")).await
        || fs.is_dir(&root.join("wp-content")).await
    {
        return true;
    }
    if load_head(fs, &root.join("style.css"), HEADER_BYTES)
        .await
        .is_some_and(|css| css.contains("Theme Name:"))
    {
        return true;
    }
    let Ok(mut entries) = fs.read_dir(root).await else {
        return false;
    };
    let mut checked = 0;
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else { continue };
        if path.extension().is_none_or(|ext| ext != "php") {
            continue;
        }
        if load_head(fs, &path, HEADER_BYTES)
            .await
            .is_some_and(|php| php.contains("Plugin Name:"))
        {
            return true;
        }
        checked += 1;
        if checked >= MAX_PHP_FILES {
            break;
        }
    }
    false
}

/// Reads the branch from `.git/HEAD`, following a worktree's `gitdir:` file.
async fn branch(fs: &dyn Fs, root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if fs.is_dir(&dot_git).await {
        dot_git
    } else {
        let pointer = load_head(fs, &dot_git, GIT_FILE_BYTES).await?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        root.join(target)
    };
    let head = load_head(fs, &git_dir.join("HEAD"), GIT_FILE_BYTES).await?;
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_string(),
            )
        }
        // Detached: show a short hash.
        None => head.get(..7).map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    async fn info(cx: &mut TestAppContext, tree: serde_json::Value) -> ProjectInfo {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/p", tree).await;
        detect(fs.as_ref(), Path::new("/p")).await
    }

    fn framework_of(info: ProjectInfo) -> Option<Framework> {
        info.framework
    }

    #[gpui::test]
    async fn test_headers_are_read_from_the_first_8_kb_only(cx: &mut TestAppContext) {
        let padding = " ".repeat(9000);
        let late = format!("/*{padding}Theme Name: Late */");
        let tree = json!({ "style.css": late, "index.php": "" });
        assert_eq!(framework_of(info(cx, tree).await), None);
        let late = format!("<?php /*{padding}Plugin Name: Late */");
        assert_eq!(
            framework_of(info(cx, json!({ "late.php": late })).await),
            None
        );
        // Within the first 8 KB it is found.
        let early = format!("<?php /* Plugin Name: Early */{padding}");
        assert_eq!(
            framework_of(info(cx, json!({ "early.php": early })).await),
            Some(Framework::WordPress)
        );
    }

    #[gpui::test]
    async fn test_non_regular_project_files_are_skipped(cx: &mut TestAppContext) {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        // Endless sources and a pipe where headers and `.git` files would be.
        symlink("/dev/zero", dir.0.join("style.css")).unwrap();
        symlink("/dev/zero", dir.0.join("plugin.php")).unwrap();
        mkfifo(&dir.0.join("fifo.php"));
        let git = dir.0.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        symlink("/dev/zero", git.join("HEAD")).unwrap();
        let real = fs::RealFs::new(None, cx.executor());
        cx.executor().allow_parking();
        let root = dir.0.to_path_buf();
        let info = detect(real.as_ref(), &root).await;
        let head = branch(real.as_ref(), &root).await;
        assert_eq!(info.framework, None);
        assert_eq!(head, None);
        assert_eq!(
            load_head(real.as_ref(), &root.join("fifo.php"), 10).await,
            None
        );
        // A regular file is read, and only up to the limit.
        std::fs::write(root.join("big.txt"), "0123456789").unwrap();
        assert_eq!(
            load_head(real.as_ref(), &root.join("big.txt"), 4).await,
            Some("0123".to_string())
        );
    }

    /// A folder removed on drop (the crate has no `tempfile` dependency).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_nanos());
            let path =
                std::env::temp_dir().join(format!("zenith-welcome-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[allow(clippy::disallowed_methods, reason = "mkfifo has no std API")]
    fn mkfifo(path: &Path) {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[gpui::test]
    async fn detects_frameworks(cx: &mut TestAppContext) {
        use Framework::*;
        let cases = [
            (json!({"artisan": ""}), Some(Laravel)),
            (json!({"artisan": "", "package.json": "{}"}), Some(Laravel)),
            (json!({"wp-config.php": ""}), Some(WordPress)),
            (json!({"wp-includes": {}}), Some(WordPress)),
            (json!({"wp-content": {}}), Some(WordPress)),
            (
                json!({"style.css": "/*\nTheme Name: Radius\n*/"}),
                Some(WordPress),
            ),
            (
                json!({"my-plugin.php": "<?php\n/*\n * Plugin Name: Mine\n */"}),
                Some(WordPress),
            ),
            (json!({"index.php": "<?php echo 1;"}), None),
            (json!({"style.css": "body {}"}), None),
            (json!({"package.json": "{}"}), Some(Node)),
            (json!({}), None),
        ];
        for (tree, expected) in cases {
            let label = tree.to_string();
            assert_eq!(framework_of(info(cx, tree).await), expected, "{label}");
        }
    }

    #[gpui::test]
    async fn reads_git_branch(cx: &mut TestAppContext) {
        let branch_of = |info: ProjectInfo| info.branch;
        let on_ref = json!({".git": {"HEAD": "ref: refs/heads/feature/x\n"}});
        assert_eq!(
            branch_of(info(cx, on_ref).await).as_deref(),
            Some("feature/x")
        );

        let detached = json!({".git": {"HEAD": "0123456789abcdef\n"}});
        assert_eq!(
            branch_of(info(cx, detached).await).as_deref(),
            Some("0123456")
        );

        let worktree = json!({
            ".git": "gitdir: wt/.gitdir\n",
            "wt": {".gitdir": {"HEAD": "ref: refs/heads/dev"}},
        });
        assert_eq!(branch_of(info(cx, worktree).await).as_deref(), Some("dev"));

        assert_eq!(branch_of(info(cx, json!({})).await), None);
    }
}
