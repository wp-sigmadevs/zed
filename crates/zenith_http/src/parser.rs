use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub line_number: usize,
}

impl HttpRequest {
    pub fn new(method: &str, url: &str, line_number: usize) -> Self {
        Self {
            name: None,
            method: method.to_uppercase(),
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
            line_number,
        }
    }

    /// A shell command that sends this request. Every piece that comes from the file (method,
    /// headers, body, URL) is single-quoted, so `$(...)`, backticks, quotes, `;` and newlines
    /// in them stay data.
    pub fn to_curl(&self) -> String {
        let mut parts = vec![
            "curl".to_string(),
            "-X".to_string(),
            shell_quote(&self.method),
        ];

        for (k, v) in &self.headers {
            parts.push("-H".to_string());
            parts.push(shell_quote(&format!("{k}: {v}")));
        }

        if let Some(ref b) = self.body {
            // --data-raw: plain --data reads a file when the text starts with `@`.
            parts.push("--data-raw".to_string());
            parts.push(shell_quote(b));
        }

        // After --url, a URL that looks like an option (`-Kfile`) stays a URL.
        parts.push("--url".to_string());
        parts.push(shell_quote(&self.url));
        parts.join(" ")
    }

    /// The request as `.http` file text, after a `###` separator: the request line, headers,
    /// then a blank line and the body when there is one.
    pub fn to_http_text(&self) -> String {
        let mut text = format!("###\n{} {}\n", self.method, self.url);
        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\n"));
        }
        if let Some(body) = &self.body {
            text.push_str(&format!("\n{body}\n"));
        }
        text
    }
}

/// JetBrains' `Authorization: Basic user password` shorthand: exactly two words after `Basic`
/// become `Basic base64(user:password)`. Anything else (an encoded value, another scheme such
/// as `Bearer token`) is sent as written. A password containing spaces needs the encoded form.
pub fn expand_authorization(value: &str) -> String {
    let mut words = value.split_whitespace();
    match (words.next(), words.next(), words.next(), words.next()) {
        (Some(scheme), Some(user), Some(password), None)
            if scheme.eq_ignore_ascii_case("basic") =>
        {
            format!(
                "Basic {}",
                base64_encode(format!("{user}:{password}").as_bytes())
            )
        }
        _ => value.to_string(),
    }
}

/// Standard base64 with padding (RFC 4648), the encoding Basic auth uses.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (index, byte)| {
            group | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[(group >> (18 - 6 * index)) as usize & 63],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Quotes `text` as one word for POSIX sh, bash, zsh and fish alike. Inside single quotes sh
/// treats everything literally, but fish gives `\'` and `\\` meaning there, so a quote and a
/// backslash are written outside them as `'"'"'` and `'"\\"'` (double quotes read `\\` as one
/// backslash in every one of those shells).
pub fn shell_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        match c {
            '\'' => quoted.push_str("'\"'\"'"),
            '\\' => quoted.push_str("'\"\\\\\"'"),
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

pub fn parse_http_file(content: &str) -> Vec<HttpRequest> {
    let mut requests = Vec::new();
    let normalized = content.replace("\r\n", "\n");
    let chunks: Vec<&str> = normalized.split("\n###").collect();
    // Byte offset of the current chunk in `normalized`, to find the line its text starts on.
    let mut offset = 0;

    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let (first_line_name, remaining_chunk) = if chunk_idx == 0 {
            if let Some(stripped) = chunk.strip_prefix("###") {
                split_name_and_rest(stripped)
            } else {
                (None, *chunk)
            }
        } else {
            split_name_and_rest(chunk)
        };

        // The 1-based line a chunk's text starts on; the name line is taken off the rest.
        let chunk_line = 1 + normalized
            .get(..offset)
            .map_or(0, |before| before.matches('\n').count());
        let rest_line = if chunk_idx == 0 && !chunk.starts_with("###") {
            chunk_line
        } else {
            chunk_line + 1
        };

        if let Some(mut req) = parse_single_chunk(remaining_chunk, rest_line) {
            if req.name.is_none() {
                req.name = first_line_name;
            }
            requests.push(req);
        }

        offset += chunk.len() + "\n###".len();
    }

    requests
}

