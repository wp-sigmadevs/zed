pub mod environment;
pub mod history;
pub mod http_response_view;
pub mod parser;
pub mod response;

pub use environment::*;
pub use history::*;
pub use http_response_view::*;
pub use parser::*;
pub use response::*;

use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use futures::AsyncReadExt;
use http::{Method, Request};
use http_client::{AsyncBody, HttpClient};

gpui::actions!(
    zenith_http,
    [
        RunRequest,
        RunAllRequests,
        CopyAsCurl,
        SelectEnvironment,
    ]
);

pub async fn execute_request(
    req: &HttpRequest,
    client: Arc<dyn HttpClient>,
    env: Option<&HashMap<String, String>>,
) -> Result<HttpResponse> {
    let empty_env = HashMap::new();
    let env_map = env.unwrap_or(&empty_env);

    let url_str = parser::substitute_variables(&req.url, env_map);
    let method = Method::from_bytes(req.method.as_bytes())?;

    let mut builder = Request::builder().method(method).uri(url_str);

    for (k, v) in &req.headers {
        let subst_v = parser::substitute_variables(v, env_map);
        builder = builder.header(k.as_str(), subst_v.as_str());
    }

    let body = if let Some(ref b) = req.body {
        let subst_b = parser::substitute_variables(b, env_map);
        AsyncBody::from(subst_b)
    } else {
        AsyncBody::default()
    };

    let start = std::time::Instant::now();
    let http_req = builder.body(body)?;

    let mut resp = client.send(http_req).await?;
    let duration_ms = start.elapsed().as_millis() as u64;

    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();

    let mut headers = Vec::new();
    for (k, v) in resp.headers() {
        headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
    }

    let mut body_bytes = Vec::new();
    resp.body_mut().read_to_end(&mut body_bytes).await?;
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    Ok(HttpResponse::new(status, &status_text, headers, &body_str, duration_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zenith_http_integration() {
        let content = r#"
### Test Ping
GET https://httpbin.org/get
Accept: application/json
"#;
        let requests = parse_http_file(content);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].url, "https://httpbin.org/get");

        let curl = requests[0].to_curl();
        assert!(curl.contains("curl -X GET"));
        assert!(curl.contains("-H \"Accept: application/json\""));
    }
}
