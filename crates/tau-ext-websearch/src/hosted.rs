//! HTTP adapters for You.com, Brave, Tavily, and Firecrawl.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tau_proto::SecretValue;

use super::{
    DEFAULT_BRAVE_ENDPOINT, DEFAULT_FIRECRAWL_ENDPOINT, DEFAULT_TAVILY_ENDPOINT,
    DEFAULT_YOU_ENDPOINT, HTTP_TOO_MANY_REQUESTS, MCP_PROTOCOL_VERSION, RATE_LIMITED_ERROR,
    WebAdapter, decode_mcp_text_result, limit_tool_output, parse_sse_or_json, provider_http_agent,
    read_capped, read_success_body, sanitize_endpoint_error,
};

const YOU_SEARCH_TOOL: &str = "you-search";

/// Runtime updates for the additional hosted providers.
pub(super) struct HostedConfig {
    /// You.com MCP endpoint.
    pub(super) you_endpoint: String,
    /// Optional You.com bearer token.
    pub(super) you_api_key: Option<SecretValue>,
    /// Optional Brave search endpoint.
    pub(super) brave_endpoint: Option<String>,
    /// Optional Brave subscription token.
    pub(super) brave_api_key: Option<SecretValue>,
    /// Optional Tavily API base endpoint.
    pub(super) tavily_endpoint: Option<String>,
    /// Optional Tavily bearer token.
    pub(super) tavily_api_key: Option<SecretValue>,
    /// Optional Firecrawl API base endpoint.
    pub(super) firecrawl_endpoint: Option<String>,
    /// Optional Firecrawl bearer token.
    pub(super) firecrawl_api_key: Option<SecretValue>,
}

/// Provider seam used by the composite scheduler.
pub(super) trait HostedClient: Send + Sync + 'static {
    /// Issue one normalized search or fetch request.
    fn call(&self, provider: WebAdapter, attempt: HostedAttempt<'_>) -> Result<String, String>;

    /// Apply a fully validated runtime configuration.
    fn configure(&self, _config: HostedConfig) {}
}

/// Validated operation-specific provider request.
pub(super) enum HostedRequest<'a> {
    /// Search query with optional provider-side domain enforcement.
    Search {
        /// Validated natural-language query.
        query: &'a str,
        /// Validated requested result count.
        count: u32,
        /// Harness-authored upstream domain restriction.
        allowed_domains: Option<&'a [String]>,
    },
    /// Caller-directed page extraction.
    Fetch {
        /// Validated absolute HTTP(S) target.
        url: &'a str,
    },
}

/// One scheduler-owned attempt passed through every adapter boundary.
pub(super) struct HostedAttempt<'a> {
    /// Valid operation-specific request.
    pub(super) request: HostedRequest<'a>,
    /// Remaining scheduler-owned attempt budget.
    pub(super) timeout: Duration,
    /// Cooperative cancellation flag for multi-request adapters.
    pub(super) cancelled: &'a AtomicBool,
}

/// Complete in-memory endpoint and credential state for hosted adapters.
#[derive(Clone)]
struct RuntimeConfig {
    /// You.com MCP endpoint.
    you_endpoint: String,
    /// Optional You.com bearer token.
    you_api_key: Option<SecretValue>,
    /// Brave search endpoint.
    brave_endpoint: String,
    /// Brave subscription token.
    brave_api_key: Option<SecretValue>,
    /// Tavily API base endpoint.
    tavily_endpoint: String,
    /// Tavily bearer token.
    tavily_api_key: Option<SecretValue>,
    /// Firecrawl API base endpoint.
    firecrawl_endpoint: String,
    /// Firecrawl bearer token.
    firecrawl_api_key: Option<SecretValue>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            you_endpoint: DEFAULT_YOU_ENDPOINT.to_owned(),
            you_api_key: None,
            brave_endpoint: DEFAULT_BRAVE_ENDPOINT.to_owned(),
            brave_api_key: None,
            tavily_endpoint: DEFAULT_TAVILY_ENDPOINT.to_owned(),
            tavily_api_key: None,
            firecrawl_endpoint: DEFAULT_FIRECRAWL_ENDPOINT.to_owned(),
            firecrawl_api_key: None,
        }
    }
}

