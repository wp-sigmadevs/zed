use crate::MAX_RESPONSE_BYTES;
use crate::parser::HttpRequest;
use crate::response::HttpResponse;
use anyhow::{Context as _, Result};
use http_client::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpHistoryItem {
    pub id: String,
    pub request: HttpRequest,
    pub response: HttpResponse,
    pub timestamp: String,
}

#[derive(Debug, Clone, Default)]
pub struct HttpHistoryStore {
    pub entries: Vec<HttpHistoryItem>,
    pub max_limit: usize,
    /// Ids come from a counter that only grows, so trimming old entries never reuses one.
    next_id: u64,
}

impl HttpHistoryStore {
    pub fn new(max_limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_limit: if max_limit == 0 { 100 } else { max_limit },
            next_id: 0,
        }
    }

    /// Records a response; credential headers are masked first, so they never sit in history.
    pub fn add(&mut self, mut request: HttpRequest, mut response: HttpResponse) -> String {
        mask_secret_headers(&mut request.headers);
        mask_secret_headers(&mut response.headers);
        self.next_id += 1;
        let id = format!("hist-{}", self.next_id);
        let timestamp = response.timestamp.clone();

        self.entries.insert(
            0,
            HttpHistoryItem {
                id: id.clone(),
                request,
                response,
                timestamp,
            },
        );

        if self.entries.len() > self.max_limit {
            self.entries.truncate(self.max_limit);
        }

        id
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn get_recent(&self, limit: usize) -> &[HttpHistoryItem] {
        let count = limit.min(self.entries.len());
        &self.entries[..count]
    }
}

/// What a masked header value is replaced with.
pub const MASKED_VALUE: &str = "***";

/// Names whose values history never keeps: header names, JSON keys, form and query fields.
/// Matched case-insensitively and ignoring `-` and `_`, against any name that contains one,
/// so `X-Api-Key`, `user_password` and `Set-Cookie` are all caught.
const SECRET_NAMES: [&str; 13] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "api_key",
    "apikey",
    "client_secret",
    "authorization",
    "private_key",
    "cookie",
];

fn is_secret_name(name: &str) -> bool {
    let normalize = |text: &str| {
        text.chars()
            .filter(|character| !matches!(character, '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let name = normalize(name);
    SECRET_NAMES
        .iter()
        .any(|secret| name.ends_with(&normalize(secret)))
}

fn mask_secret_headers(headers: &mut [(String, String)]) {
    for (name, value) in headers {
        if is_secret_name(name) {
            *value = MASKED_VALUE.to_string();
        }
    }
}

fn content_type(headers: &[(String, String)]) -> Option<&str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
}

/// `body` with the values of secret fields masked, or `None` when nothing was masked (or it
/// did not parse), so the stored body stays byte-for-byte what was sent or received.
fn mask_secret_body(body: &str, content_type: Option<&str>) -> Option<String> {
    let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
    if content_type.contains("x-www-form-urlencoded") {
        return mask_secret_pairs(body);
    }
    if content_type.starts_with("multipart/") {
        return mask_secret_multipart(body, &content_type);
    }
    // shortcut: plain text, XML and other formats are stored as is; add a format here when one
    // starts carrying credentials in practice.
    let mut json = serde_json::from_str::<serde_json::Value>(body).ok()?;
    if !mask_secret_json(&mut json) {
        return None;
    }
    if body.contains('\n') {
        serde_json::to_string_pretty(&json).ok()
    } else {
        serde_json::to_string(&json).ok()
    }
}

fn mask_secret_json(value: &mut serde_json::Value) -> bool {
    let mut masked = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_secret_name(key) && (value.is_string() || value.is_number()) {
                    *value = MASKED_VALUE.into();
                    masked = true;
                } else {
                    masked |= mask_secret_json(value);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                masked |= mask_secret_json(item);
            }
        }
        _ => {}
    }
    masked
}

