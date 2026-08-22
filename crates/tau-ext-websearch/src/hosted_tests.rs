//! Deterministic HTTP fixtures for optional hosted-provider adapters.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use super::hosted::{HostedClient, HostedConfig, HostedRequest, HttpHostedClient};
use super::{WebAdapter, WebOperation};

/// Loopback HTTP server that captures an exact provider request sequence.
struct FixtureServer {
    /// Loopback origin used for endpoint overrides.
    origin: String,
    /// Worker returning the complete HTTP requests.
    worker: thread::JoinHandle<Vec<String>>,
}

impl FixtureServer {
    fn once(status: &str, body: &str) -> Self {
        Self::sequence(vec![(status.to_owned(), String::new(), body.to_owned())])
    }

    fn sequence(responses: Vec<(String, String, String)>) -> Self {
        Self::sequence_with_cancellation(responses, None)
    }

    fn sequence_with_cancellation(
        responses: Vec<(String, String, String)>,
        cancel_after_first: Option<Arc<AtomicBool>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let origin = format!(
            "http://{}/",
            listener.local_addr().expect("fixture address")
        );
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, (status, extra_headers, body)) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                requests.push(read_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                if index == 0
                    && let Some(cancelled) = &cancel_after_first
                {
                    cancelled.store(true, Ordering::Release);
                }
                stream
                    .write_all(response.as_bytes())
                    .expect("write fixture");
            }
            requests
        });
        Self { origin, worker }
    }

    fn finish(self) -> String {
        self.finish_all()
            .into_iter()
            .next()
            .expect("one fixture request")
    }

    fn finish_all(self) -> Vec<String> {
        self.worker.join().expect("join fixture")
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set fixture timeout");
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).expect("read fixture request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|length| length.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if header_end + 4 + content_length <= request.len() {
            break;
        }
    }
    String::from_utf8(request).expect("fixture request UTF-8")
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("fixture request separates headers and body");
    serde_json::from_str(body).expect("fixture request body is JSON")
}

fn config() -> HostedConfig {
    HostedConfig {
        you_endpoint: None,
        brave_endpoint: None,
        brave_api_key: None,
        tavily_endpoint: None,
        tavily_api_key: None,
        firecrawl_endpoint: None,
        firecrawl_api_key: None,
    }
}

fn request<'a>(
    operation: WebOperation,
    query: &'a str,
    count: u32,
    url: &'a str,
    cancelled: &'a AtomicBool,
) -> HostedRequest<'a> {
    HostedRequest {
        operation,
        query,
        count,
        url,
        timeout: Duration::from_secs(1),
        cancelled,
    }
}

/// Locks the anonymous You.com MCP request tool and argument shape to its
/// documented free-profile contract.
#[test]
fn you_free_mcp_fixture_is_exact() {
    let server = FixtureServer::sequence(vec![
        (
            "200 OK".to_owned(),
            "Mcp-Session-Id: you-session\r\n".to_owned(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"you","version":"1"}}}"#.to_owned(),
        ),
        ("202 Accepted".to_owned(), String::new(), String::new()),
        (
            "200 OK".to_owned(),
            String::new(),
            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"you result"}]}}"#.to_owned(),
        ),
    ]);
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        you_endpoint: Some(format!("{}mcp?profile=free", server.origin)),
        ..config()
    });
    assert_eq!(
        client
            .call(
                WebAdapter::You,
                request(
                    WebOperation::Search,
                    "rust agents",
                    7,
                    "",
                    &AtomicBool::new(false),
                ),
            )
            .expect("You.com fixture"),
        "you result"
    );
    let requests = server.finish_all();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("POST /mcp?profile=free HTTP/1.1\r\n"));
    assert_eq!(
        request_json(&requests[0]),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "tau-ext-websearch",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        })
    );
    assert!(!requests[0].to_ascii_lowercase().contains("mcp-session-id:"));
    assert!(!requests[0].contains("MCP-Protocol-Version:"));
    assert_eq!(
        request_json(&requests[1]),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })
    );
    assert!(requests[1].contains("mcp-session-id: you-session\r\n"));
    assert!(requests[1].contains("mcp-protocol-version: 2025-06-18\r\n"));
    assert_eq!(
        request_json(&requests[2]),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "you-search",
                "arguments": {"query": "rust agents", "count": 7},
            },
        })
    );
    assert!(requests[2].contains("mcp-session-id: you-session\r\n"));
}