fn split_name_and_rest(chunk: &str) -> (Option<String>, &str) {
    if let Some(pos) = chunk.find('\n') {
        let first = chunk[..pos].trim();
        let rest = &chunk[pos + 1..];
        let name = if first.is_empty() {
            None
        } else {
            Some(first.to_string())
        };
        (name, rest)
    } else {
        let first = chunk.trim();
        let name = if first.is_empty() {
            None
        } else {
            Some(first.to_string())
        };
        (name, "")
    }
}

fn parse_single_chunk(chunk: &str, base_line: usize) -> Option<HttpRequest> {
    let lines: Vec<&str> = chunk.lines().collect();
    let mut i = 0;
    let mut request_name = None;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }

        if line.starts_with('#') || line.starts_with("//") {
            let comment_text = if let Some(stripped) = line.strip_prefix('#') {
                stripped.trim()
            } else if let Some(stripped) = line.strip_prefix("//") {
                stripped.trim()
            } else {
                ""
            };
            if request_name.is_none() && !comment_text.is_empty() {
                request_name = Some(comment_text.to_string());
            }
            i += 1;
            continue;
        }

        break;
    }

    if i >= lines.len() {
        return None;
    }

    let request_line = lines[i].trim();
    let req_line_num = base_line + i;
    i += 1;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let (method, url) = if parts.len() == 1 {
        ("GET", parts[0])
    } else {
        (parts[0], parts[1])
    };

    let mut headers = Vec::new();
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            break;
        }

        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_string();
            let value = line[colon_pos + 1..].trim().to_string();
            headers.push((key, value));
        }
        i += 1;
    }

    let mut body_lines = Vec::new();
    while i < lines.len() {
        body_lines.push(lines[i]);
        i += 1;
    }

    let body = if body_lines.is_empty() {
        None
    } else {
        let joined = body_lines.join("\n").trim().to_string();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    };

    let mut req = HttpRequest::new(method, url, req_line_num);
    req.name = request_name;
    req.headers = headers;
    req.body = body;

    Some(req)
}

pub fn substitute_variables(text: &str, env: &HashMap<String, String>) -> String {
    substitute_variables_with(text, env, &mut || uuid::Uuid::new_v4().as_u64_pair().0)
}

/// Like [`substitute_variables`] with the random source injected: `random` yields uniformly
/// random `u64`s. Each `{{$uuid}}` and `{{$randomInt}}` gets its own value.
pub fn substitute_variables_with(
    text: &str,
    env: &HashMap<String, String>,
    random: &mut dyn FnMut() -> u64,
) -> String {
    let mut result = text.to_string();

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();

    result = result.replace("{{$timestamp}}", &now_ts);
    result = replace_each(&result, "{{$randomInt}}", || (random() % 1000).to_string());
    result = replace_each(&result, "{{$uuid}}", || {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&random().to_be_bytes());
        bytes[8..].copy_from_slice(&random().to_be_bytes());
        uuid::Builder::from_random_bytes(bytes)
            .into_uuid()
            .to_string()
    });

    for (k, v) in env {
        let placeholder = format!("{{{{{k}}}}}");
        result = result.replace(&placeholder, v);
    }

    result
}

/// Replaces every occurrence of `placeholder`, calling `value` once per occurrence.
fn replace_each(text: &str, placeholder: &str, mut value: impl FnMut() -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(placeholder) {
        out.push_str(&rest[..at]);
        out.push_str(&value());
        rest = &rest[at + placeholder.len()..];
    }
    out.push_str(rest);
    out
}

