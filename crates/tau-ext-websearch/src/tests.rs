use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, BufReader as IoBufReader, BufWriter, Read as _};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use tau_proto::{
    ConfigError, Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, ToolStarted,
};

use super::*;

/// Test-side wrapper around [`HarnessInputReader`] that exposes helpers for
/// protocol events and selected non-event control messages.
struct EventReader<R> {
    inner: HarnessInputReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: HarnessInputReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                None => return Ok(None),
                Some(HarnessInputMessage::Emit(emit)) => match *emit.event {
                    Event::ToolProgress(progress)
                        if progress.message.is_none() && progress.display.is_some() =>
                    {
                        continue;
                    }
                    event => return Ok(Some(event)),
                },
                Some(_) => continue,
            }
        }
    }

    fn read_config_error(&mut self) -> Result<Option<ConfigError>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                None => return Ok(None),
                Some(HarnessInputMessage::ConfigError(err)) => return Ok(Some(err)),
                Some(_) => continue,
            }
        }
    }
}

/// Test-side wrapper around [`HarnessOutputWriter`] that accepts `Event`
/// directly.
struct EventWriter<W> {
    inner: HarnessOutputWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: HarnessOutputWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner
            .write_message(&HarnessOutputMessage::deliver(event.clone()))
    }

    fn write_message(
        &mut self,
        message: &HarnessOutputMessage,
    ) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_message(message)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct StubSearcher {
    calls: Mutex<Vec<(String, u32)>>,
    endpoints: Mutex<Vec<String>>,
    response: Mutex<Result<String, String>>,
}

impl StubSearcher {
    fn ok(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            endpoints: Mutex::new(Vec::new()),
            response: Mutex::new(Ok(text.into())),
        })
    }

    fn err(message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            endpoints: Mutex::new(Vec::new()),
            response: Mutex::new(Err(message.into())),
        })
    }
}

impl Searcher for StubSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((query.to_owned(), num_results));
        self.response.lock().expect("lock").clone()
    }

    fn set_endpoint(&self, endpoint: String) {
        self.endpoints.lock().expect("lock").push(endpoint);
    }
}

struct BlockingSearcher {
    started: (Mutex<usize>, Condvar),
    release: (Mutex<bool>, Condvar),
}

impl BlockingSearcher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: (Mutex::new(0), Condvar::new()),
            release: (Mutex::new(false), Condvar::new()),
        })
    }

    fn wait_for_started(&self, expected: usize) {
        let (lock, cond) = &self.started;
        let mut started = lock.lock().expect("lock");
        while *started < expected {
            started = cond.wait(started).expect("wait");
        }
    }

    fn release(&self) {
        let (lock, cond) = &self.release;
        *lock.lock().expect("lock") = true;
        cond.notify_all();
    }
}

impl Searcher for BlockingSearcher {
    fn search(&self, _query: &str, _num_results: u32) -> Result<String, String> {
        let (started_lock, started_cond) = &self.started;
        *started_lock.lock().expect("lock") += 1;
        started_cond.notify_all();

        let (release_lock, release_cond) = &self.release;
        let mut release = release_lock.lock().expect("lock");
        while !*release {
            release = release_cond.wait(release).expect("wait");
        }
        Ok("released".to_owned())
    }
}

struct StubParallelClient {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    endpoints: Mutex<Vec<String>>,
    response: Mutex<Result<String, String>>,
}

impl StubParallelClient {
    fn ok(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            endpoints: Mutex::new(Vec::new()),
            response: Mutex::new(Ok(text.into())),
        })
    }
}

impl ParallelClient for StubParallelClient {
    fn call(&self, remote_tool: &str, arguments: serde_json::Value) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((remote_tool.to_owned(), arguments));
        self.response.lock().expect("lock").clone()
    }

    fn set_endpoint(&self, endpoint: String) {
        self.endpoints.lock().expect("lock").push(endpoint);
    }
}

fn spawn_extension(
    searcher: Arc<dyn Searcher>,
    parallel_client: Arc<dyn ParallelClient>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    spawn_extension_with_prefix(searcher, parallel_client, None)
}