/// Prevents tool calls when the You.com initialization response does not
/// negotiate the MCP tools capability.
#[test]
fn you_mcp_requires_negotiated_tools_capability() {
    let server = FixtureServer::once(
        "200 OK",
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":null},"serverInfo":{"name":"you","version":"1"}}}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        you_endpoint: Some(format!("{}mcp?profile=free", server.origin)),
        ..config()
    });
    let error = client
        .call(
            WebAdapter::You,
            request(
                WebOperation::Search,
                "rust agents",
                7,
                "",
                &AtomicBool::new(false),
            ),
        )
        .expect_err("missing tools capability");
    assert_eq!(
        error,
        "you invalid response: initialize did not negotiate tools capability"
    );
    let request = server.finish();
    assert!(request.contains("\"method\":\"initialize\""));
}

/// Ensures cancellation after initialization prevents both the initialized
/// notification and the quota-bearing You.com tool call.
#[test]
fn you_mcp_cancellation_after_initialize_stops_later_requests() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let server = FixtureServer::sequence_with_cancellation(
        vec![(
            "200 OK".to_owned(),
            "Mcp-Session-Id: you-session\r\n".to_owned(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"you","version":"1"}}}"#.to_owned(),
        )],
        Some(Arc::clone(&cancelled)),
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        you_endpoint: Some(format!("{}mcp?profile=free", server.origin)),
        ..config()
    });
    let error = client
        .call(
            WebAdapter::You,
            request(
                WebOperation::Search,
                "rust agents",
                7,
                "",
                cancelled.as_ref(),
            ),
        )
        .expect_err("cancelled You.com call");
    assert_eq!(error, "you MCP request cancelled");
    let requests = server.finish_all();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("\"method\":\"initialize\""));
}

/// Locks Brave authentication, bounded result count, and projection to
/// `web.results` without leaking response bookkeeping.
#[test]
fn brave_search_fixture_is_exact() {
    let server = FixtureServer::once(
        "200 OK",
        r#"{"query":{"original":"rust"},"web":{"results":[{"title":"Rust","url":"https://www.rust-lang.org/","description":"A language"}]}}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        brave_endpoint: Some(format!("{}search", server.origin)),
        brave_api_key: Some("brave-fixture-secret".to_owned()),
        ..config()
    });
    let output = client
        .call(
            WebAdapter::Brave,
            request(
                WebOperation::Search,
                "rust language",
                99,
                "",
                &AtomicBool::new(false),
            ),
        )
        .expect("Brave fixture");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).expect("normalized JSON"),
        serde_json::json!([{
            "title": "Rust",
            "url": "https://www.rust-lang.org/",
            "description": "A language"
        }])
    );
    let request = server.finish();
    assert!(
        request
            .starts_with("GET /search?q=rust%20language&count=20&extra_snippets=true HTTP/1.1\r\n")
    );
    assert!(request.contains("x-subscription-token: brave-fixture-secret\r\n"));
}

/// Locks Tavily fetch to the documented `/extract` request and `results`
/// projection.
#[test]
fn tavily_fetch_fixture_is_exact() {
    let server = FixtureServer::once(
        "200 OK",
        r#"{"results":[{"url":"https://example.test/","raw_content":"page"}],"failed_results":[]}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        tavily_endpoint: Some(server.origin.clone()),
        tavily_api_key: Some("tavily-fixture-secret".to_owned()),
        ..config()
    });
    let output = client
        .call(
            WebAdapter::Tavily,
            request(
                WebOperation::Fetch,
                "",
                0,
                "https://example.test/",
                &AtomicBool::new(false),
            ),
        )
        .expect("Tavily fixture");
    assert!(output.contains("\"raw_content\": \"page\""));
    let request = server.finish();
    assert!(request.starts_with("POST /extract HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer tavily-fixture-secret\r\n"));
    assert_eq!(
        request_json(&request),
        serde_json::json!({
            "urls": ["https://example.test/"],
            "format": "markdown",
        })
    );
}

