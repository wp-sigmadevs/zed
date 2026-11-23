use std::{ffi::OsString, path::Path, pin::pin, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use futures::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, future::Either};
use util::command::{Stdio, new_command};

use crate::{
    config::ServerConfig,
    remote_path::validate_remote_path,
    transport::{
        EntryKind, MAX_DOWNLOAD_BYTES, MAX_LISTING_BYTES, RemoteEntry, Transport, retain_listable,
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
        let mut command = new_command(&self.program);
        command
            .args(&self.args)
            .current_dir(working_directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().context("starting sftp")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(batch.as_bytes()).await?;
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
            return Err(self.failure(&stderr));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    fn failure(&self, stderr: &str) -> anyhow::Error {
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
        anyhow!("sftp failed: {detail}")
    }
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
    if !is_plain(&server.host, &[':']) {
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
/// `\` and (for remote paths) the glob characters `*?[` are refused too because sftp's own
/// escaping of them does not round-trip.
fn quote(argument: &str, remote: bool) -> Result<String> {
    if argument.is_empty()
        || argument.chars().any(char::is_control)
        || argument.contains('\\')
        || (remote && argument.contains(['*', '?', '[']))
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
        "ls -la {}",
        quote(validate_remote_path(path)?, true)?
    ))
}

fn get_batch(remote_path: &str, local_path: &Path) -> Result<String> {
    let local = local_path.to_str().context("local path is not UTF-8")?;
    if !local_path.is_absolute() {
        bail!("local path {local:?} is not absolute");
    }
    batch_line(format!(
        "get {} {}",
        quote(validate_remote_path(remote_path)?, true)?,
        quote(local, false)?
    ))
}

/// Parses `ls -la` output. sftp prints the full path in the name column, so `directory` is
/// stripped from it; lines that do not parse (including the echoed `sftp> ` commands and any
/// fragment of a name that contained a newline) are dropped.
pub(crate) fn parse_ls(output: &str, directory: &str) -> Vec<RemoteEntry> {
    let prefix = format!("{}/", directory.trim_end_matches('/'));
    output
        .lines()
        .filter_map(|line| parse_ls_line(line, &prefix))
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
        for host in ["-oProxyCommand=touch x", "a b", "a@b", "", "h\nx", "a;b"] {
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
        assert_eq!(batch, "ls -la \"/x/!touch pwned\"\n");
        assert_eq!(
            get_batch("/x/a \"b\".txt", Path::new("/tmp/o")).expect("get"),
            "get \"/x/a \\\"b\\\".txt\" \"/tmp/o\"\n"
        );
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
        let alive = |pid: String| async move {
            util::command::new_command("kill")
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|status| status.success())
        };
        smol::block_on(async {
            // The timer wins; the pending operation is dropped when the result goes away.
            futures::future::select(Box::pin(transport.pwd()), Box::pin(sleep(700))).await;
            let pid = std::fs::read_to_string(&pid_file)
                .expect("the fake sftp started")
                .trim()
                .to_string();
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

        // "ls -la \"" + path + "\"" is 9 bytes plus the path; the newline does not count.
        let longest = format!("/{}", "d".repeat(MAX_BATCH_LINE - 11));
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
}
