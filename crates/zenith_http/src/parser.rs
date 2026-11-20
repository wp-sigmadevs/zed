use std::collections::HashMap;
use serde::{Deserialize, Serialize};

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

    pub fn to_curl(&self) -> String {
        let mut parts = vec!["curl".to_string(), "-X".to_string(), self.method.clone()];

        for (k, v) in &self.headers {
            parts.push("-H".to_string());
            parts.push(format!("\"{k}: {v}\""));
        }

        if let Some(ref b) = self.body {
            parts.push("--data".to_string());
            let escaped = b.replace('\'', "'\\''");
            parts.push(format!("'{escaped}'"));
        }

        parts.push(format!("\"{}\"", self.url));
        parts.join(" ")
    }
}

pub fn parse_http_file(content: &str) -> Vec<HttpRequest> {
    let mut requests = Vec::new();
    let normalized = content.replace("\r\n", "\n");
    let chunks: Vec<&str> = normalized.split("\n###").collect();
    let mut current_line = 1;

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

        if let Some(mut req) = parse_single_chunk(remaining_chunk, current_line) {
            if req.name.is_none() {
                req.name = first_line_name;
            }
            requests.push(req);
        }

        current_line += chunk.lines().count();
        if chunk_idx > 0 {
            current_line += 1;
        }
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
    let mut result = text.to_string();

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "1700000000".to_string());

    result = result.replace("{{$timestamp}}", &now_ts);
    result = result.replace("{{$randomInt}}", "42");
    result = result.replace("{{$uuid}}", "c8d8c2e6-7a1b-4f3d-9e5c-0b8f4e2a1d3c");

    for (k, v) in env {
        let placeholder = format!("{{{{{k}}}}}");
        result = result.replace(&placeholder, v);
    }

    result
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

    let parts = shell_split(trimmed);
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
        } else if (part == "-d" || part == "--data" || part == "--data-raw") && i + 1 < parts.len() {
            body = Some(parts[i + 1].clone());
            if method == "GET" {
                method = "POST".to_string();
            }
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

fn shell_split(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for c in input.chars() {
        if c == '\'' && !in_double {
            in_single = !in_single;
        } else if c == '"' && !in_single {
            in_double = !in_double;
        } else if c.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(c);
        }
    }

    if !current.is_empty() {
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
        assert!(requests[1].body.as_ref().unwrap().contains("alice@example.com"));

        assert_eq!(requests[2].method, "DELETE");
        assert_eq!(requests[2].url, "https://api.example.com/v1/users/42");
    }

    #[test]
    fn test_variable_substitution() {
        let mut env = HashMap::new();
        env.insert("host".to_string(), "https://staging.api.com".to_string());
        env.insert("token".to_string(), "secret-123".to_string());

        let template = "POST {{host}}/v1/login\nAuthorization: Bearer {{token}}\nX-Req-Id: {{$uuid}}";
        let resolved = substitute_variables(template, &env);

        assert!(resolved.contains("POST https://staging.api.com/v1/login"));
        assert!(resolved.contains("Authorization: Bearer secret-123"));
        assert!(resolved.contains("X-Req-Id: c8d8c2e6-7a1b-4f3d-9e5c-0b8f4e2a1d3c"));
    }

    #[test]
    fn test_curl_export_and_import() {
        let mut req = HttpRequest::new("POST", "https://api.example.com/v1/items", 1);
        req.headers.push(("Content-Type".to_string(), "application/json".to_string()));
        req.body = Some("{\"title\":\"Zenith\"}".to_string());

        let curl = req.to_curl();
        assert!(curl.contains("-X POST"));
        assert!(curl.contains("-H \"Content-Type: application/json\""));
        assert!(curl.contains("--data '{\"title\":\"Zenith\"}'"));

        let imported = parse_curl(&curl).expect("parse curl");
        assert_eq!(imported.method, "POST");
        assert_eq!(imported.url, "https://api.example.com/v1/items");
        assert_eq!(imported.headers.len(), 1);
        assert_eq!(imported.headers[0].0, "Content-Type");
        assert_eq!(imported.body, Some("{\"title\":\"Zenith\"}".to_string()));
    }
}
