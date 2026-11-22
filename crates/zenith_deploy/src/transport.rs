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

/// One connected session. FTP/FTPS and SFTP implement it; tests use a fake.
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
/// [`validate_name`] (a hostile server can name a file `x\n!touch y`).
pub(crate) fn retain_listable(entries: Vec<RemoteEntry>, hidden_files: bool) -> Vec<RemoteEntry> {
    entries
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
        .collect()
}

/// Downloads one listed entry into `local_directory` and returns the local path.
///
/// The only download path: it refuses symlinks and directories (the remote link target is never
/// resolved), validates the name, writes to a fresh `.part` file and renames it into place so
/// a local symlink at the destination is replaced, not written through.
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
    let remote_path = join_remote(remote_directory, &entry.name)?;
    let local_path = join_local(local_directory, &entry.name)?;
    let part_path = join_local(local_directory, &format!("{}.zenith-part", entry.name))?;

    match smol::fs::remove_file(&part_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing a stale partial download"),
    }
    if let Err(error) = transport.download_file(&remote_path, &part_path).await {
        smol::fs::remove_file(&part_path).await.ok();
        return Err(error);
    }
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
            smol::fs::write(local_path, b"abc").await?;
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
            assert!(!directory.path().join("index.php.zenith-part").exists());

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
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(visible, ["index.php", "link"], "symlinks stay listed");
        let with_hidden: Vec<_> = retain_listable(entries, true)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(with_hidden, [".env", "index.php", "link"]);
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
