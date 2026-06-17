//! Generic web-search extension backed by hosted MCP search providers.
//!
//! The extension registers Exa-backed `web_search` by default and also exposes
//! Parallel.ai-backed `web_search` / `web_fetch` tools. The Parallel tools use
//! collision-free Tau-internal names and are disabled by default so roles can
//! opt into them without creating a duplicate model-visible `web_search`.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_proto::{
    CborValue, ConfigError, Event, HarnessInputMessage, HarnessOutputMessage, PeerInputReader,
    PeerOutputWriter, ToolError, ToolProgress, ToolResult, ToolSpec, ToolStarted, ToolUseState,
    ToolUseStats, ToolUseStatus,
};
/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "websearch";

/// Tau-internal tool name for the default Exa web search.
pub const EXA_TOOL_NAME: &str = "websearch_exa";

/// Backwards-compatible alias for the default Exa tool name.
pub const TOOL_NAME: &str = EXA_TOOL_NAME;

/// Tau-internal tool name for Parallel web search.
pub const PARALLEL_SEARCH_TOOL_NAME: &str = "websearch_parallel_search";

/// Tau-internal tool name for Parallel web fetch.
pub const PARALLEL_FETCH_TOOL_NAME: &str = "websearch_parallel_fetch";

/// Tool name advertised to models for search tools.
pub const MODEL_VISIBLE_SEARCH_TOOL_NAME: &str = "web_search";

/// Backwards-compatible alias for the default search model-visible name.
pub const MODEL_VISIBLE_TOOL_NAME: &str = MODEL_VISIBLE_SEARCH_TOOL_NAME;

/// Tool name advertised to models for web fetch.
pub const MODEL_VISIBLE_FETCH_TOOL_NAME: &str = "web_fetch";

/// Default Exa MCP endpoint. Override via `config.endpoint` or
/// `config.exa_endpoint`.
pub const DEFAULT_EXA_ENDPOINT: &str = "https://mcp.exa.ai/mcp";

/// Backwards-compatible alias for the default Exa endpoint.
pub const DEFAULT_ENDPOINT: &str = DEFAULT_EXA_ENDPOINT;

/// Default unauthenticated Parallel Search MCP endpoint.
pub const DEFAULT_PARALLEL_ENDPOINT: &str = "https://search.parallel.ai/mcp";

const EXA_REMOTE_TOOL: &str = "web_search_exa";
const PARALLEL_REMOTE_SEARCH_TOOL: &str = "web_search";
const PARALLEL_REMOTE_FETCH_TOOL: &str = "web_fetch";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_NUM_RESULTS: u32 = 5;
const MAX_NUM_RESULTS: u32 = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_IN_FLIGHT: usize = 8;
const ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

/// Run the extension over stdio.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied reader/writer pair.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_clients(
        reader,
        writer,
        Arc::new(HttpExaSearcher::default()),
        Arc::new(HttpParallelClient::default()),
    )
}

/// Performs one Exa search. Abstracted so tests can stub the network call.
pub trait Searcher: Send + Sync + 'static {
    /// Search Exa for `query`, returning model-ready text.
    fn search(&self, query: &str, num_results: u32) -> Result<String, String>;

    /// Apply a runtime endpoint update from a harness `Configure`.
    fn set_endpoint(&self, _endpoint: String) {}
}

/// Performs one Parallel MCP tool call. Abstracted so tests can stub the
/// network call without contacting Parallel.ai.
pub trait ParallelClient: Send + Sync + 'static {
    /// Call one remote Parallel MCP tool with JSON arguments.
    fn call(&self, remote_tool: &str, arguments: serde_json::Value) -> Result<String, String>;

    /// Apply a runtime endpoint update from a harness `Configure`.
    fn set_endpoint(&self, _endpoint: String) {}
}