fn spawn_extension_with_prefix(
    searcher: Arc<dyn Searcher>,
    parallel_client: Arc<dyn ParallelClient>,
    tool_prefix: Option<tau_proto::ToolNamePrefix>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    thread::spawn(move || {
        // Behavior tests often close the harness side once the expected event is
        // observed. Treat resulting extension I/O errors as teardown, not as a
        // test-thread panic that can abort a later test run.
        let _ = run_with_clients(reader_stream, ext_stream, searcher, parallel_client);
    });
    let reader = EventReader::new(BufReader::new(
        harness_stream.try_clone().expect("harness clone"),
    ));
    let mut writer = EventWriter::new(BufWriter::new(harness_stream));
    let mut configure = configure_message(serde_json::json!({}));
    let HarnessOutputMessage::Configure(configure_frame) = &mut configure else {
        unreachable!();
    };
    configure_frame.tool_prefix = tool_prefix;
    writer
        .write_message(&configure)
        .expect("write initial configure");
    writer.flush().expect("flush initial configure");
    (reader, writer)
}

/// A configured namespace changes wire names without changing first-party
/// dispatch, and terminal events retain the namespaced wire identity.
#[test]
fn prefixed_exa_invocation_dispatches_by_logical_name() {
    let searcher = StubSearcher::ok("prefixed result");
    let (mut reader, mut writer) = spawn_extension_with_prefix(
        searcher.clone(),
        StubParallelClient::ok("unused"),
        Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
    );
    let tools = drain_startup(&mut reader);
    assert_eq!(tools[0].name.as_str(), "work_websearch_exa");

    let mut started = exa_started("prefixed-call", "namespaced query");
    let Event::ToolStarted(invoke) = &mut started else {
        unreachable!();
    };
    invoke.tool_name = tau_proto::ToolName::new("work_websearch_exa");
    writer.write_event(&started).expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.tool_name.as_str(), "work_websearch_exa");
    assert_eq!(
        searcher.calls.lock().expect("calls").as_slice(),
        &[("namespaced query".to_owned(), DEFAULT_NUM_RESULTS)]
    );
}

fn spawn_with_searcher(
    searcher: Arc<dyn Searcher>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    spawn_extension(searcher, StubParallelClient::ok("unused"))
}

fn exa_started(call_id: &str, query: &str) -> Event {
    Event::ToolStarted(ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text(query.to_owned()),
        )]),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn configure_message(config: serde_json::Value) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(tau_proto::Configure {
        tool_prefix: None,
        config: tau_proto::json_to_cbor(&config),
        instance_name: tau_proto::ExtensionName::new("test-extension"),
        state_dir: None,
        secrets: BTreeMap::new(),
    })
}

fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) -> Vec<ToolSpec> {
    // Startup registers Exa plus Parallel search/fetch. Parallel tools are
    // disabled by default so roles can opt into them without duplicating the
    // model-visible `web_search` provided by Exa.
    let mut tools = Vec::new();
    while tools.len() < 3 {
        let event = reader.read_event().expect("read").expect("register");
        let Event::ToolRegister(register) = event else {
            panic!("expected ToolRegister, got {event:?}");
        };
        tools.push(register.tool);
    }
    tools
}

/// Ensures startup advertises the default Exa tool and opt-in Parallel tools
/// without exposing duplicate enabled `web_search` entries.
#[test]
fn registers_exa_by_default_and_parallel_tools_disabled() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, _writer) = spawn_extension(searcher, parallel);

    let tools = drain_startup(&mut reader);
    assert_eq!(tools[0].name.as_str(), EXA_TOOL_NAME);
    assert_eq!(
        tools[0]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_SEARCH_TOOL_NAME)
    );
    assert_eq!(
        tools[0]
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.get("additionalProperties")),
        Some(&serde_json::Value::Bool(false))
    );
    assert!(tools[0].enabled_by_default);

    assert_eq!(tools[1].name.as_str(), PARALLEL_SEARCH_TOOL_NAME);
    assert_eq!(
        tools[1]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_SEARCH_TOOL_NAME)
    );
    assert!(!tools[1].enabled_by_default);

    assert_eq!(tools[2].name.as_str(), PARALLEL_FETCH_TOOL_NAME);
    assert_eq!(
        tools[2]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_FETCH_TOOL_NAME)
    );
    assert!(!tools[2].enabled_by_default);
}

