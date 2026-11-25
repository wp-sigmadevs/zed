use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use suppaftp::{
    Status,
    list::{File as ListedFile, ListParser},
    smol::{
        AsyncFtpStream, AsyncRustlsConnector, AsyncRustlsFtpStream, ImplAsyncFtpStream,
        SmolTlsStream, TransferStream,
    },
    types::FileType,
};
use util::ResultExt as _;

use crate::{
    config::{ServerConfig, ServerType},
    remote_path::validate_remote_path,
    transport::{
        EntryKind, MAX_DOWNLOAD_BYTES, MAX_LISTING_BYTES, MAX_LISTING_ENTRIES, RemoteEntry,
        RemoteStat, ServerRefused, Transport, login_credentials, retain_listable,
    },
};

/// suppaftp logs every control command at trace level under this target, `PASS <password>`
/// included (its `control.rs`).
const SUPPAFTP_CONTROL_TARGET: &str = "suppaftp::async_ftp::smol_ftp::control";

struct FtpSession<T: SmolTlsStream + Send> {
    stream: ImplAsyncFtpStream<T>,
    use_list: bool,
    hidden_files: bool,
}

/// FTPS (explicit TLS) or plain FTP. FTPS verifies the certificate with the platform verifier
/// via `http_client_tls::tls_config()`; there is no switch to turn verification off. Call it
/// through [`crate::spawn_operation`].
pub async fn connect(server: &ServerConfig) -> Result<Box<dyn Transport>> {
    connect_with_logger(server, log::logger(), log::max_level()).await
}

/// Refuses to log in while trace logging could reach suppaftp: it would write the password.
fn refuse_if_password_would_be_logged(
    logger: &dyn log::Log,
    max_level: log::LevelFilter,
) -> Result<()> {
    let metadata = log::Metadata::builder()
        .level(log::Level::Trace)
        .target(SUPPAFTP_CONTROL_TARGET)
        .build();
    if log::STATIC_MAX_LEVEL >= log::LevelFilter::Trace
        && max_level >= log::LevelFilter::Trace
        && logger.enabled(&metadata)
    {
        bail!(
            "trace logging is on, and it would record the FTP password; turn trace logging off and connect again"
        );
    }
    Ok(())
}