/// Extension-side config carried in `HarnessOutputMessage::Configure.config`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Backwards-compatible Exa endpoint override.
    endpoint: Option<String>,
    /// Explicit Exa endpoint override.
    exa_endpoint: Option<String>,
    /// Parallel endpoint override. No API-key/auth configuration is supported;
    /// Tau uses Parallel's default unauthenticated endpoint.
    parallel_endpoint: Option<String>,
}

fn run_with_clients<R, W>(
    reader: R,
    writer: W,
    searcher: Arc<dyn Searcher>,
    parallel_client: Arc<dyn ParallelClient>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));

    tau_extension::Handshake::tool("tau-ext-websearch")
        .subscribe([tau_proto::EventName::TOOL_STARTED])
        .register_tool(exa_tool_spec())
        .register_tool(parallel_search_tool_spec())
        .register_tool(parallel_fetch_tool_spec())
        .ready_message("websearch ready")
        .run(&mut writer)?;

    let (tx, rx) = mpsc::channel::<HarnessInputMessage>();
    let sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT));

    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for message in rx {
            writer
                .write_message(&message)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::Configure(msg) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                    Ok(cfg) => {
                        if let Some(endpoint) = cfg.endpoint.or(cfg.exa_endpoint) {
                            tracing::info!(target: LOG_TARGET, endpoint = %endpoint, "applying Exa endpoint override");
                            searcher.set_endpoint(endpoint);
                        }
                        if let Some(endpoint) = cfg.parallel_endpoint {
                            tracing::info!(target: LOG_TARGET, endpoint = %endpoint, "applying Parallel endpoint override");
                            parallel_client.set_endpoint(endpoint);
                        }
                    }
                    Err(message) => {
                        tracing::warn!(target: LOG_TARGET, error = %message, "rejecting config");
                        let _ = tx.send(HarnessInputMessage::ConfigError(ConfigError { message }));
                    }
                }
            }
            HarnessOutputMessage::Deliver(delivery) => {
                // Tool invocations are execution triggers; replay-marked
                // frames re-send history and must not re-run searches.
                if delivery.is_replay() {
                    continue;
                }
                if let Event::ToolStarted(invoke) = delivery.into_event() {
                    if !is_websearch_tool(invoke.tool_name.as_str()) {
                        continue;
                    }
                    let permit = sem.acquire();
                    let tx = tx.clone();
                    let searcher = Arc::clone(&searcher);
                    let parallel_client = Arc::clone(&parallel_client);
                    std::thread::spawn(move || {
                        let _permit = permit;
                        dispatch_tool_invoke(
                            invoke,
                            searcher.as_ref(),
                            parallel_client.as_ref(),
                            &tx,
                        );
                    });
                }
            }
            HarnessOutputMessage::Disconnect(_) => break,
            _ => {}
        }
    }

    drop(tx);
    writer_handle
        .join()
        .map_err(|e| -> Box<dyn Error> { format!("writer thread panicked: {e:?}").into() })?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

fn is_websearch_tool(name: &str) -> bool {
    matches!(
        name,
        EXA_TOOL_NAME | PARALLEL_SEARCH_TOOL_NAME | PARALLEL_FETCH_TOOL_NAME
    )
}

fn exa_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(EXA_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(MODEL_VISIBLE_SEARCH_TOOL_NAME)),
        description: Some(
            "Search the web via Exa's free-tier hosted MCP. Returns clean, ready-to-use \
             text content (titles, URLs, highlights) from top-ranked pages. Works best with a \
             natural-language description of the *ideal page* rather than a keyword query — \
             e.g. \"blog post comparing React and Vue performance\" beats \"React vs Vue\". \
             Use category:people / category:company prefixes to scope results to LinkedIn-style \
             profiles or company pages."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of the ideal page. May start with `category:people` or `category:company` to focus the result set."
                },
                "num_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_NUM_RESULTS,
                    "description": format!("Number of results to return (default: {DEFAULT_NUM_RESULTS}, max: {MAX_NUM_RESULTS}).")
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
    }
}