/// Locks Tavily search to the documented `/search` request, clamps its
/// provider-specific maximum, and projects only ranked results.
#[test]
fn tavily_search_fixture_is_exact() {
    let server = FixtureServer::once(
        "200 OK",
        r#"{"query":"tau","results":[{"title":"Tau","url":"https://example.test/tau","content":"agent"}],"response_time":0.1}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        tavily_endpoint: Some(server.origin.clone()),
        tavily_api_key: Some("tavily-fixture-secret".to_owned()),
        ..config()
    });
    let output = client
        .call(
            WebAdapter::Tavily,
            request(
                WebOperation::Search,
                "tau agent",
                100,
                "",
                &AtomicBool::new(false),
            ),
        )
        .expect("Tavily fixture");
    assert!(!output.contains("response_time"));
    assert!(output.contains("\"content\": \"agent\""));
    let request = server.finish();
    assert!(request.starts_with("POST /search HTTP/1.1\r\n"));
    assert_eq!(
        request_json(&request),
        serde_json::json!({
            "query": "tau agent",
            "max_results": 20,
            "search_depth": "basic",
        })
    );
}

/// Locks Firecrawl search to the v2 `/search` shape and projects only web
/// results from the provider envelope.
#[test]
fn firecrawl_search_fixture_is_exact() {
    let server = FixtureServer::once(
        "200 OK",
        r#"{"success":true,"data":{"web":[{"title":"Tau","url":"https://example.test/tau","description":"agent"}]},"creditsUsed":1}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        firecrawl_endpoint: Some(server.origin.clone()),
        firecrawl_api_key: Some("firecrawl-fixture-secret".to_owned()),
        ..config()
    });
    let output = client
        .call(
            WebAdapter::Firecrawl,
            request(
                WebOperation::Search,
                "tau agent",
                5,
                "",
                &AtomicBool::new(false),
            ),
        )
        .expect("Firecrawl fixture");
    assert!(!output.contains("creditsUsed"));
    assert!(output.contains("\"title\": \"Tau\""));
    let request = server.finish();
    assert!(request.starts_with("POST /search HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer firecrawl-fixture-secret\r\n"));
    assert_eq!(
        request_json(&request),
        serde_json::json!({"query": "tau agent", "limit": 5})
    );
}

/// Locks Firecrawl fetch to the v2 `/scrape` request and returns only bounded
/// Markdown rather than provider accounting or metadata.
#[test]
fn firecrawl_fetch_fixture_is_exact() {
    let server = FixtureServer::once(
        "200 OK",
        r##"{"success":true,"data":{"markdown":"# Page\n\nBody","metadata":{"sourceURL":"https://example.test/"}}}"##,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        firecrawl_endpoint: Some(format!("{}v2", server.origin)),
        firecrawl_api_key: Some("firecrawl-fixture-secret".to_owned()),
        ..config()
    });
    let output = client
        .call(
            WebAdapter::Firecrawl,
            request(
                WebOperation::Fetch,
                "",
                0,
                "https://example.test/",
                &AtomicBool::new(false),
            ),
        )
        .expect("Firecrawl fixture");
    assert_eq!(output, "# Page\n\nBody");
    let request = server.finish();
    assert!(request.starts_with("POST /v2/scrape HTTP/1.1\r\n"));
    assert_eq!(
        request_json(&request),
        serde_json::json!({
            "url": "https://example.test/",
            "formats": [{"type": "markdown"}],
        })
    );
}

/// Ensures provider diagnostics redact both endpoint query material and API
/// keys even when a hostile error body echoes them.
#[test]
fn hosted_errors_redact_endpoint_and_credentials() {
    let server = FixtureServer::once(
        "500 Internal Server Error",
        r#"{"error":"brave-fixture-secret endpoint-token"}"#,
    );
    let client = HttpHostedClient::default();
    client.configure(HostedConfig {
        brave_endpoint: Some(format!("{}search?token=endpoint-token", server.origin)),
        brave_api_key: Some("brave-fixture-secret".to_owned()),
        ..config()
    });
    let error = client
        .call(
            WebAdapter::Brave,
            request(
                WebOperation::Search,
                "query",
                5,
                "",
                &AtomicBool::new(false),
            ),
        )
        .expect_err("hosted error");
    assert!(!error.contains("brave-fixture-secret"));
    assert!(!error.contains("endpoint-token"));
    let _ = server.finish();
}
