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

/// What a masked header value is replaced with.
pub const MASKED_VALUE: &str = "***";

/// Names whose values history never keeps: header names, JSON keys, form, query and fragment
/// fields. A name is percent-decoded and split into lowercase segments on anything that is not a
/// letter or digit and on camelCase humps; trailing digits of a segment are dropped
/// (`password1`). So `X-Api-Key`, `XApiKey`, `user[password]`, `user%5Bpassword%5D`,
/// `new_password1` and `aws_secret_access_key` are caught. `token` counts only as the last
/// segment (`access_token`, not `total_tokens`), and the words below only as a whole segment
/// or its ending (`userpassword`, not `secretary`). Weak words (`key`, `policy`, `session`,
/// `tokens`, `refresh`) count only as the whole name; bare `access` is never a secret.
// shortcut: a descriptor ending (`password_min_length`, `forgot_password_url`) or a paging
// token (`next_token`) stays readable, so a secret hidden behind such a name is not caught;
// drop the exemptions if that bites.
fn is_secret_name(name: &str) -> bool {
    const ENDINGS: [&str; 7] = [
        "password",
        "passwd",
        "pwd",
        "secret",
        "secrets",
        "authorization",
        "cookie",
    ];
    // The word before `key` that makes it a credential: `api_key`, `X-Auth-Key`, `XApiKey`.
    const KEY_PREFIXES: [&str; 16] = [
        "api",
        "app",
        "application",
        "session",
        "client",
        "master",
        "license",
        "subscription",
        "functions",
        "access",
        "secret",
        "private",
        "auth",
        "account",
        "signing",
        "write",
    ];
    const WHOLE_NAMES: [&str; 14] = [
        "key",
        "sig",
        "policy",
        "auth",
        "session",
        "sessionid",
        "idtokenhint",
        "tokens",
        "refresh",
        "cardnumber",
        "creditcardnumber",
        "mfacode",
        "otpcode",
        "pincode",
    ];
    const LAST_SEGMENTS: [&str; 18] = [
        "pass",
        "pw",
        "signature",
        "credential",
        "credentials",
        "phpsessid",
        "jsessionid",
        "sessionid",
        "passphrase",
        "passcode",
        "authentication",
        "jwt",
        "ssn",
        "cvv",
        "cvc",
        "otp",
        "totp",
        "pin",
    ];
    const DESCRIPTORS: [&str; 8] = [
        "url", "uri", "endpoint", "length", "type", "min", "max", "required",
    ];
    const PAGING: [&str; 3] = ["next", "continuation", "page"];

    let name = percent_decode(name);
    let mut segments: Vec<String> = Vec::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if !character.is_alphanumeric() {
            segments.push(String::new());
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower {
            segments.push(String::new());
        }
        previous_lower = character.is_lowercase() || character.is_ascii_digit();
        if segments.is_empty() {
            segments.push(String::new());
        }
        segments
            .last_mut()
            .expect("pushed above")
            .extend(character.to_lowercase());
    }
    for segment in &mut segments {
        segment.truncate(segment.trim_end_matches(|c: char| c.is_ascii_digit()).len());
    }
    segments.retain(|segment| !segment.is_empty());
    let Some(last) = segments.last() else {
        return false;
    };
    if segments.len() > 1 && DESCRIPTORS.contains(&last.as_str()) {
        return false;
    }

    WHOLE_NAMES.contains(&segments.concat().as_str())
        || LAST_SEGMENTS.contains(&last.as_str())
        || segments
            .iter()
            .any(|segment| ENDINGS.iter().any(|word| segment.ends_with(word)))
        || (last.ends_with("token") && {
            // Only a name made of paging words before `token` is a paging token.
            let before = &segments[..segments.len() - 1];
            before.is_empty() || !before.iter().all(|word| PAGING.contains(&word.as_str()))
        })
        || segments.iter().enumerate().any(|(ix, segment)| {
            let word = match segment.strip_suffix("key") {
                Some("") => ix.checked_sub(1).and_then(|before| segments.get(before)),
                Some(_) => Some(segment),
                None => None,
            };
            let Some(word) = word else { return false };
            let word = word.strip_suffix("key").unwrap_or(word);
            KEY_PREFIXES.iter().any(|prefix| word.ends_with(prefix))
        })
}