fn parallel_search_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(MODEL_VISIBLE_SEARCH_TOOL_NAME)),
        description: Some(
            "Search the web via Parallel.ai's unauthenticated Search MCP endpoint. Returns concise web results suitable for answering current-information questions."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query or natural-language description of the information to find."
                }
            },
            "required": ["query"]
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
    }
}

fn parallel_fetch_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(PARALLEL_FETCH_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(MODEL_VISIBLE_FETCH_TOOL_NAME)),
        description: Some(
            "Fetch and extract a web page via Parallel.ai's unauthenticated Search MCP endpoint. Use after web_search when a specific URL needs more detail."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch."
                }
            },
            "required": ["url"]
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: false,
        background_support: None,
    }
}

fn dispatch_tool_invoke(
    invoke: ToolStarted,
    searcher: &dyn Searcher,
    parallel_client: &dyn ParallelClient,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    if let Some(display) = initial_display(&invoke) {
        let _ = tx.send(HarnessInputMessage::emit(Event::ToolProgress(
            ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: None,
                progress: None,
                display: Some(display),
            },
        )));
    }
    let event = match invoke.tool_name.as_str() {
        EXA_TOOL_NAME => dispatch_exa(invoke, searcher),
        PARALLEL_SEARCH_TOOL_NAME => {
            dispatch_parallel(invoke, parallel_client, PARALLEL_REMOTE_SEARCH_TOOL)
        }
        PARALLEL_FETCH_TOOL_NAME => {
            dispatch_parallel(invoke, parallel_client, PARALLEL_REMOTE_FETCH_TOOL)
        }
        _ => Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            display: Some(error_display("unknown tool")),
            message: "unknown tool".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    };
    let _ = tx.send(HarnessInputMessage::emit(event));
}

fn initial_display(invoke: &ToolStarted) -> Option<ToolUseState> {
    let args = match invoke.tool_name.as_str() {
        EXA_TOOL_NAME => parse_exa_args(&invoke.arguments)
            .map(|(query, _)| query)
            .unwrap_or_default(),
        PARALLEL_SEARCH_TOOL_NAME => {
            cbor_text_field(&invoke.arguments, "query").unwrap_or_default()
        }
        PARALLEL_FETCH_TOOL_NAME => cbor_text_field(&invoke.arguments, "url").unwrap_or_default(),
        _ => return None,
    };
    Some(ToolUseState {
        args,
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    })
}

fn cbor_text_field(arguments: &CborValue, key: &str) -> Option<String> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                Some(value.clone())
            }
            _ => None,
        })
}