/// Production client for the additional hosted providers.
pub(super) struct HttpHostedClient {
    /// Mutable configuration replaced by successful `Configure` messages.
    config: Mutex<RuntimeConfig>,
}

impl Default for HttpHostedClient {
    fn default() -> Self {
        Self {
            config: Mutex::new(RuntimeConfig::default()),
        }
    }
}

impl HostedClient for HttpHostedClient {
    fn call(&self, provider: WebAdapter, attempt: HostedAttempt<'_>) -> Result<String, String> {
        let config = self
            .config
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match (provider, &attempt.request) {
            (
                WebAdapter::You,
                HostedRequest::Search {
                    query,
                    count,
                    allowed_domains: _,
                },
            ) => call_you(
                &config.you_endpoint,
                config.you_api_key.as_ref(),
                query,
                *count,
                attempt.timeout,
                attempt.cancelled,
            ),
            (
                WebAdapter::Brave,
                HostedRequest::Search {
                    query,
                    count,
                    allowed_domains: _,
                },
            ) => call_brave(
                &config.brave_endpoint,
                required_key(&config.brave_api_key, "brave")?,
                query,
                *count,
                attempt.timeout,
            ),
            (WebAdapter::Tavily, _) => call_tavily(
                &config.tavily_endpoint,
                required_key(&config.tavily_api_key, "tavily")?,
                &attempt,
            ),
            (WebAdapter::Firecrawl, _) => call_firecrawl(
                &config.firecrawl_endpoint,
                required_key(&config.firecrawl_api_key, "firecrawl")?,
                &attempt,
            ),
            _ => Err(format!(
                "{} does not support {}",
                provider.as_str(),
                match attempt.request {
                    HostedRequest::Search {
                        query: _,
                        count: _,
                        allowed_domains: _,
                    } => "search",
                    HostedRequest::Fetch { url: _ } => "fetch",
                }
            )),
        }
    }

    fn configure(&self, config: HostedConfig) {
        let mut current = self
            .config
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.you_endpoint = config.you_endpoint;
        current.you_api_key = config.you_api_key;
        if let Some(endpoint) = config.brave_endpoint {
            current.brave_endpoint = endpoint;
        }
        if let Some(endpoint) = config.tavily_endpoint {
            current.tavily_endpoint = endpoint;
        }
        if let Some(endpoint) = config.firecrawl_endpoint {
            current.firecrawl_endpoint = endpoint;
        }
        current.brave_api_key = config.brave_api_key;
        current.tavily_api_key = config.tavily_api_key;
        current.firecrawl_api_key = config.firecrawl_api_key;
    }
}

fn required_key<'a>(
    key: &'a Option<SecretValue>,
    provider: &str,
) -> Result<&'a SecretValue, String> {
    key.as_ref()
        .ok_or_else(|| format!("{provider} credentials are not configured"))
}