/// Masks `name=value` pairs separated by `&`, as in a form body or a URL query.
/// shortcut: names are compared as written, so a percent-encoded name (`api%5Fkey`) is missed.
fn mask_secret_pairs(pairs: &str) -> Option<String> {
    let mut masked = false;
    let joined = pairs
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if is_secret_name(name) => {
                masked = true;
                format!("{name}={MASKED_VALUE}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    masked.then_some(joined)
}

/// Masks the query of `url`, leaving its fragment and everything before the `?` untouched.
fn mask_secret_query(url: &str) -> Option<String> {
    let (base, rest) = url.split_once('?')?;
    let (query, fragment) = match rest.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (rest, None),
    };
    let query = mask_secret_pairs(query)?;
    Some(match fragment {
        Some(fragment) => format!("{base}?{query}#{fragment}"),
        None => format!("{base}?{query}"),
    })
}

/// Masks the content of form fields with a secret name. File parts (with a `filename`, or a
/// `< path` line standing for a file) are left alone.
fn mask_secret_multipart(body: &str, content_type: &str) -> Option<String> {
    let boundary = content_type
        .split(';')
        .find_map(|parameter| parameter.trim().strip_prefix("boundary="))?
        .trim_matches('"');
    // The content type was lowercased; find the boundary as written in the body.
    let start = body.to_ascii_lowercase().find(&format!("--{boundary}"))?;
    let delimiter = body.get(start..start + boundary.len() + 2)?;
    let mut masked = false;
    let parts = body
        .split(delimiter)
        .map(|part| {
            let Some((head, separator, content)) =
                ["\r\n\r\n", "\n\n"].iter().find_map(|separator| {
                    let (head, content) = part.split_once(separator)?;
                    Some((head, *separator, content))
                })
            else {
                return part.to_string();
            };
            let name = head
                .split(['\n', ';'])
                .find_map(|parameter| parameter.trim().strip_prefix("name="))
                .map(|name| name.trim_matches('"'));
            let is_file = head.contains("filename=") || content.trim_start().starts_with('<');
            match name {
                Some(name) if is_secret_name(name) && !is_file => {
                    masked = true;
                    let line_end = &content[content.trim_end_matches(['\r', '\n']).len()..];
                    format!("{head}{separator}{MASKED_VALUE}{line_end}")
                }
                _ => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(delimiter);
    masked.then_some(parts)
}

/// One row of a project's on-disk history index: what the picker shows, without the bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryIndexEntry {
    pub id: u64,
    pub method: String,
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub duration_ms: u64,
    pub timestamp: String,
    /// Size of the entry's file on disk, counted against the byte cap.
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryIndex {
    next_id: u64,
    /// Newest first.
    entries: Vec<HistoryIndexEntry>,
}

const INDEX_FILE: &str = "index.json";

/// Appends and clears from every window go through this lock, so two writers never
/// interleave the read-modify-write of one index.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// A project's response history on disk: `index.json` (newest first) plus one `<id>.json`
/// per response holding the request and response with credential headers masked. A JSON
/// index rather than SQLite: at most a few hundred rows that are always read whole, and a
/// file that fails to parse is simply started over. Blocking IO: call it on a background task.
#[derive(Debug, Clone)]
pub struct HistoryDir {
    directory: PathBuf,
    max_entries: usize,
    max_bytes: u64,
}

impl HistoryDir {
    pub fn new(directory: PathBuf, max_entries: usize, max_bytes: u64) -> Self {
        Self {
            directory,
            max_entries: max_entries.max(1),
            max_bytes,
        }
    }

    /// `<base>/<sha256 of the project's root paths>`, so each project keeps its own history.
    pub fn for_project(
        base: &Path,
        roots: &[impl AsRef<Path>],
        max_entries: usize,
        max_bytes: u64,
    ) -> Self {
        let mut hasher = Sha256::new();
        for root in roots {
            hasher.update(root.as_ref().to_string_lossy().as_bytes());
            hasher.update([0]);
        }
        let key = format!("{:x}", hasher.finalize());
        Self::new(base.join(key), max_entries, max_bytes)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The index, newest first. A missing or unreadable index is an empty history.
    pub fn list(&self) -> Vec<HistoryIndexEntry> {
        self.read_index().entries
    }

    /// Stores a response and prunes the oldest entries past the count and byte caps.
    /// Returns `None` without writing anything for a request to a non-HTTP(S) URL.
    pub fn append(
        &self,
        mut request: HttpRequest,
        mut response: HttpResponse,
    ) -> Result<Option<u64>> {
        // A URL still holding `{{host}}` does not parse; the run already refused non-web schemes.
        if !Url::parse(request.url.trim())
            .ok()
            .is_none_or(|url| matches!(url.scheme(), "http" | "https"))
        {
            return Ok(None);
        }
        mask_secret_headers(&mut request.headers);
        mask_secret_headers(&mut response.headers);
        if let Some(url) = mask_secret_query(&request.url) {
            request.url = url;
        }
        if let Some(body) = request
            .body
            .as_deref()
            .and_then(|body| mask_secret_body(body, content_type(&request.headers)))
        {
            request.body = Some(body);
        }
        // Masked before truncating: a cut-off JSON body no longer parses.
        if let Some(body) = mask_secret_body(&response.body, content_type(&response.headers)) {
            response.body = body;
        }
        if !response.truncated && response.body.len() > MAX_RESPONSE_BYTES {
            let mut end = MAX_RESPONSE_BYTES;
            while !response.body.is_char_boundary(end) {
                end -= 1;
            }
            response.body.truncate(end);
            response = response.mark_truncated(MAX_RESPONSE_BYTES);
        }

        let _guard = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        create_private_dir(&self.directory)?;
        let mut index = self.read_index();
        index.next_id += 1;
        let id = index.next_id;
        let entry = HistoryIndexEntry {
            id,
            method: request.method.clone(),
            url: request.url.clone(),
            status: response.status,
            status_text: response.status_text.clone(),
            duration_ms: response.duration_ms,
            timestamp: response.timestamp.clone(),
            bytes: 0,
        };
        let timestamp = response.timestamp.clone();
        let item = HttpHistoryItem {
            id: id.to_string(),
            request,
            response,
            timestamp,
        };
        let contents = serde_json::to_vec(&item)?;
        write_private(&self.entry_path(id), &contents)?;
        index.entries.insert(
            0,
            HistoryIndexEntry {
                bytes: contents.len() as u64,
                ..entry
            },
        );

        let mut kept_bytes = 0;
        let keep = index
            .entries
            .iter()
            .enumerate()
            .position(|(ix, entry)| {
                kept_bytes += entry.bytes;
                // The newest entry is always kept, even when it alone is over the byte cap.
                ix > 0 && (ix >= self.max_entries || kept_bytes > self.max_bytes)
            })
            .unwrap_or(index.entries.len());
        index.entries.truncate(keep);
        self.write_index(&index)?;
        self.remove_unindexed(&index)?;
        Ok(Some(id))
    }

    /// The full entry for `id`, read from its file.
    pub fn load(&self, id: u64) -> Result<HttpHistoryItem> {
        let path = self.entry_path(id);
        let contents = std::fs::read(&path)
            .with_context(|| format!("reading history entry {}", path.display()))?;
        serde_json::from_slice(&contents)
            .with_context(|| format!("parsing history entry {}", path.display()))
    }

    /// Removes the index and every entry file; other files in the directory are left alone.
    pub fn clear(&self) -> Result<()> {
        let _guard = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match std::fs::remove_file(self.directory.join(INDEX_FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("removing the history index");
            }
            _ => {}
        }
        self.remove_unindexed(&HistoryIndex::default())
    }

    fn entry_path(&self, id: u64) -> PathBuf {
        self.directory.join(format!("{id}.json"))
    }

    fn read_index(&self) -> HistoryIndex {
        let path = self.directory.join(INDEX_FILE);
        match std::fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents).unwrap_or_else(|error| {
                log::warn!(
                    "starting HTTP history over, {} is unreadable: {error}",
                    path.display()
                );
                HistoryIndex::default()
            }),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("reading {}: {error}", path.display());
                }
                HistoryIndex::default()
            }
        }
    }

    /// Written to a temporary file and renamed, so a reader never sees half an index.
    fn write_index(&self, index: &HistoryIndex) -> Result<()> {
        let temporary = self.directory.join(format!("{INDEX_FILE}.tmp"));
        write_private(&temporary, &serde_json::to_vec(index)?)?;
        std::fs::rename(&temporary, self.directory.join(INDEX_FILE))
            .context("replacing the history index")
    }

    /// Deletes `<id>.json` files the index no longer lists: pruned entries, and those left
    /// behind by a crash or a corrupt index.
    fn remove_unindexed(&self, index: &HistoryIndex) -> Result<()> {
        let listing = match std::fs::read_dir(&self.directory) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("listing the history directory"),
        };
        for dir_entry in listing {
            let path = dir_entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|stem| stem.parse::<u64>().ok())
            else {
                continue;
            };
            if !index.entries.iter().any(|entry| entry.id == id) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
        Ok(())
    }
}

