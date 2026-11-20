use serde::{Deserialize, Serialize};
use crate::parser::HttpRequest;
use crate::response::HttpResponse;

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
}

impl HttpHistoryStore {
    pub fn new(max_limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_limit: if max_limit == 0 { 100 } else { max_limit },
        }
    }

    pub fn add(&mut self, request: HttpRequest, response: HttpResponse) -> String {
        let id = format!("hist-{}", self.entries.len() + 1);
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
}