/// Ensures Exa invocations preserve model arguments, return text, and populate
/// display stats for harness UI regressions.
#[test]
fn forwards_query_and_num_results_to_exa_searcher_and_returns_text() {
    let searcher = StubSearcher::ok("Title: hi\nURL: https://x\n");
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("rust async runtime comparison".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(3.into()),
                ),
            ]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "call-1");
    assert_eq!(result.tool_name.as_str(), EXA_TOOL_NAME);
    let CborValue::Text(text) = result.result else {
        panic!("expected Text result");
    };
    assert!(text.contains("Title: hi"));
    let display = result.display.expect("display");
    assert!(display.info_chips.is_empty());
    assert_eq!(display.stats.matches, Some(1));
    assert_eq!(display.stats.lines, Some(2));
    assert_eq!(display.stats.bytes, Some(25));

    let calls = searcher.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "rust async runtime comparison");
    assert_eq!(calls[0].1, 3);
}

/// Ensures Exa searches keep the documented result-count default when callers
/// omit the optional argument.
#[test]
fn defaults_num_results_when_omitted() {
    let searcher = StubSearcher::ok("ok");
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-2".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("hello world".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    assert!(matches!(event, Event::ToolResult(_)));
    assert_eq!(
        searcher.calls.lock().expect("lock")[0].1,
        DEFAULT_NUM_RESULTS,
    );
}

/// Ensures replayed tool-start deliveries do not rerun historical provider
/// requests or emit stale tool replies.
#[test]
fn replayed_tool_started_is_ignored_before_live_search() {
    let searcher = StubSearcher::ok("ok");
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_message(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1_700_000_000_000_000),
            exa_started("replayed-call", "historical query"),
        ))
        .expect("write replay");
    writer
        .write_event(&exa_started("live-call", "live query"))
        .expect("write live");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected live ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "live-call");

    let calls = searcher.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "live query");
}

/// Ensures invalid Exa calls fail as tool errors instead of reaching the
/// network implementation.
#[test]
fn missing_query_returns_tool_error() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-3".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains("query"), "message: {}", err.message);
}

/// Ensures upstream Exa failures are reported to the model as Tau tool errors.
#[test]
fn searcher_error_surfaces_as_tool_error() {
    let searcher = StubSearcher::err("upstream timed out");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("fixture"),
        query_id: "query-error".to_owned(),
    };

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-4".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("anything".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: originator.clone(),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert_eq!(err.message, "upstream timed out");
    assert_eq!(err.originator, originator);
}

/// Ensures tool replies keep the original prompt originator so side prompts and
/// user prompts remain distinguishable after extension execution.
#[test]
fn tool_result_preserves_prompt_originator() {
    let searcher = StubSearcher::ok("ok");
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("fixture"),
        query_id: "query-1".to_owned(),
    };

    let event = dispatch_exa(
        ToolStarted {
            call_id: "call-originator".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("anything".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: originator.clone(),
        },
        searcher.as_ref(),
    );

    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.originator, originator);
}

/// Ensures local Exa argument validation enforces the advertised result-count
/// lower bound.
#[test]
fn rejects_num_results_out_of_range() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-5".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("anything".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(0.into()),
                ),
            ]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains(">= 1"), "message: {}", err.message);
}

/// Ensures excess concurrent searches return a busy error instead of blocking
/// the protocol reader behind the in-flight limit.
///
/// See `SPEC-tau-ext-websearch-runtime-safeguards`.
#[test]
fn returns_busy_error_when_in_flight_limit_is_full() {
    let searcher = BlockingSearcher::new();
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    for idx in 0..MAX_IN_FLIGHT {
        writer
            .write_event(&exa_started(&format!("call-{idx}"), "blocked"))
            .expect("write");
    }
    writer.flush().expect("flush");
    searcher.wait_for_started(MAX_IN_FLIGHT);

    writer
        .write_event(&exa_started("busy-call", "blocked"))
        .expect("write busy");
    writer.flush().expect("flush busy");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        searcher.release();
        panic!("expected ToolError, got {event:?}");
    };
    assert_eq!(err.call_id.as_str(), "busy-call");
    assert!(err.message.contains("busy"), "message: {}", err.message);
    searcher.release();
    for _ in 0..MAX_IN_FLIGHT {
        let event = reader.read_event().expect("read").expect("event");
        assert!(matches!(event, Event::ToolResult(_)), "event: {event:?}");
    }
}

