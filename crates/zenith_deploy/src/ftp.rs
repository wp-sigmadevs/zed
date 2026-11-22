use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::AsyncWriteExt as _;
use suppaftp::{
    list::{File as ListedFile, ListParser},
    smol::{
        AsyncFtpStream, AsyncRustlsConnector, AsyncRustlsFtpStream, ImplAsyncFtpStream,
        SmolTlsStream,
    },
    types::FileType,
};

use crate::{
    config::{ServerConfig, ServerType},
    remote_path::validate_remote_path,
    transport::{EntryKind, RemoteEntry, Transport, login_credentials, retain_listable},
};

struct FtpSession<T: SmolTlsStream + Send> {
    stream: ImplAsyncFtpStream<T>,
    use_list: bool,
    hidden_files: bool,
}

/// FTPS (explicit TLS) or plain FTP. FTPS verifies the certificate with the platform verifier
/// via `http_client_tls::tls_config()`; there is no switch to turn verification off.
pub async fn connect(server: &ServerConfig) -> Result<Box<dyn Transport>> {
    if server.host.trim().is_empty() {
        bail!("{} has no host", server.name);
    }
    let (user, password) = login_credentials(server)?;
    let address = (server.host.as_str(), server.port);
    match server.server_type {
        ServerType::Ftp => {
            log::warn!(
                "deploy: {} uses plain FTP; the password is sent in clear text",
                server.name
            );
            let stream = AsyncFtpStream::connect(address).await?;
            Ok(Box::new(open(stream, server, &user, &password).await?))
        }
        ServerType::Ftps => {
            let connector = AsyncRustlsConnector::from(futures_rustls::TlsConnector::from(
                Arc::new(http_client_tls::tls_config()),
            ));
            let stream = AsyncRustlsFtpStream::connect(address)
                .await?
                .into_secure(connector, &server.host)
                .await
                .context("TLS handshake failed")?;
            Ok(Box::new(open(stream, server, &user, &password).await?))
        }
        ServerType::Sftp => bail!("{} is an SFTP server", server.name),
    }
}

async fn open<T: SmolTlsStream + Send + 'static>(
    mut stream: ImplAsyncFtpStream<T>,
    server: &ServerConfig,
    user: &str,
    password: &str,
) -> Result<FtpSession<T>> {
    if !server.advanced.passive {
        stream = stream.active_mode(Duration::from_secs(30));
    }
    stream.login(user, password).await?;
    // The default transfer type is ASCII, which rewrites line endings of downloads.
    stream.transfer_type(FileType::Binary).await?;
    Ok(FtpSession {
        stream,
        use_list: server.advanced.use_list,
        hidden_files: server.advanced.hidden_files,
    })
}

fn to_entry(file: &ListedFile) -> RemoteEntry {
    let kind = if file.is_symlink() {
        EntryKind::Symlink
    } else if file.is_directory() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    RemoteEntry {
        name: file.name().to_string(),
        kind,
        size: file.size() as u64,
        modified: Some(file.modified()),
    }
}

/// MLSD servers report links as `type=OS.unix=slink:/target` or a `UNIX.slink=` fact, which
/// suppaftp does not recognise (it only knows `type=link`); catch them so a link is never
/// mistaken for a file.
fn mlsd_symlink(line: &str) -> Option<RemoteEntry> {
    let (facts, name) = line.split_once(' ')?;
    let facts = facts.to_lowercase();
    (facts.contains("slink") || facts.contains("symlink")).then(|| RemoteEntry {
        name: name.to_string(),
        kind: EntryKind::Symlink,
        size: 0,
        modified: None,
    })
}

pub(crate) fn parse_listing(lines: &[String], use_list: bool) -> Vec<RemoteEntry> {
    lines
        .iter()
        .filter_map(|line| {
            if !use_list {
                if let Some(link) = mlsd_symlink(line) {
                    return Some(link);
                }
            }
            let parsed = if use_list {
                ListParser::parse_posix(line).or_else(|_| ListParser::parse_dos(line))
            } else {
                ListParser::parse_mlsd(line)
            };
            match parsed {
                Ok(file) => Some(to_entry(&file)),
                Err(error) => {
                    log::debug!("deploy: skipping unparsable listing line {line:?}: {error}");
                    None
                }
            }
        })
        .collect()
}

