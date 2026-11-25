use std::{ffi::OsString, path::Path, pin::pin, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, future::Either};
use util::command::{Stdio, new_command};

use crate::{
    config::ServerConfig,
    ftp::create_part_file,
    remote_path::{validate_name, validate_remote_path},
    transport::{
        EntryKind, MAX_DOWNLOAD_BYTES, MAX_LISTING_BYTES, MAX_LISTING_ENTRIES, RemoteEntry,
        RemoteStat, ServerRefused, Transport, retain_listable,
    },
};

/// OpenSSH sftp reads batch lines into a fixed buffer; a longer line could be split and its
/// tail run as a second command, so such lines are never sent.
const MAX_BATCH_LINE: usize = 1024;

/// SFTP through the system `sftp` binary, one process per operation (key or agent auth only;
/// password auth is M2). Commands go in on stdin; `-b -` makes sftp read a batch from it.
/// ponytail: a process per operation costs a handshake each time; add ControlMaster
/// multiplexing or a long-lived session when listing latency matters.
pub struct SftpTransport {
    program: OsString,
    args: Vec<String>,
    user_at_host: String,
    hidden_files: bool,
}

impl SftpTransport {
    /// Call the [`Transport`] methods through [`crate::spawn_operation`].
    pub fn new(server: &ServerConfig) -> Result<Self> {
        Self::with_program("sftp", server)
    }

    fn with_program(program: impl Into<OsString>, server: &ServerConfig) -> Result<Self> {
        let args = build_args(server)?;
        Ok(Self {
            program: program.into(),
            args,
            user_at_host: format!("{}@{}", server.username, server.host),
            hidden_files: server.advanced.hidden_files,
        })
    }

    async fn run(&self, batch: &str) -> Result<String> {
        // A private, empty working directory: a relative local path or `lcd` in a batch can
        // never reach the project or another user's files in the shared temp directory.
        let working_directory = tempfile::tempdir().context("creating a working directory")?;
        self.run_in(working_directory.path(), batch).await
    }

    /// [`Self::run`] in a directory the caller owns (an upload puts its file there).
    async fn run_in(&self, working_directory: &Path, batch: &str) -> Result<String> {
        let mut command = new_command(&self.program);
        command
            .args(&self.args)
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().context("starting sftp")?;
        if let Some(mut stdin) = child.stdin.take() {
            // A child that already exited (host-key failure) closes the pipe; its stderr says why.
            match stdin.write_all(batch.as_bytes()).await {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                result => result?,
            }
        }
        let stdout = child.stdout.take().context("sftp has no stdout")?;
        let stderr = child.stderr.take().context("sftp has no stderr")?;
        // try_join returns on the first error, so a flood on one stream stops the read at once;
        // dropping `child` kills it.
        let (stdout, stderr) =
            futures::future::try_join(read_capped(stdout), read_capped(stderr)).await?;
        let status = child.status().await?;
        let stderr = String::from_utf8_lossy(&stderr);
        if !status.success() {
            return Err(self.failure(status.code(), &stderr));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    fn failure(&self, exit_code: Option<i32>, stderr: &str) -> anyhow::Error {
        if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
            return anyhow!(
                "the host key of {} changed; check ~/.ssh/known_hosts before connecting",
                self.user_at_host
            );
        }
        if stderr.contains("Host key verification failed") {
            return anyhow!(
                "unknown host key: run `ssh {}` once to verify the key",
                self.user_at_host
            );
        }
        let detail: String = stderr.trim().chars().take(500).collect();
        if is_server_refusal(exit_code, &detail) {
            // Each operation is its own process, so a refusal never leaves a session half-used.
            anyhow::Error::new(ServerRefused(format!("sftp failed: {detail}")))
        } else {
            // ssh itself failed (refused, unreachable, auth): not "the server answered".
            anyhow!("sftp could not connect: {detail}")
        }
    }
}

/// `sftp` exits 1 when a command it sent failed (the server answered and said no) and 255 when
/// the ssh connection itself failed; authentication failures also print `Permission denied (`.
fn is_server_refusal(exit_code: Option<i32>, stderr: &str) -> bool {
    exit_code == Some(1) && !stderr.contains("Permission denied (")
}

async fn read_capped(stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    stream
        .take(MAX_LISTING_BYTES + 1)
        .read_to_end(&mut data)
        .await?;
    if data.len() as u64 > MAX_LISTING_BYTES {
        bail!(
            "sftp printed more than {} MiB",
            MAX_LISTING_BYTES / 1024 / 1024
        );
    }
    Ok(data)
}

fn too_big(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_DOWNLOAD_BYTES)
}