async fn connect_with_logger(
    server: &ServerConfig,
    logger: &dyn log::Log,
    max_level: log::LevelFilter,
) -> Result<Box<dyn Transport>> {
    refuse_if_password_would_be_logged(logger, max_level)?;
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
            let connector = tls_connector();
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

fn tls_connector() -> AsyncRustlsConnector {
    AsyncRustlsConnector::from(futures_rustls::TlsConnector::from(Arc::new(
        http_client_tls::tls_config(),
    )))
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
    // The PASV reply names an address; a hostile or misconfigured server could point the data
    // connection anywhere (FTP bounce). Always connect back to the control connection's host.
    stream.set_passive_nat_workaround(true);
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

/// Parses at most [`MAX_LISTING_ENTRIES`] + 1 entries (one over the limit, so
/// [`retain_listable`] still refuses the directory) instead of the whole 8 MiB.
pub(crate) fn parse_listing(listing: &str, use_list: bool) -> Vec<RemoteEntry> {
    listing
        .lines()
        .filter(|line| !line.is_empty())
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
        .take(MAX_LISTING_ENTRIES + 1)
        .collect()
}

/// A 4xx/5xx reply the server chose (`550 Permission denied`) leaves the control connection
/// usable. 421 is the exception: the server is closing it, so the session must be reset.
fn refusal(error: suppaftp::FtpError) -> anyhow::Error {
    match error {
        suppaftp::FtpError::UnexpectedResponse(ref response) if keeps_session(response.status) => {
            anyhow::Error::new(ServerRefused(error.to_string()))
        }
        error => error.into(),
    }
}

fn keeps_session(status: Status) -> bool {
    let code = status.code();
    (400..600).contains(&code) && code != 421
}

/// Creates the partial download readable by its owner only.
pub(crate) async fn create_part_file(path: &Path) -> std::io::Result<smol::fs::File> {
    let mut options = smol::fs::OpenOptions::new();
    // create_new: never opens (or follows) something that is already at the path.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use smol::fs::unix::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).await
}

impl<T: SmolTlsStream + Send + 'static> FtpSession<T> {
    /// Copies at most `limit` bytes of `transfer` into `sink`; a longer transfer is aborted
    /// (a server can stream forever) and is an error.
    async fn receive(
        &mut self,
        mut transfer: TransferStream<T>,
        limit: u64,
        sink: &mut (impl AsyncWrite + Unpin),
    ) -> Result<()>
    where
        TransferStream<T>: AsyncRead + Unpin,
    {
        let copied = futures::io::copy((&mut transfer).take(limit + 1), sink).await?;
        if copied > limit {
            self.stream.abort(transfer).await.log_err();
            bail!("the server sent more than {} MiB", limit / 1024 / 1024);
        }
        transfer.finish().await?;
        Ok(())
    }
}

#[async_trait]
impl<T: SmolTlsStream + Send + 'static> Transport for FtpSession<T> {
    async fn pwd(&mut self) -> Result<String> {
        Ok(self.stream.pwd().await?)
    }

    async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let path = validate_remote_path(path)?;
        let command = match (self.use_list, self.hidden_files) {
            (true, true) => format!("LIST -a {path}"),
            (true, false) => format!("LIST {path}"),
            (false, _) => format!("MLSD {path}"),
        };
        let (_, transfer) = self
            .stream
            .custom_data_command(command, &[Status::AboutToSend, Status::AlreadyOpen])
            .await
            .map_err(refusal)?;
        let mut listing = Vec::new();
        self.receive(transfer, MAX_LISTING_BYTES, &mut listing)
            .await
            .context("listing the directory")?;
        let listing = String::from_utf8_lossy(&listing);
        retain_listable(parse_listing(&listing, self.use_list), self.hidden_files)
    }

    async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
        let remote_path = validate_remote_path(remote_path)?;
        let mut file = create_part_file(local_path)
            .await
            .with_context(|| format!("creating {}", local_path.display()))?;
        let transfer = self
            .stream
            .retr_as_stream(remote_path)
            .await
            .map_err(refusal)?;
        self.receive(transfer, MAX_DOWNLOAD_BYTES, &mut file)
            .await
            .context("downloading")?;
        file.flush().await?;
        Ok(())
    }

    async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>> {
        let path = validate_remote_path(path)?;
        let size = match self.stream.size(path).await {
            Ok(size) => size as u64,
            // SIZE answers 550 for a missing file; the session stays usable.
            Err(suppaftp::FtpError::UnexpectedResponse(response))
                if response.status == Status::FileUnavailable =>
            {
                return Ok(None);
            }
            Err(error) => return Err(refusal(error)),
        };
        let stamp = match self.stream.mdtm(path).await {
            Ok(modified) => Some(modified.to_string()),
            // MDTM is optional: a refusal or an unreadable reply only costs the change stamp.
            Err(suppaftp::FtpError::UnexpectedResponse(response))
                if keeps_session(response.status) =>
            {
                None
            }
            Err(suppaftp::FtpError::BadResponse) => None,
            // 421 (the server is closing the connection) and I/O errors are real failures.
            Err(error) => return Err(refusal(error)),
        };
        Ok(Some(RemoteStat { size, stamp }))
    }

    async fn upload_file(&mut self, remote_path: &str, bytes: &[u8]) -> Result<()> {
        let remote_path = validate_remote_path(remote_path)?;
        let mut transfer = self
            .stream
            .put_with_stream(remote_path)
            .await
            .map_err(refusal)?;
        transfer.write_all(bytes).await.context("uploading")?;
        transfer.finish().await.map_err(refusal)?;
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
    fn only_a_chosen_4xx_5xx_reply_keeps_the_session() {
        assert!(keeps_session(Status::FileUnavailable));
        assert!(keeps_session(Status::BadFilename));
        assert!(keeps_session(Status::RequestFileActionIgnored));
        assert!(!keeps_session(Status::NotAvailable));
        assert!(!keeps_session(Status::CommandOk));
        assert!(!keeps_session(Status::AlreadyOpen));
    }

    #[cfg(unix)]
    #[test]
    fn the_ftp_part_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("f");
        smol::block_on(create_part_file(&path)).expect("create");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            smol::block_on(create_part_file(&path)).is_err(),
            "create_new"
        );
    }

    #[test]
    fn mlsd_listing_marks_symlinks_and_skips_dot_entries() {
        let lines = [
            "type=cdir;modify=20250101120000; .",
            "type=pdir;modify=20250101120000; ..",
            "type=dir;modify=20250101120000; wp-content",
            "type=file;size=12;modify=20250101120000; index.php",
            "type=OS.unix=slink:/etc/passwd;modify=20250101120000; evil-link",
            "type=file;unix.slink=/etc/hosts;modify=20250101120000; hosts-link",
            "type=link;modify=20250101120000; plain-link",
        ]
        .join("\n");
        let entries = retain_listable(parse_listing(&lines, false), true).expect("listable");
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
        .join("\n");
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

    #[test]
    fn parsing_stops_one_entry_past_the_limit() {
        let line = "-rw-r--r--   1 u g   12 Nov 22 10:00 a";
        let listing = vec![line; MAX_LISTING_ENTRIES + 5000].join("\n");
        let parsed = parse_listing(&listing, true);
        assert_eq!(parsed.len(), MAX_LISTING_ENTRIES + 1);
        assert!(retain_listable(parsed, false).is_err());
    }

    const LISTING: &str = "type=file;size=12;modify=20250101120000; index.php\r\n\
                           type=OS.unix=slink:/etc/passwd;modify=20250101120000; evil\r\n\
                           type=dir;modify=20250101120000; sub\r\n";

    struct FakeFtp {
        listing: Vec<u8>,
        file: Vec<u8>,
        /// The SIZE answer; `None` is a 550.
        size: Option<usize>,
        /// The MDTM answer; `None` is a refusal.
        mdtm: Option<&'static str>,
        /// MDTM answers 421 (the server is closing the control connection).
        mdtm_closing: bool,
    }

    impl Default for FakeFtp {
        fn default() -> Self {
            Self {
                listing: LISTING.as_bytes().to_vec(),
                file: b"file-contents".to_vec(),
                size: None,
                mdtm: None,
                mdtm_closing: false,
            }
        }
    }

    fn fake_ftp_server() -> (u16, std::thread::JoinHandle<Vec<String>>) {
        fake_ftp_server_with(FakeFtp::default())
    }

    /// A scripted plain-FTP server on a thread; returns its port and the commands it received.
    /// Its PASV reply names an unroutable address (0.0.0.1): only a client that connects back to
    /// the control host reaches the data listener.
    fn fake_ftp_server_with(script: FakeFtp) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
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
                            "227 Entering Passive Mode (0,0,0,1,{},{})\r\n",
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
                            &script.listing
                        } else {
                            &script.file
                        };
                        // The client hangs up mid-transfer when the body is over its limit.
                        if stream.write_all(body).is_ok() {
                            drop(stream);
                            reply("226 done\r\n");
                        } else {
                            reply("426 aborted\r\n");
                        }
                    }
                    "SIZE" => match script.size {
                        Some(size) => reply(&format!("213 {size}\r\n")),
                        None => reply("550 not found\r\n"),
                    },
                    "MDTM" if script.mdtm_closing => reply("421 closing\r\n"),
                    "MDTM" => match script.mdtm {
                        Some(stamp) => reply(&format!("213 {stamp}\r\n")),
                        None => reply("502 not implemented\r\n"),
                    },
                    "STOR" => {
                        reply("150 opening\r\n");
                        let (mut stream, _) =
                            data.take().expect("PASV first").accept().expect("data");
                        let mut received = Vec::new();
                        stream.read_to_end(&mut received).expect("receive upload");
                        commands.push(format!("DATA {received:?}"));
                        reply("226 done\r\n");
                    }
                    "ABOR" => reply("226 abort ok\r\n"),
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

    fn plain_server(port: u16) -> ServerConfig {
        let mut server = ServerConfig::new();
        server.server_type = ServerType::Ftp;
        server.host = "127.0.0.1".into();
        server.port = port;
        server.username = "deploy".into();
        server.password = Some(crate::config::Password("pw".into()));
        server
    }

    struct TraceLogger(log::Level);

    impl log::Log for TraceLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= self.0
        }
        fn log(&self, _record: &log::Record) {}
        fn flush(&self) {}
    }

    #[test]
    fn refuses_to_log_in_while_trace_logging_would_record_the_password() {
        // Port 9 on loopback is never contacted: the refusal comes before connecting.
        let server = plain_server(9);
        let result = smol::block_on(connect_with_logger(
            &server,
            &TraceLogger(log::Level::Trace),
            log::LevelFilter::Trace,
        ));
        let error = result.err().expect("must refuse").to_string();
        assert!(error.contains("trace logging is on"), "{error}");

        assert!(
            refuse_if_password_would_be_logged(
                &TraceLogger(log::Level::Debug),
                log::LevelFilter::Trace
            )
            .is_ok()
        );
        assert!(
            refuse_if_password_would_be_logged(
                &TraceLogger(log::Level::Trace),
                log::LevelFilter::Debug
            )
            .is_ok()
        );
    }

    #[test]
    fn an_oversized_listing_is_aborted_and_the_session_still_works() {
        let mut listing = Vec::new();
        // Far past the cap, so the server is still sending when the client gives up.
        while (listing.len() as u64) <= MAX_LISTING_BYTES + 16 * 1024 * 1024 {
            listing.extend_from_slice(b"type=file;size=1;modify=20250101120000; a\r\n");
        }
        let (port, server_thread) = fake_ftp_server_with(FakeFtp {
            listing,
            ..FakeFtp::default()
        });
        smol::block_on(async {
            let mut transport = connect(&plain_server(port)).await.expect("connect");
            let error = transport.list_dir("/www").await.expect_err("too big");
            assert!(format!("{error:#}").contains("more than"), "{error:#}");
            assert_eq!(transport.pwd().await.expect("pwd after abort"), "/www");
            transport.disconnect().await.expect("quit");
        });
        let commands = server_thread.join().expect("server thread");
        assert!(commands.contains(&"ABOR".to_string()), "{commands:?}");
    }

    #[test]
    fn an_oversized_download_is_aborted_and_leaves_no_file() {
        let (port, server_thread) = fake_ftp_server_with(FakeFtp {
            file: vec![b'x'; MAX_DOWNLOAD_BYTES as usize + 16 * 1024 * 1024],
            ..FakeFtp::default()
        });
        let directory = tempfile::tempdir().expect("tempdir");
        smol::block_on(async {
            let mut transport = connect(&plain_server(port)).await.expect("connect");
            let entries = transport.list_dir("/www").await.expect("list");
            // The listing says 12 bytes; the server then sends far more.
            let file = entries.first().expect("index.php").clone();
            let error = crate::transport::download_entry(
                transport.as_mut(),
                "/www",
                &file,
                directory.path(),
            )
            .await
            .expect_err("too big");
            assert!(format!("{error:#}").contains("more than"), "{error:#}");
            transport.disconnect().await.expect("quit");
        });
        server_thread.join().expect("server thread");
        let left: Vec<_> = std::fs::read_dir(directory.path())
            .expect("read_dir")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn ftp_upload_sends_binary_mode_then_stor_with_the_exact_bytes() {
        let (port, server_thread) = fake_ftp_server();
        let bytes = b"<?php\r\n\0\xff\n\r".to_vec();
        smol::block_on(async {
            let mut transport = connect(&plain_server(port)).await.expect("connect");
            crate::transport::upload_entry(transport.as_mut(), "/www/x.php", &bytes)
                .await
                .expect("upload");
            for hostile in ["/a\r\nQUIT", "/a\0b", "-la", "/a/../b"] {
                assert!(
                    transport.upload_file(hostile, b"x").await.is_err(),
                    "{hostile:?}"
                );
            }
            assert_eq!(transport.pwd().await.expect("session still works"), "/www");
            transport.disconnect().await.expect("quit");
        });
        let commands = server_thread.join().expect("server thread");
        let at = |wanted: &str| commands.iter().position(|c| c == wanted);
        let binary = at("TYPE I").expect("TYPE I");
        let store = at("STOR /www/x.php").expect("STOR");
        assert!(binary < store, "{commands:?}");
        assert!(
            at(&format!("DATA {bytes:?}")).is_some_and(|data| data > store),
            "exact bytes after STOR: {commands:?}"
        );
        assert_eq!(
            commands.iter().filter(|c| c.starts_with("STOR")).count(),
            1,
            "hostile paths must not reach the wire: {commands:?}"
        );
    }

    #[test]
    fn ftp_stat_reports_size_and_stamp_and_degrades_without_mdtm() {
        let stat = |script: FakeFtp| {
            let (port, server_thread) = fake_ftp_server_with(script);
            let result = smol::block_on(async {
                let mut transport = connect(&plain_server(port)).await.expect("connect");
                let result = transport.stat("/www/x.php").await.expect("stat");
                assert!(transport.stat("/a/../b").await.is_err());
                assert_eq!(transport.pwd().await.expect("session still works"), "/www");
                transport.disconnect().await.expect("quit");
                result
            });
            server_thread.join().expect("server thread");
            result
        };
        let both = stat(FakeFtp {
            size: Some(42),
            mdtm: Some("20250101120000"),
            ..FakeFtp::default()
        })
        .expect("exists");
        assert_eq!(both.size, 42);
        assert!(
            both.stamp.is_some_and(|stamp| stamp.contains("2025-01-01")),
            "stamp from MDTM"
        );
        let no_mdtm = stat(FakeFtp {
            size: Some(42),
            ..FakeFtp::default()
        });
        assert_eq!(
            no_mdtm,
            Some(RemoteStat {
                size: 42,
                stamp: None
            })
        );
        assert_eq!(stat(FakeFtp::default()), None, "SIZE 550 is a missing file");
    }

    #[test]
    fn ftp_stat_is_an_error_when_the_server_closes_during_mdtm() {
        let (port, server_thread) = fake_ftp_server_with(FakeFtp {
            size: Some(42),
            mdtm_closing: true,
            ..FakeFtp::default()
        });
        smol::block_on(async {
            let mut transport = connect(&plain_server(port)).await.expect("connect");
            let error = transport
                .stat("/www/x.php")
                .await
                .expect_err("421 is not a missing MDTM");
            assert!(
                error.downcast_ref::<ServerRefused>().is_none(),
                "421 must reset the session: {error:#}"
            );
        });
        server_thread.join().expect("server thread");
    }
}