fn call_you(
    endpoint: &str,
    api_key: Option<&SecretValue>,
    query: &str,
    count: u32,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "tau-ext-websearch", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    let (payload, session_id) = post_you_mcp(
        endpoint,
        api_key,
        initialize,
        None,
        false,
        remaining(deadline)?,
    )?;
    let initialized = parse_sse_or_json(&payload, "you")
        .map_err(|error| sanitize_optional_secret(&error, endpoint, api_key))?;
    let negotiated = initialized
        .pointer("/result/protocolVersion")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "you invalid response: initialize omitted protocol version".to_owned())?;
    if negotiated != MCP_PROTOCOL_VERSION {
        return Err(format!(
            "you invalid response: unsupported negotiated MCP version `{negotiated}`"
        ));
    }
    if initialized
        .pointer("/result/capabilities/tools")
        .and_then(serde_json::Value::as_object)
        .is_none()
    {
        return Err(
            "you invalid response: initialize did not negotiate tools capability".to_owned(),
        );
    }
    check_cancelled(cancelled)?;
    post_you_mcp(
        endpoint,
        api_key,
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
        session_id.as_deref(),
        true,
        remaining(deadline)?,
    )?;
    check_cancelled(cancelled)?;
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": YOU_SEARCH_TOOL,
            "arguments": {"query": query, "count": count},
        },
    });
    let (payload, _) = post_you_mcp(
        endpoint,
        api_key,
        call,
        session_id.as_deref(),
        true,
        remaining(deadline)?,
    )?;
    let text = decode_mcp_text_result(&payload, "you")
        .map_err(|error| sanitize_optional_secret(&error, endpoint, api_key))?;
    limit_tool_output(text, "you")
        .map_err(|error| sanitize_optional_secret(&error, endpoint, api_key))
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("you MCP request cancelled".to_owned())
    } else {
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "you MCP request timed out".to_owned())
}

fn post_you_mcp(
    endpoint: &str,
    api_key: Option<&SecretValue>,
    body: serde_json::Value,
    session_id: Option<&str>,
    negotiated: bool,
    timeout: Duration,
) -> Result<(String, Option<String>), String> {
    let agent = provider_http_agent(timeout);
    let mut request = agent
        .post(endpoint)
        .content_type("application/json")
        .header("Accept", "application/json, text/event-stream");
    if negotiated {
        request = request.header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION);
    }
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    if let Some(api_key) = api_key {
        request = request.header(
            "Authorization",
            &format!("Bearer {}", api_key.expose_secret()),
        );
    }
    let mut response = request.send(body.to_string()).map_err(|error| {
        format!(
            "you MCP transport error: {}",
            sanitize_optional_secret(&error.to_string(), endpoint, api_key)
        )
    })?;
    if response.status().as_u16() == HTTP_TOO_MANY_REQUESTS {
        return Err(RATE_LIMITED_ERROR.to_owned());
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = sanitize_optional_secret(
            &read_capped(response.body_mut().as_reader()),
            endpoint,
            api_key,
        );
        return Err(format!("you MCP returned HTTP {status}: {body}"));
    }
    let response_session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    read_success_body(response.body_mut().as_reader(), "you")
        .map(|payload| (payload, response_session))
}

fn sanitize_optional_secret(message: &str, endpoint: &str, key: Option<&SecretValue>) -> String {
    let message = key.map_or_else(
        || message.to_owned(),
        |key| message.replace(key.expose_secret(), "…"),
    );
    sanitize_endpoint_error(&message, endpoint)
}

fn call_brave(
    endpoint: &str,
    key: &SecretValue,
    query: &str,
    count: u32,
    timeout: Duration,
) -> Result<String, String> {
    let agent = provider_http_agent(timeout);
    let response = agent
        .get(endpoint)
        .query("q", query)
        .query("count", count.min(20).to_string())
        .query("extra_snippets", "true")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key.expose_secret())
        .call()
        .map_err(|error| safe_error("brave", error.to_string(), endpoint, key))?;
    normalize_response(response, "brave", endpoint, key, &["web", "results"])
}

fn call_tavily(
    base: &str,
    key: &SecretValue,
    attempt: &HostedAttempt<'_>,
) -> Result<String, String> {
    let (path, mut body, projection): (&str, serde_json::Value, &[&str]) = match &attempt.request {
        HostedRequest::Search {
            query,
            count,
            allowed_domains: _,
        } => (
            "search",
            serde_json::json!({
                "query": query,
                "max_results": (*count).min(20),
                "search_depth": "basic",
            }),
            &["results"],
        ),
        HostedRequest::Fetch { url } => (
            "extract",
            serde_json::json!({"urls": [url], "format": "markdown"}),
            &["results"],
        ),
    };
    if let HostedRequest::Search {
        query: _,
        count: _,
        allowed_domains: Some(domains),
    } = &attempt.request
    {
        body.as_object_mut()
            .expect("object literal")
            .insert("include_domains".to_owned(), serde_json::json!(domains));
    }
    let endpoint = endpoint_path(base, path)?;
    post_json(&endpoint, key, "tavily", body, attempt.timeout, projection)
}

