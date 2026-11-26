use crate::MAX_RESPONSE_BYTES;
use crate::parser::HttpRequest;
use crate::response::{HttpResponse, TRUNCATION_NOTE_PREFIX};
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
/// did not parse), so the stored body stays byte-for-byte what was sent or received. A multipart
/// body it cannot split comes back as a marker instead. The body is taken as complete: for one
/// the run cut off, see [`mask_secret_body_cut`].
fn mask_secret_body(body: &str, content_type: Option<&str>) -> Option<String> {
    mask_secret_body_cut(body, content_type, false)
}

/// [`mask_secret_body`] for a body that may have been cut off by the run (`truncated`): JSON
/// lines then lose the cut line, which comes back as [`UNMASKABLE_LINE`].
fn mask_secret_body_cut(body: &str, content_type: Option<&str>, truncated: bool) -> Option<String> {
    let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
    if content_type.contains("x-www-form-urlencoded") {
        return mask_secret_pairs(body);
    }
    if content_type.starts_with("multipart/") {
        return mask_secret_multipart(body, &content_type);
    }
    // json-seq frames its records on U+001E, not on lines: a record may span several.
    if content_type.contains("json-seq") {
        return mask_json_records(body, truncated, '\u{1e}');
    }
    let lines_type = ["ndjson", "jsonl", "stream+json"]
        .iter()
        .any(|name| content_type.contains(name));
    let text = body.trim_start_matches('\u{feff}');
    // shortcut: plain text, XML and other formats are stored as is; add a format here when one
    // starts carrying credentials in practice.
    if !lines_type && let Some(mut parsed) = parse_json(text) {
        if !mask_parsed(&mut parsed) {
            return None;
        }
        return render_json(&parsed.0, text.contains('\n'));
    }
    if truncated {
        // A cut body never parses whole; only JSON lines can lose just their cut line, any
        // other JSON is left for `append` to drop.
        return lines_type
            .then(|| mask_json_records(body, true, '\n'))
            .flatten();
    }
    // A complete body in a JSON type is one value, several values one after another (one per
    // line, pretty-printed, on a shared line) or lines of which some are not JSON.
    if lines_type || content_type.contains("json") {
        return match mask_json_stream(text) {
            Some(masked) => masked,
            // Not a run of JSON values (a PHP notice before the JSON, a trailing comma, a number
            // out of range): serde cannot mask it, so one that names a secret is not stored. A
            // lines type is cut per record below, or whole when a record opens a block.
            None if !lines_type && has_secret_key(text) => Some(UNPARSEABLE.to_string()),
            None => mask_json_records(body, false, '\n'),
        };
    }
    // Any other type (HTML, plain text, JSONP, none) that carried JSON a parser rejected, as a PHP
    // notice or a stray `0` after it does on WordPress `admin-ajax`, or that holds a secret as
    // `key=value`: not stored if it names a secret key, else masked when it is one line of pairs.
    if has_secret_key(text) {
        return Some(UNPARSEABLE.to_string());
    }
    let line = body.trim_end();
    if looks_like_pairs(line)
        && let Some(masked) = mask_secret_pairs(line)
    {
        return Some(format!(
            "{masked}{}",
            body.get(line.len()..).unwrap_or_default()
        ));
    }
    None
}

/// Whether `line` is one line of `name=value` pairs joined by `&`, not markup or code that has an
/// `=` and an `&` in it.
fn looks_like_pairs(line: &str) -> bool {
    !line.contains(['\n', '<', '{'])
        && line.contains('=')
        && line
            .split('&')
            .filter(|pair| !pair.is_empty())
            .all(|pair| pair.contains('='))
}

