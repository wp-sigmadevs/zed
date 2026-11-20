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

use anyhow::{Context as _, Result, ensure};
use futures::AsyncReadExt;
use http::{Method, Request};
use http_client::{AsyncBody, Host, HttpClient, HttpRequestExt as _, RedirectPolicy, Url};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

gpui::actions!(
    zenith_http,
    [RunRequest, RunAllRequests, CopyAsCurl, SelectEnvironment,]
);

/// Largest response body kept; the rest is dropped and the response says so.
pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// Most redirects followed for one request.
pub const MAX_REDIRECTS: usize = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Sends `req`, following at most [`MAX_REDIRECTS`] redirects itself (the client is told not
/// to follow any): credentials are dropped when a redirect leaves the origin, and a redirect
/// to a loopback or link-local address is refused unless the request started there.
pub async fn execute_request(
    req: &HttpRequest,
    client: Arc<dyn HttpClient>,
    env: Option<&HashMap<String, String>>,
) -> Result<HttpResponse> {
    let empty_env = HashMap::new();
    let env_map = env.unwrap_or(&empty_env);

    let mut url = Url::parse(&parser::substitute_variables(&req.url, env_map))
        .context("the request URL is not valid")?;
    ensure!(is_web_url(&url), "only http and https URLs can be sent");
    let allow_local = is_local_host(&url);
    let mut method = Method::from_bytes(req.method.as_bytes())?;
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), parser::substitute_variables(v, env_map)))
        .collect();
    let mut body = req
        .body
        .as_ref()
        .map(|b| parser::substitute_variables(b, env_map));

    let start = Instant::now();
    let mut redirects = 0;
    let mut resp = loop {
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(url.as_str())
            .follow_redirects(RedirectPolicy::NoFollow)
            .timeout(REQUEST_TIMEOUT);
        for (k, v) in &headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let async_body = body
            .clone()
            .map_or_else(AsyncBody::default, AsyncBody::from);
        let resp = client.send(builder.body(async_body)?).await?;

        let Some(target) = redirect_target(&url, &resp) else {
            break resp;
        };
        ensure!(
            redirects < MAX_REDIRECTS,
            "stopped after {MAX_REDIRECTS} redirects"
        );
        redirects += 1;
        let target = target?;
        ensure!(is_web_url(&target), "refused a redirect to {target}");
        ensure!(
            allow_local || !is_local_host(&target),
            "refused a redirect to the local address {target}"
        );
        if !same_origin(&url, &target) {
            headers.retain(|(name, _)| !is_sensitive_header(name));
        }
        let status = resp.status().as_u16();
        if status == 303 || (matches!(status, 301 | 302) && method == Method::POST) {
            method = Method::GET;
            body = None;
            headers.retain(|(name, _)| !name.to_ascii_lowercase().starts_with("content-"));
        }
        url = target;
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();

    let mut headers = Vec::new();
    for (k, v) in resp.headers() {
        headers.push((k.to_string(), v.to_str().unwrap_or("").to_string()));
    }

    // One byte over the limit tells "exactly the limit" from "cut".
    let mut body_bytes = Vec::new();
    AsyncReadExt::take(resp.body_mut(), MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body_bytes)
        .await?;
    let truncated = body_bytes.len() > MAX_RESPONSE_BYTES;
    body_bytes.truncate(MAX_RESPONSE_BYTES);
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    let response = HttpResponse::new(status, &status_text, headers, &body_str, duration_ms);
    Ok(if truncated {
        response.mark_truncated(MAX_RESPONSE_BYTES)
    } else {
        response
    })
}

fn is_web_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Where a redirect response points, resolved against `from`. `None` when it is not a redirect.
fn redirect_target<B>(from: &Url, resp: &http::Response<B>) -> Option<Result<Url>> {
    if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = resp.headers().get(http::header::LOCATION)?;
    Some(
        location
            .to_str()
            .map_err(anyhow::Error::from)
            .and_then(|location| from.join(location).map_err(anyhow::Error::from))
            .context("the redirect has an invalid Location"),
    )
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Headers that carry credentials or sessions; they never follow a redirect to another origin.
fn is_sensitive_header(name: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "auth",
        "cookie",
        "token",
        "secret",
        "key",
        "session",
        "credential",
        "password",
        "csrf",
    ];
    let name = name.to_ascii_lowercase();
    MARKERS.iter().any(|marker| name.contains(marker))
}