/// Ensures `Disconnect` is handled by the protocol reader even while all
/// in-flight search permits are occupied by blocked network calls.
#[test]
fn disconnect_exits_promptly_while_searches_are_in_flight() {
    let searcher = BlockingSearcher::new();
    let parallel = StubParallelClient::ok("unused");
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    let (done_tx, done_rx) = mpsc::channel();
    let searcher_for_thread: Arc<dyn Searcher> = searcher.clone();
    thread::spawn(move || {
        let result = run_with_clients(reader_stream, ext_stream, searcher_for_thread, parallel);
        done_tx.send(result.is_ok()).expect("send done");
    });
    let mut reader = EventReader::new(BufReader::new(
        harness_stream.try_clone().expect("harness clone"),
    ));
    let mut writer = EventWriter::new(BufWriter::new(harness_stream));
    writer
        .write_message(&configure_message(serde_json::json!({})))
        .expect("write initial configure");
    writer.flush().expect("flush initial configure");
    drain_startup(&mut reader);

    for idx in 0..MAX_IN_FLIGHT {
        writer
            .write_event(&exa_started(&format!("call-{idx}"), "blocked"))
            .expect("write");
    }
    writer.flush().expect("flush");
    searcher.wait_for_started(MAX_IN_FLIGHT);

    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("test shutdown".to_owned()),
        }))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");

    let exited = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("extension should exit promptly after disconnect");
    assert!(exited);
    searcher.release();
}

/// Ensures endpoint config validation catches conflicting aliases before a
/// later tool call and reports the problem as a configuration error.
#[test]
fn conflicting_endpoint_aliases_return_config_error() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "endpoint": "https://exa.example/mcp",
            "exa_endpoint": "https://other.example/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    let err = reader
        .read_config_error()
        .expect("read")
        .expect("config error");
    assert!(err.message.contains("cannot both be set"), "err: {err:?}");
}

/// Ensures equal legacy and explicit Exa endpoint aliases are accepted and
/// applied once, preserving backwards-compatible configuration.
#[test]
fn equal_endpoint_aliases_are_accepted_and_applied() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher.clone(), parallel);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "endpoint": "https://exa.example/mcp",
            "exa_endpoint": "https://exa.example/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    for _ in 0..100 {
        if !searcher.endpoints.lock().expect("lock").is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        searcher.endpoints.lock().expect("lock").as_slice(),
        ["https://exa.example/mcp"]
    );
}

/// Ensures endpoint validation rejects URL userinfo so providers cannot receive
/// credentials through implicit HTTP Basic Authorization handling.
#[test]
fn endpoint_userinfo_is_rejected_during_configure() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "parallel_endpoint": "https://user:secret@example.com/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    let err = reader
        .read_config_error()
        .expect("read")
        .expect("config error");
    assert!(err.message.contains("userinfo"), "err: {err:?}");
}

/// Ensures plaintext HTTP endpoints are rejected unless they target loopback,
/// keeping production provider traffic on HTTPS while preserving local tests.
#[test]
fn non_loopback_http_endpoint_is_rejected_during_configure() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "exa_endpoint": "http://example.com/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    let err = reader
        .read_config_error()
        .expect("read")
        .expect("config error");
    assert!(err.message.contains("https"), "err: {err:?}");
}

/// Ensures loopback HTTP endpoints are accepted for deterministic local tests
/// while non-loopback plaintext provider endpoints stay rejected.
#[test]
fn loopback_http_endpoint_is_accepted_and_applied() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher.clone(), parallel);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "exa_endpoint": "http://127.0.0.1:8080/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    for _ in 0..100 {
        if !searcher.endpoints.lock().expect("lock").is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        searcher.endpoints.lock().expect("lock").as_slice(),
        ["http://127.0.0.1:8080/mcp"]
    );
}

/// Ensures malformed URLs are reported during configuration instead of being
/// deferred to the next tool invocation.
#[test]
fn malformed_endpoint_is_rejected_during_configure() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_message(&configure_message(serde_json::json!({
            "exa_endpoint": "https://example.com:bad/mcp",
        })))
        .expect("write configure");
    writer.flush().expect("flush");

    let err = reader
        .read_config_error()
        .expect("read")
        .expect("config error");
    assert!(err.message.contains("valid URL"), "err: {err:?}");
}