/// Whether the brackets of `text` close on its own line, outside its double-quoted strings.
fn brackets_balance(text: &str) -> bool {
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    for c in text.chars() {
        match (in_string, escaped, c) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_string = false,
            (false, _, '"') => in_string = true,
            (false, _, '{' | '[') => depth += 1,
            (false, _, '}' | ']') => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0 && !in_string
}

/// Stored in place of a line of a cut-off JSON lines body that cannot be parsed.
const UNMASKABLE_LINE: &str = "[line not stored: truncated, secrets could not be masked]";

/// Stored in place of a multipart body that cannot be split into parts.
const UNMASKABLE_MULTIPART: &str =
    "[body not stored: multipart boundary not found, secrets could not be masked]";

/// A JSON value that remembers whether an object in it repeated a key. `serde_json::Value`
/// keeps the last value of a repeated key, so `{"token":"s","token":null}` would parse to
/// nothing secret and be stored as written, secret included.
struct Parsed(serde_json::Value, bool);

impl<'de> Deserialize<'de> for Parsed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Parsed;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("any JSON value")
            }

            fn visit_unit<E>(self) -> Result<Parsed, E> {
                Ok(Parsed(serde_json::Value::Null, false))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Parsed, E> {
                Ok(Parsed(value.into(), false))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Parsed, E> {
                Ok(Parsed(value.into(), false))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Parsed, E> {
                Ok(Parsed(value.into(), false))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Parsed, E> {
                Ok(Parsed(value.into(), false))
            }

            fn visit_str<E>(self, value: &str) -> Result<Parsed, E> {
                Ok(Parsed(value.into(), false))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Parsed, A::Error> {
                let (mut items, mut repeated) = (Vec::new(), false);
                while let Some(Parsed(item, item_repeated)) = seq.next_element()? {
                    items.push(item);
                    repeated |= item_repeated;
                }
                Ok(Parsed(items.into(), repeated))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Parsed, A::Error> {
                let (mut object, mut repeated) = (serde_json::Map::new(), false);
                while let Some(key) = map.next_key::<String>()? {
                    let Parsed(value, value_repeated) = map.next_value()?;
                    repeated |= value_repeated || object.insert(key, value).is_some();
                }
                Ok(Parsed(object.into(), repeated))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// `text` as one JSON value, or `None` when it is not exactly one.
fn parse_json(text: &str) -> Option<Parsed> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed = Parsed::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(parsed)
}

/// Masks `parsed` in place; true when what is stored must differ from what was parsed, because a
/// value was masked or a repeated key dropped an earlier value (which may have been a secret).
fn mask_parsed(parsed: &mut Parsed) -> bool {
    mask_secret_json(&mut parsed.0) | parsed.1
}

fn render_json(json: &serde_json::Value, pretty: bool) -> Option<String> {
    if pretty {
        serde_json::to_string_pretty(json).ok()
    } else {
        serde_json::to_string(json).ok()
    }
}

/// Masks every JSON value in `text`, however they are separated: one per line, pretty-printed
/// one after another, several on a line. Only the values that change are rewritten, so the
/// text between them stays as it was. `None` when `text` is not a plain run of JSON values;
/// `Some(None)` when nothing needed masking.
fn mask_json_stream(text: &str) -> Option<Option<String>> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Parsed>();
    let (mut masked, mut cursor, mut changed) = (String::new(), 0, false);
    loop {
        let before = stream.byte_offset();
        let Some(parsed) = stream.next() else { break };
        let mut parsed = parsed.ok()?;
        let end = stream.byte_offset();
        if mask_parsed(&mut parsed) {
            let rest = text.get(before..end)?;
            let start = before + (rest.len() - rest.trim_start().len());
            masked.push_str(text.get(cursor..start)?);
            let pretty = text.get(start..end)?.contains('\n');
            masked.push_str(&render_json(&parsed.0, pretty)?);
            cursor = end;
            changed = true;
        }
    }
    if !changed {
        return Some(None);
    }
    masked.push_str(text.get(cursor..)?);
    Some(Some(masked))
}

/// Masks the JSON records of a body that frames them with `separator`: `\n` for JSON lines (a
/// line that is not JSON is kept), U+001E for `json-seq`. Whitespace, a BOM and the separators
/// around each record are kept; a record written over several lines stays so.
///
/// A `truncated` body ends in a cut record, then the run's own note: only the last record may
/// fail to parse, and is replaced by a marker, since a secret may sit before the cut; the note
/// is kept. Any earlier record that fails (a pretty-printed value cut off, plain text) makes the
/// whole body unmaskable: `None`, so `append` stores [`UNMASKABLE_BODY`]. That is simpler than a
/// marker per record and as safe.
fn mask_json_records(body: &str, truncated: bool, separator: char) -> Option<String> {
    let note_start = truncated
        .then(|| body.rfind(TRUNCATION_NOTE_PREFIX))
        .flatten()
        .filter(|&ix| {
            body.get(..ix)
                .is_some_and(|before| before.is_empty() || before.ends_with('\n'))
        });
    let (data, note) = body.split_at(note_start.unwrap_or(body.len()));
    let framing = |c: char| c.is_whitespace() || c == '\u{feff}' || c == '\u{1e}';
    let records = data.split_inclusive(separator).collect::<Vec<_>>();
    let last = records
        .iter()
        .rposition(|record| !record.trim_matches(framing).is_empty());
    let mut changed = false;
    let mut out = String::with_capacity(body.len());
    for (ix, record) in records.iter().enumerate() {
        let (lead, rest) = record.split_at(record.len() - record.trim_start_matches(framing).len());
        let (content, trail) = rest.split_at(rest.trim_end_matches(framing).len());
        if content.is_empty() {
            out.push_str(record);
            continue;
        }
        let masked = match parse_json(content) {
            Some(mut parsed) => mask_parsed(&mut parsed).then(|| {
                // A value always serializes; were it not, the marker keeps the secret out.
                render_json(&parsed.0, content.contains('\n'))
                    .unwrap_or_else(|| UNMASKABLE_LINE.to_string())
            }),
            // Several values in one record (json-seq without U+001E) are masked one by one.
            None => match mask_json_stream(content) {
                Some(stream) => stream,
                None if !truncated && !has_secret_key(content) => None,
                // A key whose value runs on (a block left open, `"token":` ending the line): the
                // lines after it are its value, so no line can be cut alone. A json-seq record
                // already holds its lines.
                None if !truncated
                    && separator == '\n'
                    && (!brackets_balance(content) || content.trim_end().ends_with(':')) =>
                {
                    return Some(UNPARSEABLE.to_string());
                }
                None if !truncated => Some(UNPARSEABLE.to_string()),
                None if Some(ix) == last => Some(UNMASKABLE_LINE.to_string()),
                None => return None,
            },
        };
        match masked {
            Some(masked) => {
                changed = true;
                out.push_str(&format!("{lead}{masked}{trail}"));
            }
            None => out.push_str(record),
        }
    }
    out.push_str(note);
    changed.then_some(out)
}

/// Stored in place of a JSON-typed body, or one of its records, that does not parse and names a
/// secret: it cannot be masked, and it is not known to be cut.
const UNPARSEABLE: &str = "[not stored: secrets could not be masked]";

/// Longest key `has_secret_key` decodes; a longer one is no credential name, and the cap keeps
/// the scan linear on a body of nothing but quotes.
const MAX_KEY: usize = 256;

/// Whether `text` holds a `name:` with a secret `name` (quoted with `"` or `'`, or bare), found by
/// scanning rather than parsing, so it works on text no JSON parser accepts. Every quote is tried
/// as the start of a key (a stray quote cannot throw the later ones off); a false hit only costs a
/// marker.
// shortcut: a key written with a surrogate pair escape is read as two replacement characters,
// so an exotic name spelled that way is not caught; a comment between a key and its colon hides it.
fn has_secret_key(text: &str) -> bool {
    let mut name = String::new();
    let quoted = text
        .char_indices()
        .filter(|(_, c)| matches!(c, '"' | '\''))
        .any(|(ix, quote)| {
            let Some(rest) = text.get(ix + quote.len_utf8()..) else {
                return false;
            };
            let mut chars = rest.chars();
            name.clear();
            loop {
                match chars.next() {
                    Some(c) if c == quote => break,
                    Some('\\') => match chars.next() {
                        Some('u') => {
                            let hex = chars.by_ref().take(4).collect::<String>();
                            let decoded =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                            name.push(decoded.unwrap_or('\u{fffd}'));
                        }
                        Some('n') => name.push('\n'),
                        Some('t') => name.push('\t'),
                        Some('r') => name.push('\r'),
                        Some(other) => name.push(other),
                        None => return false,
                    },
                    // A key never spans lines.
                    Some('\n') | None => return false,
                    Some(c) => name.push(c),
                }
                if name.len() > MAX_KEY {
                    return false;
                }
            }
            chars.as_str().trim_start().starts_with(':') && is_secret_name(&name)
        });
    if quoted {
        return true;
    }
    // Bare keys (`{token: "s"}`): a run of name characters after `{` or `,`, then a colon.
    // `line_blank`: nothing but spaces, tabs or `-` since the start of this line (kept as a flag,
    // not searched back for, to stay linear on a body of one long line).
    let (mut start, mut line_blank) = (None, true);
    for (ix, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let in_name = c.is_alphanumeric() || matches!(c, '_' | '$' | '-');
        let starts_line = line_blank;
        line_blank = c == '\n' || (line_blank && matches!(c, ' ' | '\t' | '-'));
        match (in_name, start) {
            (true, None) => start = Some((ix, starts_line)),
            (false, Some((from, from_line_blank))) => {
                start = None;
                if let Some(word) = text.get(from..ix)
                    && word.len() <= MAX_KEY
                    && text
                        .get(ix..)
                        .is_some_and(|rest| rest.trim_start().starts_with(':'))
                    // A key follows `{` or `,`, or starts its line (a header, YAML, `- key:`);
                    // `a:hover`, `<b>Password:</b>`, `f(key:` do not.
                    && text.get(..from).is_some_and(|before| {
                        from_line_blank || before.trim_end().ends_with(['{', ','])
                    })
                    && is_secret_name(word)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Stored in place of a cut-off JSON body: it cannot be parsed, so its secrets cannot be masked.
const UNMASKABLE_BODY: &str = "[body not stored: truncated, secrets could not be masked]";

/// Whether `body` may be JSON, by content type or, failing that, by its first character. A form
/// body is masked by prefix, so a cut leaves no secret value unmasked; a multipart one is masked
/// by part, or replaced by a marker when it cannot be split.
fn may_be_json(body: &str, content_type: Option<&str>) -> bool {
    let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
    if content_type.contains("x-www-form-urlencoded") || content_type.starts_with("multipart/") {
        return false;
    }
    content_type.contains("json")
        || body
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with(['{', '['])
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
/// `< path` line standing for a file) are left alone. A body with no delimiter to split on
/// comes back as [`UNMASKABLE_MULTIPART`]; an empty one is `None`.
fn mask_secret_multipart(body: &str, content_type: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    let declared = content_type
        .split(';')
        .find_map(|parameter| parameter.trim().strip_prefix("boundary="))
        .map(|boundary| boundary.trim_matches('"').to_string())
        // An empty boundary would make `--` the delimiter and shred the body.
        .filter(|boundary| !boundary.is_empty());
    // The declared boundary, else the body's first line when it is a delimiter (a client may
    // declare one and send another). The content type was lowercased, so the body is searched
    // lowercased too, to find the boundary as written in the body.
    let first_line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.trim_end().strip_prefix("--"))
        .filter(|boundary| !boundary.is_empty())
        .map(|boundary| boundary.to_ascii_lowercase());
    let lowered = body.to_ascii_lowercase();
    let found = declared.into_iter().chain(first_line).find_map(|boundary| {
        let start = lowered.find(&format!("--{boundary}"))?;
        body.get(start..start + boundary.len() + 2)
    });
    // No delimiter: the body cannot be split into fields, so it is not stored.
    let Some(delimiter) = found else {
        return Some(UNMASKABLE_MULTIPART.to_string());
    };
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
        // parse (it ends mid-value, then the truncation note): JSON lines lose their cut line,
        // other JSON that may hold secrets is not stored at all.
        match mask_secret_body_cut(
            &response.body,
            content_type(&response.headers),
            response.truncated,
        ) {
            Some(body) => response.body = body,
            None if response.truncated
                && (may_be_json(&response.body, content_type(&response.headers))
                    || has_secret_key(&response.body)) =>
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

    #[test]
    fn test_mask_secret_ndjson_lines() {
        for content_type in [
            "application/x-ndjson",
            "application/jsonl",
            "application/x-jsonlines",
            "application/jsonlines",
            "text/jsonl",
            "application/x-ndjson; charset=utf-8",
        ] {
            let body = "{\"a\":1}\n{\"password\":\"SECRETVAL\",\"b\":2}\n";
            let masked = mask_secret_body(body, Some(content_type)).unwrap();
            assert_eq!(
                masked, "{\"a\":1}\n{\"password\":\"***\",\"b\":2}\n",
                "{content_type}"
            );
        }
        let ndjson = Some("application/x-ndjson");
        // Secret on the first line, CRLF separators and no trailing newline survive.
        let body = "{\"token\":\"SECRETVAL\"}\r\n\r\n[1,2]\r\nnot json\r\n{\"a\":1}";
        let masked = mask_secret_body(body, ndjson).unwrap();
        assert_eq!(
            masked,
            "{\"token\":\"***\"}\r\n\r\n[1,2]\r\nnot json\r\n{\"a\":1}"
        );
        // Nested secret, array lines.
        let masked = mask_secret_body("[{\"x\":{\"secret\":\"SECRETVAL\"}}]\n", ndjson).unwrap();
        assert!(
            !masked.contains("SECRETVAL") && masked.ends_with("]\n"),
            "{masked}"
        );
        // Nothing to mask: unchanged, byte for byte (None), complete or with a stray line.
        for clean in ["{\"a\":1}\n{\"b\":2}\n", "{\"a\":1}\r\nplain\r\n", ""] {
            assert_eq!(mask_secret_body(clean, ndjson), None);
        }
    }

    #[test]
    fn test_mask_secret_ndjson_cut_line_is_never_kept() {
        let ndjson = Some("application/x-ndjson");
        let body = "{\"password\":\"SECRETONE\"}\n{\"pad\":\"x\",\"access_token\":\"SECRETTWO";
        // Not flagged as cut, a line that is not JSON is kept as is (it may be plain text).
        let text = "{\"password\":\"SECRETONE\"}\nplain text\n";
        let kept = mask_secret_body(text, ndjson).unwrap();
        assert_eq!(kept, "{\"password\":\"***\"}\nplain text\n");
        let masked = mask_secret_body_cut(body, ndjson, true).unwrap();
        assert!(!masked.contains("SECRET"), "{masked}");
        assert_eq!(
            masked,
            format!("{{\"password\":\"***\"}}\n{UNMASKABLE_LINE}")
        );
        // Cut line only: still replaced.
        let masked = mask_secret_body_cut("{\"a\":1}\n{\"pass", ndjson, true).unwrap();
        assert_eq!(masked, format!("{{\"a\":1}}\n{UNMASKABLE_LINE}"));
        let masked = mask_secret_body_cut("{\"a\":1}\nplain", ndjson, true).unwrap();
        assert_eq!(masked, format!("{{\"a\":1}}\n{UNMASKABLE_LINE}"));
        // Earlier lines that fail mean the body is not JSON lines: none of it is stored.
        assert_eq!(
            mask_secret_body_cut("plain\n{\"a\":1}\n{\"pa", ndjson, true),
            None
        );
    }

    #[test]
    fn test_disk_history_masks_ndjson_responses_and_cut_lines() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let complete = typed(
            "application/x-ndjson",
            "{\"a\":1}\n{\"client_secret\":\"SECRETONE\"}\n",
        );
        let stored = stored_response(&history, complete)?;
        assert!(!stored.body.contains("SECRETONE"), "{}", stored.body);
        assert!(stored.body.starts_with("{\"a\":1}\n"));

        // What a run keeps of a cut stream: complete lines, a cut line, then the note.
        let cut = typed(
            "application/x-ndjson",
            "{\"password\":\"SECRETONE\"}\n{\"pad\":\"x\",\"token\":\"SECRETTWO",
        )
        .mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, cut)?;
        assert!(!stored.body.contains("SECRET"), "{}", stored.body);
        assert!(stored.truncated);

        // Request bodies go through the same masking.
        let mut request = get("https://a.test/req");
        request.headers = vec![("Content-Type".into(), "application/x-ndjson".into())];
        request.body = Some("{\"a\":1}\n{\"password\":\"SECRETREQ\"}\n".into());
        let id = history.append(request, ok("fine"))?.expect("stored");
        let body = history.load(id)?.request.body.unwrap_or_default();
        assert!(!body.contains("SECRETREQ"), "{body}");
        Ok(())
    }

    #[test]
    fn test_mask_secret_multipart_without_a_boundary() {
        let part = |disposition: &str, content: &str| {
            format!("--XyZ\r\nContent-Disposition: form-data; {disposition}\r\n\r\n{content}\r\n")
        };
        let body = [
            part("name=\"title\"", "hello"),
            part("name=\"password\"", "SECRETVAL"),
            "--XyZ--\r\n".to_string(),
        ]
        .concat();
        // The delimiter is the first line, with or without blank lines before it.
        for content_type in ["multipart/form-data", "multipart/form-data; charset=utf-8"] {
            for body in [body.clone(), format!("\r\n{body}")] {
                let masked = mask_secret_body(&body, Some(content_type)).unwrap();
                assert!(!masked.contains("SECRETVAL"), "{masked}");
                assert!(masked.contains("hello") && masked.contains("\r\n***\r\n--XyZ"));
            }
        }
        // Cut mid-value: the part head is complete, so the secret is masked all the same.
        let cut = &body[..body.find("SECRETVAL").unwrap() + 3];
        let masked = mask_secret_body(cut, Some("multipart/form-data")).unwrap();
        assert!(!masked.contains("SECRET"), "{masked}");
        // Nothing to mask stays as is.
        let plain = [part("name=\"title\"", "hello"), "--XyZ--\r\n".to_string()].concat();
        assert_eq!(mask_secret_body(&plain, Some("multipart/form-data")), None);
        // No delimiter to split on: not stored. An empty body holds nothing.
        let unknown = "name=password\r\nSECRETVAL";
        assert_eq!(
            mask_secret_body(unknown, Some("multipart/form-data")).as_deref(),
            Some(UNMASKABLE_MULTIPART)
        );
        assert_eq!(mask_secret_body("", Some("multipart/form-data")), None);
        assert_eq!(
            mask_secret_body("--\r\nSECRETVAL", Some("multipart/form-data")).as_deref(),
            Some(UNMASKABLE_MULTIPART)
        );
    }

    #[test]
    fn test_disk_history_does_not_store_a_multipart_body_without_a_boundary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let body = "no delimiter\r\nname=\"password\"\r\n\r\nSECRETVAL";
        let stored = stored_response(&history, typed("multipart/mixed", body))?;
        assert_eq!(stored.body, UNMASKABLE_MULTIPART);
        let cut = typed("multipart/mixed", body).mark_truncated(MAX_RESPONSE_BYTES);
        assert_eq!(stored_response(&history, cut)?.body, UNMASKABLE_MULTIPART);
        Ok(())
    }

    #[test]
    fn test_mask_secret_ndjson_type_with_one_json_value_over_several_lines() {
        let body = "{\n  \"token\": \"SECRETVAL\",\n  \"a\": 1\n}\n";
        for content_type in [
            "application/x-ndjson",
            "application/jsonl",
            "application/stream+json",
        ] {
            let masked = mask_secret_body(body, Some(content_type)).unwrap();
            assert!(!masked.contains("SECRETVAL"), "{content_type}: {masked}");
            assert!(masked.contains("\"a\": 1"), "{masked}");
        }
    }

    #[test]
    fn test_disk_history_drops_a_cut_pretty_value_under_an_ndjson_type() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let cut = typed(
            "application/x-ndjson",
            "{\n  \"pad\": 1,\n  \"access_token\": \"SECRETVAL\",\n  \"more\": \"x",
        )
        .mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, cut)?;
        assert!(!stored.body.contains("SECRETVAL"), "{}", stored.body);
        assert_eq!(stored.body, UNMASKABLE_BODY);

        // A cut stream keeps its masked lines and the run's note; only the cut line goes.
        let stream = typed(
            "application/x-ndjson",
            "{\"password\":\"SECRETONE\"}\n{\"a\":1}\n{\"token\":\"SECRETTWO",
        )
        .mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, stream)?;
        assert_eq!(
            stored.body,
            format!(
                "{{\"password\":\"***\"}}\n{{\"a\":1}}\n{UNMASKABLE_LINE}\n\n[Response truncated: only the first 10 MiB are shown]"
            )
        );
        Ok(())
    }

    #[test]
    fn test_mask_secret_json_lines_served_as_json_or_with_marks() {
        let lines = "{\"type\":\"ADDED\"}\n{\"password\":\"SECRETVAL\"}\n";
        // Complete JSON lines under a plain JSON type (watch streams, event feeds).
        let masked = mask_secret_body(lines, Some("application/json")).unwrap();
        assert!(!masked.contains("SECRETVAL") && masked.starts_with("{\"type\":\"ADDED\"}\n"));
        // Cut under that type: not parsable, left for `append` to drop whole.
        assert_eq!(
            mask_secret_body_cut(lines, Some("application/json"), true),
            None
        );
        // json-seq: the record separator stays.
        let seq = "\u{1e}{\"password\":\"SECRETVAL\"}\n\u{1e}{\"a\":1}\n";
        let masked = mask_secret_body(seq, Some("application/json-seq")).unwrap();
        assert_eq!(masked, "\u{1e}{\"password\":\"***\"}\n\u{1e}{\"a\":1}\n");
        // A byte order mark in front, on the whole-body and on the per-line path.
        let bom = "\u{feff}{\"token\":\"SECRETVAL\"}\n{\"a\":1}\n";
        for content_type in ["application/x-ndjson", "application/json"] {
            let masked = mask_secret_body(bom, Some(content_type)).unwrap();
            assert!(!masked.contains("SECRETVAL"), "{content_type}: {masked}");
        }
        let masked = mask_secret_body(
            "\u{feff}{\"token\":\"SECRETVAL\"}",
            Some("application/json"),
        );
        assert_eq!(masked.as_deref(), Some("{\"token\":\"***\"}"));
        // Cut under a stream type: its cut line goes, as for NDJSON.
        let cut = "{\"a\":1}\n{\"token\":\"SECR";
        let masked = mask_secret_body_cut(cut, Some("application/stream+json"), true).unwrap();
        assert_eq!(masked, format!("{{\"a\":1}}\n{UNMASKABLE_LINE}"));
        let cut = "\u{1e}{\"a\":1}\n\u{1e}{\"token\":\"SECR";
        let masked = mask_secret_body_cut(cut, Some("application/json-seq"), true).unwrap();
        assert_eq!(
            masked,
            format!("\u{1e}{{\"a\":1}}\n\u{1e}{UNMASKABLE_LINE}")
        );
    }

    #[test]
    fn test_mask_secret_multipart_boundary_not_in_the_body() {
        let body = "--WebkitBoundary\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n--WebkitBoundary\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nSECRETVAL\r\n--WebkitBoundary--\r\n";
        let content_type = Some("multipart/form-data; boundary=WebAppBoundary");
        let masked = mask_secret_body(body, content_type).unwrap();
        assert!(!masked.contains("SECRETVAL"), "{masked}");
        assert!(masked.contains("hello") && masked.contains("\r\n***\r\n--WebkitBoundary"));
        let masked = mask_secret_body("no delimiter\r\nSECRETVAL", content_type);
        assert_eq!(masked.as_deref(), Some(UNMASKABLE_MULTIPART));
    }

    #[test]
    fn test_disk_history_masks_a_secret_key_repeated_with_null() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let body = "{\"token\":\"SECRETVAL\",\"token\":null}";
        for content_type in ["application/json", "application/x-ndjson"] {
            let stored = stored_response(&history, typed(content_type, body))?;
            assert!(
                !stored.body.contains("SECRETVAL"),
                "{content_type}: {}",
                stored.body
            );
            let two = format!("{{\"a\":1}}\n{body}\n");
            let stored = stored_response(&history, typed(content_type, &two))?;
            assert!(
                !stored.body.contains("SECRETVAL"),
                "{content_type}: {}",
                stored.body
            );
        }
        Ok(())
    }

    #[test]
    fn test_mask_secret_json_that_does_not_parse_fails_closed() {
        let json = Some("application/json");
        let deep = format!("{}{}", "[".repeat(129), "]".repeat(129));
        let bodies = [
            // Number out of range.
            "{\"token\":\"SECRETVAL\",\"n\":1e999}".to_string(),
            // Past serde's recursion limit.
            format!("{{\"token\":\"SECRETVAL\",\"x\":{deep}}}"),
            // Lone surrogate.
            "{\"access_token\":\"SECRETVAL\",\"name\":\"\\ud83d\"}".to_string(),
            // PHP notice before pretty-printed JSON.
            "<br />\n<b>Notice</b>: x<br />\n{\n    \"token\": \"SECRETVAL\"\n}\n".to_string(),
            // Trailing comma.
            "{\"password\":\"SECRETVAL\",}".to_string(),
            // Key spelled with an escape.
            "{\"tok\\u0065n\":\"SECRETVAL\",}".to_string(),
        ];
        for body in &bodies {
            let masked = mask_secret_body(body, json);
            assert_eq!(masked.as_deref(), Some(UNPARSEABLE), "{body}");
        }
        // The same record under a lines type is replaced alone, its neighbours stay.
        let lines = format!("{{\"a\":1}}\n{}\n{{\"b\":2}}\n", bodies[4]);
        let masked = mask_secret_body(&lines, Some("application/x-ndjson")).unwrap();
        assert_eq!(masked, format!("{{\"a\":1}}\n{UNPARSEABLE}\n{{\"b\":2}}\n"));
        // json-seq record with a secret that does not parse.
        let seq = format!("\u{1e}{}\n", bodies[4]);
        let masked = mask_secret_body(&seq, Some("application/json-seq")).unwrap();
        assert_eq!(masked, format!("\u{1e}{UNPARSEABLE}\n"));
        // No secret key in it: stored as it was.
        for plain in [
            "<br />\n<b>Notice</b>: x<br />\n{\"a\":1,}\n",
            "{\"name\":\"password\",}",
            "not json at all",
            "{\"a\":\"say \\\"token\\\" now\",}",
        ] {
            assert_eq!(mask_secret_body(plain, json), None, "{plain}");
            assert_eq!(mask_secret_body(plain, Some("application/x-ndjson")), None);
        }
    }

    #[test]
    fn test_disk_history_does_not_store_json_that_does_not_parse_and_names_a_secret() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let stored = stored_response(
            &history,
            typed("application/json", "{\"token\":\"SECRETVAL\",\"n\":1e999}"),
        )?;
        assert_eq!(stored.body, UNPARSEABLE);
        let stored = stored_response(&history, typed("application/json", "{\"a\":1,}"))?;
        assert_eq!(stored.body, "{\"a\":1,}");
        Ok(())
    }

    #[test]
    fn test_mask_secret_json_seq_without_record_separators() {
        let seq = Some("application/json-seq");
        let masked = mask_secret_body("{\"a\":1}\n{\"token\":\"SECRETVAL\"}\n", seq).unwrap();
        assert_eq!(masked, "{\"a\":1}\n{\"token\":\"***\"}\n");
        assert_eq!(mask_secret_body("{\"a\":1}\n{\"b\":2}\n", seq), None);
    }

    #[test]
    fn test_has_secret_key_is_linear_on_a_body_of_escaped_quotes() {
        let body = format!("{{\"html\":\"{}\",}}", "\\\"".repeat(100_000));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sender
                .send(mask_secret_body(&body, Some("application/json")))
                .ok();
        });
        let masked = receiver.recv_timeout(std::time::Duration::from_secs(20));
        assert_eq!(masked.expect("scan did not finish"), None);
    }

    #[test]
    fn test_has_secret_key_is_linear_on_one_long_line() {
        let bodies = [
            "a:".repeat(1 << 20),
            "\n".repeat(2 << 20),
            format!("{}{}", " ".repeat(500_000), "a:".repeat(250_000)),
        ];
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let hits = bodies
                .iter()
                .map(|body| has_secret_key(body))
                .collect::<Vec<_>>();
            sender.send(hits).ok();
        });
        let hits = receiver.recv_timeout(std::time::Duration::from_secs(20));
        assert_eq!(hits.expect("scan did not finish"), [false, false, false]);
    }

    #[test]
    fn test_mask_secret_lines_with_a_secret_that_opens_a_block_are_not_stored() {
        let ndjson = Some("application/x-ndjson");
        for body in [
            "{\"a\":1}\n{\n  \"token\": {\n    \"value\": \"SECRETVAL\"\n  },\n}\n",
            "{\"a\":1}\n{\n  \"secrets\": [\n    \"SECRETVAL\",\n    \"S2\"\n  ],\n}\n",
            "{\"a\":1}\n{\n  \"token\":\n    \"SECRETVAL\",\n}\n",
        ] {
            let masked = mask_secret_body(body, ndjson);
            assert_eq!(masked.as_deref(), Some(UNPARSEABLE), "{body}");
        }
        // A json-seq record spans its lines, so the record is replaced alone.
        let seq = "\u{1e}{\"a\":1}\n\u{1e}{\n  \"token\": [\n    \"SECRETVAL\"\n  ],\n}\n";
        assert_eq!(
            mask_secret_body(seq, Some("application/json-seq")),
            Some(format!("\u{1e}{{\"a\":1}}\n\u{1e}{UNPARSEABLE}\n"))
        );
    }

    #[test]
    fn test_has_secret_key_reads_single_quoted_and_bare_keys() {
        let json = Some("application/json");
        for body in [
            "{'token': 'SECRETVAL'}",
            "{token: \"SECRETVAL\"}",
            "{ api_key : 'S' ,}",
        ] {
            assert_eq!(
                mask_secret_body(body, json).as_deref(),
                Some(UNPARSEABLE),
                "{body}"
            );
        }
        for body in ["{'a': 'b'}", "{name: \"token\"}", "{'token'}"] {
            assert_eq!(mask_secret_body(body, json), None, "{body}");
        }
        // A key over the cap is no credential name and is not decoded.
        let long = format!("{{\"{}token\": 1,}}", "a_".repeat(200));
        assert_eq!(mask_secret_body(&long, json), None);
    }

    #[test]
    fn test_mask_secret_body_of_other_types_that_carries_a_secret_key() {
        for (content_type, body) in [
            ("text/html", "{\"access_token\":\"SECRETVAL\"}0"),
            (
                "text/html",
                "<br />\n<b>Warning</b>: x<br />\n{\"token\":\"SECRETVAL\"}",
            ),
            (
                "application/javascript",
                "/**/cb({\"token\":\"SECRETVAL\"})",
            ),
        ] {
            let masked = mask_secret_body(body, Some(content_type));
            assert_eq!(
                masked.as_deref(),
                Some(UNPARSEABLE),
                "{content_type}: {body}"
            );
        }
        // No content type at all.
        let masked = mask_secret_body("{\"token\":\"SECRETVAL\"}0", None);
        assert_eq!(masked.as_deref(), Some(UNPARSEABLE));
        // An ordinary page, and one line that is not pairs, stay as they were.
        let page = "<html><body class=\"main\">\n<p>It's a \"page\": a=b</p></body></html>\n";
        assert_eq!(mask_secret_body(page, Some("text/html")), None);
        assert_eq!(mask_secret_body("a=b&c=d", Some("text/plain")), None);
    }

    #[test]
    fn test_mask_secret_body_of_plain_text_pairs() {
        let masked = mask_secret_body(
            "oauth_token=X&oauth_token_secret=SECRETVAL&b=2",
            Some("text/plain"),
        );
        assert_eq!(
            masked.as_deref(),
            Some("oauth_token=***&oauth_token_secret=***&b=2")
        );
    }

    #[test]
    fn test_disk_history_drops_a_cut_page_that_names_a_secret() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let page = typed(
            "text/html",
            "<br />\n<b>Warning</b>: x<br />\n{\"token\":\"SECRETVAL\",\"pad\":\"x",
        )
        .mark_truncated(MAX_RESPONSE_BYTES);
        assert_eq!(stored_response(&history, page)?.body, UNMASKABLE_BODY);
        // A cut plain-text body with a header-style secret.
        let text = typed("text/plain", "Authorization: Bearer SECRETVAL\nmore")
            .mark_truncated(MAX_RESPONSE_BYTES);
        assert_eq!(stored_response(&history, text)?.body, UNMASKABLE_BODY);
        Ok(())
    }

    #[test]
    fn test_mask_secret_body_of_other_types_names_colon_style_secrets() {
        for (content_type, body) in [
            (Some("text/plain"), "Authorization: Bearer SECRETVAL"),
            (None, "token: abc123"),
            (
                Some("application/yaml"),
                "users:\n- user:\n    token: SECRETVAL\n",
            ),
            (Some("application/json"), "token: SECRETVAL"),
        ] {
            let masked = mask_secret_body(body, content_type);
            assert_eq!(masked.as_deref(), Some(UNPARSEABLE), "{body}");
        }
        // Per record in a lines type.
        let masked = mask_secret_body(
            "{\"a\":1}\ntoken: SECRETVAL\n",
            Some("application/x-ndjson"),
        );
        let expected = format!("{{\"a\":1}}\n{UNPARSEABLE}\n");
        assert_eq!(masked.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn test_mask_secret_body_of_pairs_with_empty_segments_or_semicolons() {
        let plain = Some("text/plain");
        for (body, expected) in [
            (
                "access_token=SECRETVAL&scope=read;write",
                "access_token=***&scope=read;write",
            ),
            (
                "oauth_token=X&oauth_token_secret=SECRETVAL&",
                "oauth_token=***&oauth_token_secret=***&",
            ),
            ("token=SECRETVAL&&a=1", "token=***&&a=1"),
        ] {
            assert_eq!(
                mask_secret_body(body, plain).as_deref(),
                Some(expected),
                "{body}"
            );
        }
        // Still code, not pairs: a segment without `=` rejects the line.
        assert_eq!(mask_secret_body("var password=e.value&&f()", plain), None);
    }

    #[test]
    fn test_mask_secret_body_of_other_types_keeps_json_secrets_out_before_any_equals() {
        let html = Some("text/html");
        for body in [
            "{\"api_key\":\"SECRETVAL\",\"redirect\":\"https://s/?page=x\"}0",
            "{\"password\":\"hunter2\",\"url\":\"/x?a=1\"}0",
        ] {
            let masked = mask_secret_body(body, html);
            assert_eq!(masked.as_deref(), Some(UNPARSEABLE), "{body}");
        }
    }

    #[test]
    fn test_mask_secret_body_of_other_types_leaves_code_and_markup_alone() {
        for (content_type, body) in [
            ("text/javascript", "var password=e.value&&f()"),
            (
                "text/html",
                "<p>Enter your password</p><a href=\"/x?a=1\">x</a>",
            ),
            ("text/html", "<label>Password:</label>"),
            (
                "text/html",
                "<style>.cookie-notice:hover{color:red}</style>",
            ),
            ("text/html", "<auth:token>x</auth:token>"),
            ("text/javascript", "f(key:1)"),
        ] {
            assert_eq!(mask_secret_body(body, Some(content_type)), None, "{body}");
        }
        // Real pairs still are masked, a trailing newline kept.
        let masked = mask_secret_body("oauth_token=X&oauth_token_secret=S\n", Some("text/plain"));
        assert_eq!(
            masked.as_deref(),
            Some("oauth_token=***&oauth_token_secret=***\n")
        );
    }

    #[test]
    fn test_mask_secret_lines_with_a_secret_in_an_open_block_are_not_stored() {
        let ndjson = Some("application/x-ndjson");
        let body =
            "{\"a\":1}\n{\"credentials\": {\"user\": \"u\",\n  \"value\": \"SECRETVAL\"},}\n";
        assert_eq!(mask_secret_body(body, ndjson).as_deref(), Some(UNPARSEABLE));
        // A record whose brackets close on its line is replaced alone.
        let body = "{\"a\":1}\n{\"secret\":\"SECRETVAL\",}\n{\"b\":[1,2]}\n";
        assert_eq!(
            mask_secret_body(body, ndjson),
            Some(format!("{{\"a\":1}}\n{UNPARSEABLE}\n{{\"b\":[1,2]}}\n"))
        );
    }

    #[test]
    fn test_mask_secret_json_seq_records_over_several_lines() {
        let seq = Some("application/json-seq");
        let body = "\u{1e}{\n  \"password\": \"SECRETVAL\"\n}\n";
        let masked = mask_secret_body(body, seq).unwrap();
        assert_eq!(masked, "\u{1e}{\n  \"password\": \"***\"\n}\n");
        // A mixed body: compact and pretty records, one clean, framing kept as it was.
        let mixed = "\u{1e}{\"a\":1}\n\u{1e}{\n  \"token\": \"SECRETVAL\"\n}\n\u{1e}[2]\n\u{1e}{\"secret\":\"SECRETTWO\"}\n";
        let masked = mask_secret_body(mixed, seq).unwrap();
        assert!(!masked.contains("SECRET"), "{masked}");
        assert!(
            masked.starts_with("\u{1e}{\"a\":1}\n\u{1e}{\n  \"token\": \"***\"\n}\n\u{1e}[2]\n")
        );
        let clean = "\u{1e}{\n  \"a\": 1\n}\n\u{1e}[2]\n";
        assert_eq!(mask_secret_body(clean, seq), None);
        // Cut: the last record goes; the run's note stays.
        let cut = "\u{1e}{\"a\":1}\n\u{1e}{\n  \"token\": \"SECR\n\n[Response truncated: only the first 10 MiB are shown]";
        let masked = mask_secret_body_cut(cut, seq, true).unwrap();
        assert_eq!(
            masked,
            format!(
                "\u{1e}{{\"a\":1}}\n\u{1e}{UNMASKABLE_LINE}\n\n[Response truncated: only the first 10 MiB are shown]"
            )
        );
    }

    #[test]
    fn test_mask_secret_json_values_in_any_layout() {
        let layouts = [
            "{\"a\":1}\n{\n  \"token\": \"SECRETVAL\"\n}\n",
            "{\"a\":1} {\"token\":\"SECRETVAL\"}",
            "{\n  \"a\": 1\n}\n{\n  \"token\": \"SECRETVAL\"\n}\n",
            "{\"a\":1}\r\n{\"token\":\"SECRETVAL\"}\r\n",
        ];
        for content_type in [
            "application/x-ndjson",
            "application/jsonl",
            "application/json",
            "application/stream+json",
        ] {
            for body in layouts {
                let masked = mask_secret_body(body, Some(content_type)).unwrap();
                assert!(
                    !masked.contains("SECRETVAL"),
                    "{content_type} {body:?}: {masked}"
                );
                assert!(masked.contains("\"a\""), "{masked}");
            }
        }
        // Values that need no masking are not rewritten; the rest of the body stays as written.
        let masked = mask_secret_body(layouts[3], Some("application/jsonl")).unwrap();
        assert_eq!(masked, "{\"a\":1}\r\n{\"token\":\"***\"}\r\n");
        assert_eq!(
            mask_secret_body("{\"a\":1} {\"b\":2}\n", Some("application/json")),
            None
        );
    }

    #[test]
    fn test_mask_secret_json_repeated_keys_never_keep_the_earlier_value() {
        for content_type in [
            "application/json",
            "application/x-ndjson",
            "application/json-seq",
        ] {
            for body in [
                "{\"token\":\"SECRETVAL\",\"token\":null}",
                "{\"a\":{\"token\":\"SECRETVAL\"},\"a\":1}",
                "[{\"x\":1,\"secret\":\"SECRETVAL\",\"secret\":true}]",
            ] {
                let masked = mask_secret_body(body, Some(content_type)).unwrap();
                assert!(
                    !masked.contains("SECRETVAL"),
                    "{content_type} {body}: {masked}"
                );
            }
        }
        // Distinct keys are untouched.
        assert_eq!(
            mask_secret_body("{\"a\":1,\"b\":null}", Some("application/json")),
            None
        );
    }

    #[test]
    fn test_disk_history_drops_a_cut_bom_json_body_without_a_content_type() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let history = HistoryDir::new(temp.path().join("h"), 10, u64::MAX);
        let cut = ok("\u{feff}{\"access_token\":\"SECRETVAL\",\"pad\":\"x")
            .mark_truncated(MAX_RESPONSE_BYTES);
        let stored = stored_response(&history, cut)?;
        assert_eq!(stored.body, UNMASKABLE_BODY);
        Ok(())
    }

    #[test]
    fn test_mask_secret_multipart_with_an_empty_declared_boundary() {
        let body =
            "--X\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nab--cd\r\n--X--\r\n";
        for content_type in [
            "multipart/form-data; boundary=",
            "multipart/form-data; boundary=\"\"",
        ] {
            let masked = mask_secret_body(body, Some(content_type)).unwrap();
            assert!(
                !masked.contains("ab--cd") && masked.contains("\r\n***\r\n--X--"),
                "{masked}"
            );
        }
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

        // Not cut: plain text is stored as is (the documented shortcut) unless it is pairs.
        let text = typed("text/plain", "just words, no secrets");
        assert_eq!(
            stored_response(&history, text)?.body,
            "just words, no secrets"
        );
        let text = typed("text/plain", "token=SECRETVAL and more");
        assert_eq!(stored_response(&history, text)?.body, "token=***");

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
