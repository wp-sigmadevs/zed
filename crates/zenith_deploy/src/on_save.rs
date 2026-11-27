//! Auto-upload of mapped project files (plan 37 M4b), the pure rules.
//!
//! No store and no UI here. The mapping lives only in the user's own `servers.json`
//! (`ServerConfig::mappings`); nothing in a repo is read for a mapping or for permission.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use util::{paths::PathMatcher, paths::PathStyle};
use workspace::SaveIntent;

use crate::{
    config::ServerConfig,
    remote_path::{join_remote, validate_name, validate_remote_path},
};

/// An explicit save by the user. Closing, quitting, skipping and "Save As" never upload.
/// `SaveAll` and `Overwrite` are not explicit: the close prompt's "Save all", the quit prompt and
/// vim's `:wa!`, `:xa!`, `:wqa!` and `:exi!` send them through Save All or Close All and save
/// every open file, clean ones too. So vim's `:w` uploads, `:w!` (Overwrite) does not.
pub fn is_explicit(intent: SaveIntent) -> bool {
    matches!(
        intent,
        SaveIntent::Save | SaveIntent::FormatAndSave | SaveIntent::SaveWithoutFormat
    )
}

/// What a one-time confirmation or a session answer was given for: the target (host, port, type,
/// user), whether it signs in anonymously, the root folder, the mappings and the excluded local
/// names. Equal fingerprints, equal places and files that go. It holds no secret (target, root and
/// paths only); the store keeps it in `servers.json` as `upload_on_save_fingerprint` to tell a
/// confirmation from a hand edit, and nothing logs it. Changing the exclusions also forgets a session "skip" answer
/// (accepted: it asks once more).
pub(crate) fn fingerprint(server: &ServerConfig) -> String {
    serde_json::json!([
        crate::secrets::target_account(server),
        server.anonymous,
        server.root_path,
        server.mappings,
        server.excluded_local
    ])
    .to_string()
}

/// Whether saving `new` over `old` must ask the user again before auto-upload may stay on: the
/// one-time confirmation was given for what [`fingerprint`] covers. A server without an `old`
/// (a new one) has nothing confirmed.
pub fn needs_reconfirm(old: Option<&ServerConfig>, new: &ServerConfig) -> bool {
    old.is_none_or(|old| fingerprint(old) != fingerprint(new))
}

/// Whether `new` may be saved with `upload_on_save_confirmed` on, over `old` (the saved server, if
/// any). The flag becomes true only when the user confirmed these very settings now
/// (`reconfirmed` is the [`fingerprint`] they confirmed) or it was already true and nothing the
/// fingerprint covers changed. A flag turned on any other way (a stale confirmation, a flag the
/// caller set without asking) does not hold. "Already true and unchanged" means
/// [`confirmation_matches`] for `old`: a flag whose stored fingerprint is missing or stale (a file
/// edited by hand) is not carried over. The store applies this on every save; the dialog uses it
/// to show the checkbox. A hand edit between saves is caught at upload time by the same check.
pub(crate) fn confirmation_holds(
    old: Option<&ServerConfig>,
    new: &ServerConfig,
    reconfirmed: Option<&String>,
) -> bool {
    new.upload_on_save_confirmed
        && (reconfirmed.is_some_and(|confirmed| *confirmed == fingerprint(new))
            || old.is_some_and(|old| confirmation_matches(old) && !needs_reconfirm(Some(old), new)))
}

/// Whether the server's stored confirmation still belongs to its current settings: the flag is on
/// and the fingerprint recorded when the user confirmed equals the server's own now. A host edited
/// by hand, a flag flipped on by hand and a file from before the fingerprint existed all fail.
pub(crate) fn confirmation_matches(server: &ServerConfig) -> bool {
    server.upload_on_save_confirmed
        && server.upload_on_save_fingerprint.as_deref() == Some(fingerprint(server).as_str())
}

/// One `Normal` component as a validated UTF-8 name; `..` and everything else is an error.
fn part(component: Component<'_>) -> Result<&str, String> {
    let Component::Normal(name) = component else {
        return Err(format!("{component:?} is not a plain path part"));
    };
    let name = name
        .to_str()
        .ok_or_else(|| format!("{name:?} is not valid UTF-8"))?;
    validate_name(name).map_err(|error| format!("{error:#}"))
}

