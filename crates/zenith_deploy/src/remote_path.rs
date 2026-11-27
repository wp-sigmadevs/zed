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

/// Characters that show nothing or reorder text: format characters (zero-width, bidi
/// overrides and isolates, tags, soft hyphen, BOM).
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(u32::from(c),
        0xAD | 0x34F | 0x61C | 0x115F | 0x1160 | 0x180E | 0x200B | 0x200E..=0x200F
        | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFEFF | 0xFFA0 | 0xFFF9..=0xFFFB
        | 0xE0000..=0xE007F)
}

/// The longest remote root or folder path the dialog accepts, in bytes.
pub(crate) const MAX_REMOTE_PATH: usize = 1024;

/// A remote root folder from the server (its `pwd`, a listing) or from the user, strict enough
/// to show and to store: absolute, no `..`/`.`/empty components (one trailing `/` is dropped),
/// no control, invisible or `"` characters, at most [`MAX_REMOTE_PATH`] bytes. The error never
/// repeats the path.
pub(crate) fn validate_root(path: &str) -> Result<String> {
    if path.len() > MAX_REMOTE_PATH {
        bail!("the remote path is longer than {MAX_REMOTE_PATH} bytes");
    }
    if validate_remote_path(path).is_err() {
        bail!("the remote path is not an absolute path without control characters or `..`");
    }
    // Whitespace other than a plain space (line and paragraph separators, no-break and wide
    // spaces) can fake a break or a gap when shown.
    if path
        .chars()
        .any(|c| is_invisible(c) || c == '"' || (c.is_whitespace() && c != ' '))
    {
        bail!("the remote path contains an invisible character, odd whitespace or a quote");
    }
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok("/".into());
    }
    if trimmed[1..]
        .split('/')
        .any(|component| component.is_empty() || component == ".")
    {
        bail!("the remote path contains an empty or `.` component");
    }
    Ok(trimmed.into())
}

/// `/a/b` -> `/a`, `/a` -> `/`, `/` -> `/`.
pub(crate) fn parent_of(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.into(),
        _ => "/".into(),
    }
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
    fn validate_root_accepts_clean_paths_and_trims_one_slash() {
        assert_eq!(validate_root("/www").unwrap(), "/www");
        assert_eq!(validate_root("/www/").unwrap(), "/www");
        assert_eq!(validate_root("/").unwrap(), "/");
        assert_eq!(validate_root("/a b/ü").unwrap(), "/a b/ü");
        assert_eq!(
            validate_root(&format!("/{}", "d".repeat(1023)))
                .unwrap()
                .len(),
            1024
        );
    }

    #[test]
    fn validate_root_rejects_each_hostile_path() {
        let long = format!("/{}", "d".repeat(2000));
        let msg = validate_root(&long).unwrap_err().to_string();
        assert!(msg.len() < 200, "the error must not repeat the path: {msg}");
        for bad in [
            "",
            "relative",
            "-la",
            "/a/../b",
            "/..",
            "/a\nb",
            "/a\rb",
            "/a\0b",
            "/a\u{1b}[31m",
            "/a\u{202E}b",
            "/a\u{200B}b",
            "/a\u{2028}b",
            "/a\u{2029}b",
            "/a\u{00A0}b",
            "/a\u{2003}b",
            "/a\u{3000}b",
            "/www\" is \"x",
            "/a/./b",
            "/./a",
            "/a//b",
            "//a",
            "/a//",
            "/a\\b",
        ] {
            assert!(validate_root(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn parent_of_climbs_to_the_root() {
        assert_eq!(parent_of("/a/b"), "/a");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("/"), "/");
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