fn is_plain(value: &str, extra: &[char]) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' || extra.contains(&c)
        })
}

/// Everything fixed on the command line; the password is deliberately not an input.
/// Host keys are checked against `known_hosts` and never auto-accepted.
pub(crate) fn build_args(server: &ServerConfig) -> Result<Vec<String>> {
    if !is_plain(&server.host, &[]) {
        bail!("invalid SFTP host {:?}", server.host);
    }
    if !is_plain(&server.username, &[]) {
        bail!("invalid SFTP user name {:?}", server.username);
    }
    Ok([
        "-b",
        "-",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=20",
        "-P",
        &server.port.to_string(),
        "--",
        &format!("{}@{}", server.username, server.host),
    ]
    .map(String::from)
    .to_vec())
}

/// One sftp argument in double quotes. sftp executes a batch line starting with `!` as a local
/// shell command and treats a newline as a new line, so control characters are refused outright;
/// `\` and (for remote paths) the glob characters `*?[{}` are refused too because sftp's own
/// escaping of them does not round-trip (and a `{` brace makes a listing match nothing, which
/// would read as "the file does not exist").
fn quote(argument: &str, remote: bool) -> Result<String> {
    if argument.is_empty()
        || argument.chars().any(char::is_control)
        || argument.contains('\\')
        || (remote && argument.contains(['*', '?', '[', '{', '}']))
    {
        bail!("{argument:?} cannot be sent to sftp safely");
    }
    Ok(format!("\"{}\"", argument.replace('"', "\\\"")))
}

fn batch_line(line: String) -> Result<String> {
    if line.len() >= MAX_BATCH_LINE {
        bail!("the sftp command is longer than {MAX_BATCH_LINE} bytes");
    }
    Ok(format!("{line}\n"))
}

fn list_batch(path: &str) -> Result<String> {
    batch_line(format!(
        "ls -lan {}",
        quote(validate_remote_path(path)?, true)?
    ))
}

fn local_argument(local_path: &Path) -> Result<String> {
    let local = local_path.to_str().context("local path is not UTF-8")?;
    if !local_path.is_absolute() {
        bail!("local path {local:?} is not absolute");
    }
    quote(local, false)
}

fn get_batch(remote_path: &str, local_path: &Path) -> Result<String> {
    batch_line(format!(
        "get {} {}",
        quote(validate_remote_path(remote_path)?, true)?,
        local_argument(local_path)?
    ))
}

/// No `-p`: the local file's times are not copied. `put` still sends the local mode bits, which
/// only matter for a remote file that does not exist yet (see `upload_file`).
fn put_batch(local_path: &Path, remote_path: &str) -> Result<String> {
    batch_line(format!(
        "put {} {}",
        local_argument(local_path)?,
        quote(validate_remote_path(remote_path)?, true)?
    ))
}

/// `/a/b/c.php` -> (`/a/b`, `c.php`); the parent of `/c.php` is `/`.
fn split_remote(path: &str) -> Result<(&str, &str)> {
    let (parent, name) = validate_remote_path(path)?
        .rsplit_once('/')
        .context("remote path has no file name")?;
    Ok((
        if parent.is_empty() { "/" } else { parent },
        validate_name(name)?,
    ))
}

