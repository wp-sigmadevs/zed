use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;

use crate::{
    config::{Password, ServerConfig, ServerType},
    remote_path::{join_local, join_remote, validate_name},
    sftp::SftpTransport,
};

/// A server can stream a listing forever; one directory larger than this is refused.
pub const MAX_LISTING_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_LISTING_ENTRIES: usize = 20_000;
/// Downloads open as read-only copies in an editor; a larger file is refused.
pub const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// The server answered and said no (a 550 on one folder, an `sftp` error exit): the connection
/// itself is still good, so the panel keeps its session. Any other error (timeout, I/O) may
/// leave the connection mid-command.
#[derive(Debug)]
pub struct ServerRefused(pub String);

impl std::fmt::Display for ServerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ServerRefused {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// Listed so the tree can show it, never followed or downloaded.
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// One connected session. FTP/FTPS and SFTP implement it; tests use a fake. Every method waits
/// on the network: UI code calls them through [`crate::spawn_operation`].
#[async_trait]
pub trait Transport: Send {
    /// The server's initial directory, used to autodetect the root.
    async fn pwd(&mut self) -> Result<String>;
    /// Entries of `path`, already filtered by [`retain_listable`].
    async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>>;
    /// Writes the remote regular file to `local_path`. Callers go through [`download_entry`].
    async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()>;
    async fn disconnect(&mut self) -> Result<()>;
}

/// Opens a session. Network calls block until the server answers, so UI code runs this and
/// every [`Transport`] call through [`crate::spawn_operation`] (background thread, timeout,
/// cancelled when the task is dropped), never directly on the main thread.
pub async fn connect(server: &ServerConfig) -> Result<Box<dyn Transport>> {
    match server.server_type {
        ServerType::Ftps | ServerType::Ftp => crate::ftp::connect(server).await,
        ServerType::Sftp => Ok(Box::new(SftpTransport::new(server)?)),
    }
}

/// The password an FTP login uses; anonymous servers send a conventional address instead.
pub(crate) fn login_credentials(server: &ServerConfig) -> Result<(String, String)> {
    if server.anonymous {
        return Ok(("anonymous".into(), "anonymous@".into()));
    }
    let Some(Password(password)) = server.password.clone() else {
        bail!("no password is stored for {}", server.name);
    };
    Ok((server.username.clone(), password))
}

/// Drops `.`/`..`, hidden files unless asked for, and every name that fails
/// [`validate_name`] (a hostile server can name a file `x\n!touch y`). More than
/// [`MAX_LISTING_ENTRIES`] entries is an error.
pub(crate) fn retain_listable(
    entries: Vec<RemoteEntry>,
    hidden_files: bool,
) -> Result<Vec<RemoteEntry>> {
    if entries.len() > MAX_LISTING_ENTRIES {
        bail!("the directory has more than {MAX_LISTING_ENTRIES} entries");
    }
    Ok(entries
        .into_iter()
        .filter(|entry| {
            if validate_name(&entry.name).is_err() {
                if entry.name != "." && entry.name != ".." {
                    log::warn!(
                        "deploy: ignoring remote entry with unsafe name {:?}",
                        entry.name
                    );
                }
                return false;
            }
            hidden_files || !entry.name.starts_with('.')
        })
        .collect())
}

/// Downloads one listed entry into `local_directory` and returns the local path.
///
/// The only download path: it refuses symlinks and directories (the remote link target is never
/// resolved), validates the name, writes into a fresh private directory inside `local_directory` (so no remote name can collide
/// with it) and renames the file into place so
/// a local symlink at the destination is replaced, not written through. Run it through
/// [`crate::spawn_operation`].
pub async fn download_entry(
    transport: &mut dyn Transport,
    remote_directory: &str,
    entry: &RemoteEntry,
    local_directory: &Path,
) -> Result<PathBuf> {
    match entry.kind {
        EntryKind::File => {}
        EntryKind::Symlink => bail!("{:?} is a symlink; symlinks are not downloaded", entry.name),
        EntryKind::Directory => bail!("{:?} is a directory", entry.name),
    }
    if entry.size > MAX_DOWNLOAD_BYTES {
        bail!(
            "{:?} is larger than {} MiB",
            entry.name,
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        );
    }
    let remote_path = join_remote(remote_directory, &entry.name)?;
    let local_path = join_local(local_directory, &entry.name)?;
    // Dropped (removed, partial file included) on error or when the task is cancelled mid-transfer.
    let mut part_builder = tempfile::Builder::new();
    part_builder.prefix(".zenith-part-");
    // Owner-only: the half-written file of a private remote must not be listable by other users.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        part_builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let part_directory = part_builder
        .tempdir_in(local_directory)
        .context("creating a directory for the partial download")?;
    let part_path = join_local(part_directory.path(), &entry.name)?;
    transport.download_file(&remote_path, &part_path).await?;
    smol::fs::rename(&part_path, &local_path)
        .await
        .with_context(|| format!("moving the download to {}", local_path.display()))?;
    Ok(local_path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn entry(name: &str, kind: EntryKind) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            kind,
            size: 3,
            modified: None,
        }
    }

