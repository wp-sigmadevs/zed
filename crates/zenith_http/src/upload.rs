//! Request bodies that read files, in JetBrains syntax: a `< ./path` line stands for that
//! file's bytes, as the whole body or as the content of a multipart part.

use crate::HttpRequest;
use crate::parser::substitute_variables;
use anyhow::{Context as _, Result, bail, ensure};
use fs::Fs;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Most bytes read from files for one request body.
pub const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;

/// The bytes to send as `request`'s body: variables substituted (in the body text, not in the
/// files), then each `< path` line replaced by that file's bytes. Paths are relative to `dir`,
/// the `.http` file's folder, and must stay inside `root`; without both, a body naming a file
/// is refused. Multipart bodies get the CRLF line breaks RFC 2046 asks for.
pub async fn request_body(
    request: &HttpRequest,
    env: &HashMap<String, String>,
    fs: &Arc<dyn Fs>,
    dir: Option<&Path>,
    root: Option<&Path>,
) -> Result<Option<Vec<u8>>> {
    let Some(body) = &request.body else {
        return Ok(None);
    };
    let body = substitute_variables(body, env);
    let multipart = request.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/")
    });
    let separator: &[u8] = if multipart { b"\r\n" } else { b"\n" };
    let mut out = Vec::new();
    let mut read = 0;
    for (index, line) in body.split('\n').enumerate() {
        if index > 0 {
            out.extend_from_slice(separator);
        }
        let Some(path) = file_reference(line) else {
            out.extend_from_slice(line.as_bytes());
            continue;
        };
        let (Some(dir), Some(root)) = (dir, root) else {
            bail!("`< {path}` needs a saved .http file in a local project");
        };
        let bytes = read_upload(fs, dir, root, path, MAX_UPLOAD_BYTES - read).await?;
        read += bytes.len() as u64;
        out.extend_from_slice(&bytes);
    }
    Ok(Some(out))
}

/// The path of a `< path` line. `<tag>` (XML, HTML) and JetBrains' `<@` are not file lines.
fn file_reference(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('<')?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim()).filter(|path| !path.is_empty())
}

/// Reads `path` (relative to `dir`, or absolute) when it is a regular file, not a symlink,
/// resolves inside `root`, and holds at most `limit` bytes. A cloned project could otherwise
/// send `~/.ssh/id_rsa` or another worktree's secrets with one innocent-looking request.
pub async fn read_upload(
    fs: &Arc<dyn Fs>,
    dir: &Path,
    root: &Path,
    path: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let full = dir.join(path);
    let metadata = fs
        .metadata(&full)
        .await?
        .with_context(|| format!("{path} does not exist"))?;
    ensure!(
        !metadata.is_symlink,
        "{path} is a symlink; only regular files are sent"
    );
    ensure!(
        !metadata.is_dir && !metadata.is_fifo,
        "{path} is not a regular file"
    );
    let root = fs.canonicalize(root).await?;
    let canonical = fs.canonicalize(&full).await?;
    ensure!(
        canonical.starts_with(&root),
        "{path} is outside the project folder {}",
        root.display()
    );
    let too_big = || format!("{path}: files sent in one request may total at most 50 MB");
    ensure!(metadata.len <= limit, too_big());
    let bytes = fs.load_bytes(&canonical).await?;
    // The file may have grown since the size check.
    ensure!(bytes.len() as u64 <= limit, too_big());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use std::path::PathBuf;

    async fn project(cx: &TestAppContext) -> Arc<dyn Fs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({
                "api": { "data.json": "{\"a\":1}", "logo.png": "PNG" },
                "big.bin": "0123456789A",
            }),
        )
        .await;
        fs.insert_tree("/outside", json!({ "secret.txt": "s3cret" }))
            .await;
        fs.insert_symlink("/root/api/link.json", PathBuf::from("/root/api/data.json"))
            .await;
        fs.insert_symlink("/root/api/escape", PathBuf::from("/outside"))
            .await;
        fs
    }

    fn post(content_type: &str, body: &str) -> HttpRequest {
        let mut request = HttpRequest::new("POST", "https://a.test/upload", 1);
        request.headers = vec![("Content-Type".into(), content_type.into())];
        request.body = Some(body.into());
        request
    }

    async fn body(fs: &Arc<dyn Fs>, request: &HttpRequest) -> Result<Vec<u8>> {
        let env = HashMap::from([("dir".to_string(), ".".to_string())]);
        request_body(
            request,
            &env,
            fs,
            Some(Path::new("/root/api")),
            Some(Path::new("/root")),
        )
        .await
        .map(Option::unwrap_or_default)
    }

    #[gpui::test]
    async fn test_whole_body_from_file(cx: &mut TestAppContext) {
        let fs = project(cx).await;
        let request = post("application/json", "< {{dir}}/data.json");
        assert_eq!(body(&fs, &request).await.unwrap(), b"{\"a\":1}");
        let parent = post("application/octet-stream", "< ../big.bin");
        assert_eq!(body(&fs, &parent).await.unwrap(), b"0123456789A");
    }

    #[gpui::test]
    async fn test_multipart_parts_from_files_use_crlf(cx: &mut TestAppContext) {
        let fs = project(cx).await;
        let text = "--B\nContent-Disposition: form-data; name=\"title\"\n\nhello\n--B\nContent-Disposition: form-data; name=\"file\"; filename=\"logo.png\"\nContent-Type: image/png\n\n< ./logo.png\n--B--";
        let request = post("multipart/form-data; boundary=B", text);
        let expected = "--B\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"logo.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--B--";
        assert_eq!(
            String::from_utf8(body(&fs, &request).await.unwrap()).unwrap(),
            expected
        );
    }

    #[gpui::test]
    async fn test_bodies_without_file_lines_are_unchanged(cx: &mut TestAppContext) {
        let fs = project(cx).await;
        let xml = "<root>\n  <item/>\n</root>";
        assert_eq!(
            body(&fs, &post("application/xml", xml)).await.unwrap(),
            xml.as_bytes()
        );
        let no_file = request_body(
            &post("text/plain", "< ./data.json"),
            &HashMap::new(),
            &fs,
            None,
            None,
        )
        .await;
        assert!(no_file.is_err(), "no folder to read from");
    }

    #[gpui::test]
    async fn test_unsafe_paths_are_refused(cx: &mut TestAppContext) {
        let fs = project(cx).await;
        for line in [
            "< ../../outside/secret.txt",
            "< /outside/secret.txt",
            "< ./link.json",
            "< ./escape/secret.txt",
            "< ./missing.json",
            "< ../api",
        ] {
            let result = body(&fs, &post("text/plain", line)).await;
            assert!(result.is_err(), "{line} must be refused");
        }
    }

    #[gpui::test]
    async fn test_upload_size_is_capped(cx: &mut TestAppContext) {
        let fs = project(cx).await;
        let (dir, root) = (Path::new("/root"), Path::new("/root"));
        assert!(read_upload(&fs, dir, root, "big.bin", 10).await.is_err());
        assert_eq!(
            read_upload(&fs, dir, root, "big.bin", 11).await.unwrap(),
            b"0123456789A"
        );
    }
}