fn call_firecrawl(
    base: &str,
    key: &SecretValue,
    attempt: &HostedAttempt<'_>,
) -> Result<String, String> {
    let (path, mut body, projection): (&str, serde_json::Value, &[&str]) = match &attempt.request {
        HostedRequest::Search {
            query,
            count,
            allowed_domains: _,
        } => (
            "search",
            serde_json::json!({"query": query, "limit": count}),
            &["data", "web"],
        ),
        HostedRequest::Fetch { url } => (
            "scrape",
            serde_json::json!({"url": url, "formats": [{"type": "markdown"}]}),
            &["data", "markdown"],
        ),
    };
    if let HostedRequest::Search {
        query: _,
        count: _,
        allowed_domains: Some(domains),
    } = &attempt.request
    {
        body.as_object_mut()
            .expect("object literal")
            .insert("includeDomains".to_owned(), serde_json::json!(domains));
    }
    let endpoint = endpoint_path(base, path)?;
    post_json(
        &endpoint,
        key,
        "firecrawl",
        body,
        attempt.timeout,
        projection,
    )
}

fn endpoint_path(base: &str, path: &str) -> Result<String, String> {
    let mut base = url::Url::parse(base)
        .map_err(|_| "configured provider endpoint cannot accept an API path".to_owned())?;
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.join(path)
        .map(String::from)
        .map_err(|_| "configured provider endpoint cannot accept an API path".to_owned())
}

fn post_json(
    endpoint: &str,
    key: &SecretValue,
    provider: &str,
    body: serde_json::Value,
    timeout: Duration,
    path: &[&str],
) -> Result<String, String> {
    let response = provider_http_agent(timeout)
        .post(endpoint)
        .content_type("application/json")
        .header("Accept", "application/json")
        .header("Authorization", &format!("Bearer {}", key.expose_secret()))
        .send(body.to_string())
        .map_err(|error| safe_error(provider, error.to_string(), endpoint, key))?;
    normalize_response(response, provider, endpoint, key, path)
}

fn normalize_response(
    mut response: ureq::http::Response<ureq::Body>,
    provider: &str,
    endpoint: &str,
    key: &SecretValue,
    path: &[&str],
) -> Result<String, String> {
    if response.status().as_u16() == HTTP_TOO_MANY_REQUESTS {
        return Err(RATE_LIMITED_ERROR.to_owned());
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = read_capped(response.body_mut().as_reader());
        return Err(safe_diagnostic(
            format!("{provider} API returned HTTP {status}: {body}"),
            endpoint,
            key,
        ));
    }
    let payload = read_success_body(response.body_mut().as_reader(), provider)?;
    let json: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|error| format!("{provider} invalid response: invalid JSON: {error}"))?;
    let selected = path.iter().try_fold(&json, |value, segment| {
        value
            .get(*segment)
            .ok_or_else(|| format!("{provider} invalid response: omitted `{}`", path.join(".")))
    })?;
    let text = match selected {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(values) if values.is_empty() => String::new(),
        serde_json::Value::Object(values) if values.is_empty() => String::new(),
        selected => serde_json::to_string_pretty(selected).map_err(|error| {
            format!("{provider} invalid response: normalization failed: {error}")
        })?,
    };
    limit_tool_output(text, provider)
}

fn safe_error(provider: &str, message: String, endpoint: &str, key: &SecretValue) -> String {
    format!(
        "{provider} transport error: {}",
        safe_diagnostic(message, endpoint, key)
    )
}

fn safe_diagnostic(message: String, endpoint: &str, key: &SecretValue) -> String {
    let key = key.expose_secret();
    sanitize_endpoint_error(&message.replace(key, "…"), endpoint)
        .replace(&format!("Bearer {key}"), "Bearer …")
        .replace(key, "…")
}