/// Whether the server's root folder and mappings are safe to upload by, checked when they are
/// saved and again before every upload (fail closed). A server without mappings is fine. Else the
/// root is an absolute remote path without `..`; every local folder is absolute with plain parts;
/// no two mappings name the same local folder (which of them would a file go by?); and every
/// part of a remote folder is a plain name (`""`, `"/"` and `"a/b"` are fine, `..`, `.` and `\`
/// are not). The message names the offending mapping.
pub fn validate_mappings(server: &ServerConfig) -> Result<(), String> {
    if server.mappings.is_empty() {
        return Ok(());
    }
    validate_remote_path(&server.root_path)
        .map_err(|error| format!("The root folder is not usable: {error:#}"))?;
    let mut seen: Vec<PathBuf> = Vec::new();
    for mapping in &server.mappings {
        let label = format!("Mapping {:?} -> {:?}", mapping.local, mapping.remote);
        let local = Path::new(&mapping.local);
        if !local.is_absolute() {
            return Err(format!("{label}: the local folder is not an absolute path"));
        }
        for component in local.components() {
            if !matches!(component, Component::RootDir | Component::Prefix(_)) {
                part(component).map_err(|error| format!("{label}: {error}"))?;
            }
        }
        let normalised: PathBuf = local.components().collect();
        if seen.contains(&normalised) {
            return Err(format!(
                "{label}: another mapping already uses this local folder"
            ));
        }
        seen.push(normalised);
        for name in mapping.remote.split('/').filter(|name| !name.is_empty()) {
            join_remote("/", name)
                .map_err(|error| format!("{label}: the remote folder is not usable: {error:#}"))?;
        }
    }
    Ok(())
}

/// The remote path of the project file `local`, from the longest of the server's mappings that
/// contains it (whole path parts, never a text prefix). `Ok(None)`: no mapping contains it.
/// `Err`: the path or the matched mapping is unsafe, or the file is in the download cache.
pub fn resolve_mapped(
    server: &ServerConfig,
    local: &Path,
    cache_dir: &Path,
) -> Result<Option<String>, String> {
    Ok(resolve(server, local, cache_dir)?.map(|resolved| resolved.remote))
}

/// A mapped file: where it goes, the mapping folder it is under and its path inside it.
pub struct Resolved {
    pub remote: String,
    pub folder: PathBuf,
    pub rel: PathBuf,
}

pub fn resolve(
    server: &ServerConfig,
    local: &Path,
    cache_dir: &Path,
) -> Result<Option<Resolved>, String> {
    if !local.is_absolute() {
        return Err(format!("{} is not an absolute path", local.display()));
    }
    if local.starts_with(cache_dir) {
        return Err("Files of the download cache are never uploaded by a mapping.".into());
    }
    validate_mappings(server)?;
    let best = server
        .mappings
        .iter()
        .filter_map(|mapping| {
            let folder = Path::new(&mapping.local);
            let rest = local.strip_prefix(folder).ok()?;
            folder.is_absolute().then_some((folder, mapping, rest))
        })
        .max_by_key(|(folder, ..)| folder.components().count());
    // A file no mapping contains is none of our business: its name is not checked (an explicit
    // save of any such file would otherwise answer an error every time).
    let Some((folder, mapping, rest)) = best else {
        return Ok(None);
    };
    // Every part of the file path is checked, also the ones above the mapping folder.
    for component in local.components() {
        if !matches!(component, Component::RootDir | Component::Prefix(_)) {
            part(component)?;
        }
    }
    // The file is the mapped folder itself: nothing to upload.
    if rest.as_os_str().is_empty() {
        return Ok(None);
    }
    // `join_remote` checks the root and every part again.
    let mut remote = server.root_path.clone();
    for name in mapping.remote.split('/').filter(|name| !name.is_empty()) {
        remote = join_remote(&remote, name).map_err(|error| format!("{error:#}"))?;
    }
    for component in rest.components() {
        remote = join_remote(&remote, part(component)?).map_err(|error| format!("{error:#}"))?;
    }
    Ok(Some(Resolved {
        remote,
        folder: folder.to_path_buf(),
        rel: rest.to_path_buf(),
    }))
}