fn create_private_dir(directory: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(directory)
        .with_context(|| format!("creating {}", directory.display()))
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut file| file.write_all(contents))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history_store() {
        let mut store = HttpHistoryStore::new(2);
        let req1 = HttpRequest::new("GET", "https://api.com/1", 1);
        let res1 = HttpResponse::new(200, "OK", vec![], "body1", 10);
        store.add(req1, res1);

        let req2 = HttpRequest::new("GET", "https://api.com/2", 2);
        let res2 = HttpResponse::new(200, "OK", vec![], "body2", 20);
        store.add(req2, res2);

        let req3 = HttpRequest::new("GET", "https://api.com/3", 3);
        let res3 = HttpResponse::new(200, "OK", vec![], "body3", 30);
        store.add(req3, res3);

        // Max limit is 2
        assert_eq!(store.entries.len(), 2);
        assert_eq!(store.entries[0].request.url, "https://api.com/3");
        assert_eq!(store.entries[1].request.url, "https://api.com/2");
    }

    #[test]
    fn test_history_ids_are_never_reused() {
        let mut store = HttpHistoryStore::new(2);
        let mut ids = Vec::new();
        for n in 0..5 {
            let request = HttpRequest::new("GET", &format!("https://api.com/{n}"), 1);
            ids.push(store.add(request, HttpResponse::new(200, "OK", vec![], "", 1)));
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5);
        let kept: Vec<&str> = store
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(kept, vec!["hist-5", "hist-4"]);
    }

    #[test]
    fn test_history_masks_credential_headers() {
        let mut store = HttpHistoryStore::new(10);
        let mut request = HttpRequest::new("GET", "https://api.com/me", 1);
        request.headers = vec![
            ("Authorization".into(), "Bearer secret".into()),
            ("cookie".into(), "session=abc".into()),
            ("Accept".into(), "*/*".into()),
        ];
        let response = HttpResponse::new(
            200,
            "OK",
            vec![
                ("SET-COOKIE".into(), "session=def".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            "{}",
            5,
        );
        store.add(request, response);
        let entry = &store.entries[0];
        assert_eq!(
            entry.request.headers,
            vec![
                ("Authorization".to_string(), MASKED_VALUE.to_string()),
                ("cookie".to_string(), MASKED_VALUE.to_string()),
                ("Accept".to_string(), "*/*".to_string()),
            ]
        );
        assert_eq!(
            entry.response.headers,
            vec![
                ("SET-COOKIE".to_string(), MASKED_VALUE.to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ]
        );
        assert!(!entry.response.to_raw_http().contains("def"));
        assert!(!entry.request.to_curl().contains("secret"));
    }

    #[test]
    fn test_is_secret_name() {
        for name in [
            "access_token",
            "client_secret",
            "X-Api-Key",
            "user_password",
            "Set-Cookie",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in ["total_tokens", "secretary", "username", "Accept"] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    #[test]
    fn test_mask_secret_body_json() {
        let body = r#"{"user":"ann","password":"not-a-real-value-1","n":{"access_token":"not-a-real-value-2"},"total_tokens":5}"#;
        let masked = mask_secret_body(body, Some("application/json")).unwrap();
        assert!(!masked.contains("not-a-real-value"), "{masked}");
        assert!(masked.contains("\"total_tokens\":5") && masked.contains("ann"));
        let clean = r#"{"user":"ann"}"#;
        assert_eq!(mask_secret_body(clean, Some("application/json")), None);
        assert_eq!(mask_secret_body("not json", None), None);
    }

    #[test]
    fn test_mask_secret_body_form_and_pairs() {
        let form = "user=ann&password=not-a-real-value-1&flag";
        let masked = mask_secret_body(form, Some("application/x-www-form-urlencoded"));
        assert_eq!(masked.as_deref(), Some("user=ann&password=***&flag"));
        assert_eq!(mask_secret_pairs("a=1&b=2"), None);
        assert_eq!(
            mask_secret_pairs("client_secret=not-a-real-value-1").as_deref(),
            Some("client_secret=***")
        );
    }

    #[test]
    fn test_mask_secret_query() {
        assert_eq!(
            mask_secret_query("https://a.test/p?q=1&api_key=not-a-real-value-1#top").as_deref(),
            Some("https://a.test/p?q=1&api_key=***#top")
        );
        assert_eq!(
            mask_secret_query("https://a.test/p?token=not-a-real-value-1").as_deref(),
            Some("https://a.test/p?token=***")
        );
        assert_eq!(mask_secret_query("https://a.test/p?q=1"), None);
        assert_eq!(mask_secret_query("https://a.test/p"), None);
    }

    #[test]
    fn test_mask_secret_multipart() {
        let part = |disposition: &str, content: &str| {
            format!("--XyZ\r\nContent-Disposition: form-data; {disposition}\r\n\r\n{content}\r\n")
        };
        let body = [
            part("name=\"title\"", "hello"),
            part("name=\"password\"", "not-a-real-value-1"),
            part("name=\"token\"; filename=\"t.txt\"", "not-a-real-value-2"),
            "--XyZ--\r\n".to_string(),
        ]
        .concat();
        let content_type = Some("multipart/form-data; boundary=XyZ");
        let masked = mask_secret_body(&body, content_type).unwrap();
        assert!(!masked.contains("not-a-real-value-1"), "{masked}");
        assert!(masked.contains("not-a-real-value-2"), "a file part is kept");
        assert!(masked.contains("hello") && masked.contains("\r\n***\r\n--XyZ"));
        let plain = [part("name=\"title\"", "hello"), "--XyZ--\r\n".to_string()].concat();
        assert_eq!(mask_secret_body(&plain, content_type), None);
    }

    fn get(url: &str) -> HttpRequest {
        HttpRequest::new("GET", url, 1)
    }

    fn ok(body: &str) -> HttpResponse {
        HttpResponse::new(200, "OK", vec![], body, 1)
    }

    fn urls(history: &HistoryDir) -> Vec<String> {
        history.list().into_iter().map(|entry| entry.url).collect()
    }

    fn entry_files(directory: &Path) -> usize {
        std::fs::read_dir(directory)
            .map(|listing| {
                listing
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_name() != INDEX_FILE)
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn test_disk_history_round_trip_survives_reopening() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::for_project(temp.path(), &["/work/app"], 10, u64::MAX);
        let mut request = HttpRequest::new("POST", "https://a.test/items", 4);
        request.body = Some("{\"a\": 1}".into());
        let response = HttpResponse::new(
            201,
            "Created",
            vec![("Content-Type".into(), "application/json".into())],
            "{\"id\": 7}",
            12,
        );
        let id = history
            .append(request.clone(), response.clone())?
            .expect("an https request is stored");
        history.append(get("https://a.test/second"), ok(""))?;

        let reopened = HistoryDir::for_project(temp.path(), &["/work/app"], 10, u64::MAX);
        assert_eq!(
            urls(&reopened),
            vec!["https://a.test/second", "https://a.test/items"]
        );
        let item = reopened.load(id)?;
        assert_eq!(item.request, request);
        assert_eq!(item.response, response);
        assert_eq!(item.id, id.to_string());

        let other = HistoryDir::for_project(temp.path(), &["/work/other"], 10, u64::MAX);
        assert!(other.list().is_empty(), "each project has its own history");
        Ok(())
    }

    #[test]
    fn test_disk_history_persists_masked_headers_only() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let mut request = get("https://a.test/me");
        request.headers = vec![("Authorization".into(), "Bearer secret-token".into())];
        let response = HttpResponse::new(
            200,
            "OK",
            vec![("Set-Cookie".into(), "session=secret-cookie".into())],
            "{}",
            1,
        );
        let id = history.append(request, response)?.expect("stored");
        let raw = std::fs::read_to_string(history.directory().join(format!("{id}.json")))?;
        assert!(!raw.contains("secret-token"), "{raw}");
        assert!(!raw.contains("secret-cookie"), "{raw}");
        let item = history.load(id)?;
        assert_eq!(item.request.headers[0].1, MASKED_VALUE);
        assert_eq!(item.response.headers[0].1, MASKED_VALUE);
        Ok(())
    }

    #[test]
    fn test_disk_history_prunes_oldest_by_count() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 3, u64::MAX);
        for n in 0..5 {
            history.append(get(&format!("https://a.test/{n}")), ok(""))?;
        }
        assert_eq!(
            urls(&history),
            vec!["https://a.test/4", "https://a.test/3", "https://a.test/2"]
        );
        assert_eq!(entry_files(history.directory()), 3, "pruned files deleted");
        Ok(())
    }

    #[test]
    fn test_disk_history_prunes_oldest_by_bytes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let probe = HistoryDir::new(temp.path().join("probe"), 10, u64::MAX);
        probe.append(get("https://a.test/0"), ok(&"x".repeat(1000)))?;
        let one_entry = probe.list()[0].bytes;

        let history = HistoryDir::new(temp.path().join("h"), 100, one_entry * 2 + 10);
        for n in 0..4 {
            history.append(get(&format!("https://a.test/{n}")), ok(&"x".repeat(1000)))?;
        }
        assert_eq!(urls(&history), vec!["https://a.test/3", "https://a.test/2"]);
        let total: u64 = history.list().iter().map(|entry| entry.bytes).sum();
        assert!(total <= one_entry * 2 + 10);
        assert_eq!(entry_files(history.directory()), 2);

        let tiny = HistoryDir::new(temp.path().join("tiny"), 100, 1);
        tiny.append(get("https://a.test/big"), ok(&"x".repeat(1000)))?;
        assert_eq!(
            urls(&tiny),
            vec!["https://a.test/big"],
            "newest always kept"
        );
        Ok(())
    }

    #[test]
    fn test_disk_history_truncates_large_bodies() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        // A multi-byte char straddling the limit must not split.
        let body = format!("{}é{}", "a".repeat(MAX_RESPONSE_BYTES - 1), "b".repeat(10));
        let id = history
            .append(get("https://a.test/big"), ok(&body))?
            .expect("stored");
        let response = history.load(id)?.response;
        assert!(response.truncated);
        assert!(
            response
                .body
                .starts_with(&"a".repeat(MAX_RESPONSE_BYTES - 1))
        );
        assert!(response.body.contains("[Response truncated"));
        assert!(!response.body.contains('b'));
        Ok(())
    }

    #[test]
    fn test_disk_history_skips_non_http_schemes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        assert_eq!(history.append(get("ftp://a.test/file"), ok("x"))?, None);
        assert_eq!(history.append(get("file:///etc/passwd"), ok("x"))?, None);
        assert!(!history.directory().exists(), "nothing written");
        assert!(history.append(get("{{host}}/x"), ok(""))?.is_some());
        assert!(history.append(get("HTTP://a.test/"), ok(""))?.is_some());
        Ok(())
    }

    #[test]
    fn test_disk_history_tolerates_a_corrupt_index() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        history.append(get("https://a.test/old"), ok(""))?;
        history.append(get("https://a.test/older"), ok(""))?;
        std::fs::write(history.directory().join(INDEX_FILE), "{not json")?;
        assert!(history.list().is_empty());

        history.append(get("https://a.test/new"), ok(""))?;
        assert_eq!(urls(&history), vec!["https://a.test/new"]);
        assert_eq!(entry_files(history.directory()), 1, "orphan removed");
        Ok(())
    }

    #[test]
    fn test_disk_history_clear() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        history.clear()?;
        history.append(get("https://a.test/1"), ok(""))?;
        history.append(get("https://a.test/2"), ok(""))?;
        history.clear()?;
        assert!(history.list().is_empty());
        assert_eq!(entry_files(history.directory()), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_disk_history_files_are_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("base").join("h"), 10, u64::MAX);
        let id = history
            .append(get("https://a.test/"), ok(""))?
            .expect("stored");
        let mode = |path: &Path| -> Result<u32> {
            Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
        };
        assert_eq!(mode(history.directory())?, 0o700);
        assert_eq!(mode(&temp.path().join("base"))?, 0o700);
        assert_eq!(mode(&history.directory().join(INDEX_FILE))?, 0o600);
        assert_eq!(
            mode(&history.directory().join(format!("{id}.json")))?,
            0o600
        );
        Ok(())
    }
}