pub fn parse_curl(curl_cmd: &str) -> Result<HttpRequest, String> {
    let trimmed = curl_cmd.trim();
    if !trimmed.starts_with("curl") {
        return Err("Command must start with 'curl'".to_string());
    }

    let mut method = "GET".to_string();
    let mut url = String::new();
    let mut headers = Vec::new();
    let mut body = None;

    let parts = shell_split(&trimmed.replace("\r\n", "\n"));
    let mut i = 1;

    while i < parts.len() {
        let part = &parts[i];
        if (part == "-X" || part == "--request") && i + 1 < parts.len() {
            method = parts[i + 1].to_uppercase();
            i += 2;
        } else if (part == "-H" || part == "--header") && i + 1 < parts.len() {
            let h = &parts[i + 1];
            if let Some(pos) = h.find(':') {
                headers.push((h[..pos].trim().to_string(), h[pos + 1..].trim().to_string()));
            }
            i += 2;
        } else if (part == "-d" || part == "--data" || part == "--data-raw") && i + 1 < parts.len()
        {
            body = Some(parts[i + 1].clone());
            if method == "GET" {
                method = "POST".to_string();
            }
            i += 2;
        } else if part == "--url" && i + 1 < parts.len() {
            url = parts[i + 1].clone();
            i += 2;
        } else if !part.starts_with('-') && url.is_empty() {
            url = part.clone();
            i += 1;
        } else {
            i += 1;
        }
    }

    if url.is_empty() {
        return Err("No URL found in curl command".to_string());
    }

    let mut req = HttpRequest::new(&method, &url, 1);
    req.headers = headers;
    req.body = body;

    Ok(req)
}