fn dispatch_exa(invoke: ToolStarted, searcher: &dyn Searcher) -> Event {
    match parse_exa_args(&invoke.arguments) {
        Ok((query, num_results)) => match searcher.search(&query, num_results) {
            Ok(text) => {
                tracing::debug!(target: LOG_TARGET, query = %query, num_results, response_len = text.len(), "exa search returned");
                Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(text.clone()),
                    kind: tau_proto::ToolResultKind::Final,
                    display: Some(exa_ok_display(&text)),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(message) => tool_error(invoke, message),
        },
        Err(message) => tool_error(invoke, message),
    }
}

fn dispatch_parallel(
    invoke: ToolStarted,
    client: &dyn ParallelClient,
    remote_tool: &'static str,
) -> Event {
    match cbor_to_json(&invoke.arguments) {
        Ok(arguments) => match client.call(remote_tool, arguments) {
            Ok(text) => {
                tracing::debug!(target: LOG_TARGET, remote_tool, response_len = text.len(), "parallel search MCP returned");
                Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(text.clone()),
                    kind: tau_proto::ToolResultKind::Final,
                    display: Some(ok_display(&text)),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(message) => tool_error(invoke, message),
        },
        Err(message) => tool_error(invoke, message),
    }
}

fn tool_error(invoke: ToolStarted, message: String) -> Event {
    Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        display: Some(error_display(&message)),
        message,
        details: Some(invoke.arguments),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn ok_display(response: &str) -> ToolUseState {
    let has_response = !response.is_empty();
    ToolUseState {
        args: String::new(),
        stats: ToolUseStats {
            matches: None,
            lines: has_response.then_some(response.lines().count() as u64),
            bytes: has_response.then_some(response.len() as u64),
        },
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

fn exa_ok_display(response: &str) -> ToolUseState {
    let mut display = ok_display(response);
    let titles = response
        .lines()
        .filter(|line| line.starts_with("Title:"))
        .count();
    let urls = response
        .lines()
        .filter(|line| line.starts_with("URL:"))
        .count();
    display.stats.matches = (0 < titles.max(urls)).then_some(titles.max(urls) as u64);
    display
}

fn error_display(message: &str) -> ToolUseState {
    let status_text = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned();
    ToolUseState {
        args: String::new(),
        status: ToolUseStatus::Error,
        status_text,
        ..Default::default()
    }
}

fn parse_exa_args(arguments: &CborValue) -> Result<(String, u32), String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut query: Option<String> = None;
    let mut num_results: Option<u32> = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "query" => match v {
                CborValue::Text(text) => query = Some(text.clone()),
                _ => return Err("`query` must be a string".to_owned()),
            },
            "num_results" => num_results = Some(parse_num_results(v)?),
            _ => {}
        }
    }
    let query = query.ok_or_else(|| "missing string argument: query".to_owned())?;
    if query.trim().is_empty() {
        return Err("`query` must not be empty".to_owned());
    }
    Ok((query, num_results.unwrap_or(DEFAULT_NUM_RESULTS)))
}

fn parse_num_results(value: &CborValue) -> Result<u32, String> {
    let raw: i128 = match value {
        CborValue::Integer(n) => (*n).into(),
        CborValue::Float(f) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err("`num_results` must be an integer".to_owned());
            }
            *f as i128
        }
        _ => return Err("`num_results` must be an integer".to_owned()),
    };
    if raw < 1 {
        return Err("`num_results` must be >= 1".to_owned());
    }
    if raw > i128::from(MAX_NUM_RESULTS) {
        return Err(format!("`num_results` must be <= {MAX_NUM_RESULTS}"));
    }
    Ok(raw as u32)
}

fn cbor_to_json(value: &CborValue) -> Result<serde_json::Value, String> {
    match value {
        CborValue::Null => Ok(serde_json::Value::Null),
        CborValue::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            if let Ok(n) = i64::try_from(n) {
                Ok(serde_json::Value::Number(n.into()))
            } else if let Ok(n) = u64::try_from(n) {
                Ok(serde_json::Value::Number(n.into()))
            } else {
                Err("integer argument is outside JSON number range".to_owned())
            }
        }
        CborValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "float argument must be finite".to_owned()),
        CborValue::Text(s) => Ok(serde_json::Value::String(s.clone())),
        CborValue::Bytes(_) => Err("byte string arguments are not supported".to_owned()),
        CborValue::Array(items) => items
            .iter()
            .map(cbor_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let CborValue::Text(key) = key else {
                    return Err("argument object keys must be strings".to_owned());
                };
                map.insert(key.clone(), cbor_to_json(value)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => Err("unsupported CBOR argument value".to_owned()),
    }
}

struct HttpExaSearcher {
    endpoint: Mutex<String>,
    agent: ureq::Agent,
}

impl Default for HttpExaSearcher {
    fn default() -> Self {
        Self::new(DEFAULT_EXA_ENDPOINT.to_owned())
    }
}

impl HttpExaSearcher {
    fn new(endpoint: String) -> Self {
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .tls_config(tls_config)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        Self {
            endpoint: Mutex::new(endpoint),
            agent,
        }
    }
}