#[async_trait]
impl<T: SmolTlsStream + Send + 'static> Transport for FtpSession<T> {
    async fn pwd(&mut self) -> Result<String> {
        Ok(self.stream.pwd().await?)
    }

    async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let path = validate_remote_path(path)?;
        let lines = if self.use_list {
            let argument = if self.hidden_files {
                format!("-a {path}")
            } else {
                path.to_string()
            };
            self.stream.list(Some(&argument)).await?
        } else {
            self.stream.mlsd(Some(path)).await?
        };
        Ok(retain_listable(
            parse_listing(&lines, self.use_list),
            self.hidden_files,
        ))
    }

    async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
        let remote_path = validate_remote_path(remote_path)?;
        // create_new: never opens (or follows) something that is already at the path.
        let mut file = smol::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(local_path)
            .await
            .with_context(|| format!("creating {}", local_path.display()))?;
        let mut transfer = self.stream.retr_as_stream(remote_path).await?;
        smol::io::copy(&mut transfer, &mut file).await?;
        transfer.finish().await?;
        file.flush().await?;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(self.stream.quit().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mlsd_listing_marks_symlinks_and_skips_dot_entries() {
        let lines: Vec<String> = [
            "type=cdir;modify=20250101120000; .",
            "type=pdir;modify=20250101120000; ..",
            "type=dir;modify=20250101120000; wp-content",
            "type=file;size=12;modify=20250101120000; index.php",
            "type=OS.unix=slink:/etc/passwd;modify=20250101120000; evil-link",
            "type=file;unix.slink=/etc/hosts;modify=20250101120000; hosts-link",
            "type=link;modify=20250101120000; plain-link",
        ]
        .map(String::from)
        .to_vec();
        let entries = retain_listable(parse_listing(&lines, false), true);
        let by_name = |name: &str| entries.iter().find(|entry| entry.name == name);
        for link in ["evil-link", "hosts-link", "plain-link"] {
            assert_eq!(
                by_name(link).map(|e| e.kind),
                Some(EntryKind::Symlink),
                "{link}"
            );
        }
        assert_eq!(
            by_name("wp-content").map(|e| e.kind),
            Some(EntryKind::Directory)
        );
        assert_eq!(by_name("index.php").map(|e| e.kind), Some(EntryKind::File));
        assert_eq!(by_name("index.php").map(|e| e.size), Some(12));
        assert!(by_name(".").is_none() && by_name("..").is_none());
    }

    #[test]
    fn list_output_symlink_is_never_a_file() {
        let lines = [
            "lrwxrwxrwx   1 u g   11 Nov 22 10:00 passwd -> /etc/passwd",
            "-rw-r--r--   1 u g   12 Nov 22 10:00 index.php",
            "drwxr-xr-x   2 u g 4096 Nov 22 10:00 wp-content",
        ]
        .map(String::from)
        .to_vec();
        let entries = parse_listing(&lines, true);
        let kinds: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("passwd", EntryKind::Symlink),
                ("index.php", EntryKind::File),
                ("wp-content", EntryKind::Directory)
            ]
        );
    }

    /// A scripted plain-FTP server on a thread; returns its port and the commands it received.
    fn fake_ftp_server() -> (u16, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let mut commands = Vec::new();
            let (control, _) = listener.accept().expect("accept");
            control
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut writer = control.try_clone().expect("clone");
            let mut reader = BufReader::new(control);
            let mut data: Option<TcpListener> = None;
            let mut reply = |text: &str| writer.write_all(text.as_bytes()).expect("reply");
            reply("220 fake\r\n");
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).expect("read") == 0 {
                    break;
                }
                let command = line.trim_end().to_string();
                commands.push(command.clone());
                let verb = command.split(' ').next().unwrap_or("").to_uppercase();
                match verb.as_str() {
                    "USER" => reply("331 password\r\n"),
                    "PASS" => reply("230 in\r\n"),
                    "TYPE" => reply("200 ok\r\n"),
                    "PWD" => reply("257 \"/www\" is the directory\r\n"),
                    "PASV" => {
                        let listener = TcpListener::bind("127.0.0.1:0").expect("bind data");
                        let data_port = listener.local_addr().expect("addr").port();
                        reply(&format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                            data_port / 256,
                            data_port % 256
                        ));
                        data = Some(listener);
                    }
                    "MLSD" | "RETR" => {
                        reply("150 opening\r\n");
                        let (mut stream, _) =
                            data.take().expect("PASV first").accept().expect("data");
                        let body = if verb == "MLSD" {
                            "type=file;size=12;modify=20250101120000; index.php\r\n\
                             type=OS.unix=slink:/etc/passwd;modify=20250101120000; evil\r\n\
                             type=dir;modify=20250101120000; sub\r\n"
                        } else {
                            "file-contents"
                        };
                        stream.write_all(body.as_bytes()).expect("data write");
                        drop(stream);
                        reply("226 done\r\n");
                    }
                    "QUIT" => {
                        reply("221 bye\r\n");
                        break;
                    }
                    _ => reply("502 not implemented\r\n"),
                }
            }
            commands
        });
        (port, handle)
    }

    #[test]
    fn plain_ftp_lists_downloads_and_never_sends_a_hostile_path() {
        let (port, server_thread) = fake_ftp_server();
        let mut server = ServerConfig::new();
        server.server_type = ServerType::Ftp;
        server.host = "127.0.0.1".into();
        server.port = port;
        server.username = "deploy".into();
        server.password = Some(crate::config::Password("pw".into()));
        let directory = tempfile::tempdir().expect("tempdir");
        smol::block_on(async {
            let mut transport = connect(&server).await.expect("connect");
            assert_eq!(transport.pwd().await.expect("pwd"), "/www");
            let entries = transport.list_dir("/www").await.expect("list");
            let kinds: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
            assert_eq!(
                kinds,
                [
                    ("index.php", EntryKind::File),
                    ("evil", EntryKind::Symlink),
                    ("sub", EntryKind::Directory)
                ]
            );
            let file = entries.first().expect("index.php").clone();
            let path = crate::transport::download_entry(
                transport.as_mut(),
                "/www",
                &file,
                directory.path(),
            )
            .await
            .expect("download");
            assert_eq!(
                std::fs::read_to_string(path).expect("read"),
                "file-contents"
            );

            let symlink = entries.get(1).expect("evil").clone();
            assert!(
                crate::transport::download_entry(
                    transport.as_mut(),
                    "/www",
                    &symlink,
                    directory.path()
                )
                .await
                .is_err()
            );
            for hostile in ["/a\r\nQUIT", "/a\nQUIT", "/a\0b", "-la", "/a/../b"] {
                assert!(transport.list_dir(hostile).await.is_err(), "{hostile:?}");
                let target = directory.path().join("x");
                assert!(
                    transport.download_file(hostile, &target).await.is_err(),
                    "{hostile:?}"
                );
            }
            transport.disconnect().await.expect("quit");
        });
        let commands = server_thread.join().expect("server thread");
        assert!(commands.contains(&"PASS pw".to_string()), "{commands:?}");
        assert!(
            commands.contains(&"TYPE I".to_string()),
            "binary: {commands:?}"
        );
        assert!(commands.contains(&"MLSD /www".to_string()), "{commands:?}");
        let retrievals: Vec<_> = commands.iter().filter(|c| c.starts_with("RETR")).collect();
        assert_eq!(retrievals.len(), 1, "only index.php: {commands:?}");
        assert_eq!(
            commands.iter().filter(|c| *c == "QUIT").count(),
            1,
            "{commands:?}"
        );
        assert!(!commands.iter().any(|c| c.contains("..")), "{commands:?}");
    }
}