/// Ensures Parallel search forwards provider-specific passthrough arguments to
/// the remote MCP tool.
#[test]
fn forwards_parallel_search_to_web_search_and_returns_text() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("search result");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("fixture"),
        query_id: "parallel-query".to_owned(),
    };

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-6".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("latest rust release".to_owned()),
                ),
                (
                    CborValue::Text("max_results".to_owned()),
                    CborValue::Integer(3.into()),
                ),
            ]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: originator.clone(),
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "call-6");
    assert_eq!(result.tool_name.as_str(), PARALLEL_SEARCH_TOOL_NAME);
    assert_eq!(result.result, CborValue::Text("search result".to_owned()));
    assert_eq!(result.originator, originator);

    let calls = parallel.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PARALLEL_REMOTE_SEARCH_TOOL);
    assert_eq!(calls[0].1["query"], "latest rust release");
    assert_eq!(calls[0].1["max_results"], 3);
}

/// Ensures Parallel fetch dispatches to the remote `web_fetch` MCP tool with
/// its URL argument.
#[test]
fn forwards_parallel_fetch_to_web_fetch() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("page text");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-7".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_FETCH_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("url".to_owned()),
                CborValue::Text("https://example.com".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    assert!(matches!(event, Event::ToolResult(_)));

    let calls = parallel.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PARALLEL_REMOTE_FETCH_TOOL);
    assert_eq!(calls[0].1["url"], "https://example.com");
}

/// Ensures Parallel calls enforce their advertised required fields locally
/// instead of forwarding malformed requests to the provider.
#[test]
fn parallel_missing_required_argument_is_rejected_before_forwarding() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-missing-query".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains("query"), "message: {}", err.message);
    assert!(parallel.calls.lock().expect("lock").is_empty());
}

/// Ensures Parallel JSON conversion rejects CBOR maps that cannot be
/// represented as JSON objects.
#[test]
fn parallel_non_string_argument_keys_are_rejected_before_forwarding() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-8".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Integer(1.into()),
                CborValue::Text("anything".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(
        err.message.contains("keys must be strings"),
        "message: {}",
        err.message
    );
    assert!(parallel.calls.lock().expect("lock").is_empty());
}

// ---- Wire decoding ----

/// Ensures hosted MCP server-sent-event responses are decoded from `data:`
/// frames.
#[test]
fn decodes_sse_message_frame() {
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body, "exa").expect("decode");
    assert_eq!(text, "hello");
}

/// Ensures multiple textual MCP content parts are preserved in order for
/// model-visible output.
#[test]
fn concatenates_multiple_text_content_parts() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#;
    let text = decode_mcp_text_result(body, "exa").expect("decode");
    assert_eq!(text, "first\n\nsecond");
}

/// Ensures JSON-RPC errors from MCP providers surface their provider message.
#[test]
fn surfaces_jsonrpc_error_message() {
    let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}"#;
    let err = decode_mcp_text_result(body, "exa").expect_err("should fail");
    assert!(err.contains("bad params"), "err: {err}");
}

/// Ensures malformed successful MCP responses without text content are
/// rejected.
#[test]
fn fails_when_response_has_no_text_content() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":""}]}}"#;
    let err = decode_mcp_text_result(body, "exa").expect_err("should fail");
    assert!(err.contains("no text content"), "err: {err}");
}

/// Ensures SSE decoding uses the first complete MCP message frame and ignores
/// later frames.
#[test]
fn first_wellformed_sse_frame_wins() {
    // Two complete `message` frames, blank-line-terminated. The documented
    // contract is "take the first well-formed one".
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n\
                \n\
                event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body, "parallel").expect("decode");
    assert_eq!(text, "first");
}

/// Ensures successful MCP HTTP bodies are capped so a provider cannot force
/// unbounded memory growth before JSON/SSE decoding.
#[test]
fn oversized_success_body_is_rejected() {
    let body = vec![b'x'; SUCCESS_BODY_MAX_BYTES + 1];
    let err = read_success_body(&body[..], "exa").expect_err("should fail");
    assert!(err.contains("exceeded"), "err: {err}");
}

/// Ensures decoded text is capped before it becomes model-visible tool output.
#[test]
fn oversized_decoded_text_is_rejected() {
    let err = limit_tool_output("x".repeat(TOOL_OUTPUT_MAX_BYTES + 1), "parallel")
        .expect_err("should fail");
    assert!(err.contains("exceeded"), "err: {err}");
}