/// The three date columns of an `ls -la` line (`Nov 22 23:48`): the only change marker the long
/// format carries, compared for equality and never parsed.
fn ls_stamp(line: &str) -> String {
    line.split_whitespace()
        .skip(5)
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Picks `name` out of a listing of `parent` and returns its size and stamp. Fails when the
/// answer could hide a symlink: the name appears twice (a file name holding a newline can forge
/// a line), or it is absent while a line shows `name -> target` (a link whose own name contains
/// ` -> ` parses under a shorter name). Lines without the folder prefix are matched by bare
/// name, since servers older than OpenSSH 9.1 can print a longname without it.
fn stat_from_listing(output: &str, parent: &str, name: &str) -> Result<Option<RemoteStat>> {
    let prefix = format!("{}/", parent.trim_end_matches('/'));
    let matches: Vec<(RemoteEntry, &str)> = output
        .lines()
        .filter_map(|line| {
            let entry = parse_ls_line(line, &prefix).or_else(|| parse_ls_line(line, ""))?;
            (entry.name == name).then_some((entry, line))
        })
        .collect();
    let (entry, line) = match matches.as_slice() {
        [] => {
            let arrows = [format!("/{name} -> "), format!(" {name} -> ")];
            if output
                .lines()
                .any(|line| arrows.iter().any(|arrow| line.contains(arrow)))
            {
                bail!("{name:?} is ambiguous in the listing; it may be a symlink");
            }
            return Ok(None);
        }
        [only] => only,
        _ => bail!("{name:?} appears more than once in the listing"),
    };
    match entry.kind {
        EntryKind::File => Ok(Some(RemoteStat {
            size: entry.size,
            stamp: Some(ls_stamp(line)),
        })),
        EntryKind::Symlink => bail!("{name:?} is a symlink; symlinks are not edited"),
        EntryKind::Directory => bail!("{name:?} is a directory"),
    }
}

/// Parses `ls -la` output, stopping one entry past [`MAX_LISTING_ENTRIES`] (the caller then refuses it). sftp prints the full path in the name column, so `directory` is
/// stripped from it; lines that do not parse (including the echoed `sftp> ` commands and any
/// fragment of a name that contained a newline) are dropped.
pub(crate) fn parse_ls(output: &str, directory: &str) -> Vec<RemoteEntry> {
    let prefix = format!("{}/", directory.trim_end_matches('/'));
    output
        .lines()
        .filter_map(|line| parse_ls_line(line, &prefix))
        .take(MAX_LISTING_ENTRIES + 1)
        .collect()
}

fn parse_ls_line(line: &str, prefix: &str) -> Option<RemoteEntry> {
    let mut rest = line;
    let mut fields = Vec::with_capacity(8);
    for _ in 0..8 {
        let (field, remainder) = rest.trim_start().split_once(char::is_whitespace)?;
        fields.push(field);
        rest = remainder;
    }
    let kind = match fields.first()?.chars().next()? {
        '-' => EntryKind::File,
        'd' => EntryKind::Directory,
        'l' => EntryKind::Symlink,
        _ => return None,
    };
    let size = fields.get(4)?.parse().ok()?;
    let name = rest.strip_prefix(prefix)?;
    let name = match (kind, name.split_once(" -> ")) {
        (EntryKind::Symlink, Some((link, _target))) => link,
        _ => name,
    };
    Some(RemoteEntry {
        name: name.to_string(),
        kind,
        size,
        // ponytail: the long format has no year or seconds; parse it (or use SFTP attrs) in M2.
        modified: None,
    })
}

#[async_trait]
impl Transport for SftpTransport {
    async fn pwd(&mut self) -> Result<String> {
        let output = self.run("pwd\n").await?;
        output
            .lines()
            .find_map(|line| line.strip_prefix("Remote working directory: "))
            .map(str::to_string)
            .context("sftp did not report a working directory")
    }

    async fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>> {
        let output = self.run(&list_batch(path)?).await?;
        retain_listable(parse_ls(&output, path), self.hidden_files)
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "runs under spawn_operation on a background thread with a real clock; this crate has no GPUI executor in the transport layer"
    )]
    async fn download_file(&mut self, remote_path: &str, local_path: &Path) -> Result<()> {
        let batch = get_batch(remote_path, local_path)?;
        // sftp writes the file itself, so its size is watched; losing the race drops the run,
        // which kills the child.
        let watch = async {
            loop {
                smol::Timer::after(Duration::from_millis(100)).await;
                if too_big(local_path) {
                    break;
                }
            }
        };
        match futures::future::select(pin!(self.run(&batch)), pin!(watch)).await {
            Either::Left((result, _)) => {
                result?;
            }
            Either::Right(_) => {}
        }
        if too_big(local_path) {
            bail!(
                "the server sent more than {} MiB",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            );
        }
        Ok(())
    }

    /// Lists the parent folder and picks the entry out: `ls` of a single path follows a symlink
    /// and would show its target as a regular file, a folder listing shows the link itself.
    async fn stat(&mut self, path: &str) -> Result<Option<RemoteStat>> {
        let (parent, name) = split_remote(path)?;
        let output = match self.run(&list_batch(parent)?).await {
            Ok(output) => output,
            Err(error)
                if error
                    .downcast_ref::<ServerRefused>()
                    .is_some_and(|refused| refused.0.trim_end().ends_with("\" not found")) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        stat_from_listing(&output, parent, name)
    }

    /// The bytes go through a file in a private (0700) directory that is removed afterwards,
    /// then one `put`. The remote file is written in place. The file is 0644 because `put`
    /// sends its mode bits: a new remote file is created with them, and a 0600 file would be
    /// unreadable to the web server; an existing remote file keeps its own mode.
    async fn upload_file(&mut self, remote_path: &str, bytes: &[u8]) -> Result<()> {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder
            .tempdir()
            .context("creating a directory for the upload")?;
        let local_path = directory.path().join("upload");
        let batch = put_batch(&local_path, remote_path)?;
        let mut file = create_part_file(&local_path)
            .await
            .context("writing the upload file")?;
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            smol::fs::set_permissions(&local_path, std::fs::Permissions::from_mode(0o644))
                .await
                .context("setting the upload file's mode")?;
        }
        self.run_in(directory.path(), &batch).await?;
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Password, ServerType};
    use crate::transport::download_entry;

    fn server() -> ServerConfig {
        let mut server = ServerConfig::new();
        server.server_type = ServerType::Sftp;
        server.host = "example.com".into();
        server.port = 2222;
        server.username = "deploy".into();
        server.password = Some(Password("hunter2-secret".into()));
        server
    }

    #[test]
    fn argv_enforces_host_key_checking_and_carries_no_password() {
        let args = build_args(&server()).expect("args");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-o", "StrictHostKeyChecking=yes"]),
            "{args:?}"
        );
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|pair| pair == ["-P", "2222"]));
        assert_eq!(args.last().map(String::as_str), Some("deploy@example.com"));
        assert!(!args.iter().any(|arg| arg.contains("hunter2")), "{args:?}");
    }

    #[test]
    fn argv_rejects_option_injection_through_host_or_user() {
        for host in [
            "-oProxyCommand=touch x",
            "a b",
            "a@b",
            "",
            "h\nx",
            "a;b",
            "a:b",
        ] {
            let mut bad = server();
            bad.host = host.into();
            assert!(build_args(&bad).is_err(), "host {host:?}");
        }
        for user in ["-oProxyCommand=x", "a b", "a@b", "", "u\n"] {
            let mut bad = server();
            bad.username = user.into();
            assert!(build_args(&bad).is_err(), "user {user:?}");
        }
    }

    #[test]
    fn batch_never_emits_a_bang_line_or_a_second_line() {
        let hostile = [
            "/x\n!touch pwned",
            "/x\r\n!touch pwned",
            "\n!touch pwned",
            "!touch pwned",
            "/x/!touch pwned",
            "/x/\"; !touch pwned",
            "/a/*",
            "/a/b?",
            "/a/[x]",
            "/a/{b,c}",
            "/a{",
            "/a}",
            "/a\\b",
        ];
        for path in hostile {
            let batches = [list_batch(path), get_batch(path, Path::new("/tmp/out"))];
            for batch in batches.into_iter().flatten() {
                assert_eq!(batch.lines().count(), 1, "{path:?} -> {batch:?}");
                assert!(
                    batch.lines().all(|line| !line.starts_with('!')),
                    "{path:?} -> {batch:?}"
                );
                assert!(!path.contains('\n'), "{path:?} must have been rejected");
            }
        }
        assert!(list_batch("/x\n!touch pwned").is_err());
        assert!(get_batch("/x/a", Path::new("/tmp/o\n!touch pwned")).is_err());
        assert!(get_batch("/x/a", Path::new("relative")).is_err());
        let batch = list_batch("/x/!touch pwned").expect("a bang inside a quoted path is inert");
        assert_eq!(batch, "ls -lan \"/x/!touch pwned\"\n");
        assert_eq!(
            get_batch("/x/a \"b\".txt", Path::new("/tmp/o")).expect("get"),
            "get \"/x/a \\\"b\\\".txt\" \"/tmp/o\"\n"
        );
    }

    #[test]
    fn ls_parsing_stops_one_entry_past_the_limit() {
        let line = "-rw-r--r--    ? u   g           1 Nov 22 23:48 /www/x\n";
        let output = line.repeat(MAX_LISTING_ENTRIES + 5000);
        let parsed = parse_ls(&output, "/www");
        assert_eq!(parsed.len(), MAX_LISTING_ENTRIES + 1);
        assert!(retain_listable(parsed, false).is_err());
    }

    #[test]
    fn parses_ls_output_including_symlinks_spaces_and_echoed_commands() {
        let output = "\
sftp> ls -la \"/www\"
drwxr-xr-x    ? u   g         256 Nov 22 23:48 /www/.
drwxr-xr-x    ? u   g         160 Nov 22 23:48 /www/..
-rw-r--r--    ? u   g           2 Nov 22 23:48 /www/.env
-rw-r--r--    ? u   g          12 Nov 22 23:48 /www/my file.php
lrwxr-xr-x    ? u   g          11 Nov 22 23:48 /www/evil -> /etc/passwd
drwxr-xr-x    ? u   g          64 Nov 22 23:48 /www/sub
-rw-r--r--    ? u   g           1 Nov 22 23:48 /www/x
!touch pwned
";
        let entries = retain_listable(parse_ls(output, "/www"), true).expect("listable");
        let summary: Vec<_> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.size))
            .collect();
        assert_eq!(
            summary,
            [
                (".env", EntryKind::File, 2),
                ("my file.php", EntryKind::File, 12),
                ("evil", EntryKind::Symlink, 11),
                ("sub", EntryKind::Directory, 64),
                ("x", EntryKind::File, 1),
            ]
        );
    }

    #[cfg(unix)]
    fn write_script(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[cfg(unix)]
    #[test]
    fn unknown_host_key_asks_the_user_to_run_ssh_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        let program = write_script(
            directory.path(),
            "sftp",
            "cat >/dev/null; echo 'Host key verification failed.' >&2; exit 255",
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let error = smol::block_on(transport.pwd()).expect_err("must fail");
        let message = error.to_string();
        assert!(
            message.contains("run `ssh deploy@example.com` once to verify the key"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_host_key_failure_survives_a_child_that_exits_before_reading() {
        let directory = tempfile::tempdir().expect("tempdir");
        let program = write_script(
            directory.path(),
            "sftp",
            "echo 'Host key verification failed.' >&2; exit 255",
        );
        let transport = SftpTransport::with_program(program, &server()).expect("transport");
        // Larger than a pipe buffer, so the write is still pending when the child exits.
        let batch = "x".repeat(1 << 20);
        let message = smol::block_on(transport.run(&batch))
            .expect_err("must fail")
            .to_string();
        assert!(message.contains("run `ssh"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_operation_kills_the_sftp_child() {
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("pid");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!("echo $$ > '{}'; exec sleep 60", pid_file.display()),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let sleep =
            |millis: u64| smol::unblock(move || std::thread::sleep(Duration::from_millis(millis)));
        // A child that was killed but not yet reaped shows as `Z`; it no longer runs.
        let alive = |pid: String| async move {
            util::command::new_command("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .await
                .is_ok_and(|output| {
                    output.status.success()
                        && !String::from_utf8_lossy(&output.stdout)
                            .trim()
                            .starts_with('Z')
                })
        };
        smol::block_on(async {
            let mut operation = Box::pin(transport.pwd());
            // Poll the operation until the fake sftp has written its pid, then drop it.
            let started = Instant::now() + Duration::from_secs(10);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                {
                    break pid;
                }
                assert!(Instant::now() < started, "the fake sftp never started");
                futures::future::select(operation.as_mut(), Box::pin(sleep(50))).await;
            };
            drop(operation);
            let deadline = Instant::now() + Duration::from_secs(5);
            while alive(pid.clone()).await && Instant::now() < deadline {
                sleep(50).await;
            }
            assert!(
                !alive(pid.clone()).await,
                "sftp child {pid} survived its dropped operation"
            );
        });
    }

    /// The real `sftp` client talking to the local sftp-server (`-D`), no network: proves the
    /// quoting, the `ls` parser and the symlink guard against the actual binary.
    #[cfg(unix)]
    #[test]
    fn real_sftp_lists_downloads_and_never_follows_symlinks() {
        let Some(sftp_server) = [
            "/usr/libexec/sftp-server",
            "/usr/lib/openssh/sftp-server",
            "/usr/lib/ssh/sftp-server",
        ]
        .into_iter()
        .find(|path| Path::new(path).exists()) else {
            eprintln!("skipped: no sftp-server binary found");
            return;
        };
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().canonicalize().expect("canonicalize");
        let remote = root.join("remote");
        let local = root.join("local");
        std::fs::create_dir_all(&remote).expect("mkdir");
        std::fs::create_dir_all(&local).expect("mkdir");
        std::fs::write(remote.join("sp ace \"q\".txt"), "quoted").expect("write");
        std::fs::write(remote.join("secret.txt"), "secret").expect("write");
        std::os::unix::fs::symlink(remote.join("secret.txt"), remote.join("evil")).expect("link");
        std::fs::create_dir(remote.join("sub")).expect("mkdir");

        let program = write_script(&root, "sftp", &format!("exec sftp -b - -D {sftp_server}"));
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let remote_path = remote.to_str().expect("utf8");
        smol::block_on(async {
            let entries = transport.list_dir(remote_path).await.expect("list");
            let find = |name: &str| entries.iter().find(|entry| entry.name == name);
            assert_eq!(find("evil").map(|e| e.kind), Some(EntryKind::Symlink));
            assert_eq!(find("sub").map(|e| e.kind), Some(EntryKind::Directory));
            assert_eq!(find("secret.txt").map(|e| e.size), Some(6));

            let quoted = find("sp ace \"q\".txt")
                .expect("quoted name listed")
                .clone();
            let path = download_entry(&mut transport, remote_path, &quoted, &local)
                .await
                .expect("download");
            assert_eq!(std::fs::read_to_string(path).expect("read"), "quoted");

            let evil = find("evil").expect("symlink listed").clone();
            assert!(
                download_entry(&mut transport, remote_path, &evil, &local)
                    .await
                    .is_err()
            );
            assert!(!local.join("evil").exists(), "the link target was fetched");
        });
    }

    #[test]
    fn batch_lines_of_1024_bytes_or_more_are_never_sent() {
        let directory = format!("/{}", "d".repeat(MAX_BATCH_LINE));
        assert!(list_batch(&directory).is_err());
        assert!(get_batch(&format!("{directory}/f"), Path::new("/tmp/o")).is_err());
        let local = format!("/{}", "l".repeat(MAX_BATCH_LINE));
        assert!(get_batch("/www/f", Path::new(&local)).is_err());

        // "ls -lan \"" + path + "\"" is 10 bytes plus the path; the newline does not count.
        let longest = format!("/{}", "d".repeat(MAX_BATCH_LINE - 12));
        let batch = list_batch(&longest).expect("1023-byte line");
        assert_eq!(batch.len(), MAX_BATCH_LINE);
        assert!(list_batch(&format!("{longest}d")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sftp_runs_in_a_private_working_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let recorded = directory.path().join("cwd");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!(
                "pwd -P > '{}'; ls -A >> '{}'; cat >/dev/null; echo 'Remote working directory: /w'",
                recorded.display(),
                recorded.display()
            ),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        assert_eq!(smol::block_on(transport.pwd()).expect("pwd"), "/w");
        let output = std::fs::read_to_string(&recorded).expect("recorded");
        let mut lines = output.lines();
        let working_directory = Path::new(lines.next().expect("cwd")).to_path_buf();
        assert_eq!(lines.next(), None, "the working directory was not empty");
        let shared = std::env::temp_dir().canonicalize().expect("temp dir");
        assert_ne!(working_directory, shared, "sftp ran in the shared temp dir");
        assert!(
            !working_directory.exists(),
            "{} outlived the operation",
            working_directory.display()
        );
    }

    #[test]
    fn only_a_failed_remote_command_counts_as_a_server_refusal() {
        assert!(is_server_refusal(
            Some(1),
            "remote readdir(\"/x\"): Permission denied"
        ));
        assert!(!is_server_refusal(
            Some(255),
            "ssh: connect: Connection refused"
        ));
        assert!(!is_server_refusal(
            Some(1),
            "u@h: Permission denied (publickey)."
        ));
        assert!(!is_server_refusal(None, "killed by a signal"));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_sftp_output_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!(
                "cat >/dev/null; head -c {} /dev/zero",
                MAX_LISTING_BYTES + 1
            ),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let error = smol::block_on(transport.list_dir("/www")).expect_err("too big");
        assert!(error.to_string().contains("more than"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn an_oversized_sftp_download_is_stopped_while_it_runs() {
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("out");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!(
                "cat >/dev/null; head -c {} /dev/zero > '{}'; exec sleep 30",
                MAX_DOWNLOAD_BYTES + 1,
                target.display()
            ),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let started = Instant::now();
        let error =
            smol::block_on(transport.download_file("/www/f", &target)).expect_err("too big");
        assert!(error.to_string().contains("more than"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the transfer ran to the end: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn put_batch_never_emits_a_bang_line_a_second_line_or_a_glob() {
        let local = Path::new("/tmp/zenith-upload/upload");
        for hostile in [
            "/x\n!touch pwned",
            "/x\r\n!touch pwned",
            "/x\r!touch pwned",
            "/x\0y",
            "/a\\b",
            "/a/*",
            "/a/b?",
            "/a/[x]",
            "/a/{b,c}",
            "/a{",
            "/a}",
            "!touch pwned",
            "/www/../etc/passwd",
        ] {
            assert!(put_batch(local, hostile).is_err(), "{hostile:?}");
        }
        for hostile in ["/tmp/o\n!touch pwned", "/tmp/o\\x", "relative"] {
            assert!(
                put_batch(Path::new(hostile), "/www/a.php").is_err(),
                "{hostile:?}"
            );
        }
        let batch = put_batch(local, "/www/!touch \"a\".php").expect("a bang inside quotes");
        assert_eq!(
            batch,
            "put \"/tmp/zenith-upload/upload\" \"/www/!touch \\\"a\\\".php\"\n"
        );
        assert_eq!(batch.lines().count(), 1);
        assert!(!batch.starts_with('!') && !batch.contains(" -p"), "{batch}");
        let long = format!("/{}", "d".repeat(MAX_BATCH_LINE));
        assert!(put_batch(local, &long).is_err());
    }

    #[cfg(unix)]
    const STAT_LISTING: &str = "\
cat >/dev/null
cat <<'EOF'
sftp> ls -la \"/www\"
-rw-r--r--    ? u   g          12 Nov 22 23:48 /www/index.php
lrwxr-xr-x    ? u   g          11 Nov 22 23:48 /www/evil -> /etc/passwd
drwxr-xr-x    ? u   g          64 Nov 22 23:48 /www/sub
EOF";

    #[cfg(unix)]
    #[test]
    fn sftp_stat_reports_a_file_and_refuses_a_symlink_or_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let program = write_script(directory.path(), "sftp", STAT_LISTING);
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        smol::block_on(async {
            assert_eq!(
                transport.stat("/www/index.php").await.expect("stat"),
                Some(RemoteStat {
                    size: 12,
                    stamp: Some("Nov 22 23:48".into())
                })
            );
            let error = transport.stat("/www/evil").await.expect_err("symlink");
            assert!(error.to_string().contains("symlink"), "{error}");
            assert!(transport.stat("/www/sub").await.is_err(), "directory");
            assert_eq!(transport.stat("/www/missing").await.expect("stat"), None);
            assert!(transport.stat("/www/../etc/passwd").await.is_err());
            assert!(transport.stat("/www/a\nb").await.is_err());
        });
    }

    #[cfg(unix)]
    #[test]
    fn sftp_stat_of_a_missing_folder_is_none_but_a_denied_one_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let program = write_script(
            directory.path(),
            "sftp",
            "cat >/dev/null; echo 'Can'\\''t ls: \"/nope\" not found' >&2; exit 1",
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        assert_eq!(
            smol::block_on(transport.stat("/nope/x.php")).expect("stat"),
            None
        );
        let program = write_script(
            directory.path(),
            "denied",
            "cat >/dev/null; echo 'remote readdir(\"/www\"): Permission denied' >&2; exit 1",
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        assert!(smol::block_on(transport.stat("/www/x.php")).is_err());
        // "not found" somewhere in other output is not the folder-is-missing answer.
        let program = write_script(
            directory.path(),
            "elsewhere",
            "cat >/dev/null; echo 'remote: key not found in cache; Permission denied' >&2; exit 1",
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        assert!(smol::block_on(transport.stat("/www/x.php")).is_err());
    }

    #[test]
    fn stat_matches_a_longname_without_the_folder_prefix() {
        // Servers older than OpenSSH 9.1 print their own longname, with the bare name.
        let output = "\
-rw-r--r--    1 0   0          12 Nov 22 23:48 index.php
lrwxrwxrwx    1 0   0          11 Nov 22 23:48 evil -> /etc/passwd
";
        assert_eq!(
            stat_from_listing(output, "/www", "index.php").expect("stat"),
            Some(RemoteStat {
                size: 12,
                stamp: Some("Nov 22 23:48".into())
            })
        );
        let error = stat_from_listing(output, "/www", "evil").expect_err("symlink");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert_eq!(
            stat_from_listing(output, "/www", "none").expect("stat"),
            None
        );
    }

    #[test]
    fn stat_does_not_read_a_symlink_with_an_arrow_in_its_name_as_missing() {
        let output = "\
-rw-r--r--    ? u   g          12 Nov 22 23:48 /www/index.php
lrwxr-xr-x    ? u   g           5 Nov 22 23:48 /www/x -> y -> /etc/passwd
";
        let error = stat_from_listing(output, "/www", "x -> y").expect_err("could be a link");
        assert!(error.to_string().contains("ambiguous"), "{error}");
        assert_eq!(stat_from_listing(output, "/www", "z").expect("stat"), None);
    }

    #[test]
    fn stat_refuses_a_listing_that_names_the_file_twice() {
        // A file named "a\n-rw-r--r-- 1 u g 3 Nov 1 10:00 index.php" forges a second line.
        let output = "\
lrwxr-xr-x    ? u   g           5 Nov 22 23:48 /www/index.php -> /etc/passwd
-rw-r--r--    ? u   g           3 Nov 22 23:48 index.php
";
        let error = stat_from_listing(output, "/www", "index.php").expect_err("two lines");
        assert!(error.to_string().contains("more than once"), "{error}");
    }

    #[test]
    fn brace_characters_are_refused_in_remote_paths() {
        for path in ["/a/{b,c}", "/a{", "/a}", "/www{/x.php"] {
            assert!(list_batch(path).is_err(), "{path:?}");
            assert!(put_batch(Path::new("/tmp/u"), path).is_err(), "{path:?}");
            assert!(get_batch(path, Path::new("/tmp/u")).is_err(), "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_sftp_upload_file_is_world_readable_in_a_private_directory_holds_the_bytes_and_is_removed()
     {
        let directory = tempfile::tempdir().expect("tempdir");
        let recorded = directory.path().join("rec");
        std::fs::create_dir(&recorded).expect("mkdir");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!(
                "cat > '{rec}/batch'\n\
                 local=$(sed -n 's/^put \"\\([^\"]*\\)\".*/\\1/p' '{rec}/batch')\n\
                 echo \"$local\" > '{rec}/local'\n\
                 find \"$local\" -perm 644 > '{rec}/mode644'\n\
                 find \"$(dirname \"$local\")\" -perm 700 > '{rec}/dir700'\n\
                 cat \"$local\" > '{rec}/content'",
                rec = recorded.display()
            ),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let bytes = b"<?php\r\n\0\xff\n".to_vec();
        smol::block_on(crate::transport::upload_entry(
            &mut transport,
            "/www/index.php",
            &bytes,
        ))
        .expect("upload");
        let read = |name: &str| std::fs::read(recorded.join(name)).expect(name);
        let local = String::from_utf8(read("local")).expect("utf8");
        let local = Path::new(local.trim());
        assert!(
            !read("mode644").is_empty(),
            "the upload file was not exactly 0644 (a new remote file would be unreadable)"
        );
        assert!(
            !read("dir700").is_empty(),
            "the upload directory was not exactly 0700"
        );
        assert_eq!(read("content"), bytes);
        let batch = String::from_utf8(read("batch")).expect("utf8");
        assert_eq!(
            batch,
            format!("put \"{}\" \"/www/index.php\"\n", local.display())
        );
        assert!(!local.exists(), "{} was left behind", local.display());
        assert!(
            !local.parent().is_some_and(Path::exists),
            "the private directory was left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_sftp_upload_never_starts_sftp() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("started");
        let program = write_script(
            directory.path(),
            "sftp",
            &format!("touch '{}'; cat >/dev/null", marker.display()),
        );
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        for hostile in ["/www/a\r\n!touch x", "/www/a\\b", "/www/*", "/www/../x"] {
            assert!(
                smol::block_on(transport.upload_file(hostile, b"x")).is_err(),
                "{hostile:?}"
            );
        }
        assert!(!marker.exists(), "sftp was started for a refused path");
    }

    /// The real `sftp` client against the local sftp-server: `put` over an existing file keeps its
    /// mode, a folder listing shows a symlink as one, and a missing file is `None`.
    #[cfg(unix)]
    #[test]
    fn real_sftp_stats_and_uploads_in_place() {
        use std::os::unix::fs::PermissionsExt as _;
        let Some(sftp_server) = [
            "/usr/libexec/sftp-server",
            "/usr/lib/openssh/sftp-server",
            "/usr/lib/ssh/sftp-server",
        ]
        .into_iter()
        .find(|path| Path::new(path).exists()) else {
            eprintln!("skipped: no sftp-server binary found");
            return;
        };
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().canonicalize().expect("canonicalize");
        let remote = root.join("remote");
        std::fs::create_dir(&remote).expect("mkdir");
        let file = remote.join("sp ace.php");
        std::fs::write(&file, "old").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        std::os::unix::fs::symlink(&file, remote.join("evil")).expect("link");

        let program = write_script(&root, "sftp", &format!("exec sftp -b - -D {sftp_server}"));
        let mut transport = SftpTransport::with_program(program, &server()).expect("transport");
        let path = |name: &str| remote.join(name).to_str().expect("utf8").to_string();
        smol::block_on(async {
            let stat = transport
                .stat(&path("sp ace.php"))
                .await
                .expect("stat")
                .expect("exists");
            assert_eq!(stat.size, 3);
            assert!(stat.stamp.is_some());
            assert!(transport.stat(&path("evil")).await.is_err(), "symlink");
            assert_eq!(transport.stat(&path("missing")).await.expect("stat"), None);

            crate::transport::upload_entry(&mut transport, &path("sp ace.php"), b"<?php new")
                .await
                .expect("upload");
            assert_eq!(std::fs::read(&file).expect("read"), b"<?php new");
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o644, "put changed the remote file's mode");

            crate::transport::upload_entry(&mut transport, &path("new.php"), b"<?php")
                .await
                .expect("upload a new file");
            let created = std::fs::metadata(remote.join("new.php"))
                .expect("created")
                .permissions()
                .mode();
            assert_eq!(
                created & 0o777,
                0o644,
                "a new remote file must be world-readable"
            );
        });
    }
}
