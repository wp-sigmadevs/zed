use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// A single file name from a remote listing. Rejects everything that could climb out of its
/// directory or smuggle a second command line: separators, `.`/`..`, and every control
/// character (NUL, CR, LF included).
pub fn validate_name(name: &str) -> Result<&str> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("invalid remote file name {name:?}");
    }
    if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        bail!("remote file name {name:?} contains a path separator or control character");
    }
    Ok(name)
}

/// A remote directory or file path as sent in a protocol command. Absolute, so it can never be
/// read as an option (`-a`) by `LIST` or `sftp`.
pub fn validate_remote_path(path: &str) -> Result<&str> {
    if !path.starts_with('/') || path.chars().any(|c| c.is_control() || c == '\\') {
        bail!("remote path {path:?} is not absolute or contains a control character or `\\`");
    }
    if path.split('/').any(|component| component == "..") {
        bail!("remote path {path:?} contains `..`");
    }
    Ok(path)
}

/// `/a/b/c.php` -> (`/a/b`, `c.php`); the parent of `/c.php` is `/`.
pub(crate) fn split_remote(path: &str) -> Result<(&str, &str)> {
    let (parent, name) = validate_remote_path(path)?
        .rsplit_once('/')
        .context("remote path has no file name")?;
    Ok((
        if parent.is_empty() { "/" } else { parent },
        validate_name(name)?,
    ))
}

pub fn join_remote(directory: &str, name: &str) -> Result<String> {
    validate_remote_path(directory)?;
    validate_name(name)?;
    Ok(format!("{}/{name}", directory.trim_end_matches('/')))
}

/// Where a downloaded `name` lands inside `base`; always a direct child of `base`.
pub fn join_local(base: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let joined = base.join(name);
    if joined.parent() != Some(base) {
        bail!("download path for {name:?} would leave {}", base.display());
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rejects_traversal_and_control_characters() {
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "../x",
            "a\\b",
            "a\0b",
            "a\nb",
            "a\rb",
            "x\n!touch x",
            "\u{7}",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in [
            "index.php",
            ".htaccess",
            "wp-config.php",
            "a b",
            "ünï.txt",
            "...",
        ] {
            assert_eq!(validate_name(good).ok(), Some(good));
        }
    }

    #[test]
    fn join_local_stays_under_target() {
        let base = Path::new("/tmp/target");
        assert_eq!(
            join_local(base, "index.php").ok(),
            Some(PathBuf::from("/tmp/target/index.php"))
        );
        for bad in [
            "../evil",
            "a/../../evil",
            "/etc/passwd",
            "..",
            "x\n!touch x",
        ] {
            assert!(join_local(base, bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn remote_paths_reject_dotdot_and_control_characters() {
        assert!(validate_remote_path("/var/www/html").is_ok());
        for bad in [
            "", "-la", "a/b", "/a/../b", "..", "/a\nb", "/a\0b", "/a\r", "/a\\b", "/a\\../b",
            "/a/b\\",
        ] {
            assert!(
                validate_remote_path(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert_eq!(
            join_remote("/var/www/", "a.php").ok().as_deref(),
            Some("/var/www/a.php")
        );
        assert!(join_remote("/var/www", "../a").is_err());
    }
}