/// Ensures oversized JSON-RPC error messages are rejected with a compact error
/// before becoming model-visible tool error text.
#[test]
fn oversized_jsonrpc_error_message_is_rejected() {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": -32000,
            "message": "x".repeat(TOOL_OUTPUT_MAX_BYTES + 1),
        }
    })
    .to_string();
    let err = decode_mcp_text_result(&body, "exa").expect_err("should fail");
    assert!(err.contains("exceeded"), "err: {err}");
    assert!(
        err.len() < 200,
        "oversized provider message should not be echoed: {}",
        err.len()
    );
}

/// Ensures endpoint redaction strips credentials and query strings from any
/// transport error that includes the configured endpoint verbatim.
#[test]
fn endpoint_redaction_removes_query_and_userinfo_secrets() {
    let endpoint = "https://user:secret@example.com/mcp?exaApiKey=secret#frag";
    let redacted = redact_endpoint_in_error(&format!("failed to connect to {endpoint}"), endpoint);
    assert!(!redacted.contains("secret"), "redacted: {redacted}");
    assert!(!redacted.contains("exaApiKey"), "redacted: {redacted}");
    assert_eq!(redacted, "failed to connect to https://example.com/mcp");
}

/// Ensures non-success HTTP responses cannot echo endpoint query credentials
/// back into model-visible provider error text.
#[test]
fn http_error_body_redacts_endpoint_query_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/mcp?exaApiKey=secret#frag",
        listener.local_addr().expect("addr")
    );
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = IoBufReader::new(stream.try_clone().expect("clone"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            if line == "\r\n" {
                break;
            }
        }
        let response_body = "failed /mcp?exaApiKey=secret";
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        std::io::Write::write_all(&mut &stream, response.as_bytes()).expect("write");
    });

    let err = HttpExaSearcher::new(endpoint)
        .search("rust", 1)
        .expect_err("server returned HTTP 500");
    server.join().expect("join");
    assert!(!err.contains("secret"), "err: {err}");
    assert!(!err.contains("exaApiKey"), "err: {err}");
}

/// Ensures JSON-RPC provider errors cannot echo endpoint query credentials back
/// into model-visible tool error text.
#[test]
fn jsonrpc_error_message_redacts_endpoint_query_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/mcp?exaApiKey=secret#frag",
        listener.local_addr().expect("addr")
    );
    let echoed_endpoint = endpoint.trim_end_matches("#frag").to_owned();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = IoBufReader::new(stream.try_clone().expect("clone"));
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_len = value.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0; content_len];
        reader.read_exact(&mut body).expect("body");
        let response_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32000,
                "message": format!("failed {echoed_endpoint} and /mcp?exaApiKey=secret"),
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        std::io::Write::write_all(&mut &stream, response.as_bytes()).expect("write");
    });

    let err = HttpExaSearcher::new(endpoint)
        .search("rust", 1)
        .expect_err("server returned JSON-RPC error");
    server.join().expect("join");
    assert!(!err.contains("secret"), "err: {err}");
    assert!(!err.contains("exaApiKey"), "err: {err}");
}

/// Ensures the Parallel client applies the same JSON-RPC endpoint redaction as
/// Exa before surfacing provider errors to the model.
#[test]
fn parallel_jsonrpc_error_message_redacts_endpoint_query_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!(
        "http://{}/mcp?parallelApiKey=secret#frag",
        listener.local_addr().expect("addr")
    );
    let echoed_endpoint = endpoint.trim_end_matches("#frag").to_owned();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = IoBufReader::new(stream.try_clone().expect("clone"));
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_len = value.trim().parse().expect("content length");
            }
        }
        let mut body = vec![0; content_len];
        reader.read_exact(&mut body).expect("body");
        let response_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32000,
                "message": format!("failed {echoed_endpoint} and /mcp?parallelApiKey=secret"),
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        std::io::Write::write_all(&mut &stream, response.as_bytes()).expect("write");
    });

    let err = HttpParallelClient::new(endpoint)
        .call(
            PARALLEL_REMOTE_SEARCH_TOOL,
            serde_json::json!({ "query": "rust" }),
        )
        .expect_err("server returned JSON-RPC error");
    server.join().expect("join");
    assert!(!err.contains("secret"), "err: {err}");
    assert!(!err.contains("parallelApiKey"), "err: {err}");
}

