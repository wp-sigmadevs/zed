//! Cheap file-presence detection for the welcome screen's project rows.
//! Moves to `zenith_core` once a second crate needs it (plans 19/20/24/25).

use std::path::Path;

use fs::Fs;
use futures::StreamExt as _;

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
    if fs
        .load(&root.join("style.css"))
        .await
        .is_ok_and(|css| css.contains("Theme Name:"))
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
        if fs
            .load(&path)
            .await
            .is_ok_and(|php| php.contains("Plugin Name:"))
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
        let pointer = fs.load(&dot_git).await.ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        root.join(target)
    };
    let head = fs.load(&git_dir.join("HEAD")).await.ok()?;
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