impl Searcher for HttpExaSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        let endpoint = self
            .endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": EXA_REMOTE_TOOL,
                "arguments": {
                    "query": query,
                    "numResults": num_results,
                },
            },
        });
        let payload = post_mcp(&self.agent, &endpoint, body, "exa")?;
        decode_mcp_text_result(&payload, "exa")
    }

    fn set_endpoint(&self, endpoint: String) {
        *self.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = endpoint;
    }
}

struct HttpParallelClient {
    endpoint: Mutex<String>,
    agent: ureq::Agent,
}

impl Default for HttpParallelClient {
    fn default() -> Self {
        Self::new(DEFAULT_PARALLEL_ENDPOINT.to_owned())
    }
}

impl HttpParallelClient {
    fn new(endpoint: String) -> Self {
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .tls_config(tls_config)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        Self {
            endpoint: Mutex::new(endpoint),
            agent,
        }
    }
}

impl ParallelClient for HttpParallelClient {
    fn call(&self, remote_tool: &str, arguments: serde_json::Value) -> Result<String, String> {
        let endpoint = self
            .endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": remote_tool,
                "arguments": arguments,
            },
        });
        let payload = post_mcp(&self.agent, &endpoint, body, "parallel")?;
        decode_mcp_text_result(&payload, "parallel")
    }

    fn set_endpoint(&self, endpoint: String) {
        *self.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = endpoint;
    }
}

fn post_mcp(
    agent: &ureq::Agent,
    endpoint: &str,
    body: serde_json::Value,
    provider: &str,
) -> Result<String, String> {
    let response = agent
        .post(endpoint)
        .content_type("application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .send(body.to_string())
        .map_err(|e| format!("{provider} MCP transport error: {e}"))?;
    let mut response = response;
    if !response.status().is_success() {
        let code = response.status().as_u16();
        let body = read_capped(response.body_mut().as_reader());
        return Err(format!("{provider} MCP returned HTTP {code}: {body}"));
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("reading {provider} MCP response: {e}"))
}

fn read_capped(reader: impl std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = reader
        .take(ERROR_BODY_MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > ERROR_BODY_MAX_BYTES;
    if truncated {
        buf.truncate(ERROR_BODY_MAX_BYTES);
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str("… (truncated)");
    }
    s
}

fn decode_mcp_text_result(payload: &str, provider: &str) -> Result<String, String> {
    let json = parse_sse_or_json(payload, provider)?;
    if let Some(error) = json.get("error") {
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("MCP returned a JSON-RPC error");
        return Err(message.to_owned());
    }
    let content = json
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| format!("{provider} MCP response missing `result.content`"))?;
    let mut chunks = Vec::new();
    for part in content {
        if part.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            chunks.push(text.to_owned());
        }
    }
    if chunks.is_empty() {
        return Err(format!("{provider} MCP response had no text content"));
    }
    Ok(chunks.join("\n\n"))
}

fn parse_sse_or_json(payload: &str, provider: &str) -> Result<serde_json::Value, String> {
    let trimmed = payload.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON from {provider} MCP: {e}"));
    }
    let mut buf = String::new();
    for line in payload.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            buf.push_str(rest.trim_start());
            buf.push('\n');
        } else if line.is_empty() && !buf.is_empty() {
            return serde_json::from_str(buf.trim())
                .map_err(|e| format!("invalid JSON from {provider} MCP SSE frame: {e}"));
        }
    }
    if buf.is_empty() {
        return Err(format!("{provider} MCP returned no SSE data frames"));
    }
    serde_json::from_str(buf.trim())
        .map_err(|e| format!("invalid JSON from {provider} MCP SSE frame: {e}"))
}

struct Semaphore {
    state: Mutex<usize>,
    cond: Condvar,
}

struct OwnedPermit(Arc<Semaphore>);

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(permits),
            cond: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> OwnedPermit {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count == 0 {
            count = self.cond.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        OwnedPermit(Arc::clone(self))
    }
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        let mut count = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
        self.0.cond.notify_one();
    }
}

#[cfg(test)]
mod tests;