    #[derive(Default)]
    pub struct FakeTransport {
        pub downloads: Vec<String>,
        /// Writes the partial file, then never finishes (a stalled transfer).
        pub stall: bool,
        /// Unix mode of the folder holding the partial file, seen while downloading.
        pub part_directory_mode: Option<u32>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn pwd(&mut self) -> Result<String> {
            Ok("/".into())
        }
        async fn list_dir(&mut self, _path: &str) -> Result<Vec<RemoteEntry>> {
            Ok(Vec::new())
        }
        async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
            self.downloads.push(remote_path.to_string());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                self.part_directory_mode = std::fs::metadata(local_path.parent().expect("parent"))
                    .ok()
                    .map(|metadata| metadata.permissions().mode() & 0o777);
            }
            smol::fs::write(local_path, b"abc").await?;
            if self.stall {
                futures::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn download_refuses_symlinks_and_directories() {
        let directory = tempfile::tempdir().expect("tempdir");
        smol::block_on(async {
            let mut transport = FakeTransport::default();
            for kind in [EntryKind::Symlink, EntryKind::Directory] {
                let result = download_entry(
                    &mut transport,
                    "/www",
                    &entry("link", kind),
                    directory.path(),
                )
                .await;
                assert!(result.is_err(), "{kind:?} must not be downloaded");
            }
            assert!(transport.downloads.is_empty(), "nothing may reach the wire");
            assert!(
                std::fs::read_dir(directory.path())
                    .expect("read_dir")
                    .next()
                    .is_none()
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn the_partial_download_folder_is_owner_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut transport = FakeTransport::default();
        smol::block_on(download_entry(
            &mut transport,
            "/www",
            &entry("index.php", EntryKind::File),
            directory.path(),
        ))
        .expect("download");
        assert_eq!(transport.part_directory_mode, Some(0o700));
    }

    #[test]
    fn download_writes_inside_the_target_and_rejects_hostile_names() {
        let directory = tempfile::tempdir().expect("tempdir");
        smol::block_on(async {
            let mut transport = FakeTransport::default();
            let path = download_entry(
                &mut transport,
                "/www",
                &entry("index.php", EntryKind::File),
                directory.path(),
            )
            .await
            .expect("download");
            assert_eq!(path, directory.path().join("index.php"));
            assert_eq!(std::fs::read(&path).expect("read"), b"abc");
            assert_eq!(transport.downloads, ["/www/index.php"]);
            assert_eq!(
                std::fs::read_dir(directory.path())
                    .expect("read_dir")
                    .count(),
                1
            );

            for bad in ["../evil", "a/b", "x\n!touch pwned", "a\rb", ".."] {
                let result = download_entry(
                    &mut transport,
                    "/www",
                    &entry(bad, EntryKind::File),
                    directory.path(),
                )
                .await;
                assert!(result.is_err(), "{bad:?} must be rejected");
            }
            assert_eq!(transport.downloads.len(), 1);
            assert!(
                !directory
                    .path()
                    .parent()
                    .expect("parent")
                    .join("evil")
                    .exists()
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn download_replaces_a_local_symlink_instead_of_writing_through_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let victim = directory.path().join("victim.txt");
        std::fs::write(&victim, "keep").expect("write");
        let target = directory.path().join("dl");
        std::fs::create_dir(&target).expect("mkdir");
        std::os::unix::fs::symlink(&victim, target.join("a.txt")).expect("symlink");
        smol::block_on(async {
            let mut transport = FakeTransport::default();
            download_entry(
                &mut transport,
                "/w",
                &entry("a.txt", EntryKind::File),
                &target,
            )
            .await
            .expect("download");
        });
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep");
        assert_eq!(std::fs::read(target.join("a.txt")).expect("read"), b"abc");
    }

    #[test]
    fn a_remote_name_ending_in_zenith_part_cannot_clash_with_the_partial_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let keep = directory.path().join("x.zenith-part");
        std::fs::write(&keep, "keep").expect("write");
        smol::block_on(async {
            let mut transport = FakeTransport::default();
            download_entry(
                &mut transport,
                "/w",
                &entry("x", EntryKind::File),
                directory.path(),
            )
            .await
            .expect("download x");
            assert_eq!(std::fs::read(&keep).expect("keep"), b"keep");
            download_entry(
                &mut transport,
                "/w",
                &entry("x.zenith-part", EntryKind::File),
                directory.path(),
            )
            .await
            .expect("download x.zenith-part");
        });
        assert_eq!(
            std::fs::read(directory.path().join("x")).expect("x"),
            b"abc"
        );
        assert_eq!(std::fs::read_dir(directory.path()).expect("dir").count(), 2);
    }

    #[test]
    fn listing_drops_unsafe_and_hidden_names() {
        let entries = vec![
            entry(".", EntryKind::Directory),
            entry("..", EntryKind::Directory),
            entry("x\n!touch pwned", EntryKind::File),
            entry("a\rb", EntryKind::File),
            entry(".env", EntryKind::File),
            entry("index.php", EntryKind::File),
            entry("link", EntryKind::Symlink),
        ];
        let visible: Vec<_> = retain_listable(entries.clone(), false)
            .expect("listable")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(visible, ["index.php", "link"], "symlinks stay listed");
        let with_hidden: Vec<_> = retain_listable(entries, true)
            .expect("listable")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(with_hidden, [".env", "index.php", "link"]);
    }

    #[test]
    fn listing_with_too_many_entries_is_refused() {
        let at_limit = vec![entry("a", EntryKind::File); MAX_LISTING_ENTRIES];
        assert!(retain_listable(at_limit, false).is_ok());
        let over = vec![entry("a", EntryKind::File); MAX_LISTING_ENTRIES + 1];
        assert!(retain_listable(over, false).is_err());
    }

    #[test]
    fn download_refuses_a_listed_size_over_the_limit_without_touching_the_wire() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut transport = FakeTransport::default();
        let mut big = entry("big.sql", EntryKind::File);
        big.size = MAX_DOWNLOAD_BYTES + 1;
        let result = smol::block_on(download_entry(
            &mut transport,
            "/www",
            &big,
            directory.path(),
        ));
        assert!(result.is_err());
        assert!(transport.downloads.is_empty());
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the stalled transfer writes the real file system, so the test needs a real clock"
    )]
    fn a_cancelled_download_leaves_no_partial_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut transport = FakeTransport {
            stall: true,
            ..FakeTransport::default()
        };
        let file = entry("index.php", EntryKind::File);
        smol::block_on(async {
            let mut download = Box::pin(download_entry(
                &mut transport,
                "/www",
                &file,
                directory.path(),
            ));
            // Poll until the partial file is written, then drop the future (a cancelled task).
            let written = || {
                std::fs::read_dir(directory.path())
                    .expect("read_dir")
                    .flatten()
                    .any(|dir| dir.path().join("index.php").exists())
            };
            for _ in 0..500 {
                assert!(futures::poll!(download.as_mut()).is_pending());
                if written() {
                    break;
                }
                smol::Timer::after(std::time::Duration::from_millis(10)).await;
            }
            assert!(written());
        });
        let left: Vec<_> = std::fs::read_dir(directory.path())
            .expect("read_dir")
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()
            .expect("entries");
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn anonymous_login_needs_no_password_and_others_do() {
        let mut server = ServerConfig::new();
        server.username = "u".into();
        assert!(login_credentials(&server).is_err());
        server.password = Some(Password("p".into()));
        assert_eq!(
            login_credentials(&server).expect("creds"),
            ("u".into(), "p".into())
        );
        server.anonymous = true;
        assert_eq!(login_credentials(&server).expect("anon").0, "anonymous");
    }
}