/// Why a file is not auto-uploaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Excluded {
    /// The built-in secret list; the user cannot remove it. Worth a message.
    Fixed,
    /// The user's own `excluded_local`. Silent.
    User,
}

/// The fixed secret list, as data: the one place `is_secret_name` and the dialog's note read.
/// Private keys by name start with one of these (their `.pub` half is not a secret).
pub(crate) const SECRET_KEY_PREFIXES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
pub(crate) const SECRET_PREFIXES: &[&str] = &[".env", "wp-config"];
pub(crate) const SECRET_SUFFIXES: &[&str] = &[".env", ".pem", ".key", ".p12", ".pfx", ".ppk"];
pub(crate) const SECRET_NAMES: &[&str] = &[
    ".htpasswd",
    "auth.json",
    ".npmrc",
    ".git",
    ".git-credentials",
    ".netrc",
    ".pgpass",
    "sftp-config.json",
    ".aws",
];
/// The one fixed exclusion that is a path, not a name: this file in this folder.
pub(crate) const VSCODE_SFTP_JSON: (&str, &str) = (".vscode", "sftp.json");

/// The fixed list for the dialog's note, built from the constants above.
pub(crate) fn fixed_exclusions_text() -> String {
    let quoted = |names: &[&str], before: &str, after: &str| {
        names
            .iter()
            .map(|name| format!("{before}{name}{after}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{}, {}, {}, {}, {}/{}",
        quoted(SECRET_PREFIXES, "", "*"),
        quoted(SECRET_SUFFIXES, "*", ""),
        quoted(SECRET_KEY_PREFIXES, "", "* (not .pub)"),
        quoted(SECRET_NAMES, "", ""),
        VSCODE_SFTP_JSON.0,
        VSCODE_SFTP_JSON.1,
    )
}

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| name.starts_with(prefix));
    let ends = |suffixes: &[&str]| suffixes.iter().any(|suffix| name.ends_with(suffix));
    (starts(SECRET_KEY_PREFIXES) && !name.ends_with(".pub"))
        || starts(SECRET_PREFIXES)
        || ends(SECRET_SUFFIXES)
        || SECRET_NAMES.contains(&name.as_str())
}

/// Refuses an `excluded_local` entry that is not a valid glob. [`user_matcher`] skips such an
/// entry, so a file the user meant to keep out would be uploaded: the dialog and
/// `validate_changed` refuse it before it is saved.
pub fn validate_excluded(server: &ServerConfig) -> Result<(), String> {
    for pattern in &server.excluded_local {
        PathMatcher::new([pattern], PathStyle::local())
            .map_err(|error| format!("The excluded pattern {pattern:?} is not valid: {error}"))?;
    }
    Ok(())
}

/// The user's `excluded_local` as a matcher; an invalid pattern is skipped and logged.
pub fn user_matcher(server: &ServerConfig) -> PathMatcher {
    PathMatcher::new_lenient(&server.excluded_local, PathStyle::local(), |error| {
        log::warn!("deploy: excluded path pattern skipped: {error}");
    })
}

/// Whether `rel` (relative to the mapped folder) must not be uploaded. The secret list is tried
/// on every part of the path (so anything under `.git`, `.env.d`, ...); the user's matcher on the
/// whole path, on every folder above the file and on every single name.
pub fn excluded(rel: &Path, user: &PathMatcher) -> Option<Excluded> {
    let names: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let in_vscode = names.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case(VSCODE_SFTP_JSON.0)
            && pair[1].eq_ignore_ascii_case(VSCODE_SFTP_JSON.1)
    });
    if in_vscode
        || names
            .iter()
            .any(|name| name.to_str().is_none_or(is_secret_name))
    {
        return Some(Excluded::Fixed);
    }
    let mut folder = PathBuf::new();
    for name in names {
        folder.push(name);
        if user.is_match_std_path(&folder) || user.is_match_std_path(Path::new(name)) {
            return Some(Excluded::User);
        }
    }
    None
}