/// Decodes `%XX` escapes; anything malformed is kept as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        match (
            bytes[ix],
            bytes.get(ix + 1).and_then(|b| hex(*b)),
            bytes.get(ix + 2).and_then(|b| hex(*b)),
        ) {
            (b'%', Some(high), Some(low)) => {
                decoded.push((high * 16 + low) as u8);
                ix += 3;
            }
            (byte, ..) => {
                decoded.push(byte);
                ix += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
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

/// Stored in place of a cut-off JSON body: it cannot be parsed, so its secrets cannot be masked.
const UNMASKABLE_BODY: &str = "[body not stored: truncated, secrets could not be masked]";

/// Whether `body` may be JSON, by content type or, failing that, by its first character. Form
/// and multipart bodies are masked by prefix, so a cut leaves no secret value unmasked.
fn may_be_json(body: &str, content_type: Option<&str>) -> bool {
    let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
    if content_type.contains("x-www-form-urlencoded") || content_type.starts_with("multipart/") {
        return false;
    }
    content_type.contains("json") || body.trim_start().starts_with(['{', '['])
}

fn mask_secret_json(value: &mut serde_json::Value) -> bool {
    let mut masked = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_secret_name(key) {
                    // null and booleans carry no secret; keep them readable.
                    if !matches!(value, serde_json::Value::Null | serde_json::Value::Bool(_)) {
                        *value = MASKED_VALUE.into();
                        masked = true;
                    }
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

/// `url` without its `user:password@` part, or `None` when it has none.
///
/// A template (`{{var}}`) is cut by hand, so its variables keep their spelling. Any other URL
/// that parses is rebuilt from the parsed form, so every spelling the client accepts
/// (`https:/u:p@h`, `https:u:p@h`, extra slashes, backslashes) loses its userinfo.
fn strip_url_userinfo(url: &str) -> Option<String> {
    let url = url.trim_start();
    // A template keeps its `{{var}}` spelling: `Url` would percent-encode path variables and
    // lowercase host ones, so they would no longer resolve. Parsing is the fallback.
    if url.contains("{{")
        && let Some(stripped) = cut_url_userinfo(url)
    {
        return Some(stripped);
    }
    if let Ok(mut parsed) = Url::parse(url.trim_end()) {
        if parsed.username().is_empty() && parsed.password().is_none() {
            return None;
        }
        // Never fails for http(s); otherwise the cut by hand below still runs.
        if parsed.set_username("").is_ok() && parsed.set_password(None).is_ok() {
            return Some(parsed.into());
        }
    }
    cut_url_userinfo(url)
}

/// The cut by hand behind [`strip_url_userinfo`]: scheme up to the first `:`, any run of `/`
/// and `\`, then an authority ending at `/`, `\`, `?` or `#`.
fn cut_url_userinfo(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once(':')?;
    // The scheme may be a template (`{{scheme}}`), but a `:` further in, after a path or a
    // query, is not the URL's own.
    if scheme.is_empty()
        || scheme
            .chars()
            .any(|c| c.is_whitespace() || "/\\?#@".contains(c))
    {
        return None;
    }
    let authority_start = rest.len() - rest.trim_start_matches(['/', '\\']).len();
    let (slashes, rest) = rest.split_at(authority_start);
    let authority_end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
    let (_, host) = rest[..authority_end].rsplit_once('@')?;
    Some(format!(
        "{scheme}:{slashes}{host}{}",
        &rest[authority_end..]
    ))
}

/// Masks the query and the fragment (`#access_token=...`) of `url` like pairs; everything
/// before the `?` is left untouched.
fn mask_secret_query(url: &str) -> Option<String> {
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let (base, query) = match rest.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (rest, None),
    };
    let masked_query = query.and_then(mask_secret_pairs);
    let masked_fragment = fragment.and_then(mask_secret_pairs);
    if masked_query.is_none() && masked_fragment.is_none() {
        return None;
    }
    let mut masked = base.to_string();
    if let Some(query) = masked_query.as_deref().or(query) {
        masked.push('?');
        masked.push_str(query);
    }
    if let Some(fragment) = masked_fragment.as_deref().or(fragment) {
        masked.push('#');
        masked.push_str(fragment);
    }
    Some(masked)
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
        if let Some(url) = strip_url_userinfo(&request.url) {
            request.url = url;
        }
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
        // Masked before the cut at the size limit below. A body the run already cut does not
        // parse (it ends mid-value, then the truncation note), so JSON that may hold secrets
        // is not stored at all.
        match mask_secret_body(&response.body, content_type(&response.headers)) {
            Some(body) => response.body = body,
            None if response.truncated
                && may_be_json(&response.body, content_type(&response.headers)) =>
            {
                response.body = UNMASKABLE_BODY.to_string();
            }
            None => {}
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
    fn test_is_secret_name() {
        for name in [
            "access_token",
            "client_secret",
            "X-Api-Key",
            "user_password",
            "Set-Cookie",
            "password_confirmation",
            "user[password]",
            "user%5Bpassword%5D",
            "user%5bpassword%5d",
            "secret_key",
            "aws_secret_access_key",
            "access_key",
            "X-Auth-Key",
            "accessToken",
            "apiKey",
            "userpassword",
            "pwd",
            "userpwd",
            "dbpwd",
            "XApiKey",
            "password1",
            "password2",
            "new_password1",
            "key",
            "Ocp-Apim-Subscription-Key",
            "x-functions-key",
            "app_key",
            "client_key",
            "master_key",
            "license_key",
            "x-amz-signature",
            "sig",
            "signature",
            "policy",
            "x-amz-credential",
            "x-amz-security-token",
            "jwt",
            "auth",
            "session",
            "sessionid",
            "session_id",
            "PHPSESSID",
            "JSESSIONID",
            "pass",
            "pw",
            "passcode",
            "passphrase",
            "credentials",
            "authentication",
            "page_access_token",
            "next_access_token",
            "token",
            "db_pass",
            "smtp_pass",
            "admin_pass",
            "db_pw",
            "DD-APPLICATION-KEY",
            "session_key",
            "sessionKey",
            "id_token_hint",
            "password2Confirm",
            "Proxy-Authorization",
            "X-Auth-Token",
            "X-CSRF-Token",
            "refresh_token",
            "id_token",
            "private_key",
            "apikey",
            "api-key",
            "ssn",
            "user_ssn",
            "cvv",
            "card_cvc",
            "otp",
            "totp",
            "pin",
            "user_pin",
            "card_number",
            "creditCardNumber",
            "mfa_code",
            "otp_code",
            "pin_code",
            "secrets",
            "client_secrets",
            "tokens",
            "refresh",
            "AccountKey",
            "signing_key",
            "write_key",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "total_tokens",
            "secretary",
            "username",
            "Accept",
            "primary_key",
            "token_type",
            "100%",
            "authorization_endpoint",
            "forgot_password_url",
            "password_min_length",
            "next_token",
            "nextPageToken",
            "continuation_token",
            "monkey",
            "compass",
            "bypass",
            "access",
            "access_level",
            "refresh_rate",
            "pinned",
            "spin",
            "pin_length",
            "card_type",
        ] {
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
    fn test_mask_secret_body_json_nested_values() {
        let object =
            r#"{"password":{"old":"not-a-real-value-1","new":"not-a-real-value-2"},"ok":1}"#;
        let masked = mask_secret_body(object, Some("application/json")).unwrap();
        assert_eq!(masked, r#"{"password":"***","ok":1}"#);
        let array = r#"{"api_key":["not-a-real-value-3","not-a-real-value-4"]}"#;
        let masked = mask_secret_body(array, Some("application/json")).unwrap();
        assert_eq!(masked, r#"{"api_key":"***"}"#);
    }

    #[test]
    fn test_mask_secret_body_json_keeps_null_and_booleans() {
        let body = r#"{"password":null,"token":true,"api_key":false,"secret":0,"auth":"not-a-real-value-1"}"#;
        let masked = mask_secret_body(body, Some("application/json")).unwrap();
        assert_eq!(
            masked,
            r#"{"password":null,"token":true,"api_key":false,"secret":"***","auth":"***"}"#
        );
        let only_flags = r#"{"password":null}"#;
        assert_eq!(mask_secret_body(only_flags, Some("application/json")), None);
    }

    #[test]
    fn test_mask_secret_url_fragment() {
        assert_eq!(
            mask_secret_query("https://a.test/cb#access_token=not-a-real-value-1&state=x")
                .as_deref(),
            Some("https://a.test/cb#access_token=***&state=x")
        );
        assert_eq!(
            mask_secret_query(
                "https://a.test/cb?code=1&api_key=not-a-real-value-2#id_token=not-a-real-value-3"
            )
            .as_deref(),
            Some("https://a.test/cb?code=1&api_key=***#id_token=***")
        );
        assert_eq!(mask_secret_query("https://a.test/cb#top"), None);
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
    fn test_disk_history_append_masks_bodies_and_query_on_disk() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let json_headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let mut json = HttpRequest::new(
            "POST",
            "https://a.test/login?token=not-a-real-value-1&q=1",
            1,
        );
        json.headers = json_headers.clone();
        json.body = Some(r#"{"user":"ann","password":"not-a-real-value-2"}"#.into());
        let response = HttpResponse::new(
            200,
            "OK",
            json_headers,
            r#"{"access_token":"not-a-real-value-3"}"#,
            1,
        );
        let mut form = HttpRequest::new("POST", "https://a.test/form", 2);
        form.headers = vec![(
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        )];
        form.body = Some("user=ann&user%5Bpassword%5D=not-a-real-value-4".into());
        let first = history.append(json, response)?.expect("stored");
        let second = history.append(form, ok(""))?.expect("stored");

        for id in [first, second] {
            let raw = std::fs::read_to_string(history.directory().join(format!("{id}.json")))?;
            assert!(!raw.contains("not-a-real-value"), "{raw}");
        }
        let raw_index = std::fs::read_to_string(history.directory().join(INDEX_FILE))?;
        assert!(!raw_index.contains("not-a-real-value"), "{raw_index}");
        let item = history.load(first)?;
        assert!(item.request.url.contains("q=1") && item.request.url.contains("token=***"));
        assert!(item.request.body.unwrap().contains("ann"));
        Ok(())
    }

    #[test]
    fn test_strip_url_userinfo() {
        assert_eq!(
            strip_url_userinfo("https://ann:not-a-real-value-1@a.test:8080/p?q=1#x").as_deref(),
            Some("https://a.test:8080/p?q=1#x")
        );
        assert_eq!(
            strip_url_userinfo("https://not-a-real-value-2@{{host}}/p").as_deref(),
            Some("https://{{host}}/p")
        );
        assert_eq!(strip_url_userinfo("https://a.test/p?email=a@b.test"), None);
        assert_eq!(strip_url_userinfo("{{base}}/p"), None);
        // Variables keep their spelling: no percent-encoding, no lowercasing.
        assert_eq!(
            strip_url_userinfo("https://ann:not-a-real-value-30@{{apiHost}}/users/{{userId}}")
                .as_deref(),
            Some("https://{{apiHost}}/users/{{userId}}")
        );
        // A scheme-less templated URL with a `://` and an `@` in its query stays as sent.
        assert_eq!(
            strip_url_userinfo("{{base}}/p?next=https://a.test/x&mail=a@b.test"),
            None
        );
        assert_eq!(
            strip_url_userinfo("{{base}}?next=https://a.test@b.test/x"),
            None
        );
        // The last `@` of the authority ends the userinfo.
        assert_eq!(
            strip_url_userinfo("https://ann:not-a-real-value-3@x@a.test/p").as_deref(),
            Some("https://a.test/p")
        );
        // A username alone is userinfo too.
        assert_eq!(
            strip_url_userinfo("https://ann@a.test/p").as_deref(),
            Some("https://a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("http://ann:not-a-real-value-4@[::1]:80/p").as_deref(),
            Some("http://[::1]/p")
        );
        // An `@` only in the path, query or fragment is not userinfo; nor is no `@` at all.
        assert_eq!(strip_url_userinfo("https://a.test/@ann"), None);
        assert_eq!(strip_url_userinfo("https://a.test?m=a@b.test"), None);
        assert_eq!(strip_url_userinfo("https://a.test#a@b"), None);
        assert_eq!(strip_url_userinfo("https://a.test/p"), None);
        // A templated scheme still gets its userinfo stripped; a parsed URL is stored as parsed (lowercase scheme, no default port).
        assert_eq!(
            strip_url_userinfo("{{scheme}}://ann:not-a-real-value-5@a.test/p").as_deref(),
            Some("{{scheme}}://a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("http{{tls}}://ann:not-a-real-value-6@a.test/p").as_deref(),
            Some("http{{tls}}://a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("HTTPS://ann:not-a-real-value-7@a.test/p").as_deref(),
            Some("https://a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("  https://ann:not-a-real-value-8@a.test/p").as_deref(),
            Some("https://a.test/p")
        );
        // Every spelling `Url::parse` accepts loses its userinfo; the stored URL is the parsed one.
        for url in [
            "https:/ann:not-a-real-value-10@a.test/p",
            "https:ann:not-a-real-value-11@a.test/p",
            "https:///ann:not-a-real-value-12@a.test/p",
            "https:\\\\ann:not-a-real-value-13@a.test\\p",
            "https:/\\/ann:not-a-real-value-14@a.test/p",
        ] {
            assert_eq!(
                strip_url_userinfo(url).as_deref(),
                Some("https://a.test/p"),
                "{url}"
            );
        }
        // A template that does not parse: slashes and backslashes after the colon are kept, and
        // a backslash ends the authority.
        assert_eq!(
            strip_url_userinfo("{{scheme}}:/ann:not-a-real-value-15@a.test/p").as_deref(),
            Some("{{scheme}}:/a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("{{scheme}}:ann:not-a-real-value-16@a.test/p").as_deref(),
            Some("{{scheme}}:a.test/p")
        );
        assert_eq!(
            strip_url_userinfo("{{scheme}}:\\\\ann:not-a-real-value-17@a.test\\p?x").as_deref(),
            Some("{{scheme}}:\\\\a.test\\p?x")
        );
        assert_eq!(strip_url_userinfo("{{scheme}}://a.test\\@ann"), None);
        // A raw `/ ? #` inside a password ends the authority early, so the rest of the
        // password stays. Such a URL mostly fails `Url::parse` in the client and
        // `history_view::record` skips failed requests; a digits-only password before the `/`
        // parses as a port, the request goes to the wrong host and fails. Negligible.
    }

    #[test]
    fn test_disk_history_strips_url_userinfo() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let request = HttpRequest::new("GET", "https://ann:not-a-real-value-3@a.test/p", 1);
        let id = history.append(request, ok(""))?.expect("stored");
        let raw = std::fs::read_to_string(history.directory().join(format!("{id}.json")))?;
        assert!(!raw.contains("not-a-real-value"), "{raw}");
        let raw_index = std::fs::read_to_string(history.directory().join(INDEX_FILE))?;
        assert!(!raw_index.contains("not-a-real-value"), "{raw_index}");
        assert_eq!(history.load(id)?.request.url, "https://a.test/p");

        // A templated scheme fails `Url::parse`, so the request is still stored; its
        // userinfo must not be.
        let request = HttpRequest::new("GET", "{{scheme}}://ann:not-a-real-value-9@a.test/p", 2);
        let id = history.append(request, ok(""))?.expect("stored");
        let raw = std::fs::read_to_string(history.directory().join(format!("{id}.json")))?;
        assert!(!raw.contains("not-a-real-value"), "{raw}");
        let raw_index = std::fs::read_to_string(history.directory().join(INDEX_FILE))?;
        assert!(!raw_index.contains("not-a-real-value"), "{raw_index}");
        assert_eq!(history.load(id)?.request.url, "{{scheme}}://a.test/p");

        // Spellings the client sends that do not contain `://`.
        for (ix, url) in [
            "https:/ann:not-a-real-value-20@a.test/p",
            "https:ann:not-a-real-value-21@a.test/p",
            "https:///ann:not-a-real-value-22@a.test/p",
            "https:\\\\ann:not-a-real-value-23@a.test/p",
            "{{scheme}}:\\\\ann:not-a-real-value-24@a.test\\p",
        ]
        .into_iter()
        .enumerate()
        {
            let request = HttpRequest::new("GET", url, 3 + ix);
            let id = history.append(request, ok(""))?.expect("stored");
            let raw = std::fs::read_to_string(history.directory().join(format!("{id}.json")))?;
            assert!(!raw.contains("not-a-real-value"), "{url}: {raw}");
            let raw_index = std::fs::read_to_string(history.directory().join(INDEX_FILE))?;
            assert!(
                !raw_index.contains("not-a-real-value"),
                "{url}: {raw_index}"
            );
        }
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

    fn typed(content_type: &str, body: &str) -> HttpResponse {
        let headers = vec![("Content-Type".into(), content_type.into())];
        HttpResponse::new(200, "OK", headers, body, 1)
    }

    fn stored_response(history: &HistoryDir, response: HttpResponse) -> Result<HttpResponse> {
        let id = history
            .append(get("https://a.test/big"), response)?
            .expect("stored");
        Ok(history.load(id)?.response)
    }

    #[test]
    fn test_disk_history_drops_a_cut_off_json_body_it_cannot_mask() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        // What a run keeps of a 10 MiB+ response: cut mid-value, no closing brace.
        let body = format!(
            "{{\"access_token\":\"SECRETVAL\",\"pad\":\"{}",
            "x".repeat(MAX_RESPONSE_BYTES)
        );
        let cut = typed("application/json", &body[..MAX_RESPONSE_BYTES]);
        let stored = stored_response(&history, cut.mark_truncated(MAX_RESPONSE_BYTES))?;
        assert!(!stored.body.contains("SECRETVAL"));
        assert_eq!(stored.body, UNMASKABLE_BODY);
        assert!(stored.truncated);
        Ok(())
    }

    #[test]
    fn test_disk_history_drops_a_cut_off_json_body_by_its_shape_without_a_content_type()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let body = format!(
            "[{{\"pad\":\"{}\",\"password\":\"SECRETVAL\"",
            "x".repeat(64)
        );
        let cut = ok(&body).mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, cut)?;
        assert!(!stored.body.contains("SECRETVAL"));
        assert_eq!(stored.body, UNMASKABLE_BODY);
        Ok(())
    }

    #[test]
    fn test_disk_history_keeps_what_it_can_mask_or_that_carries_no_json() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);

        // Not cut: JSON is still masked, even past the size limit, and cut afterwards.
        let big = format!(
            "{{\"access_token\":\"SECRETVAL\",\"pad\":\"{}\"}}",
            "x".repeat(MAX_RESPONSE_BYTES + 10)
        );
        let stored = stored_response(&history, typed("application/json", &big))?;
        assert!(stored.truncated);
        assert!(!stored.body.contains("SECRETVAL"));
        assert!(stored.body.contains(MASKED_VALUE));

        let small = typed(
            "application/json",
            "{\"access_token\":\"SECRETVAL\",\"a\":1}",
        );
        let stored = stored_response(&history, small)?;
        assert!(!stored.body.contains("SECRETVAL"));
        assert!(stored.body.contains("\"a\":1"));

        // Not cut: plain text is stored as is (the documented shortcut).
        let text = typed("text/plain", "token=SECRETVAL and more");
        assert_eq!(
            stored_response(&history, text)?.body,
            "token=SECRETVAL and more"
        );

        // Cut, but not JSON: stored with its truncation note.
        let page = typed("text/html", "<html>no secrets").mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, page)?;
        assert!(stored.body.starts_with("<html>no secrets"));
        assert!(stored.body.contains("[Response truncated"));

        // Cut form body: masked by prefix, so the value of a complete secret pair is hidden.
        let form = typed(
            "application/x-www-form-urlencoded",
            "a=1&password=SECRETVAL&b=2&pa",
        )
        .mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, form)?;
        assert!(!stored.body.contains("SECRETVAL"));
        assert!(stored.body.contains("[Response truncated"));
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