/// Loopback, link-local and unspecified addresses, and `localhost`. (A name that merely
/// resolves to one of them is not detected.)
fn is_local_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_link_local() || ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_link_local() || v4.is_unspecified())
        }
        None => false,
    }
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
        assert!(curl.contains("curl -X 'GET'"));
        assert!(curl.contains("-H 'Accept: application/json'"));
    }

    use http_client::FakeHttpClient;
    use std::sync::Mutex;

    type Seen = Arc<Mutex<Vec<(String, String, Vec<String>, bool)>>>;

    /// A client answering from `reply(request_number, uri)`; records (method, uri, header
    /// names, has body) of every request it gets.
    fn client(
        seen: &Seen,
        reply: impl Fn(usize, &str) -> http::response::Builder + Send + Sync + 'static,
        body: impl Fn() -> AsyncBody + Send + Sync + 'static,
    ) -> Arc<dyn HttpClient> {
        let seen = seen.clone();
        FakeHttpClient::create(move |req| {
            let uri = req.uri().to_string();
            let method = req.method().to_string();
            let names = req
                .headers()
                .keys()
                .map(|k| k.as_str().to_string())
                .collect();
            assert_eq!(
                req.extensions().get::<RedirectPolicy>(),
                Some(&RedirectPolicy::NoFollow),
                "the client must never follow redirects on its own"
            );
            // Bodies here are bytes or empty: reading them completes immediately.
            let mut sent = Vec::new();
            futures::executor::block_on(req.into_body().read_to_end(&mut sent)).unwrap();
            let has_body = !sent.is_empty();
            let number = {
                let mut seen = seen.lock().unwrap();
                seen.push((method, uri.clone(), names, has_body));
                seen.len()
            };
            assert!(number <= 20, "runaway redirect loop");
            let response = reply(number, &uri).body(body());
            async move { Ok(response?) }
        })
    }

    fn redirect(to: &'static str) -> impl Fn(usize, &str) -> http::response::Builder {
        move |n, _| {
            if n == 1 {
                http::Response::builder().status(302).header("location", to)
            } else {
                http::Response::builder().status(200)
            }
        }
    }

    fn request_with_secrets(url: &str) -> HttpRequest {
        let mut req = HttpRequest::new("GET", url, 1);
        req.headers = vec![
            ("Authorization".into(), "Bearer t".into()),
            ("X-Api-Key".into(), "k".into()),
            ("Cookie".into(), "s=1".into()),
            ("Accept".into(), "*/*".into()),
        ];
        req
    }

    #[gpui::test]
    async fn test_cross_origin_redirect_drops_credentials(_cx: &mut gpui::TestAppContext) {
        let seen = Seen::default();
        let http = client(&seen, redirect("https://other.test/y"), AsyncBody::default);
        let response = execute_request(&request_with_secrets("https://a.test/x"), http, None)
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let seen = seen.lock().unwrap();
        assert!(
            seen[0].2.contains(&"authorization".to_string())
                && seen[0].2.contains(&"x-api-key".to_string())
        );
        assert_eq!(seen[1].1, "https://other.test/y");
        assert_eq!(
            seen[1].2,
            vec!["accept".to_string()],
            "only harmless headers cross origins"
        );
    }

    #[gpui::test]
    async fn test_same_origin_redirect_keeps_credentials(_cx: &mut gpui::TestAppContext) {
        let seen = Seen::default();
        let http = client(&seen, redirect("/next"), AsyncBody::default);
        execute_request(&request_with_secrets("https://a.test/x"), http, None)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1].1, "https://a.test/next");
        assert!(seen[1].2.contains(&"authorization".to_string()));
    }

    #[gpui::test]
    async fn test_redirect_to_local_address_is_refused_unless_started_there(
        _cx: &mut gpui::TestAppContext,
    ) {
        for target in [
            "http://127.0.0.1:8080/admin",
            "http://localhost/x",
            "http://169.254.10.10/x",
            "http://[::1]/",
            "http://[fe80::1]/",
        ] {
            let seen = Seen::default();
            let http = client(&seen, redirect(target), AsyncBody::default);
            let error = execute_request(
                &HttpRequest::new("GET", "https://public.test/", 1),
                http,
                None,
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("local address"),
                "{target}: {error:#}"
            );
            assert_eq!(seen.lock().unwrap().len(), 1, "{target} was not requested");
        }
        let seen = Seen::default();
        let http = client(
            &seen,
            redirect("http://127.0.0.1:9000/b"),
            AsyncBody::default,
        );
        execute_request(
            &HttpRequest::new("GET", "http://localhost:8000/a", 1),
            http,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "a local request may redirect locally"
        );
    }

    #[gpui::test]
    async fn test_redirect_loop_is_limited(_cx: &mut gpui::TestAppContext) {
        let seen = Seen::default();
        let http = client(
            &seen,
            |_, _| {
                http::Response::builder()
                    .status(301)
                    .header("location", "https://a.test/again")
            },
            AsyncBody::default,
        );
        let error = execute_request(&HttpRequest::new("GET", "https://a.test/", 1), http, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("redirects"), "{error:#}");
        assert_eq!(seen.lock().unwrap().len(), MAX_REDIRECTS + 1);
    }

    #[gpui::test]
    async fn test_303_turns_a_post_into_a_get_without_body(_cx: &mut gpui::TestAppContext) {
        let seen = Seen::default();
        let http = client(
            &seen,
            |n, _| {
                http::Response::builder()
                    .status(if n == 1 { 303 } else { 200 })
                    .header("location", "/done")
            },
            AsyncBody::default,
        );
        let mut req = HttpRequest::new("POST", "https://a.test/form", 1);
        req.body = Some("a=1".into());
        req.headers = vec![("Content-Type".into(), "text/plain".into())];
        execute_request(&req, http, None).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].0.as_str(), seen[0].3), ("POST", true));
        assert_eq!((seen[1].0.as_str(), seen[1].3), ("GET", false));
        assert!(!seen[1].2.contains(&"content-type".to_string()));
    }

    #[gpui::test]
    async fn test_response_body_is_bounded(_cx: &mut gpui::TestAppContext) {
        let seen = Seen::default();
        let http = client(
            &seen,
            |_, _| http::Response::builder().status(200),
            || AsyncBody::from("a".repeat(MAX_RESPONSE_BYTES + 4096)),
        );
        let response = execute_request(&HttpRequest::new("GET", "https://a.test/", 1), http, None)
            .await
            .unwrap();
        assert!(response.truncated);
        assert_eq!(response.size_bytes, MAX_RESPONSE_BYTES);
        assert!(response.body.len() < MAX_RESPONSE_BYTES + 200);
        assert!(response.body.ends_with("shown]"), "the cut is announced");

        let http = client(
            &seen,
            |_, _| http::Response::builder().status(200),
            || AsyncBody::from("b".repeat(MAX_RESPONSE_BYTES)),
        );
        let exact = execute_request(&HttpRequest::new("GET", "https://a.test/", 1), http, None)
            .await
            .unwrap();
        assert!(!exact.truncated, "a body of exactly the limit is complete");
        assert_eq!(exact.size_bytes, MAX_RESPONSE_BYTES);
    }

    #[gpui::test]
    async fn test_only_http_urls_are_sent(_cx: &mut gpui::TestAppContext) {
        for url in ["file:///etc/passwd", "ftp://a.test/x", "not a url"] {
            let seen = Seen::default();
            let http = client(
                &seen,
                |_, _| http::Response::builder().status(200),
                AsyncBody::default,
            );
            assert!(
                execute_request(&HttpRequest::new("GET", url, 1), http, None)
                    .await
                    .is_err(),
                "{url}"
            );
            assert!(seen.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn test_local_hosts_and_sensitive_headers() {
        for local in [
            "http://localhost/",
            "http://app.localhost/",
            "http://127.1.2.3/",
            "http://169.254.1.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(is_local_host(&Url::parse(local).unwrap()), "{local}");
        }
        for public in [
            "https://example.com/",
            "http://8.8.8.8/",
            "http://10.0.0.1/",
        ] {
            assert!(!is_local_host(&Url::parse(public).unwrap()), "{public}");
        }
        for name in [
            "Authorization",
            "COOKIE",
            "X-Api-Key",
            "X-Auth-Token",
            "x-csrf-header",
            "Proxy-Authorization",
        ] {
            assert!(is_sensitive_header(name), "{name}");
        }
        for name in ["Accept", "Content-Type", "User-Agent", "X-Request-Id"] {
            assert!(!is_sensitive_header(name), "{name}");
        }
    }
}