/// Splits like a POSIX shell: single quotes are literal, double quotes allow `\\` escapes of
/// `"`, `\\`, `$` and backtick, and outside quotes a backslash escapes the next character.
fn shell_split(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
        } else if in_double {
            match c {
                '"' => in_double = false,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '\\' | '$' | '`')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => current.push('\\'),
                },
                _ => current.push(c),
            }
        } else if c == '\'' {
            in_single = true;
            started = true;
        } else if c == '"' {
            in_double = true;
            started = true;
        } else if c == '\\' {
            match chars.next() {
                // A line continuation, as in a curl command copied from docs or a browser.
                Some('\n') => {}
                Some(next) => {
                    current.push(next);
                    started = true;
                }
                None => started = true,
            }
        } else if c.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut current));
                started = false;
            }
        } else {
            current.push(c);
            started = true;
        }
    }

    if started {
        tokens.push(current);
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_file_multiple_requests() {
        let content = r#"
# Get User List
GET https://api.example.com/v1/users HTTP/1.1
Accept: application/json
Authorization: Bearer my-token

### Create User
POST https://api.example.com/v1/users
Content-Type: application/json

{
  "name": "Alice",
  "email": "alice@example.com"
}

###
DELETE https://api.example.com/v1/users/42
"#;

        let requests = parse_http_file(content);
        assert_eq!(requests.len(), 3);

        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].url, "https://api.example.com/v1/users");
        assert_eq!(requests[0].name, Some("Get User List".to_string()));
        assert_eq!(requests[0].headers.len(), 2);
        assert!(requests[0].body.is_none());

        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].name, Some("Create User".to_string()));
        assert_eq!(requests[1].headers.len(), 1);
        assert!(
            requests[1]
                .body
                .as_ref()
                .unwrap()
                .contains("alice@example.com")
        );

        assert_eq!(requests[2].method, "DELETE");
        assert_eq!(requests[2].url, "https://api.example.com/v1/users/42");
    }

    #[test]
    fn test_variable_substitution() {
        let mut env = HashMap::new();
        env.insert("host".to_string(), "https://staging.api.com".to_string());
        env.insert("token".to_string(), "secret-123".to_string());

        let template =
            "POST {{host}}/v1/login\nAuthorization: Bearer {{token}}\nX-Req-Id: {{$uuid}}";
        let resolved = substitute_variables(template, &env);

        assert!(resolved.contains("POST https://staging.api.com/v1/login"));
        assert!(resolved.contains("Authorization: Bearer secret-123"));
        let id = resolved.rsplit("X-Req-Id: ").next().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok(), "{id}");
    }

    #[test]
    fn test_curl_export_and_import() {
        let mut req = HttpRequest::new("POST", "https://api.example.com/v1/items", 1);
        req.headers
            .push(("Content-Type".to_string(), "application/json".to_string()));
        req.body = Some("{\"title\":\"Zenith\"}".to_string());

        let curl = req.to_curl();
        assert!(curl.contains("-X 'POST'"));
        assert!(curl.contains("-H 'Content-Type: application/json'"));
        assert!(curl.contains("--data-raw '{\"title\":\"Zenith\"}'"));

        let imported = parse_curl(&curl).expect("parse curl");
        assert_eq!(imported.method, "POST");
        assert_eq!(imported.url, "https://api.example.com/v1/items");
        assert_eq!(imported.headers.len(), 1);
        assert_eq!(imported.headers[0].0, "Content-Type");
        assert_eq!(imported.body, Some("{\"title\":\"Zenith\"}".to_string()));
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_quote("a\\b"), "'a'\"\\\\\"'b'");
        // fish reads \' and \\ inside single quotes as escapes: neither may appear there.
        let quoted = shell_quote("x\\'y\\");
        let mut inside_single = false;
        let mut chars = quoted.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\'' => inside_single = !inside_single,
                '"' if !inside_single => {
                    for c in chars.by_ref() {
                        if c == '"' {
                            break;
                        }
                    }
                }
                '\\' => assert!(!inside_single, "{quoted}"),
                _ => {}
            }
        }
    }

    /// Prints each argument `shell` receives for the words after `curl`, NUL separated.
    /// `None` when the shell is not installed.
    // A real shell is the point; a blocking spawn is fine in a unit test.
    #[allow(clippy::disallowed_methods)]
    fn args_through(shell: &str, curl: &str) -> Option<Vec<String>> {
        let words = &curl["curl".len()..];
        let script = match shell {
            "fish" => format!("set args {words}; for a in $args; printf '%s\\0' $a; end"),
            _ => format!("set -- {words}; for a in \"$@\"; do printf '%s\\0' \"$a\"; done"),
        };
        let out = std::process::Command::new(shell)
            .arg("-c")
            .arg(script)
            .output()
            .ok()?;
        Some(
            out.stdout
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).to_string())
                .collect(),
        )
    }

    /// The nastiest values must come back out of the shell as the same single words.
    #[test]
    fn test_curl_export_quotes_hostile_values() {
        // Hostile values only ever `touch` a marker in a scratch directory, never anything real.
        let scratch = tempfile::tempdir().unwrap();
        let marker = scratch.path().join("pwned");
        let marker = marker.display();
        let nasty = [
            format!("$(touch {marker})"),
            format!("`touch {marker}`"),
            format!("a\"b; touch {marker}; echo \""),
            format!("a'b; touch {marker}; echo 'c"),
            format!("line1\ntouch {marker}\nline3"),
            "it's a 'quote'".to_string(),
            "back\\slash $HOME ${IFS}".to_string(),
            // Backslash next to a quote: fish would read `\'` as an escaped quote.
            "a\\'b".to_string(),
            "trailing\\".to_string(),
            "\\\\'\\\\".to_string(),
            format!("\\'; touch {marker}; echo '"),
        ];
        let mut ran_in = vec!["sh"];
        if args_through("fish", "curl 'x'").is_some() {
            ran_in.push("fish");
        }
        for value in nasty.iter().map(String::as_str) {
            let mut req = HttpRequest::new("GET", &format!("https://x.test/?q={value}"), 1);
            req.headers.push(("X-Test".to_string(), value.to_string()));
            req.body = Some(value.to_string());
            let curl = req.to_curl();

            for shell in &ran_in {
                assert_eq!(
                    args_through(shell, &curl).unwrap(),
                    vec![
                        "-X".to_string(),
                        "GET".to_string(),
                        "-H".to_string(),
                        format!("X-Test: {value}"),
                        "--data-raw".to_string(),
                        value.to_string(),
                        "--url".to_string(),
                        format!("https://x.test/?q={value}"),
                    ],
                    "{shell}: {curl}"
                );
            }

            // And our own importer agrees.
            let back = parse_curl(&curl).unwrap();
            assert_eq!(back.url, format!("https://x.test/?q={value}"));
            assert_eq!(back.body.as_deref(), Some(value));
            assert_eq!(back.headers[0].1, value);
        }
        assert!(
            !scratch.path().join("pwned").exists(),
            "a value was executed by the shell"
        );
        // fish is optional on a dev machine; sh always ran.
        eprintln!("shells exercised: {ran_in:?}");
    }

    #[test]
    fn test_option_like_url_and_file_body_stay_data() {
        let mut req = HttpRequest::new("POST", "-Kevil.cfg", 1);
        req.body = Some("@/some/file".to_string());
        let curl = req.to_curl();
        assert!(curl.contains("--url '-Kevil.cfg'"), "{curl}");
        assert!(curl.contains("--data-raw '@/some/file'"), "{curl}");
        assert!(!curl.contains(" --data "), "{curl}");

        let args = args_through("sh", &curl).unwrap();
        // The URL is the argument right after --url, never a bare option.
        let url_at = args.iter().position(|a| a == "--url").unwrap();
        assert_eq!(args[url_at + 1], "-Kevil.cfg");

        let back = parse_curl(&curl).unwrap();
        assert_eq!(back.url, "-Kevil.cfg");
        assert_eq!(back.body.as_deref(), Some("@/some/file"));
        assert_eq!(back.method, "POST");
    }

    #[test]
    fn test_method_is_quoted() {
        let req = HttpRequest {
            method: "GET; echo hi".to_string(),
            ..HttpRequest::new("GET", "https://x.test", 1)
        };
        assert!(req.to_curl().starts_with("curl -X 'GET; echo hi' "));
    }

    #[test]
    fn test_uuid_and_random_int_are_real_and_injectable() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let out = substitute_variables_with(
            "{{$uuid}} {{$uuid}} {{$randomInt}} {{$randomInt}}",
            &HashMap::new(),
            &mut rng,
        );
        let parts: Vec<&str> = out.split(' ').collect();
        let (a, b) = (
            uuid::Uuid::parse_str(parts[0]).unwrap(),
            uuid::Uuid::parse_str(parts[1]).unwrap(),
        );
        assert_eq!(a.get_version_num(), 4);
        assert_eq!(a.get_variant(), uuid::Variant::RFC4122);
        assert_ne!(a, b, "every occurrence is its own uuid");
        assert_ne!(parts[0], "c8d8c2e6-7a1b-4f3d-9e5c-0b8f4e2a1d3c");
        let (x, y): (u32, u32) = (parts[2].parse().unwrap(), parts[3].parse().unwrap());
        assert!(x < 1000 && y < 1000);
        assert_ne!(x, 42);
        // The default source differs between calls.
        let one = substitute_variables("{{$uuid}}", &HashMap::new());
        let two = substitute_variables("{{$uuid}}", &HashMap::new());
        assert_ne!(one, two);
    }

    #[test]
    fn test_base64_matches_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded, "{plain:?}");
        }
        assert_eq!(base64_encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn test_basic_and_bearer_authorization() {
        assert_eq!(
            expand_authorization("Basic user password"),
            "Basic dXNlcjpwYXNzd29yZA=="
        );
        assert_eq!(
            expand_authorization("basic  admin   p@ss:w0rd"),
            "Basic YWRtaW46cEBzczp3MHJk"
        );
        for unchanged in [
            "Basic dXNlcjpwYXNzd29yZA==",
            "Bearer abc.def",
            "Basic a b c",
            "Digest user password",
        ] {
            assert_eq!(expand_authorization(unchanged), unchanged);
        }
        let env = HashMap::from([
            ("user".to_string(), "user".to_string()),
            ("password".to_string(), "password".to_string()),
            ("token".to_string(), "t0k3n".to_string()),
        ]);
        let basic = substitute_variables("Basic {{user}} {{password}}", &env);
        assert_eq!(expand_authorization(&basic), "Basic dXNlcjpwYXNzd29yZA==");
        let bearer = substitute_variables("Bearer {{token}}", &env);
        assert_eq!(expand_authorization(&bearer), "Bearer t0k3n");
    }

    #[test]
    fn test_curl_import_to_http_text() {
        let curl = "curl 'https://api.example.com/v1/items?page=2' \\\r\n  -X PUT \\\n  -H 'Accept: application/json' \\\n  -H \"Authorization: Bearer abc\" \\\n  --data-raw '{\"title\":\"Zenith\"}'";
        let request = parse_curl(curl).expect("parse curl");
        let text = request.to_http_text();
        assert_eq!(
            text,
            "###\nPUT https://api.example.com/v1/items?page=2\nAccept: application/json\nAuthorization: Bearer abc\n\n{\"title\":\"Zenith\"}\n"
        );
        let parsed = parse_http_file(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].method, request.method);
        assert_eq!(parsed[0].url, request.url);
        assert_eq!(parsed[0].headers, request.headers);
        assert_eq!(parsed[0].body, request.body);

        let get = parse_curl("curl https://a.test/").expect("parse curl");
        assert_eq!(get.to_http_text(), "###\nGET https://a.test/\n");
    }
}
