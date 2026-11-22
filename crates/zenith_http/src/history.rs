use crate::parser::HttpRequest;
use crate::response::HttpResponse;
use serde::{Deserialize, Serialize};

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

/// Headers whose values history never keeps (names compared case-insensitively).
const MASKED_HEADERS: [&str; 4] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

fn mask_secret_headers(headers: &mut [(String, String)]) {
    for (name, value) in headers {
        if MASKED_HEADERS
            .iter()
            .any(|masked| name.eq_ignore_ascii_case(masked))
        {
            *value = MASKED_VALUE.to_string();
        }
    }
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
}
