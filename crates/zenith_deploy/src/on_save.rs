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
    remote_path::{join_remote, validate_name},
};

/// An explicit save by the user. Closing, skipping and "Save As" never upload. `SaveAll` is not
/// explicit: the close prompt's "Save all" and the quit prompt send the same intent and cannot be
/// told apart, so closing or quitting must never upload.
pub fn is_explicit(intent: SaveIntent) -> bool {
    matches!(
        intent,
        SaveIntent::Save
            | SaveIntent::FormatAndSave
            | SaveIntent::SaveWithoutFormat
            | SaveIntent::Overwrite
    )
}

/// Whether saving `new` over `old` must ask the user again before auto-upload may stay on: the
/// one-time confirmation was given for one host, port, type, user, root folder and set of
/// mappings. A server without an `old` (a new one) has nothing confirmed.
pub fn needs_reconfirm(old: Option<&ServerConfig>, new: &ServerConfig) -> bool {
    old.is_none_or(|old| {
        old.credentials_target_changed(new)
            || old.root_path != new.root_path
            || old.mappings != new.mappings
    })
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
    // Every part of the file path is checked, also the ones above the mapping folder.
    for component in local.components() {
        if !matches!(component, Component::RootDir | Component::Prefix(_)) {
            part(component)?;
        }
    }
    let best = server
        .mappings
        .iter()
        .filter_map(|mapping| {
            let folder = Path::new(&mapping.local);
            let rest = local.strip_prefix(folder).ok()?;
            folder.is_absolute().then_some((folder, mapping, rest))
        })
        .max_by_key(|(folder, ..)| folder.components().count());
    let Some((folder, mapping, rest)) = best else {
        return Ok(None);
    };
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

fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    // A private key by name (`id_rsa`, `id_rsa_old`), never its public half.
    let is_key = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|key| name.starts_with(key))
        && !name.ends_with(".pub");
    is_key
        || name.starts_with(".env")
        || name.ends_with(".env")
        || name.starts_with("wp-config")
        || [".pem", ".key", ".p12", ".pfx", ".ppk"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
        || matches!(
            name.as_str(),
            ".htpasswd"
                | "auth.json"
                | ".npmrc"
                | ".git"
                | ".git-credentials"
                | ".netrc"
                | ".pgpass"
                | "sftp-config.json"
                | ".aws"
        )
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
        pair[0].eq_ignore_ascii_case(".vscode") && pair[1].eq_ignore_ascii_case("sftp.json")
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
            SaveIntent::Overwrite,
        ] {
            assert!(is_explicit(intent), "{intent:?}");
        }
        for intent in [
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
    fn a_change_of_target_root_or_mappings_needs_a_new_confirmation() {
        let old = server(&[("/p", "x")]);
        assert!(!needs_reconfirm(Some(&old), &old.clone()));
        let mut renamed = old.clone();
        renamed.name = "Renamed".into();
        renamed.excluded_local.push("x".into());
        assert!(!needs_reconfirm(Some(&old), &renamed));
        assert!(needs_reconfirm(None, &old));
        let changes: [fn(&mut ServerConfig); 6] = [
            |s| s.host = "other.example.com".into(),
            |s| s.port += 1,
            |s| s.server_type = crate::config::ServerType::Sftp,
            |s| s.username = "someone".into(),
            |s| s.root_path = "/public_html".into(),
            |s| s.mappings[0].remote = "y".into(),
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
