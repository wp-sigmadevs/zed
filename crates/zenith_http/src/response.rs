use serde::{Deserialize, Serialize};

/// Start of the note [`HttpResponse::mark_truncated`] appends to a cut body.
pub(crate) const TRUNCATION_NOTE_PREFIX: &str = "[Response truncated:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub duration_ms: u64,
    pub size_bytes: usize,
    pub timestamp: String,
    /// The body was cut at the size limit; `body` ends with a note saying so.
    #[serde(default)]
    pub truncated: bool,
}

impl HttpResponse {
    pub fn new(
        status: u16,
        status_text: &str,
        headers: Vec<(String, String)>,
        body: &str,
        duration_ms: u64,
    ) -> Self {
        let size_bytes = body.len();
        let timestamp = chrono_or_fallback_ts();

        Self {
            status,
            status_text: status_text.to_string(),
            headers,
            body: body.to_string(),
            duration_ms,
            size_bytes,
            timestamp,
            truncated: false,
        }
    }

    /// Marks the body as cut at `limit` bytes and says so at its end.
    pub fn mark_truncated(mut self, limit: usize) -> Self {
        self.truncated = true;
        self.body.push_str(&format!(
            "\n\n{TRUNCATION_NOTE_PREFIX} only the first {} MiB are shown]",
            limit / (1024 * 1024)
        ));
        self
    }

    pub fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    pub fn is_client_error(&self) -> bool {
        self.status >= 400 && self.status < 500
    }

    pub fn is_server_error(&self) -> bool {
        self.status >= 500 && self.status < 600
    }

    pub fn format_body(&self) -> String {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&self.body) {
            serde_json::to_string_pretty(&val).unwrap_or_else(|_| self.body.clone())
        } else {
            self.body.clone()
        }
    }

    pub fn to_raw_http(&self) -> String {
        let mut raw = format!("HTTP/1.1 {} {}\n", self.status, self.status_text);
        for (k, v) in &self.headers {
            raw.push_str(&format!("{k}: {v}\n"));
        }
        raw.push('\n');
        raw.push_str(&self.body);
        raw
    }
}

fn chrono_or_fallback_ts() -> String {
    let now = std::time::SystemTime::now();
    let since = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s", since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_json_body() {
        let raw_json = "{\"status\":\"ok\",\"count\":2}";
        let res = HttpResponse::new(
            200,
            "OK",
            vec![("Content-Type".into(), "application/json".into())],
            raw_json,
            120,
        );

        assert!(res.is_success());
        let formatted = res.format_body();
        assert!(formatted.contains("  \"status\": \"ok\""));
        assert!(formatted.contains("  \"count\": 2"));

        let raw = res.to_raw_http();
        assert!(raw.starts_with("HTTP/1.1 200 OK"));
        assert!(raw.contains("Content-Type: application/json"));
    }
}