/// Ensures endpoint sanitization cannot expand short repeated query secrets
/// beyond the model-visible error cap.
#[test]
fn endpoint_sanitization_caps_short_pattern_expansion() {
    let err = sanitize_endpoint_error(
        &"a".repeat(TOOL_OUTPUT_MAX_BYTES),
        "https://example.com/mcp?a=a",
    );
    assert!(err.len() <= TOOL_OUTPUT_MAX_BYTES, "len: {}", err.len());
    assert!(err.ends_with(TRUNCATED_SUFFIX), "err suffix missing");
}

/// Ensures an empty URL fragment is not treated as a replacement pattern, which
/// would otherwise insert redaction text between every character.
#[test]
fn endpoint_sanitization_ignores_empty_fragment() {
    let err = sanitize_endpoint_error("ordinary error", "https://example.com/mcp#");
    assert_eq!(err, "ordinary error");
}

/// Ensures sanitization covers both raw percent-encoded endpoint query
/// components and their decoded forms.
#[test]
fn endpoint_sanitization_redacts_percent_encoded_query_components() {
    let err = sanitize_endpoint_error(
        "failed exa%41piKey=secret token=s%2Bcret decoded s+cret",
        "https://example.com/mcp?exa%41piKey=secret&token=s%2Bcret",
    );
    for leaked in ["exa%41piKey", "exaApiKey", "secret", "s%2Bcret", "s+cret"] {
        assert!(!err.contains(leaked), "leaked {leaked:?} in {err}");
    }
}

/// Ensures integer-valued CBOR floats remain compatible with callers that
/// encode numbers as floats.
#[test]
fn parse_num_results_accepts_integer_valued_float() {
    let v = parse_num_results(&CborValue::Float(3.0)).expect("ok");
    assert_eq!(v, 3);
}

/// Ensures fractional numeric result counts are rejected before network
/// dispatch.
#[test]
fn parse_num_results_rejects_non_integer_float() {
    let err = parse_num_results(&CborValue::Float(3.5)).expect_err("should fail");
    assert!(err.contains("integer"), "err: {err}");
}

/// Ensures stale Parallel credential config is rejected because the extension
/// intentionally sends no auth header.
#[test]
fn parallel_config_rejects_api_key_field() {
    // Parallel runs against the unauthenticated Search MCP endpoint; keeping
    // `deny_unknown_fields` catches stale configs that try to pass credentials.
    let err = serde_json::from_value::<ExtConfig>(serde_json::json!({
        "api_key": "secret"
    }))
    .expect_err("api_key is not a supported Parallel config field");
    assert!(err.to_string().contains("api_key"), "err: {err}");
}

/// Ensures the real Parallel HTTP client sends MCP headers/body but never an
/// Authorization header.
#[test]
fn parallel_http_client_posts_tools_call_without_authorization_header() {
    // Regression coverage for the Parallel.ai integration: the first-party
    // extension intentionally uses the default unauthenticated MCP endpoint and
    // must not invent API-key config or send Authorization headers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = IoBufReader::new(stream.try_clone().expect("clone"));
        let mut headers = Vec::new();
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_len = value.trim().parse().expect("content length");
            }
            headers.push(line);
        }
        let mut body = vec![0; content_len];
        reader.read_exact(&mut body).expect("body");
        let response_body =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        std::io::Write::write_all(&mut &stream, response.as_bytes()).expect("write");
        (headers, String::from_utf8(body).expect("utf8"))
    });

    let client = HttpParallelClient::new(endpoint);
    let text = client
        .call(
            PARALLEL_REMOTE_SEARCH_TOOL,
            serde_json::json!({ "query": "rust" }),
        )
        .expect("call");
    assert_eq!(text, "ok");

    let (headers, body) = server.join().expect("join");
    assert!(
        !headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("authorization:")),
        "headers: {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("MCP-Protocol-Version: 2025-06-18\r\n")),
        "headers: {headers:?}"
    );
    let body: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], PARALLEL_REMOTE_SEARCH_TOOL);
    assert_eq!(body["params"]["arguments"]["query"], "rust");
}