/// Refuses a file whose real location is not where its path says: `file` (under `root`) must
/// canonicalize to the canonical `root` plus its relative path, so a symlink anywhere in the
/// mapped tree, pointing inside or outside, is refused. Blocking: call it from a background
/// task (`cx.background_spawn`), never on the main thread.
///
/// The mapped folder `root` itself must not be a symbolic link (unless it is the project root,
/// which may have been opened through a link), and when it lies under
/// `project_root` it must canonicalize to the canonical project root plus its own relative path
/// (so a link in a folder between the two is refused too).
pub async fn check_real_path(
    root: PathBuf,
    file: PathBuf,
    project_root: Option<PathBuf>,
) -> Result<()> {
    // A project opened through a link is the user's own choice: the project root itself is
    // exempt; the real-path check on the file below still covers everything under it.
    let is_project_root = project_root.as_deref() == Some(root.as_path());
    if !is_project_root && std::fs::symlink_metadata(&root)?.file_type().is_symlink() {
        bail!(
            "{} is a symbolic link; files under it are not uploaded",
            root.display()
        );
    }
    if let Some(project_root) = project_root
        && let Ok(root_rel) = root.strip_prefix(&project_root)
        && std::fs::canonicalize(&root)? != std::fs::canonicalize(&project_root)?.join(root_rel)
    {
        bail!(
            "{} is not at its own path (a symbolic link is in the way); it is not uploaded",
            root.display()
        );
    }
    let rel = file.strip_prefix(&root)?.to_path_buf();
    let real_root = std::fs::canonicalize(&root)?;
    let real_file = std::fs::canonicalize(&file)?;
    if real_file != real_root.join(rel) {
        bail!(
            "{} is not at its own path (a symbolic link is in the way); it is not uploaded",
            file.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mapping;

    #[test]
    fn a_confirmation_matches_only_with_the_flag_and_the_fingerprint_of_these_settings() {
        let mut on = server(&[("/p", "")]);
        on.upload_on_save_confirmed = true;
        assert!(!confirmation_matches(&on), "no stored fingerprint");
        on.upload_on_save_fingerprint = Some(fingerprint(&on));
        assert!(confirmation_matches(&on));
        let mut moved = on.clone();
        moved.host = "evil.example.com".into();
        assert!(!confirmation_matches(&moved), "the target changed by hand");
        let mut off = on.clone();
        off.upload_on_save_confirmed = false;
        assert!(!confirmation_matches(&off), "the flag is off");
        // The same fingerprint does not hold over other settings in `confirmation_holds` either.
        assert!(!confirmation_holds(Some(&moved), &moved, None));
        assert!(confirmation_holds(Some(&on), &on, None));
    }

    fn server(mappings: &[(&str, &str)]) -> ServerConfig {
        ServerConfig {
            root_path: "/var/www".into(),
            mappings: mappings
                .iter()
                .map(|(local, remote)| Mapping {
                    local: local.to_string(),
                    remote: remote.to_string(),
                })
                .collect(),
            ..ServerConfig::default()
        }
    }

    fn resolve(server: &ServerConfig, local: &str) -> Result<Option<String>, String> {
        resolve_mapped(server, Path::new(local), Path::new("/cache"))
    }

    fn user() -> PathMatcher {
        PathMatcher::new(
            ["node_modules", "*.cache", "*/cache", "?.php"],
            PathStyle::Unix,
        )
        .expect("matcher")
    }

    #[test]
    fn only_explicit_saves_are_explicit() {
        for intent in [
            SaveIntent::Save,
            SaveIntent::FormatAndSave,
            SaveIntent::SaveWithoutFormat,
        ] {
            assert!(is_explicit(intent), "{intent:?}");
        }
        for intent in [
            SaveIntent::Overwrite,
            SaveIntent::SaveAll,
            SaveIntent::Close,
            SaveIntent::Skip,
            SaveIntent::SaveAs,
        ] {
            assert!(!is_explicit(intent), "{intent:?}");
        }
    }

    #[test]
    fn a_mapped_file_lands_under_the_root_and_the_mapped_folder() {
        let server = server(&[("/p", "wp-content/themes/t")]);
        assert_eq!(
            resolve(&server, "/p/inc/a.php"),
            Ok(Some("/var/www/wp-content/themes/t/inc/a.php".into()))
        );
    }

    #[test]
    fn a_sibling_folder_with_the_same_text_prefix_is_not_mapped() {
        let server = server(&[("/p", "x")]);
        assert_eq!(resolve(&server, "/p2/a.php"), Ok(None));
        assert_eq!(resolve(&server, "/other/a.php"), Ok(None));
        // The mapped folder itself is not a file.
        assert_eq!(resolve(&server, "/p"), Ok(None));
    }

    #[test]
    fn the_longest_mapping_wins() {
        let server = server(&[("/p", "x"), ("/p/theme", "t")]);
        assert_eq!(
            resolve(&server, "/p/theme/a.php"),
            Ok(Some("/var/www/t/a.php".into()))
        );
        assert_eq!(
            resolve(&server, "/p/b.php"),
            Ok(Some("/var/www/x/b.php".into()))
        );
    }

    #[test]
    fn unsafe_paths_are_errors() {
        let server = server(&[("/p", "x")]);
        assert!(resolve(&server, "/p/../etc/passwd").is_err());
        assert!(resolve(&server, "p/a.php").is_err());
        assert!(resolve(&server, "/p/a\nb.php").is_err());
        // The cache is never uploaded by a mapping, even when a mapping covers it.
        let covering = self::server(&[("/", "x")]);
        assert!(resolve(&covering, "/cache/s/h/a.php").is_err());
        // A bad remote part of the matched mapping is an error, not a fallback.
        assert!(resolve(&self::server(&[("/p", "../up")]), "/p/a.php").is_err());
        assert!(resolve(&self::server(&[("/p", "a")]), "/p/b\\c.php").is_err());
        // A file that matches no mapping is `None`, whatever its name: no error to show.
        assert_eq!(
            resolve(&self::server(&[("/p", "a")]), "/other/a\nb.php"),
            Ok(None)
        );
        // A root that is not an absolute remote path is an error.
        let mut relative_root = self::server(&[("/p", "x")]);
        relative_root.root_path = "www".into();
        assert!(resolve(&relative_root, "/p/a.php").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_an_error() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};
        let server = server(&[("/p", "x")]);
        let bad = Path::new(OsStr::from_bytes(b"/p/\xff.php"));
        assert!(resolve_mapped(&server, bad, Path::new("/cache")).is_err());
    }

    #[test]
    fn the_root_and_an_empty_mapped_folder_join_cleanly() {
        let mut at_root = server(&[("/p", "")]);
        at_root.root_path = "/".into();
        assert_eq!(resolve(&at_root, "/p/a.php"), Ok(Some("/a.php".into())));
        let in_root = server(&[("/p", "/")]);
        assert_eq!(
            resolve(&in_root, "/p/s/a.php"),
            Ok(Some("/var/www/s/a.php".into()))
        );
    }

    #[test]
    fn secrets_are_excluded_by_the_fixed_list() {
        let user = user();
        for secret in [
            "a/.git/config",
            "sub/wp-config.php",
            ".env.local",
            "WP-Config.php",
            "k/site.pem",
            "k/site.KEY",
            "id_rsa",
            "d/.htpasswd",
            "auth.json",
            ".npmrc",
            ".env.d/x.php",
        ] {
            assert_eq!(
                excluded(Path::new(secret), &user),
                Some(Excluded::Fixed),
                "{secret}"
            );
        }
        assert_eq!(excluded(Path::new("index.php"), &user), None);
        assert_eq!(excluded(Path::new("inc/keys.php"), &user), None);
    }

    #[test]
    fn the_secret_list_ignores_case_as_a_case_insensitive_disk_does() {
        let user = user();
        for secret in [
            ".ENV",
            "WP-CONFIG.PHP",
            "ID_RSA",
            "Id_Ed25519.bak",
            ".GIT/config",
            ".HTPASSWD",
            "AUTH.JSON",
            ".NpmRc",
            "SFTP-Config.json",
            ".AWS/credentials",
            "CERT.PEM",
        ] {
            assert_eq!(
                excluded(Path::new(secret), &user),
                Some(Excluded::Fixed),
                "{secret}"
            );
        }
        assert_eq!(excluded(Path::new("ID_RSA.PUB"), &user), None);
    }

    #[test]
    fn more_secret_names_are_excluded_and_a_public_key_is_not() {
        let user = user();
        for secret in [
            "wp-config.php.bak",
            "wp-config.php~",
            "wp-config.old.php",
            "wp-config-sample.php",
            "production.env",
            "foo.ENV",
            ".git-credentials",
            ".netrc",
            ".pgpass",
            "putty/site.ppk",
            "id_rsa_old",
            "id_ed25519.bak",
            "id_dsa",
            "id_ecdsa",
            "sftp-config.json",
            ".vscode/sftp.json",
            "a/.VSCode/SFTP.json",
            ".aws/credentials",
            "x/.aws/config",
        ] {
            assert_eq!(
                excluded(Path::new(secret), &user),
                Some(Excluded::Fixed),
                "{secret}"
            );
        }
        for fine in [
            "id_rsa.pub",
            "id_ed25519.pub",
            "sftp.json",
            ".vscode/settings.json",
            "foo.environment",
            "inc/aws.php",
        ] {
            assert_eq!(excluded(Path::new(fine), &user), None, "{fine}");
        }
    }

    #[test]
    fn the_dialog_note_names_every_fixed_exclusion() {
        let text = fixed_exclusions_text();
        for (entries, before, after) in [
            (SECRET_PREFIXES, "", "*"),
            (SECRET_SUFFIXES, "*", ""),
            (SECRET_KEY_PREFIXES, "", "*"),
            (SECRET_NAMES, "", ""),
        ] {
            for entry in entries {
                let shown = format!("{before}{entry}{after}");
                assert!(text.contains(&shown), "{shown} is missing from {text}");
            }
        }
        assert!(text.contains(".vscode/sftp.json"), "{text}");
    }

    #[test]
    fn an_invalid_excluded_pattern_is_refused_and_valid_ones_are_not() {
        let mut server = ServerConfig::new();
        assert_eq!(validate_excluded(&server), Ok(()), "the defaults are valid");
        server.excluded_local = ["vendor", "*.log", "build/**", "[ab]c"]
            .map(String::from)
            .to_vec();
        assert_eq!(validate_excluded(&server), Ok(()));
        server.excluded_local.push("[unclosed".into());
        let error = validate_excluded(&server).expect_err("an unclosed class is no glob");
        assert!(error.contains("[unclosed"), "{error}");
    }

    #[test]
    fn the_users_patterns_apply_to_the_path_every_folder_and_every_name() {
        let user = user();
        assert_eq!(
            excluded(Path::new("node_modules/x.js"), &user),
            Some(Excluded::User)
        );
        // Only the folder above matches (`*.cache` does not match the whole path).
        assert_eq!(
            excluded(Path::new("a.cache/x.js"), &user),
            Some(Excluded::User)
        );
        // Matches only the folder above (`*/cache` does not match the whole path or one name).
        assert_eq!(
            excluded(Path::new("d/cache/x.js"), &user),
            Some(Excluded::User)
        );
        // Matches only the single name (`?.php` does not match `d/b.php`).
        assert_eq!(excluded(Path::new("d/b.php"), &user), Some(Excluded::User));
        assert_eq!(
            excluded(Path::new("deep/node_modules"), &user),
            Some(Excluded::User)
        );
        assert_eq!(excluded(Path::new("src/x.js"), &user), None);
        // The user cannot switch the fixed list off.
        assert_eq!(
            excluded(Path::new("wp-config.php"), &PathMatcher::default()),
            Some(Excluded::Fixed)
        );
    }

    #[test]
    fn a_real_path_equal_to_the_given_path_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("inc")).expect("mkdir");
        std::fs::write(dir.path().join("inc/a.php"), "x").expect("write");
        let result = smol::block_on(check_real_path(
            dir.path().to_path_buf(),
            dir.path().join("inc/a.php"),
            None,
        ));
        assert!(result.is_ok(), "{result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_mapped_tree_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("outside")).expect("mkdir");
        std::fs::write(dir.path().join("outside/s.txt"), "secret").expect("write");
        std::fs::write(root.join("real.php"), "x").expect("write");
        std::os::unix::fs::symlink("../outside/s.txt", root.join("link.php")).expect("link");
        // A link to a file inside the tree and a link to a folder are refused too.
        std::os::unix::fs::symlink("real.php", root.join("inside.php")).expect("link");
        std::os::unix::fs::symlink("../outside", root.join("dirlink")).expect("link");
        for file in ["link.php", "inside.php", "dirlink/s.txt"] {
            let result = smol::block_on(check_real_path(root.clone(), root.join(file), None));
            assert!(result.is_err(), "{file} must be refused");
        }
        let result = smol::block_on(check_real_path(root.clone(), root.join("real.php"), None));
        assert!(result.is_ok(), "{result:?}");
        // A missing file is an error, not a pass.
        assert!(
            smol::block_on(check_real_path(root.clone(), root.join("gone.php"), None)).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_mapped_folder_that_is_a_link_or_sits_behind_one_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        let other = dir.path().join("other");
        std::fs::create_dir_all(project.join("real/inner")).expect("mkdir");
        std::fs::create_dir_all(other.join("inner")).expect("mkdir");
        std::fs::write(other.join("x.php"), "x").expect("write");
        std::fs::write(other.join("inner/x.php"), "x").expect("write");
        std::fs::write(project.join("real/inner/x.php"), "x").expect("write");
        // The mapped folder itself is a link to another directory.
        std::os::unix::fs::symlink(&other, project.join("sub")).expect("link");
        let root = project.join("sub");
        let result = smol::block_on(check_real_path(
            root.clone(),
            root.join("x.php"),
            Some(project.clone()),
        ));
        assert!(result.is_err(), "a link as the mapped folder: {result:?}");
        let result = smol::block_on(check_real_path(root.clone(), root.join("x.php"), None));
        assert!(result.is_err(), "also without a project root: {result:?}");

        // A folder between the project root and the mapped folder is a link.
        std::fs::rename(project.join("real"), dir.path().join("moved")).expect("move");
        std::os::unix::fs::symlink(&other, project.join("real")).expect("link");
        let root = project.join("real/inner");
        let file = root.join("x.php");
        let result = smol::block_on(check_real_path(root, file, Some(project)));
        assert!(
            result.is_err(),
            "a link above the mapped folder: {result:?}"
        );

        // A plain tree passes, with and without the project root; a mapped folder above the
        // project root only gets the link check on itself.
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(plain.join("a/b")).expect("mkdir");
        std::fs::write(plain.join("a/b/f.php"), "x").expect("write");
        for project_root in [None, Some(plain.clone()), Some(plain.join("a/b/c"))] {
            let result = smol::block_on(check_real_path(
                plain.join("a/b"),
                plain.join("a/b/f.php"),
                project_root,
            ));
            assert!(result.is_ok(), "{result:?}");
        }
    }

    #[test]
    fn mappings_are_validated() {
        let ok: [&[(&str, &str)]; 5] = [
            &[],
            &[("/p", "")],
            &[("/p", "/")],
            &[("/p", "a/b"), ("/q", "a//c/")],
            &[("/p", "wp-content"), ("/p/inner", "")],
        ];
        for mappings in ok {
            assert_eq!(validate_mappings(&server(mappings)), Ok(()), "{mappings:?}");
        }
        // No mappings: nothing to check, whatever the root says.
        let mut no_mappings = server(&[]);
        no_mappings.root_path = "relative".into();
        assert_eq!(validate_mappings(&no_mappings), Ok(()));

        let bad: [(&str, &[(&str, &str)]); 11] = [
            ("relative local", &[("p/q", "")]),
            ("dot-dot local", &[("/p/../q", "")]),
            ("backslash local", &[("/p/a\\b", "")]),
            ("dot-dot remote", &[("/p", "..")]),
            ("dot-dot in the middle", &[("/p", "a/../b")]),
            ("dot remote", &[("/p", "./a")]),
            ("backslash remote", &[("/p", "a\\b")]),
            ("control remote", &[("/p", "a\u{7}b")]),
            ("same folder twice", &[("/p", "x"), ("/p", "y")]),
            ("same folder with a slash", &[("/p", "x"), ("/p/", "y")]),
            ("second one bad", &[("/p", "x"), ("/q", "..")]),
        ];
        for (what, mappings) in bad {
            let error = validate_mappings(&server(mappings)).expect_err(what);
            assert!(
                mappings
                    .iter()
                    .any(|(local, _)| error.contains(&format!("{local:?}"))),
                "{what}: the message names no mapping: {error}"
            );
        }
        let mut bad_root = server(&[("/p", "")]);
        for root in ["var/www", "/a/../b", "/a\\b", ""] {
            bad_root.root_path = root.into();
            assert!(validate_mappings(&bad_root).is_err(), "{root:?}");
        }
    }

    #[test]
    fn two_mappings_of_one_folder_refuse_the_upload() {
        let two = server(&[("/p", "x"), ("/p/", "y")]);
        let error = resolve(&two, "/p/f.php").expect_err("ambiguous");
        assert!(error.contains("/p"), "{error}");
        assert!(resolve(&server(&[("/p", "x")]), "/p/f.php").is_ok());
    }

    #[test]
    fn a_change_of_target_root_or_mappings_needs_a_new_confirmation() {
        let old = server(&[("/p", "x")]);
        assert!(!needs_reconfirm(Some(&old), &old.clone()));
        let mut renamed = old.clone();
        renamed.name = "Renamed".into();
        assert!(!needs_reconfirm(Some(&old), &renamed));
        assert!(needs_reconfirm(None, &old));
        let changes: [fn(&mut ServerConfig); 9] = [
            |s| s.host = "other.example.com".into(),
            |s| s.port += 1,
            |s| s.server_type = crate::config::ServerType::Sftp,
            |s| s.username = "someone".into(),
            |s| s.root_path = "/public_html".into(),
            |s| s.mappings[0].remote = "y".into(),
            |s| s.anonymous = !s.anonymous,
            |s| s.excluded_local.push("x".into()),
            |s| s.excluded_local.clear(),
        ];
        for change in changes {
            let mut new = old.clone();
            change(&mut new);
            assert!(needs_reconfirm(Some(&old), &new));
        }
        let mut more = old.clone();
        more.mappings.clear();
        assert!(needs_reconfirm(Some(&old), &more));
    }

    #[cfg(unix)]
    #[test]
    fn a_project_root_opened_through_a_link_passes_but_links_below_it_do_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        let other = dir.path().join("other");
        std::fs::create_dir_all(real.join("sub")).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::write(real.join("a.php"), "x").expect("write");
        std::fs::write(real.join("sub/b.php"), "x").expect("write");
        std::fs::write(other.join("c.php"), "x").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("link");
        let result = smol::block_on(check_real_path(
            link.clone(),
            link.join("a.php"),
            Some(link.clone()),
        ));
        assert!(result.is_ok(), "{result:?}");
        // Without being the project root the same link is still refused.
        assert!(smol::block_on(check_real_path(link.clone(), link.join("a.php"), None)).is_err());
        // A link inside the project opened through a link is refused, as a mapped folder too.
        std::os::unix::fs::symlink(&other, real.join("jump")).expect("link");
        let root = link.join("jump");
        assert!(
            smol::block_on(check_real_path(
                root.clone(),
                root.join("c.php"),
                Some(link.clone())
            ))
            .is_err()
        );
        // A link between the project root and the mapped folder.
        std::fs::create_dir_all(other.join("inner")).expect("mkdir");
        std::fs::write(other.join("inner/d.php"), "x").expect("write");
        let root = link.join("jump/inner");
        assert!(
            smol::block_on(check_real_path(
                root.clone(),
                root.join("d.php"),
                Some(link)
            ))
            .is_err()
        );
    }
}
