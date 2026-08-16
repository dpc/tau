use std::io::BufRead;
use std::net::TcpListener;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Condvar, Mutex, atomic as path_std_sync_atomic, mpsc};
use std::{io as path_std_io, sync as path_std_sync, time as path_std_time};

use tau_proto::{HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, ToolStarted};

use super::gateway_supervisor::{
    GATEWAY_RECONNECT_INITIAL_DELAY, GATEWAY_RECONNECT_MAX_DELAY, next_gateway_retry_delay,
};
use super::*;
use crate::gateway_client::test_support::authenticate_test_gateway;

static SATURATION_TEST_LOCK: Mutex<()> = Mutex::new(());

const TEST_GEN_ONE: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const TEST_GEN_TWO: &str = "0202020202020202020202020202020202020202020202020202020202020202";
const TEST_GEN_THREE: &str = "0303030303030303030303030303030303030303030303030303030303030303";
const TEST_GEN_LATE: &str = "0404040404040404040404040404040404040404040404040404040404040404";
const TEST_GEN_OLD: &str = "0505050505050505050505050505050505050505050505050505050505050505";
const TEST_GEN_CURRENT: &str = "0606060606060606060606060606060606060606060606060606060606060606";

/// Clears the correlated production saturation hook even after a test panic.
struct SaturationHookGuard;

impl Drop for SaturationHookGuard {
    fn drop(&mut self) {
        SATURATION_HOOK
            .lock()
            .expect("telegram saturation hook")
            .take();
    }
}

/// Production writer that blocks on the first detached saturation filler.
struct SaturationWriter {
    /// Serialized protocol output.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Writer gate, initially closed.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Notification that the writer reached the filler frame.
    entered: mpsc::Sender<()>,
    /// Notification that the checked ingress report reached the writer.
    report_written: mpsc::Sender<()>,
    /// Whether this writer already blocked once.
    blocked: bool,
    /// Whether the checked ingress report is awaiting its flush barrier.
    report_seen: bool,
}

impl Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        if !self.blocked && bytes.windows(9).any(|window| window == b"term.bell") {
            self.blocked = true;
            let _ = self.entered.send(());
            let (lock, wake) = &*self.gate;
            let closed = lock.lock().expect("writer gate");
            drop(
                wake.wait_while(closed, |closed| *closed)
                    .expect("wait for writer release"),
            );
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        if bytes
            .windows(b"message.delivered_reported".len())
            .any(|window| window == b"message.delivered_reported")
        {
            self.report_seen = true;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        if self.report_seen {
            self.report_seen = false;
            let _ = self.report_written.send(());
        }
        Ok(())
    }
}

/// Production writer that fails when one selected mandatory frame is written.
struct FailingWriter {
    /// Complete bytes written before failure.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Event name whose frame must fail.
    target: &'static [u8],
    /// Optional notification that the selected frame reached the writer.
    failed: Option<mpsc::Sender<()>>,
}

/// Writer that blocks one checked report until the test releases it.
struct BlockingReportWriter {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Write for BlockingReportWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        if bytes
            .windows(b"message.delivered_reported".len())
            .any(|window| window == b"message.delivered_reported")
        {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        if bytes
            .windows(self.target.len())
            .any(|window| window == self.target)
        {
            if let Some(failed) = self.failed.take() {
                let _ = failed.send(());
            }
            return Err(path_std_io::Error::other("forced Telegram writer failure"));
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// HTTP framing emitted by a hermetic Bot API fixture.
#[derive(Clone, Copy)]
enum HttpResponseFraming {
    /// A response with its body size declared up front.
    ContentLength,
    /// A response whose payload arrives in aggregate-sized chunks.
    Chunked,
}

/// Start a loopback Bot API fixture that returns one successful response body.
fn telegram_response_fixture(
    framing: HttpResponseFraming,
    body: Vec<u8>,
    content_type: &'static str,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Bot API request");
        let mut request = path_std_io::BufReader::new(&mut stream);
        let mut request_body_length = 0;
        loop {
            let mut line = Vec::new();
            let read = request
                .read_until(b'\n', &mut line)
                .expect("read request header");
            assert!(
                0 < read,
                "Bot API client closed before completing request headers"
            );
            if line == b"\r\n" {
                break;
            }
            let line = String::from_utf8_lossy(&line);
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                request_body_length = value.trim().parse().expect("parse request Content-Length");
            }
        }
        let mut request_body = vec![0; request_body_length];
        request
            .read_exact(&mut request_body)
            .expect("read request body");
        drop(request);

        match framing {
            HttpResponseFraming::ContentLength => {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len(),
                )
                .expect("write response headers");
                stream.write_all(&body).expect("write response body");
            }
            HttpResponseFraming::Chunked => {
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .expect("write response headers");
                for chunk in body.chunks(8 * 1024) {
                    write!(stream, "{:X}\r\n", chunk.len()).expect("write chunk size");
                    stream.write_all(chunk).expect("write chunk");
                    stream.write_all(b"\r\n").expect("write chunk terminator");
                }
                stream
                    .write_all(b"0\r\n\r\n")
                    .expect("write chunked response terminator");
            }
        }
    });
    (format!("http://{address}"), thread)
}

/// Build a syntactically valid JSON response at the exact successful-body cap.
fn successful_response_at_body_limit() -> Vec<u8> {
    let prefix = br#"{"ok":true,"result":""#;
    let suffix = br#""}"#;
    let maximum_body_bytes =
        usize::try_from(MAX_SUCCESSFUL_RESPONSE_BODY_BYTES).expect("10 MiB fits usize");
    let padding_bytes = maximum_body_bytes - prefix.len() - suffix.len();
    let mut body = Vec::with_capacity(maximum_body_bytes);
    body.extend_from_slice(prefix);
    body.extend(std::iter::repeat_n(b'x', padding_bytes));
    body.extend_from_slice(suffix);
    body
}

/// Assert that an oversized successful response failed while reading rather
/// than after a JSON decode attempt.
fn assert_oversized_successful_response(error: TelegramApiFailure) {
    let TelegramApiFailure::Protocol(message) = error else {
        panic!("expected Protocol failure");
    };
    assert!(
        message.contains("reading Telegram HTTP 200"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("invalid Telegram JSON"),
        "response must fail before JSON decoding: {message}"
    );
}

/// Remote diagnostic bounding preserves valid UTF-8, enforces the exact byte
/// ceiling, and replaces control characters before stderr use.
#[test]
fn bot_api_diagnostic_is_bounded_and_sanitized_at_utf8_boundary() {
    let exact = "x".repeat(1024);
    assert_eq!(bounded_api_diagnostic(&exact), exact);
    let over = format!("\n{}ésecret", "x".repeat(1022));
    let bounded = bounded_api_diagnostic(&over);
    assert!(bounded.len() <= 1024);
    assert!(bounded.starts_with(' '));
    assert!(!bounded.contains('\n'));
    assert!(!bounded.contains("secret"));
}

impl Extension {
    /// Wait for a fixture-driven supervisor connection notification.
    fn wait_for_gateway_connection(&self) {
        let state = self.state.lock();
        let _state = self
            .state
            .wait_timeout_while(state, Duration::from_secs(2), |_| {
                self.gateway_client().is_none()
            });
        assert!(
            self.gateway_client().is_some(),
            "gateway supervisor did not connect"
        );
    }
}

/// Valid fake gateway report IDs used across sidecar correlation tests.
const GATEWAY_REPORT_1: &str =
    "telegram-gateway-report:1111111111111111111111111111111111111111111111111111111111111111";
const GATEWAY_REPORT_2: &str =
    "telegram-gateway-report:2222222222222222222222222222222222222222222222222222222222222222";
const GATEWAY_REPORT_EXACT: &str =
    "telegram-gateway-report:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the tau-client writer thread.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl std::io::Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FakeClient {
    sent: Mutex<Vec<(i64, String)>>,
    update_batches: Mutex<Vec<Vec<TgUpdate>>>,
    poll_timeouts: Mutex<Vec<u64>>,
    webhook_info: Mutex<Result<TgWebhookInfo, TelegramApiFailure>>,
    send_error: Mutex<Option<String>>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(Vec::new()),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(Ok(TgWebhookInfo::default())),
            send_error: Mutex::new(None),
        })
    }

    fn with_updates(update_batches: Vec<Vec<TgUpdate>>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(update_batches),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(Ok(TgWebhookInfo::default())),
            send_error: Mutex::new(None),
        })
    }

    fn with_webhook_info(info: Result<TgWebhookInfo, TelegramApiFailure>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            update_batches: Mutex::new(Vec::new()),
            poll_timeouts: Mutex::new(Vec::new()),
            webhook_info: Mutex::new(info),
            send_error: Mutex::new(None),
        })
    }

    fn fail_sends(&self, message: &str) {
        *self.send_error.lock().expect("lock") = Some(message.to_owned());
    }
}

impl TelegramClient for FakeClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, TelegramApiFailure> {
        self.webhook_info.lock().expect("lock").clone()
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, TelegramApiFailure> {
        self.poll_timeouts
            .lock()
            .expect("lock")
            .push(_cfg.poll_timeout_seconds);
        let mut batches = self.update_batches.lock().expect("lock");
        if batches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(batches.remove(0))
        }
    }

    fn send_message(
        &self,
        _cfg: &RuntimeConfig,
        chat_id: i64,
        text: &str,
    ) -> Result<(), TelegramApiFailure> {
        if let Some(message) = self.send_error.lock().expect("lock").clone() {
            return Err(TelegramApiFailure::Protocol(message));
        }
        self.sent
            .lock()
            .expect("lock")
            .push((chat_id, text.to_owned()));
        Ok(())
    }
}

struct SlowPollClient;

impl TelegramClient for SlowPollClient {
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, TelegramApiFailure> {
        Ok(TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        _offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, TelegramApiFailure> {
        std::thread::sleep(Duration::from_secs(2));
        Ok(Vec::new())
    }

    fn send_message(
        &self,
        _cfg: &RuntimeConfig,
        _chat_id: i64,
        _text: &str,
    ) -> Result<(), TelegramApiFailure> {
        Ok(())
    }
}

/// Fixture state for pausing exactly one webhook preflight at a deterministic
/// point in registration.
#[derive(Default)]
enum WebhookCheckGate {
    /// Webhook checks proceed immediately.
    #[default]
    Open,
    /// The next webhook check must pause before returning.
    BlockNext,
    /// A webhook check is paused until the test releases it.
    Waiting,
}

struct ControlledPollClient {
    first_response: Mutex<Option<Result<Vec<TgUpdate>, TelegramApiFailure>>>,
    response_ready: Condvar,
    called: Mutex<usize>,
    called_ready: Condvar,
    /// Exact offsets captured before each response gate.
    offsets: Mutex<Vec<Option<i64>>>,
    /// State machine controlling one deterministic webhook-check pause.
    webhook_check_gate: Mutex<WebhookCheckGate>,
    /// Signals webhook-check gate transitions.
    webhook_check_changed: Condvar,
    /// Number of currently executing poll calls.
    active_polls: AtomicUsize,
    /// Number of poll-loop exits.
    poller_exits: AtomicUsize,
}

/// Decrements the active provider-call count on every return path.
struct ActivePoll<'a>(&'a AtomicUsize);

impl Drop for ActivePoll<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

impl ControlledPollClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            first_response: Mutex::new(None),
            response_ready: Condvar::new(),
            called: Mutex::new(0),
            called_ready: Condvar::new(),
            offsets: Mutex::new(Vec::new()),
            webhook_check_gate: Mutex::new(WebhookCheckGate::Open),
            webhook_check_changed: Condvar::new(),
            active_polls: AtomicUsize::new(0),
            poller_exits: AtomicUsize::new(0),
        })
    }

    fn block_next_webhook_check(&self) {
        let mut gate = self.webhook_check_gate.lock().expect("lock");
        assert!(matches!(*gate, WebhookCheckGate::Open));
        *gate = WebhookCheckGate::BlockNext;
    }

    fn wait_for_blocked_webhook_check(&self) {
        let gate = self.webhook_check_gate.lock().expect("lock");
        drop(
            self.webhook_check_changed
                .wait_while(gate, |gate| !matches!(*gate, WebhookCheckGate::Waiting))
                .expect("wait"),
        );
    }

    fn release_webhook_check(&self) {
        let mut gate = self.webhook_check_gate.lock().expect("lock");
        assert!(matches!(*gate, WebhookCheckGate::Waiting));
        *gate = WebhookCheckGate::Open;
        self.webhook_check_changed.notify_all();
    }

    fn wait_for_call(&self) {
        self.wait_for_call_count(1);
    }

    fn wait_for_call_count(&self, expected: usize) {
        let called = self.called.lock().expect("lock");
        let called = self
            .called_ready
            .wait_while(called, |called| *called < expected)
            .expect("wait");
        assert!(
            *called >= expected,
            "poller issued {called} getUpdates calls, expected {expected}"
        );
    }

    fn release_first_response(&self, updates: Vec<TgUpdate>) {
        self.release_response(Ok(updates));
    }

    fn release_error(&self, message: &str) {
        self.release_response(Err(TelegramApiFailure::Protocol(message.to_owned())));
    }

    fn release_response(&self, response: Result<Vec<TgUpdate>, TelegramApiFailure>) {
        *self.first_response.lock().expect("lock") = Some(response);
        self.response_ready.notify_all();
    }
}

impl TelegramClient for ControlledPollClient {
    fn poller_exited(&self) {
        self.poller_exits.fetch_add(1, AtomicOrdering::SeqCst);
    }
    fn get_webhook_info(&self, _cfg: &RuntimeConfig) -> Result<TgWebhookInfo, TelegramApiFailure> {
        let mut gate = self.webhook_check_gate.lock().expect("lock");
        if matches!(*gate, WebhookCheckGate::BlockNext) {
            *gate = WebhookCheckGate::Waiting;
            self.webhook_check_changed.notify_all();
            gate = self
                .webhook_check_changed
                .wait_while(gate, |gate| matches!(*gate, WebhookCheckGate::Waiting))
                .expect("wait");
        }
        drop(gate);
        Ok(TgWebhookInfo::default())
    }

    fn get_updates(
        &self,
        _cfg: &RuntimeConfig,
        offset: Option<i64>,
    ) -> Result<Vec<TgUpdate>, TelegramApiFailure> {
        self.active_polls.fetch_add(1, AtomicOrdering::SeqCst);
        let _active = ActivePoll(&self.active_polls);
        self.offsets.lock().expect("lock").push(offset);
        {
            let mut called = self.called.lock().expect("lock");
            *called += 1;
            self.called_ready.notify_all();
        }
        let response = self.first_response.lock().expect("lock");
        let mut response = self
            .response_ready
            .wait_while(response, |response| response.is_none())
            .expect("wait");
        response.take().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn send_message(
        &self,
        _cfg: &RuntimeConfig,
        _chat_id: i64,
        _text: &str,
    ) -> Result<(), TelegramApiFailure> {
        Ok(())
    }
}

fn cfg() -> RuntimeConfig {
    RuntimeConfig {
        bot_token: "token".to_owned(),
        allowed_user_ids: [123].into_iter().collect(),
        configured_chat_id: Some(123),
        api_base: DEFAULT_API_BASE.to_owned(),
        poll_timeout_seconds: 1,
    }
}

fn temp_ext_root() -> std::path::PathBuf {
    static NEXT: path_std_sync::atomic::AtomicU64 = path_std_sync_atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tau-ext-telegram-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create temp state dir");
    dir
}

fn temp_state_dir() -> std::path::PathBuf {
    temp_ext_root().join("std-telegram")
}

fn agent_id(text: &str) -> AgentId {
    AgentId::parse(text).expect("agent id")
}

fn tool(name: &str, agent: &str, args: CborValue) -> ToolStarted {
    ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: tau_proto::ToolName::new(name),
        arguments: args,
        agent_id: agent_id(agent),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn bool_args(value: bool) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("enabled".to_owned()),
        CborValue::Bool(value),
    )])
}

fn message_args(value: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("message".to_owned()),
        CborValue::Text(value.to_owned()),
    )])
}

fn gateway_mode(socket_path: std::path::PathBuf) -> BridgeMode {
    BridgeMode::GatewayClient(GatewayClientConfig::for_test(socket_path))
}

/// Write one flushed fake gateway response line.
fn write_gateway_response(stream: &mut UnixStream, response: serde_json::Value) {
    writeln!(stream, "{response}").expect("write gateway response");
    stream.flush().expect("flush gateway response");
}

fn set_test_publisher(ext: &Extension) {
    ext.set_publisher_name(
        tau_proto::ExtensionName::parse("std-telegram").expect("test publisher name"),
    );
}

fn test_extension(client: Arc<dyn TelegramClient>, output: impl Into<Output>) -> Extension {
    let ext = Extension::new(client, output);
    set_test_publisher(&ext);
    ext
}

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    (ext, rx, client)
}

fn process_update(ext: &Extension, update: TgUpdate) {
    let config_generation = ext.state.lock().config_generation;
    ext.process_update_for_generation(update, config_generation);
}

/// Construct one typed Telegram cursor offset for state assertions.
fn update_offset(value: i64) -> TelegramUpdateOffset {
    TelegramUpdateId::new(value - 1)
        .expect("test offset predecessor")
        .next_offset()
}

/// Construct one validated Telegram update ID for test fixtures.
fn telegram_update_id(value: i64) -> TelegramUpdateId {
    TelegramUpdateId::new(value).expect("test update id")
}

fn expect_tool_finished(rx: &mpsc::Receiver<HarnessInputMessage>) {
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
}

fn expect_tool_success(rx: &mpsc::Receiver<HarnessInputMessage>) {
    let _progress = rx.recv().expect("progress");
    let result = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = result else {
        panic!("emit")
    };
    assert!(
        matches!(emit.event.as_ref(), Event::ToolResultReported(_)),
        "tool call must finish successfully, got {:?}",
        emit.event
    );
}

/// A successful Telegram send must submit `message.sent_reported` before its
/// ordinary terminal tool result on the serialized extension output.
fn expect_successful_send(
    rx: &mpsc::Receiver<HarnessInputMessage>,
) -> MessageSent<tau_proto::RawMessagePublisherId> {
    let _progress = rx.recv().expect("progress");
    let message = rx.recv().expect("message.sent_reported");
    let HarnessInputMessage::Emit(emit) = message else {
        panic!("emit")
    };
    assert!(!emit.persist);
    let Event::MessageSentReported(report) = *emit.event else {
        panic!("message.sent_reported event")
    };
    assert_eq!(report.publisher_extension_id.as_str(), "std-telegram");
    let result = rx.recv().expect("tool result");
    let HarnessInputMessage::Emit(emit) = result else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::ToolResultReported(_)));
    report
}

fn expect_tool_error(rx: &mpsc::Receiver<HarnessInputMessage>) -> String {
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    error.message
}

fn expect_notice(rx: &mpsc::Receiver<HarnessInputMessage>) -> tau_proto::ExtensionNoticeRequest {
    let msg = rx.recv().expect("notice");
    let HarnessInputMessage::ExtensionNoticeRequest(notice) = msg else {
        panic!("extension notice request")
    };
    notice
}

fn expect_delivered(
    rx: &mpsc::Receiver<HarnessInputMessage>,
) -> MessageDelivered<tau_proto::RawMessagePublisherId> {
    let msg = rx.recv().expect("prompt");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    assert!(!emit.persist);
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.publisher_extension_id.as_str(), "std-telegram");
    report
}

/// Convert a retained report into the exact canonical fact the harness returns
/// after stamping publisher authority.
fn canonical_delivered(
    report: MessageDelivered<tau_proto::RawMessagePublisherId>,
) -> MessageDelivered {
    report.with_publisher(
        tau_proto::MessagePublisherId::parse("std-telegram").expect("canonical publisher"),
    )
}

/// Stamp a bridge-produced delivered report payload as the harness would, then
/// prove its projection is identical after a serde round trip.
fn assert_delivered_live_replay_parity(report: MessageDelivered<tau_proto::RawMessagePublisherId>) {
    let live = Event::MessageDeliveredReported(report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-telegram").expect("canonical publisher"),
        )
        .expect("delivered report converts to a canonical fact");
    let encoded = serde_json::to_value(&live).expect("encode fact");
    let replay: Event = serde_json::from_value(encoded).expect("decode replay fact");
    assert_eq!(
        tau_proto::project_message_fact(&live),
        tau_proto::project_message_fact(&replay)
    );
}

/// Telegram prompt references remain deterministic and bounded without exposing
/// native chat, update, or user identifiers.
#[test]
fn telegram_prompt_references_are_opaque_and_domain_separated() {
    let message = telegram_message_ref("native-chat", "native-update");
    assert_eq!(
        message,
        telegram_message_ref("native-chat", "native-update")
    );
    assert_ne!(message, telegram_message_ref("other-chat", "native-update"));
    assert_ne!(message, telegram_message_ref("native-chat", "other-update"));
    assert!(message.as_str().starts_with("telegram-message:"));
    assert!(message.as_str().len() <= 256);
    assert!(!message.as_str().contains("native"));

    let sender = telegram_sender_ref("42");
    assert_eq!(sender, telegram_sender_ref("42"));
    assert_ne!(sender, telegram_sender_ref("43"));
    assert!(sender.starts_with("telegram-sender:"));
    assert_eq!(sender.len(), "telegram-sender:".len() + 64);
}

/// Telegram bridge tools are disabled by default because each role must make an
/// explicit policy choice before exposing the external chat bridge to a model.
#[test]
fn telegram_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
}

/// Telegram bridge tools expose group and tag metadata so role policy can
/// enable the bridge broadly or select registration/sending capabilities
/// separately.
#[test]
fn telegram_tools_have_group_and_tags() {
    assert_eq!(telegram_tool_group().name.as_str(), TOOL_GROUP_NAME);

    let register = register_tool_spec();
    assert!(
        register
            .tags
            .iter()
            .any(|tag| tag.as_str() == REGISTER_TOOL_TAG)
    );

    let send = send_tool_spec();
    assert!(send.tags.iter().any(|tag| tag.as_str() == SEND_TOOL_TAG));
}

/// Telegram uses only the generic configured SDK prefix and never derives names
/// from the operational instance key.
#[test]
fn telegram_uses_generic_tool_prefix() {
    let scope = tau_client::ToolNameScope::from_configure(&tau_proto::Configure {
        tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
        config: CborValue::Null,
        instance_name: tau_proto::ExtensionName::parse("arbitrary-instance")
            .expect("test extension name must satisfy the identifier grammar"),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    });
    let names = ToolNames::from_scope(&scope).expect("scoped names");
    assert_eq!(names.register.as_str(), "work_telegram_register");
    assert_eq!(names.send.as_str(), "work_telegram_send");
    assert_eq!(names.group.as_str(), "work_telegram");
}

/// Provider-owned repair examples must stay schema-valid as bridge tool
/// argument shapes evolve.
#[test]
fn telegram_tool_examples_are_schema_valid() {
    for spec in [register_tool_spec(), send_tool_spec()] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }
}

/// Enabled config must name a non-empty token secret and a non-empty allowlist;
/// otherwise the extension cannot safely decide who may use the bot.
#[test]
fn config_rejects_missing_token_or_empty_allowlist() {
    let err = ExtConfig::default()
        .validate(&BTreeMap::new())
        .expect_err("missing token secret");
    assert!(err.contains("bot_token_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    let err = ExtConfig {
        bot_token_secret: Some("bot".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect_err("empty allowlist");
    assert!(err.contains("allowed_user_ids"));
}

/// Gateway-client mode requires a declared gateway key, but no bot token or
/// Telegram allowlist; those belong to the standalone gateway.
#[test]
fn gateway_client_config_requires_socket_and_declared_secret() {
    let err = ExtConfig {
        mode: ExtMode::GatewayClient,
        ..ExtConfig::default()
    }
    .validate(&BTreeMap::new())
    .expect_err("missing socket should fail");
    assert!(err.contains("gateway_socket_path"));

    let err = ExtConfig {
        mode: ExtMode::GatewayClient,
        gateway_socket_path: Some(PathBuf::from("/tmp/tau-telegram-test.sock")),
        ..ExtConfig::default()
    }
    .validate(&BTreeMap::new())
    .expect_err("missing key declaration should fail");
    assert!(err.contains("gateway_client_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert(
        "gateway-key".to_owned(),
        tau_proto::SecretValue::new("11".repeat(32)),
    );
    let config = || ExtConfig {
        mode: ExtMode::GatewayClient,
        gateway_socket_path: Some(PathBuf::from("/tmp/tau-telegram-test.sock")),
        gateway_client_secret: Some("gateway-key".to_owned()),
        ..ExtConfig::default()
    };
    let first = config().validate(&secrets).expect("gateway client config");
    let second = config()
        .validate(&secrets)
        .expect("fresh gateway client config");
    let (BridgeMode::GatewayClient(first), BridgeMode::GatewayClient(second)) = (first, second)
    else {
        panic!("gateway mode");
    };
    assert_ne!(first.client_generation, second.client_generation);

    secrets.insert(
        "gateway-key".to_owned(),
        tau_proto::SecretValue::new("AA".repeat(32)),
    );
    let error = config()
        .validate(&secrets)
        .expect_err("uppercase gateway key must fail");
    assert_eq!(error, "invalid Telegram gateway client secret");
    assert!(!error.contains("AA"));
}

/// In gateway-client mode the sidecar must not touch Telegram polling APIs.
/// Registration goes to the local gateway socket, and any queued inbound
/// delivery is submitted locally as a transient `message.delivered_reported`
/// report.
#[test]
fn gateway_client_registers_without_polling_and_submits_delivery() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let hello = authenticate_test_gateway(&mut stream, &"44".repeat(32));
        seen_requests_thread.lock().expect("requests").push(hello);
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for index in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            seen_requests_thread.lock().expect("requests").push(request);
            let response = match index {
                1 => serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [{
                        "request_id": GATEWAY_REPORT_1,
                        "session_id": "s1",
                        "agent_id": "agent-1",
                        "message_id": "telegram:10:99",
                        "sender_id": "42",
                        "source": "alice",
                        "conversation_id": "10",
                        "text": "hello"
                    }],
                }),
                _ => serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                }),
            };
            writeln!(stream, "{response}").expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
    }
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let _progress = rx.recv().expect("progress");
    let delivered = expect_delivered(&rx);
    let _result = rx.recv().expect("tool result");
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent = expect_successful_send(&rx);
    server.join().expect("fake gateway thread");

    assert_eq!(delivered.agent_id.as_str(), "agent-1");
    assert_eq!(delivered.text, "hello");
    assert_eq!(
        delivered.message_id,
        telegram_message_ref("10", "telegram:10:99")
    );
    assert_eq!(delivered.sender.stable_id, telegram_sender_ref("42"));
    assert_eq!(
        delivered.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(sent.text, "reply");
    assert!(client.poll_timeouts.lock().expect("polls").is_empty());
    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "complete_reannouncement");
    assert_eq!(requests[2]["kind"], "register_agent");
    assert_eq!(requests[2]["session_id"], "s1");
    assert_eq!(requests[2]["agent_id"], "agent-1");
    assert_eq!(requests[3]["kind"], "send_message");
    assert_eq!(requests[3]["session_id"], "s1");
    assert_eq!(requests[3]["agent_id"], "agent-1");
    assert_eq!(requests[3]["message"], "reply");
    assert!(client.sent.lock().expect("sent").is_empty());
}

/// In gateway-client mode `telegram_send` must forward only message text plus
/// local session/agent identity to the gateway, leaving Telegram destination
/// selection entirely inside the gateway.
#[test]
fn gateway_client_send_forwards_registered_agent_to_gateway() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let hello = authenticate_test_gateway(&mut stream, &"44".repeat(32));
        seen_requests_thread.lock().expect("requests").push(hello);
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            seen_requests_thread.lock().expect("requests").push(request);
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
        state.registered_agents.insert(agent_id("agent-1"));
    }

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent = expect_successful_send(&rx);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "complete_reannouncement");
    assert_eq!(requests[2]["kind"], "send_message");
    assert_eq!(requests[2]["session_id"], "s1");
    assert_eq!(requests[2]["agent_id"], "agent-1");
    assert_eq!(requests[2]["message"], "reply");
    assert!(requests[2].get("chat_id").is_none());
    assert!(client.sent.lock().expect("sent").is_empty());
    assert_eq!(sent.agent_id.as_str(), "agent-1");
    assert_eq!(sent.text, "reply");
}

/// A gateway-declared send failure must return only a tool error and must not
/// claim remote success with `message.sent_reported`.
#[test]
fn gateway_client_send_failure_does_not_submit_sent_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        authenticate_test_gateway(&mut stream, &"44".repeat(32));
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for index in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            let response = if index == 0 {
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                })
            } else {
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": false,
                    "error": "gateway send failed",
                    "keep_connection": true,
                })
            };
            writeln!(stream, "{response}").expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
        state.registered_agents.insert(agent_id("agent-1"));
    }

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    assert!(expect_tool_error(&rx).contains("gateway send failed"));
    assert!(
        ext.gateway_client().is_some(),
        "keep_connection rejection must not churn the live lease"
    );
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent_reported after failure"
    );
    server.join().expect("fake gateway thread");
}

/// Registering before the sidecar has observed `session.started` must fail
/// locally and must not create an incomplete gateway route.
#[test]
fn gateway_client_register_before_session_started_does_not_announce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let hello = authenticate_test_gateway(&mut stream, &"44".repeat(32));
        seen_requests_thread.lock().expect("requests").push(hello);
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                break;
            }
            seen_requests_thread
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&line).expect("gateway request JSON"));
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let message = expect_tool_error(&rx);
    assert!(message.contains("session.started"), "{message}");
    drop(ext);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert!(
        requests
            .iter()
            .all(|request| request["kind"] != "register_agent"),
        "{requests:?}"
    );
}

/// Gateway deliveries are only accepted for the current session and a currently
/// registered local agent; stale gateway records cannot be submitted after
/// local state has failed closed or unregistered.
#[test]
fn gateway_delivery_requires_live_local_registration() {
    let (tx, rx) = mpsc::channel();
    let state = SharedState::new();
    let gateway = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        PathBuf::from("/tmp/nonexistent-telegram-gateway.sock"),
    )));
    let gateway_cell = Mutex::new(Some(Arc::clone(&gateway)));
    {
        let mut state = state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
        state.publisher_name =
            Some(tau_proto::ExtensionName::parse("std-telegram").expect("test publisher name"));
    }
    emit_gateway_deliveries(
        &state,
        &Output::from(tx.clone()),
        &gateway_cell,
        Arc::clone(&gateway),
        vec![GatewayMessageDelivery {
            request_id: GATEWAY_REPORT_1.to_owned(),
            session_id: "s1".to_owned(),
            agent_id: "agent-1".to_owned(),
            message_id: "telegram:1:1".to_owned(),
            sender_id: "7".to_owned(),
            source: "alice".to_owned(),
            conversation_id: "1".to_owned(),
            text: "hello".to_owned(),
        }],
    );
    assert!(rx.try_recv().is_err());

    state.lock().registered_agents.insert(agent_id("agent-1"));
    emit_gateway_deliveries(
        &state,
        &Output::from(tx),
        &gateway_cell,
        gateway,
        vec![GatewayMessageDelivery {
            request_id: GATEWAY_REPORT_2.to_owned(),
            session_id: "s1".to_owned(),
            agent_id: "agent-1".to_owned(),
            message_id: "telegram:1:2".to_owned(),
            sender_id: "7".to_owned(),
            source: "alice".to_owned(),
            conversation_id: "1".to_owned(),
            text: "hello again".to_owned(),
        }],
    );
    let delivered = expect_delivered(&rx);
    assert_eq!(delivered.text, "hello again");
    assert_eq!(delivered.sender.stable_id, telegram_sender_ref("7"));
    assert_eq!(delivered.conversation.expect("conversation").stable_id, "1");
}

/// Gateway delivery stops at the first failed mandatory report without
/// precommitting its batch suffix.
#[test]
fn gateway_report_failure_stops_delivery_suffix() {
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let state = SharedState::new();
    let gateway = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        PathBuf::from("/tmp/nonexistent-telegram-gateway.sock"),
    )));
    let gateway_cell = Mutex::new(Some(Arc::clone(&gateway)));
    {
        let mut state = state.lock();
        state.current_session_id = Some("s1".parse().expect("session"));
        state.publisher_name =
            Some(tau_proto::ExtensionName::parse("std-telegram").expect("publisher"));
        state.registered_agents.insert(agent_id("agent-1"));
    }
    let delivery = |request_id: &str| GatewayMessageDelivery {
        request_id: request_id.to_owned(),
        session_id: "s1".to_owned(),
        agent_id: "agent-1".to_owned(),
        message_id: request_id.to_owned(),
        sender_id: "7".to_owned(),
        source: "alice".to_owned(),
        conversation_id: "1".to_owned(),
        text: request_id.to_owned(),
    };
    assert_eq!(
        emit_gateway_deliveries(
            &state,
            &Output::from(tx),
            &gateway_cell,
            gateway,
            vec![delivery(GATEWAY_REPORT_1), delivery(GATEWAY_REPORT_2)],
        ),
        ProcessingControl::Stop
    );
    let state = state.lock();
    assert_eq!(state.gateway_pending_deliveries.len(), 1);
}

/// Ensures gateway mode ignores a partial canonical collision and sends an
/// exact frozen-route ACK after the target agent unloads.
#[test]
fn gateway_delivery_ack_requires_exact_canonical_echo_after_agent_unload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_ack = Arc::new(Mutex::new(None::<serde_json::Value>));
    let seen_ack_server = Arc::clone(&seen_ack);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        authenticate_test_gateway(&mut stream, &"44".repeat(32));
        let mut reader = path_std_io::BufReader::new(stream.try_clone().expect("clone stream"));
        let mut unregister = String::new();
        reader
            .read_line(&mut unregister)
            .expect("read unregister request");
        let unregister: serde_json::Value =
            serde_json::from_str(&unregister).expect("unregister JSON");
        assert_eq!(unregister["kind"], "unregister_agent");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                "ok": true,
                "gateway_generation": "44".repeat(32),
                "deliveries": [],
            })
        )
        .expect("write unregister response");
        stream.flush().expect("flush unregister");
        drop(reader);
        drop(stream);
        let (mut stream, _) = listener.accept().expect("accept replacement client");
        authenticate_test_gateway(&mut stream, &"55".repeat(32));
        let mut reader = path_std_io::BufReader::new(stream.try_clone().expect("clone stream"));
        let mut ack = String::new();
        reader.read_line(&mut ack).expect("read ack");
        *seen_ack_server.lock().expect("seen ack") =
            Some(serde_json::from_str(&ack).expect("ack JSON"));
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                "ok": true,
                "gateway_generation": "55".repeat(32),
                "deliveries": [],
            })
        )
        .expect("write ack response");
        stream.flush().expect("flush ack");
    });

    let replacement_socket_path = socket_path.clone();
    let gateway = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        socket_path,
    )));
    gateway
        .connect_cancellable(|| false)
        .expect("connect gateway client");
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client, tx);
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
        state.publisher_name =
            Some(tau_proto::ExtensionName::parse("std-telegram").expect("publisher"));
        state.registered_agents.insert(agent_id("agent-1"));
    }
    *ext.gateway.lock().expect("gateway lock") = Some(Arc::clone(&gateway));
    emit_gateway_deliveries(
        &ext.state,
        &ext.output,
        &ext.gateway,
        Arc::clone(&gateway),
        vec![GatewayMessageDelivery {
            request_id: GATEWAY_REPORT_EXACT.to_owned(),
            session_id: "s1".to_owned(),
            agent_id: "agent-1".to_owned(),
            message_id: "telegram:10:99".to_owned(),
            sender_id: "42".to_owned(),
            source: "alice".to_owned(),
            conversation_id: "10".to_owned(),
            text: "hello".to_owned(),
        }],
    );
    let report = expect_delivered(&rx);
    let runtime = TelegramRuntime { ext };
    handle_live_event_value(
        &runtime,
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("agent-1"),
        }),
    );
    let mut wrong = canonical_delivered(report.clone());
    wrong.message_id = MessageFactId::new("wrong");
    runtime.ext.acknowledge_live_delivery(&wrong);
    *runtime.ext.gateway.lock().expect("gateway lock") = None;
    gateway.disconnect();
    runtime
        .ext
        .acknowledge_live_delivery(&canonical_delivered(report));
    assert!(
        runtime
            .ext
            .state
            .lock()
            .gateway_pending_deliveries
            .values()
            .any(|pending| pending.canonical_echo_observed),
        "validated late echo remains a retryable ACK obligation"
    );
    let replacement = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        replacement_socket_path,
    )));
    replacement
        .connect_cancellable(|| false)
        .expect("connect replacement client");
    *runtime.ext.gateway.lock().expect("gateway lock") = Some(Arc::clone(&replacement));
    assert!(retry_gateway_acknowledgements(
        &runtime.ext.state,
        &runtime.ext.output,
        &runtime.ext.gateway,
        &replacement,
    ));
    server.join().expect("fake gateway server");

    let ack = seen_ack.lock().expect("seen ack").clone().expect("ack");
    assert_eq!(ack["kind"], "ack_delivery");
    assert_eq!(ack["report_id"], GATEWAY_REPORT_EXACT);
    assert_eq!(ack["session_id"], "s1");
    assert_eq!(ack["agent_id"], "agent-1");
}

/// Run an automatic ACK retry where the first replacement ACK response carries
/// stale authority and deliveries, followed by one valid replacement response.
fn assert_stale_ack_response_reconnects(
    stale_generation: &'static str,
    stale_reannounce_required: bool,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind gateway");
    let (final_ack_tx, final_ack_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let delivery = serde_json::json!({
            "request_id": GATEWAY_REPORT_1,
            "session_id": "s1",
            "agent_id": "agent-1",
            "message_id": "telegram:10:99",
            "sender_id": "42",
            "source": "alice",
            "conversation_id": "10",
            "text": "original"
        });
        {
            let (mut stream, _) = listener.accept().expect("accept original client");
            authenticate_test_gateway(&mut stream, TEST_GEN_ONE);
            let mut reader =
                path_std_io::BufReader::new(stream.try_clone().expect("clone original"));
            for index in 0..3 {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read original request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("original request JSON");
                if index == 2 {
                    assert_eq!(request["kind"], "send_message");
                    break;
                }
                write_gateway_response(
                    &mut stream,
                    serde_json::json!({
                        "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                        "ok": true,
                        "gateway_generation": TEST_GEN_ONE,
                        "deliveries": vec![delivery.clone()],
                    }),
                );
            }
        }
        {
            let (mut stream, _) = listener.accept().expect("accept stale ACK client");
            authenticate_test_gateway(&mut stream, TEST_GEN_TWO);
            let mut reader = path_std_io::BufReader::new(stream.try_clone().expect("clone stale"));
            for index in 0..3 {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read stale request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("stale request JSON");
                assert_eq!(
                    request["kind"],
                    ["register_agent", "complete_reannouncement", "ack_delivery"][index]
                );
                write_gateway_response(
                    &mut stream,
                    serde_json::json!({
                        "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                        "ok": true,
                        "gateway_generation": if index == 2 {
                            stale_generation
                        } else {
                            TEST_GEN_TWO
                        },
                        "reannounce_required": index == 2 && stale_reannounce_required,
                        "deliveries": if index == 2 {
                            vec![serde_json::json!({
                                "request_id": GATEWAY_REPORT_2,
                                "session_id": "s1",
                                "agent_id": "agent-1",
                                "message_id": "telegram:10:100",
                                "sender_id": "42",
                                "source": "alice",
                                "conversation_id": "10",
                                "text": "stale"
                            })]
                        } else {
                            Vec::<serde_json::Value>::new()
                        },
                    }),
                );
            }
        }
        let final_generation = if stale_generation == TEST_GEN_THREE {
            TEST_GEN_THREE
        } else {
            TEST_GEN_TWO
        };
        let (mut stream, _) = listener.accept().expect("accept final ACK client");
        authenticate_test_gateway(&mut stream, final_generation);
        let mut reader = path_std_io::BufReader::new(stream.try_clone().expect("clone final"));
        for index in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read final request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("final request JSON");
            assert_eq!(
                request["kind"],
                ["register_agent", "complete_reannouncement", "ack_delivery"][index]
            );
            write_gateway_response(
                &mut stream,
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": final_generation,
                    "deliveries": if index == 2 {
                        vec![serde_json::json!({
                            "request_id": GATEWAY_REPORT_2,
                            "session_id": "s1",
                            "agent_id": "agent-1",
                            "message_id": "telegram:10:100",
                            "sender_id": "42",
                            "source": "alice",
                            "conversation_id": "10",
                            "text": "current"
                        })]
                    } else {
                        Vec::<serde_json::Value>::new()
                    },
                }),
            );
        }
        final_ack_tx.send(()).expect("signal final ACK");
    });

    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("configure gateway");
    ext.wait_for_gateway_connection();
    ext.state.lock().current_session_id = Some(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("register progress");
    let original = expect_delivered(&rx);
    let _result = rx.recv().expect("register result");
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("disconnect")));
    assert!(expect_tool_error(&rx).contains("closed"));
    ext.acknowledge_live_delivery(&canonical_delivered(original));

    final_ack_rx.recv().expect("automatic final ACK");
    let current = expect_delivered(&rx);
    assert_eq!(current.text, "current");
    assert!(
        rx.try_iter().all(|message| !matches!(
            message,
            HarnessInputMessage::Emit(ref emit)
                if matches!(emit.event.as_ref(), Event::MessageDeliveredReported(report) if report.text == "stale")
        )),
        "stale ACK-response deliveries must not publish"
    );
    assert!(
        ext.state
            .lock()
            .gateway_pending_deliveries
            .keys()
            .all(|report_id| report_id.as_str() != GATEWAY_REPORT_1),
        "valid replacement ACK removes the original obligation"
    );
    server.join().expect("ACK authority server");
}

/// A generation-only ACK response change must preserve and automatically retry
/// the canonical obligation without publishing stale deliveries.
#[test]
fn gateway_ack_generation_change_reconnects_and_retries() {
    assert_stale_ack_response_reconnects(TEST_GEN_THREE, false);
}

/// A hint-only ACK response must preserve and automatically retry the canonical
/// obligation without publishing stale deliveries.
#[test]
fn gateway_ack_reannouncement_hint_reconnects_and_retries() {
    assert_stale_ack_response_reconnects(TEST_GEN_TWO, true);
}

/// A heartbeat failure from a stale gateway connection must not clear
/// registrations that belong to a newer active gateway or mode.
#[test]
fn stale_gateway_heartbeat_failure_does_not_clear_new_registration_state() {
    let gateway_cell = Mutex::new(None);
    let state = SharedState::new();
    let old_gateway = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        PathBuf::from("/tmp/old-gateway.sock"),
    )));
    let new_gateway = Arc::new(GatewayClient::new(GatewayClientConfig::for_test(
        PathBuf::from("/tmp/new-gateway.sock"),
    )));
    *gateway_cell.lock().expect("gateway lock") = Some(Arc::clone(&new_gateway));
    {
        let mut state = state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.selected_agent_by_chat.insert(10, agent_id("agent-1"));
    }

    assert!(!fail_gateway_client_if_current(
        &gateway_cell,
        &state,
        &old_gateway
    ));
    assert!(
        state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
    assert!(gateway_cell.lock().expect("gateway lock").is_some());

    assert!(fail_gateway_client_if_current(
        &gateway_cell,
        &state,
        &new_gateway
    ));
    assert!(
        state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1")),
        "desired registrations survive current-connection failure"
    );
    assert!(gateway_cell.lock().expect("gateway lock").is_none());
}

/// Clearing configuration must retire the worker before sending `goodbye`, so
/// the old worker cannot close the socket before the supervisor releases its
/// gateway lease.
#[test]
fn gateway_client_config_error_sends_goodbye() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let (goodbye_seen_tx, goodbye_seen_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let hello = authenticate_test_gateway(&mut stream, &"44".repeat(32));
        seen_requests_thread.lock().expect("requests").push(hello);
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                break;
            }
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            if request["kind"] == "goodbye" {
                goodbye_seen_tx.send(()).expect("signal goodbye");
            }
            seen_requests_thread.lock().expect("requests").push(request);
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, _rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    let (post_join_tx, post_join_rx) = mpsc::channel();
    *ext.gateway_supervisor
        .post_join_observer
        .lock()
        .expect("post-join observer lock") = Some(post_join_tx);
    let (goodbye_release_tx, goodbye_release_rx) = mpsc::channel();
    *ext.gateway_supervisor
        .post_join_gate
        .lock()
        .expect("post-join gate lock") = Some(goodbye_release_rx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    std::thread::scope(|scope| {
        let clear = scope.spawn(|| ext.clear_config_after_error());
        post_join_rx.recv().expect("stop joined worker");
        let goodbye_after_join = goodbye_seen_rx.try_recv();
        goodbye_release_tx
            .send(())
            .expect("release goodbye after worker join");
        clear.join().expect("clear configuration");
        if matches!(goodbye_after_join, Err(mpsc::TryRecvError::Empty)) {
            goodbye_seen_rx
                .recv()
                .expect("goodbye after supervisor worker retirement");
        }
    });
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "complete_reannouncement");
    assert_eq!(requests[2]["kind"], "goodbye");
}

/// Agent unload must explicitly unregister the route from the gateway before
/// local state is cleared.
#[test]
fn gateway_client_agent_unload_sends_unregister() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind fake gateway");
    let seen_requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen_requests_thread = Arc::clone(&seen_requests);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept gateway client");
        let hello = authenticate_test_gateway(&mut stream, &"44".repeat(32));
        seen_requests_thread.lock().expect("requests").push(hello);
        let reader = stream.try_clone().expect("clone stream");
        let mut reader = path_std_io::BufReader::new(reader);
        for _ in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                break;
            }
            seen_requests_thread
                .lock()
                .expect("requests")
                .push(serde_json::from_str(&line).expect("gateway request JSON"));
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": "44".repeat(32),
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("apply gateway client config");
    ext.wait_for_gateway_connection();
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
    }
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    let runtime = TelegramRuntime { ext };
    handle_live_event_value(
        &runtime,
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("agent-1"),
        }),
    );
    drop(runtime);
    server.join().expect("fake gateway thread");

    let requests = seen_requests.lock().expect("requests");
    assert_eq!(requests[0]["kind"], "hello");
    assert_eq!(requests[1]["kind"], "complete_reannouncement");
    assert_eq!(requests[2]["kind"], "register_agent");
    assert_eq!(requests[3]["kind"], "unregister_agent");
    assert_eq!(requests[3]["session_id"], "s1");
    assert_eq!(requests[3]["agent_id"], "agent-1");
    assert!(
        requests
            .get(4)
            .is_none_or(|request| request["kind"] == "goodbye")
    );
}

/// Gateway-client configuration must remain valid while the socket is absent;
/// the sole supervisor connects later without touching Telegram polling.
#[test]
fn gateway_supervisor_recovers_when_gateway_starts_late() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let (tx, _rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(gateway_mode(socket_path.clone()), Some(temp_state_dir()))
        .expect("absent gateway is a recoverable runtime condition");
    assert!(ext.gateway_client().is_none());

    let listener = UnixListener::bind(&socket_path).expect("start fake gateway later");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept reconnecting sidecar");
        authenticate_test_gateway(&mut stream, TEST_GEN_LATE);
        let mut reader =
            path_std_io::BufReader::new(stream.try_clone().expect("clone gateway stream"));
        for expected_kind in ["complete_reannouncement", "goodbye"] {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read gateway request");
            if line.trim().is_empty() {
                assert_eq!(expected_kind, "goodbye");
                break;
            }
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("gateway request JSON");
            assert_eq!(request["kind"], expected_kind);
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": TEST_GEN_LATE,
                    "deliveries": [],
                })
            )
            .expect("write gateway response");
            stream.flush().expect("flush gateway response");
        }
    });

    ext.wait_for_gateway_connection();
    drop(ext);
    server.join().expect("late gateway server");
    assert!(client.poll_timeouts.lock().expect("polls").is_empty());
}

/// Run one fixture-controlled response-authority replacement scenario.
fn assert_gateway_response_forces_exact_reannouncement(
    restart_generation: &'static str,
    reannounce_required: bool,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind first gateway");
    let seen = Arc::new(Mutex::new(Vec::<(u8, String)>::new()));
    let seen_thread = Arc::clone(&seen);
    let server_path = socket_path.clone();
    let server = std::thread::spawn(move || {
        {
            let (mut stream, _) = listener.accept().expect("accept first connection");
            let hello = authenticate_test_gateway(&mut stream, TEST_GEN_ONE);
            seen_thread
                .lock()
                .expect("seen")
                .push((1, hello["kind"].as_str().expect("hello kind").to_owned()));
            let mut reader =
                path_std_io::BufReader::new(stream.try_clone().expect("clone first stream"));
            for index in 0..3 {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read first request");
                let request: serde_json::Value =
                    serde_json::from_str(&line).expect("first request JSON");
                seen_thread.lock().expect("seen").push((
                    1,
                    request["kind"].as_str().expect("request kind").to_owned(),
                ));
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                        "ok": true,
                        "gateway_generation": if index == 2 {
                            restart_generation
                        } else {
                            TEST_GEN_ONE
                        },
                        "reannounce_required": index == 2 && reannounce_required,
                        "deliveries": [],
                    })
                )
                .expect("write first response");
                stream.flush().expect("flush first response");
            }
        }
        std::fs::remove_file(&server_path).expect("remove first listener path");
        let replacement = UnixListener::bind(&server_path).expect("bind replacement gateway");
        let (mut stream, _) = replacement.accept().expect("accept replacement connection");
        let hello = authenticate_test_gateway(&mut stream, restart_generation);
        seen_thread
            .lock()
            .expect("seen")
            .push((2, hello["kind"].as_str().expect("hello kind").to_owned()));
        let mut reader =
            path_std_io::BufReader::new(stream.try_clone().expect("clone replacement stream"));
        for index in 0..3 {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .expect("read replacement request");
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("replacement request JSON");
            seen_thread.lock().expect("seen").push((
                2,
                request["kind"].as_str().expect("request kind").to_owned(),
            ));
            if index == 0 {
                assert_eq!(request["session_id"], "s1");
                assert_eq!(request["agent_id"], "agent-1");
            }
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": restart_generation,
                    "deliveries": [],
                })
            )
            .expect("write replacement response");
            stream.flush().expect("flush replacement response");
        }
    });

    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(gateway_mode(socket_path), Some(temp_state_dir()))
        .expect("configure gateway client");
    ext.wait_for_gateway_connection();
    ext.state.lock().current_session_id = Some(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("first")));
    assert!(expect_tool_error(&rx).contains("disconnected"));
    ext.wait_for_gateway_connection();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("second")));
    let sent = expect_successful_send(&rx);
    assert_eq!(sent.text, "second");
    server.join().expect("restart gateway server");

    assert_eq!(
        *seen.lock().expect("seen"),
        vec![
            (1, "hello".to_owned()),
            (1, "complete_reannouncement".to_owned()),
            (1, "register_agent".to_owned()),
            (1, "send_message".to_owned()),
            (2, "hello".to_owned()),
            (2, "register_agent".to_owned()),
            (2, "complete_reannouncement".to_owned()),
            (2, "send_message".to_owned()),
        ]
    );
}

/// A live response reporting a changed generation must force fresh hello,
/// exact route replay, and recovered operation without relying on a hint bit.
#[test]
fn gateway_generation_change_forces_exact_reannouncement() {
    assert_gateway_response_forces_exact_reannouncement(TEST_GEN_TWO, false);
}

/// A live response requesting reannouncement must force fresh hello, exact
/// route replay, and recovered operation even when generation is unchanged.
#[test]
fn gateway_reannouncement_hint_forces_exact_reannouncement() {
    assert_gateway_response_forces_exact_reannouncement(TEST_GEN_ONE, true);
}

/// Replacing gateway configuration must wait for an old worker blocked in its
/// hello response before it can return and publish the new worker.
#[test]
fn gateway_reconfiguration_joins_fixture_blocked_worker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old_path = dir.path().join("old.sock");
    let new_path = dir.path().join("new.sock");
    let old_listener = UnixListener::bind(&old_path).expect("bind old gateway");
    let (hello_seen_tx, hello_seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let old_server = std::thread::spawn(move || {
        let (mut stream, _) = old_listener.accept().expect("accept old connection");
        let mut line = String::new();
        path_std_io::BufReader::new(stream.try_clone().expect("clone old stream"))
            .read_line(&mut line)
            .expect("read old hello");
        let hello: serde_json::Value = serde_json::from_str(&line).expect("old gateway hello JSON");
        assert_eq!(hello["kind"], "hello");
        hello_seen_tx.send(()).expect("signal blocked hello");
        release_rx.recv().expect("release old hello");
        let _ = writeln!(
            stream,
            "{}",
            serde_json::json!({
                "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                "ok": true,
                "kind": "challenge",
                "gateway_generation": TEST_GEN_OLD,
                "server_nonce": "33".repeat(32),
                "server_mac": "00".repeat(32),
            })
        );
        let _ = stream.flush();
    });
    let new_listener = UnixListener::bind(&new_path).expect("bind current gateway");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = new_listener.accept().expect("accept current connection");
        authenticate_test_gateway(&mut stream, TEST_GEN_CURRENT);
        let mut reader =
            path_std_io::BufReader::new(stream.try_clone().expect("clone current stream"));
        for _ in 0..1 {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read current request");
            if line.trim().is_empty() {
                break;
            }
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "protocol_version": crate::gateway_auth::PROTOCOL_VERSION,
                    "ok": true,
                    "gateway_generation": TEST_GEN_CURRENT,
                    "deliveries": [],
                })
            )
            .expect("write current response");
            stream.flush().expect("flush current response");
        }
    });

    let (tx, _rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    let (retirement_release_tx, retirement_release_rx) = mpsc::channel();
    *ext.gateway_supervisor
        .retirement_gate
        .lock()
        .expect("retirement gate lock") = Some(retirement_release_rx);
    ext.apply_config(gateway_mode(old_path), Some(temp_state_dir()))
        .expect("configure blocked old gateway");
    hello_seen_rx.recv().expect("old worker reached hello read");
    let (pre_join_tx, pre_join_rx) = mpsc::channel();
    *ext.gateway_supervisor
        .pre_join_observer
        .lock()
        .expect("pre-join observer lock") = Some(pre_join_tx);
    let (post_join_tx, post_join_rx) = mpsc::channel();
    *ext.gateway_supervisor
        .post_join_observer
        .lock()
        .expect("post-join observer lock") = Some(post_join_tx);
    let (replace_done_tx, replace_done_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = ext.apply_config(gateway_mode(new_path), Some(temp_state_dir()));
            replace_done_tx
                .send(result)
                .expect("signal replacement result");
        });
        release_tx.send(()).expect("release old socket I/O");
        pre_join_rx
            .recv()
            .expect("replacement reached actual pre-join boundary");
        retirement_release_tx
            .send(())
            .expect("release worker retirement");
        post_join_rx
            .recv()
            .expect("replacement produced successful-join token");
        replace_done_rx
            .recv()
            .expect("replacement result")
            .expect("replace gateway configuration");
    });
    ext.wait_for_gateway_connection();
    drop(ext);
    old_server.join().expect("old gateway server");
    server.join().expect("current gateway server");
}

/// Reconnect delays must grow exponentially but remain at the documented
/// low-rate cap even across an arbitrarily long outage.
#[test]
fn gateway_reconnect_backoff_is_bounded() {
    let mut delay = GATEWAY_RECONNECT_INITIAL_DELAY;
    let mut observed = Vec::new();
    for _ in 0..16 {
        observed.push(delay);
        delay = next_gateway_retry_delay(delay);
    }
    assert_eq!(observed[0], Duration::from_millis(100));
    assert_eq!(observed.last(), Some(&GATEWAY_RECONNECT_MAX_DELAY));
    assert!(
        observed
            .iter()
            .all(|delay| *delay <= GATEWAY_RECONNECT_MAX_DELAY)
    );
}

/// Removing a desired route while the gateway is disconnected must succeed
/// locally so a later supervisor connection cannot reannounce retired
/// authority.
#[test]
fn gateway_disconnected_unregister_removes_desired_route() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (tx, rx) = mpsc::channel();
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(
        gateway_mode(dir.path().join("absent.sock")),
        Some(temp_state_dir()),
    )
    .expect("configure absent gateway");
    {
        let mut state = ext.state.lock();
        state.current_session_id = Some(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        );
        state.registered_agents.insert(agent_id("agent-1"));
    }

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx);
    assert!(ext.state.lock().registered_agents.is_empty());
}

/// Bot tokens are embedded in Bot API request paths, so endpoint overrides must
/// not let production plaintext or URL credentials leak the token.
#[test]
fn config_rejects_unsafe_api_base_overrides() {
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));

    for api_base in [
        "http://example.com",
        "https://user@example.com",
        "https://example.com?debug=1",
        "https://example.com/#frag",
    ] {
        let err = ExtConfig {
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec![123],
            api_base: Some(api_base.to_owned()),
            ..Default::default()
        }
        .validate(&secrets)
        .expect_err("unsafe api_base should be rejected");
        assert!(err.contains("api_base"), "{api_base}: {err}");
    }

    ExtConfig {
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec![123],
        api_base: Some("http://127.0.0.1:1234".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect("loopback http test endpoint should be allowed");
}

/// `telegram_send` is intentionally gated on prior registration so arbitrary
/// agents cannot send messages without opting into the Telegram bridge first.
#[test]
fn telegram_send_fails_before_registration() {
    let (ext, rx, _client) = extension();
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("telegram_register"));
}

/// A Telegram API send failure must produce a tool error without submitting a
/// preceding or later `message.sent_reported` report.
#[test]
fn telegram_send_transport_failure_does_not_submit_sent_report() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    client.fail_sends("Telegram transport error");

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("hi")));
    assert_eq!(expect_tool_error(&rx), "Telegram transport error");
    assert!(
        rx.try_recv().is_err(),
        "unexpected message.sent_reported after failure"
    );
}

/// Registering an agent updates in-memory runtime state and lazily marks the
/// poller as started, without persisting a stale registration anywhere.
#[test]
fn telegram_register_true_registers_agent_and_starts_poller() {
    let (ext, rx, _client) = extension();
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    let state = ext.state.lock();
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.poller_started);
}

/// Two Tau sessions using the same Telegram Bot API base and bot token would
/// race on Telegram's singleton `getUpdates` cursor, so the second registration
/// must fail closed before it starts polling and without exposing the raw
/// token.
#[test]
fn telegram_register_fails_when_update_stream_lock_is_held() {
    let root = temp_ext_root();
    let cfg = cfg();
    let (tx1, _rx1) = mpsc::channel();
    let ext1 = test_extension(FakeClient::new(), tx1);
    ext1.apply_config(cfg.clone(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = test_extension(FakeClient::new(), tx2);
    ext2.apply_config(cfg, Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));

    let _progress = rx2.recv().expect("progress");
    let msg = rx2.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(
        error.message.contains("already locked"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("token"),
        "lock contention leaked token: {}",
        error.message
    );
    assert!(
        !ext2
            .state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-2")),
        "failed registration must not leave the agent registered"
    );
}

/// A configured Telegram webhook and getUpdates polling are mutually exclusive,
/// so registration must fail visibly instead of claiming success and leaving
/// the background poller to fail later. Tau must not delete the webhook or drop
/// pending updates on the user's behalf.
#[test]
fn telegram_register_fails_when_webhook_is_active() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_webhook_info(Ok(TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: Some(7),
        last_error_message: Some("delivery failed".to_owned()),
    }));
    let ext = test_extension(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let message = expect_tool_error(&rx);
    assert!(message.contains("active webhook"), "{message}");
    assert!(message.contains("did not delete"), "{message}");
    assert!(message.contains("7 pending"), "{message}");
    assert!(
        !ext.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
    assert_eq!(ext.state.lock().pending_local_registrations, 0);
}

/// Successful webhook preflight responses can reflect the request credential,
/// so production registration must redact it before the active-webhook error
/// reaches the local tool output.
#[test]
fn telegram_register_redacts_token_reflected_by_webhook_diagnostic() {
    let token = "secret-token-reflected-by-webhook";
    let body = format!(
        r#"{{"ok":true,"result":{{"url":"https://example.invalid/hook","last_error_message":"delivery failed for {token} after retry"}}}}"#
    );
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::ContentLength,
        body.into_bytes(),
        "application/json",
    );
    let (tx, rx) = mpsc::channel();
    let ext = test_extension(Arc::new(HttpTelegramClient::default()), tx);
    let mut config = cfg();
    config.api_base = api_base;
    config.bot_token = token.to_owned();
    ext.apply_config(config, Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let message = expect_tool_error(&rx);
    assert!(
        message.contains("delivery failed for <redacted> after retry"),
        "{message}"
    );
    assert!(!message.contains(token), "{message}");
    fixture.join().expect("fixture should exit");
}

/// If webhook status cannot be checked, registration fails closed so the tool
/// result cannot imply that Tau owns Telegram's singleton update stream.
#[test]
fn telegram_register_fails_when_webhook_preflight_fails() {
    let (tx, rx) = mpsc::channel();
    let ext = test_extension(
        FakeClient::with_webhook_info(Err(TelegramApiFailure::Transport)),
        tx,
    );
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));

    let message = expect_tool_error(&rx);
    assert!(
        message.contains("could not verify Telegram webhook status"),
        "{message}"
    );
    assert!(
        !ext.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
    assert_eq!(ext.state.lock().pending_local_registrations, 0);
}

/// Once Tau already owns and polls the update stream, additional local agents
/// should not lose ownership because a later webhook status check fails.
/// Runtime webhook/consumer contention after ownership is detected reactively
/// through `getUpdates` errors.
#[test]
fn additional_registration_does_not_drop_existing_stream_ownership_on_webhook_state() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_webhook_info(Ok(TgWebhookInfo::default()));
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    *client.webhook_info.lock().expect("lock") = Ok(TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: None,
        last_error_message: None,
    });
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx);

    let state = ext.state.lock();
    assert!(state.registered_agents.contains(&agent_id("agent-1")));
    assert!(state.registered_agents.contains(&agent_id("agent-2")));
    assert!(state.update_stream_lock.is_some());
}

/// The advisory lock identity includes the bot token as hashed input, not just
/// the API base, so independent bots served from the same endpoint can poll
/// concurrently.
#[test]
fn update_stream_lock_allows_different_bot_tokens() {
    let root = temp_ext_root();
    let (tx1, _rx1) = mpsc::channel();
    let ext1 = test_extension(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = test_extension(FakeClient::new(), tx2);
    let mut second_cfg = cfg();
    second_cfg.bot_token = "other-secret-token".to_owned();
    ext2.apply_config(second_cfg, Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));

    expect_tool_finished(&rx2);
    assert!(
        ext2.state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-2"))
    );
}

/// After the final local agent unregisters and the poller returns to idle, the
/// stream lock must be released so another Tau process can take over. A later
/// re-registration in the original process must then reacquire the lock and
/// fail closed if that other process still owns the stream.
#[test]
fn register_after_idle_must_reacquire_update_stream_lock() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let ext1 = test_extension(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = test_extension(FakeClient::new(), tx2);
    ext2.apply_config(cfg(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx1);
    std::thread::sleep(Duration::from_millis(100));

    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let message = expect_tool_error(&rx1);
    assert!(message.contains("already locked"), "{message}");
    assert!(
        !ext1
            .state
            .lock()
            .registered_agents
            .contains(&agent_id("agent-1"))
    );
}

/// Unregistering while a long-poll request is in flight must not release the OS
/// lock until that request has returned, otherwise another Tau process could
/// issue a concurrent `getUpdates` against the singleton Telegram cursor.
#[test]
fn in_flight_poll_keeps_update_stream_lock_after_unregister() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let client1 = ControlledPollClient::new();
    let ext1 = test_extension(client1.clone(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = test_extension(FakeClient::new(), tx2);
    ext2.apply_config(cfg(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    client1.wait_for_call();
    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_finished(&rx1);

    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    let message = expect_tool_error(&rx2);
    assert!(message.contains("already locked"), "{message}");

    client1.release_first_response(Vec::new());
    std::thread::sleep(Duration::from_millis(100));
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);
}

/// Telegram reports out-of-band long-poll contention as HTTP 409 conflicts. The
/// background poller must turn that into a user-visible diagnostic and clear
/// the active registration instead of silently leaving the agent apparently
/// connected.
#[test]
fn get_updates_409_conflict_emits_notice_and_unregisters_agents() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);
    client.wait_for_call();
    client.release_error(
        "Telegram returned HTTP 409: Conflict: terminated by other getUpdates request; \
         make sure that only one bot instance is running",
    );

    let notice = expect_notice(&rx);
    assert_eq!(notice.level, tau_proto::NoticeLevel::Warning);
    assert!(
        notice.message.contains("another long-poll consumer"),
        "{}",
        notice.message
    );
    assert!(
        notice.message.contains("stopped Telegram polling"),
        "{}",
        notice.message
    );
    let state = ext.state.lock();
    assert!(state.registered_agents.is_empty());
    assert!(state.update_stream_lock.is_none());
}

/// 409 conflict classification must remain robust enough to distinguish the
/// actionable webhook and competing-long-poll cases while ignoring unrelated
/// transient polling failures.
#[test]
fn telegram_contention_diagnostic_classifies_409_conflicts() {
    let cases = [
        (
            "Telegram returned HTTP 409: Conflict: terminated by setWebhook request",
            Some("webhook"),
        ),
        (
            "Telegram returned HTTP 409: Conflict: terminated by other getUpdates request; make sure that only one bot instance is running",
            Some("another long-poll consumer"),
        ),
        (
            "Telegram returned HTTP 409: Conflict: unknown",
            Some("HTTP 409 conflict"),
        ),
        ("Telegram transport error", None),
    ];

    for (input, expected) in cases {
        let diagnostic = telegram_contention_diagnostic(input);
        match expected {
            Some(expected) => assert!(
                diagnostic
                    .as_deref()
                    .is_some_and(|text| text.contains(expected)),
                "{input}: {diagnostic:?}"
            ),
            None => assert_eq!(diagnostic, None, "{input}"),
        }
    }
}

/// Webhook error text is Telegram-provided diagnostic content, so it must be
/// bounded and stripped of non-whitespace control characters before being shown
/// to the user.
#[test]
fn webhook_active_message_bounds_and_sanitizes_last_error() {
    let message = webhook_active_message(&TgWebhookInfo {
        url: "https://example.invalid/hook".to_owned(),
        pending_update_count: None,
        last_error_message: Some(format!("bad\u{1b}{}", "x".repeat(2000))),
    });

    assert!(message.contains("bad�"));
    assert!(message.ends_with('…'));
    assert!(message.len() < 1300, "message too long: {}", message.len());
}

/// Active reconfiguration to a Telegram stream already locked by another Tau
/// process must fail closed: no raw token in diagnostics, no stale
/// registration, and no old config left available for later sends.
#[test]
fn active_reconfigure_to_locked_stream_fails_closed() {
    let root = temp_ext_root();
    let (tx1, rx1) = mpsc::channel();
    let ext1 = test_extension(FakeClient::new(), tx1);
    ext1.apply_config(cfg(), Some(root.join("std-telegram-1")))
        .expect("apply first config");
    let (tx2, rx2) = mpsc::channel();
    let ext2 = test_extension(FakeClient::new(), tx2);
    let mut locked_cfg = cfg();
    locked_cfg.bot_token = "super-secret-telegram-token".to_owned();
    ext2.apply_config(locked_cfg.clone(), Some(root.join("std-telegram-2")))
        .expect("apply second config");

    ext1.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx1);
    ext2.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-2", bool_args(true)));
    expect_tool_finished(&rx2);

    let message = ext1
        .apply_config(locked_cfg, Some(root.join("std-telegram-1")))
        .expect_err("active reconfigure to locked stream should fail");
    assert!(message.contains("already locked"), "{message}");
    assert!(
        !message.contains("super-secret-telegram-token"),
        "lock contention leaked token: {message}"
    );
    {
        let state = ext1.state.lock();
        assert!(state.config.is_none());
        assert!(state.registered_agents.is_empty());
    }

    ext1.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("stale send")));
    let message = expect_tool_error(&rx1);
    assert!(message.contains("telegram_register"), "{message}");
}

/// Messages from users outside the allowlist must not become Tau prompts.
#[test]
fn incoming_unallowed_user_is_not_routed() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 999,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
}

/// Attachments without text or captions must be acknowledged as unsupported
/// instead of being silently dropped, so allowlisted Telegram users know no Tau
/// prompt was routed.
#[test]
fn textless_allowed_message_gets_unsupported_reply() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: None,
            }),
        },
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(
        client.sent.lock().expect("lock")[0].1,
        "Only text messages are supported by this Tau bridge."
    );
}

/// With exactly one registered agent, plain Telegram text submits a transient
/// delivered report with transport-neutral source metadata.
#[test]
fn one_registered_agent_routes_plain_text() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("hello".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.agent_id.as_str(), "agent-1");
    assert_eq!(report.text, "hello");
    assert_eq!(report.sender.stable_id, telegram_sender_ref("123"));
    assert_eq!(
        report.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert_eq!(
        report
            .conversation
            .as_ref()
            .and_then(|value| value.alias.as_ref()),
        None
    );
    assert_eq!(
        report
            .conversation
            .as_ref()
            .expect("conversation")
            .stable_id,
        "123"
    );
    assert_delivered_live_replay_parity(report);
}

/// A routed update must remain before the Telegram cursor until the exact
/// canonical delivery fact returns from this configured publisher.
#[test]
fn routed_update_advances_only_after_exact_canonical_echo() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);
    assert_eq!(ext.state.lock().next_update_offset, None);

    ext.acknowledge_live_delivery(&canonical_delivered(report));

    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(42)));
}

/// Telegram redelivery before canonical confirmation must replay the retained
/// report exactly rather than recomputing its target from mutable routing.
#[test]
fn missing_canonical_echo_replays_exact_routed_report() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let update = TgUpdate {
        update_id: telegram_update_id(41),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("hello".to_owned()),
        }),
    };

    process_update(&ext, update.clone());
    let first = expect_delivered(&rx);
    {
        let mut state = ext.state.lock();
        state.registered_agents.clear();
        state.registered_agents.insert(agent_id("agent-2"));
    }
    process_update(&ext, update);
    let replay = expect_delivered(&rx);

    assert_eq!(replay, first);
    assert_eq!(replay.agent_id.as_str(), "agent-1");
    assert_eq!(ext.state.lock().next_update_offset, None);
}

/// Publisher, agent, message, and report-ID correlation all prevent unrelated
/// canonical facts from retiring a routed Telegram update.
#[test]
fn unrelated_canonical_deliveries_do_not_ack_routed_update() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);

    let mut wrong_publisher = report
        .clone()
        .with_publisher(tau_proto::MessagePublisherId::parse("other-telegram").expect("publisher"));
    ext.acknowledge_live_delivery(&wrong_publisher);
    assert_eq!(ext.state.lock().next_update_offset, None);

    wrong_publisher = canonical_delivered(report.clone());
    wrong_publisher.agent_id = MessageAgentTarget::new("agent-2");
    ext.acknowledge_live_delivery(&wrong_publisher);
    assert_eq!(ext.state.lock().next_update_offset, None);

    let mut wrong_message = canonical_delivered(report.clone());
    wrong_message.message_id = telegram_message_ref("123", "999");
    ext.acknowledge_live_delivery(&wrong_message);
    assert_eq!(ext.state.lock().next_update_offset, None);

    let mut wrong_report = canonical_delivered(report.clone());
    wrong_report.extension_data = tau_proto::MessageExtensionData::default();
    ext.acknowledge_live_delivery(&wrong_report);
    assert_eq!(ext.state.lock().next_update_offset, None);

    ext.acknowledge_live_delivery(&canonical_delivered(report));
    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(42)));
}

/// The live event dispatcher must reject the extension's own transient report;
/// only the harness-authored canonical event is an acknowledgement.
#[test]
fn reported_delivery_event_does_not_ack_routed_update() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);
    let runtime = TelegramRuntime { ext };

    handle_live_event_value(&runtime, Event::MessageDeliveredReported(report.clone()));
    assert_eq!(runtime.ext.state.lock().next_update_offset, None);

    handle_live_event_value(
        &runtime,
        Event::MessageDelivered(canonical_delivered(report)),
    );
    assert_eq!(
        runtime.ext.state.lock().next_update_offset,
        Some(update_offset(42))
    );
}

/// Mixed routed and non-routed updates may acknowledge out of order, but the
/// Telegram cursor advances only through their acknowledged ordered prefix.
#[test]
fn mixed_update_checkpoints_advance_only_contiguous_acknowledged_prefix() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let message = |update_id, text: &str| TgUpdate {
        update_id: telegram_update_id(update_id),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some(text.to_owned()),
        }),
    };

    process_update(&ext, message(10, "first"));
    let first = expect_delivered(&rx);
    process_update(&ext, message(12, "/agents"));
    process_update(&ext, message(15, "third"));
    let third = expect_delivered(&rx);
    assert_eq!(ext.state.lock().next_update_offset, None);

    ext.acknowledge_live_delivery(&canonical_delivered(third));
    assert_eq!(ext.state.lock().next_update_offset, None);

    ext.acknowledge_live_delivery(&canonical_delivered(first));
    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(16)));
}

/// A non-routed update acknowledges at processing return and emits no Tau
/// event, including when Telegram update IDs contain numeric gaps.
#[test]
fn non_routed_updates_acknowledge_at_processing_return() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent.lock().expect("lock").len(), 1);
    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(42)));
}

/// A routed head may force Telegram to replay later commands. Those
/// non-routed commands may repeat best-effort replies, while `/start` and
/// `/select` continue replacing the same local state idempotently.
#[test]
fn blocked_non_routed_replay_preserves_command_replacement_semantics() {
    let (ext, rx, client) = extension();
    let mut dynamic_cfg = cfg();
    dynamic_cfg.configured_chat_id = None;
    ext.apply_config(dynamic_cfg, Some(temp_state_dir()))
        .expect("apply dynamic-chat config");
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state.learned_chat = Some(LinkedChat {
            chat_id: 123,
            user_id: 123,
        });
        state
            .selected_agent_by_chat
            .insert(123, agent_id("agent-1"));
    }
    let message = |update_id, text: &str| TgUpdate {
        update_id: telegram_update_id(update_id),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some(text.to_owned()),
        }),
    };

    process_update(&ext, message(10, "block cursor"));
    let _routed = expect_delivered(&rx);
    process_update(&ext, message(11, "/start"));
    process_update(&ext, message(11, "/start"));
    process_update(&ext, message(12, "/select agent-2"));
    process_update(&ext, message(12, "/select agent-2"));

    let state = ext.state.lock();
    assert_eq!(state.next_update_offset, None);
    assert_eq!(state.learned_chat.expect("linked chat").chat_id, 123);
    assert_eq!(
        state
            .selected_agent_by_chat
            .get(&123)
            .expect("selected agent")
            .as_str(),
        "agent-2"
    );
    drop(state);
    let sent = client.sent.lock().expect("lock");
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0].1, sent[1].1);
    assert_eq!(sent[2].1, sent[3].1);
}

/// Report visibility must happen after pending insertion, so an echo handled as
/// soon as another thread receives the report cannot race ahead of checkpoint
/// creation.
#[test]
fn immediate_canonical_echo_cannot_beat_checkpoint_insertion() {
    let (tx, rx) = mpsc::channel();
    let ext = Arc::new(test_extension(FakeClient::new(), tx));
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let processing_ext = Arc::clone(&ext);

    let processing = std::thread::spawn(move || {
        process_update(
            &processing_ext,
            TgUpdate {
                update_id: telegram_update_id(41),
                message: Some(TgMessage {
                    chat_id: 123,
                    chat_type: Some("private".to_owned()),
                    user_id: 123,
                    from_name: None,
                    text: Some("hello".to_owned()),
                }),
            },
        );
    });
    let report = expect_delivered(&rx);
    ext.acknowledge_live_delivery(&canonical_delivered(report));
    processing.join().expect("update processing");

    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(42)));
}

/// Re-entering backlog drain after local listener loss must replay a retained
/// routed update, discard unseen stale work, and advance both only after the
/// routed canonical echo.
#[test]
fn reconnect_drain_replays_pending_route_and_discards_unseen_backlog() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let routed = TgUpdate {
        update_id: telegram_update_id(10),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("retain me".to_owned()),
        }),
    };
    process_update(&ext, routed.clone());
    let first = expect_delivered(&rx);
    let generation = ext.state.lock().config_generation;

    ext.process_draining_update_for_generation(routed, generation);
    let replay = expect_delivered(&rx);
    assert_eq!(replay, first);
    ext.process_draining_update_for_generation(
        TgUpdate {
            update_id: telegram_update_id(11),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
        generation,
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
    assert_eq!(ext.state.lock().next_update_offset, None);

    ext.acknowledge_live_delivery(&canonical_delivered(first));
    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(12)));
}

/// Retained-drain replay stops before a suffix when mandatory output fails.
#[test]
fn drain_report_failure_stops_suffix() {
    let (ext, rx, _) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let update = |id| TgUpdate {
        update_id: telegram_update_id(id),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("retained".to_owned()),
        }),
    };
    process_update(&ext, update(31));
    let _ = expect_delivered(&rx);
    drop(rx);
    let generation = ext.state.lock().config_generation;
    assert_eq!(
        ext.process_draining_batch(vec![update(31), update(32)], generation),
        ProcessingControl::Stop
    );
    let state = ext.state.lock();
    assert!(matches!(
        state
            .live_checkpoints
            .existing_update(telegram_update_id(32), state.next_update_offset),
        ExistingUpdate::New
    ));
}

/// Local-poll checkpoints are intentionally process-memory state. A fresh
/// extension after a crash treats an unconfirmed redelivery as startup backlog
/// rather than claiming durable recovery it does not provide.
#[test]
fn crash_forgets_pending_checkpoint_and_startup_drain_discards_redelivery() {
    let (old_ext, old_rx, _client) = extension();
    old_ext
        .state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let update = TgUpdate {
        update_id: telegram_update_id(41),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("lost across crash".to_owned()),
        }),
    };
    process_update(&old_ext, update.clone());
    let _pending = expect_delivered(&old_rx);

    let (new_ext, new_rx, client) = extension();
    let generation = new_ext.state.lock().config_generation;
    new_ext.process_draining_update_for_generation(update, generation);

    assert!(new_rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
    assert_eq!(
        new_ext.state.lock().next_update_offset,
        Some(update_offset(42))
    );
}

/// A same-stream configuration generation must retain an already submitted
/// checkpoint so its delayed canonical echo can still advance the cursor.
#[test]
fn same_stream_reconfiguration_preserves_pending_checkpoint() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);
    let mut same_stream = cfg();
    same_stream.poll_timeout_seconds = 9;

    ext.apply_config(same_stream, Some(temp_state_dir()))
        .expect("same-stream reconfiguration");
    ext.acknowledge_live_delivery(&canonical_delivered(report));

    assert_eq!(ext.state.lock().next_update_offset, Some(update_offset(42)));
}

/// Switching API-base-plus-token stream identity must clear old checkpoints so
/// a delayed fact from the retired stream cannot move the new cursor.
#[test]
fn stream_reconfiguration_rejects_old_checkpoint_echo() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);
    let mut other_stream = cfg();
    other_stream.bot_token = "other-token".to_owned();

    ext.apply_config(other_stream, Some(temp_state_dir()))
        .expect("stream reconfiguration");
    ext.acknowledge_live_delivery(&canonical_delivered(report));

    assert_eq!(ext.state.lock().next_update_offset, None);
}

/// The pending retry schedule grows deterministically and caps permanently
/// missing echoes instead of polling and re-emitting at a fixed high rate.
#[test]
fn missing_echo_retry_backoff_is_exponential_and_bounded() {
    let mut backoff = PendingRetryBackoff::new();
    let delays = (0..8).map(|_| backoff.take_delay()).collect::<Vec<_>>();

    assert_eq!(
        delays,
        vec![
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ]
    );
    backoff.reset();
    assert_eq!(backoff.take_delay(), Duration::from_millis(250));
}

/// Shutdown notification must cancel a long pending-retry delay without
/// waiting for its wall-clock deadline.
#[test]
fn shutdown_cancels_pending_retry_wait() {
    let state = Arc::new(SharedState::new());
    let generation = state.lock().coordination_generation;
    let (wait_tx, wait_rx) = mpsc::channel();
    state.observe_next_wait(wait_tx);
    let waiter_state = Arc::clone(&state);
    let waiter = std::thread::spawn(move || {
        wait_for_coordination_change_or_shutdown(
            &waiter_state,
            &AtomicBool::new(false),
            Duration::from_secs(60),
            generation,
        );
    });

    wait_rx.recv().expect("waiter reached coordination wait");
    state.lock().shutdown_requested = true;
    state.notify_all();
    waiter.join().expect("shutdown wakes retry wait");
}

/// Unrelated and repeated canonical deliveries must not mutate poller
/// coordination, so high-rate ambient message traffic cannot bypass retry
/// backoff for a missing Telegram echo.
#[test]
fn unrelated_canonical_delivery_does_not_wake_pending_retry() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(41),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let report = expect_delivered(&rx);
    let generation = ext.state.lock().coordination_generation;
    let unrelated = MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("other-telegram").expect("publisher"),
        MessageAgentTarget::new("agent-2"),
        telegram_message_ref("123", "999"),
        MessageParty {
            stable_id: "other".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "unrelated",
    );

    for _ in 0..100 {
        ext.acknowledge_live_delivery(&unrelated);
    }
    assert_eq!(ext.state.lock().coordination_generation, generation);

    let waiter_state = Arc::clone(&ext.state);
    let (wait_tx, wait_rx) = mpsc::channel();
    ext.state.observe_next_wait(wait_tx);
    let waiter = std::thread::spawn(move || {
        wait_for_coordination_change_or_shutdown(
            &waiter_state,
            &AtomicBool::new(false),
            Duration::from_secs(60),
            generation,
        );
    });
    wait_rx.recv().expect("waiter reached coordination wait");
    ext.acknowledge_live_delivery(&canonical_delivered(report.clone()));
    waiter.join().expect("matching echo wakes retry wait");
    let progressed = ext.state.lock().coordination_generation;
    assert_ne!(progressed, generation);
    ext.acknowledge_live_delivery(&canonical_delivered(report));
    assert_eq!(ext.state.lock().coordination_generation, progressed);
}

/// A local listener reconnect enters the real poll-loop drain path: retained
/// routed input replays exactly, unseen backlog is discarded, an empty batch
/// completes drain, and live polling resumes at the echo-gated offset.
#[test]
fn poll_loop_reconnect_replays_pending_then_resumes_after_empty_drain() {
    let (tx, rx) = mpsc::channel();
    let pending = TgUpdate {
        update_id: telegram_update_id(10),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("pending".to_owned()),
        }),
    };
    let unseen = TgUpdate {
        update_id: telegram_update_id(11),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("/agents".to_owned()),
        }),
    };
    let fresh = TgUpdate {
        update_id: telegram_update_id(12),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("fresh".to_owned()),
        }),
    };
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx.clone());
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state
            .ensure_update_stream_locked(&cfg())
            .expect("stream lock");
        state.poller_drained_initial_backlog = true;
    }
    process_update(&ext, pending.clone());
    let first = expect_delivered(&rx);
    {
        let mut state = ext.state.lock();
        state.registered_agents.clear();
        state.poller_drained_initial_backlog = false;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::clone(&ext.state);
    let poll_shutdown = Arc::clone(&shutdown);
    let poll_client: Arc<dyn TelegramClient> = client.clone();
    let poller =
        std::thread::spawn(move || poll_loop(state, poll_client, tx.into(), poll_shutdown));

    client.wait_for_call_count(1);
    assert_eq!(client.offsets.lock().expect("lock")[0], None);
    client.release_first_response(vec![pending, unseen]);
    let replay = expect_delivered(&rx);
    assert_eq!(replay, first);
    ext.acknowledge_live_delivery(&canonical_delivered(first));
    client.wait_for_call_count(2);
    assert_eq!(client.offsets.lock().expect("lock")[1], Some(12));
    client.release_first_response(Vec::new());
    client.wait_for_call_count(3);
    assert_eq!(client.offsets.lock().expect("lock")[2], Some(12));
    client.release_first_response(vec![fresh]);
    let resumed = expect_delivered(&rx);
    assert_eq!(resumed.text, "fresh");

    shutdown.store(true, Ordering::Relaxed);
    {
        let mut state = ext.state.lock();
        state.shutdown_requested = true;
        state.mark_coordination_changed();
    }
    ext.state.notify_all();
    poller.join().expect("poller");
}

/// A fresh process has no retained checkpoints: the real poll loop discards
/// redelivered startup backlog, observes the empty drain boundary, then routes
/// only a later live update.
#[test]
fn poll_loop_restart_discards_unconfirmed_backlog_then_resumes_live() {
    let (tx, rx) = mpsc::channel();
    let stale = TgUpdate {
        update_id: telegram_update_id(41),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("stale after crash".to_owned()),
        }),
    };
    let fresh = TgUpdate {
        update_id: telegram_update_id(42),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("fresh after drain".to_owned()),
        }),
    };
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx.clone());
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state
            .ensure_update_stream_locked(&cfg())
            .expect("stream lock");
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::clone(&ext.state);
    let poll_shutdown = Arc::clone(&shutdown);
    let poll_client: Arc<dyn TelegramClient> = client.clone();
    let poller =
        std::thread::spawn(move || poll_loop(state, poll_client, tx.into(), poll_shutdown));

    client.wait_for_call_count(1);
    assert_eq!(client.offsets.lock().expect("lock")[0], None);
    client.release_first_response(vec![stale]);
    client.wait_for_call_count(2);
    assert_eq!(client.offsets.lock().expect("lock")[1], Some(42));
    client.release_first_response(Vec::new());
    client.wait_for_call_count(3);
    assert_eq!(client.offsets.lock().expect("lock")[2], Some(42));
    client.release_first_response(vec![fresh]);
    let report = expect_delivered(&rx);
    assert_eq!(report.text, "fresh after drain");

    shutdown.store(true, Ordering::Relaxed);
    {
        let mut state = ext.state.lock();
        state.shutdown_requested = true;
        state.mark_coordination_changed();
    }
    ext.state.notify_all();
    poller.join().expect("poller");
}

/// The maximum signed update ID must fail Bot API response decoding so the poll
/// loop uses its cancellable error backoff instead of spinning on redelivery.
#[test]
fn maximum_update_id_enters_poll_error_path_without_offset_overflow() {
    let error = decode_updates(&[serde_json::json!({
        "update_id": i64::MAX,
        "message": {
            "chat": {"id": 123, "type": "private"},
            "from": {"id": 123},
            "text": "overflow",
        },
    })])
    .expect_err("maximum update ID must not decode");

    assert!(error.contains("outside the supported offset range"));
}

/// Repeated unrepresentable update IDs use the poller's cancellable error
/// backoff in both startup-drain and already-drained modes instead of issuing
/// an immediate second request.
#[test]
fn maximum_update_id_poll_error_is_bounded_before_and_after_drain() {
    for drained in [false, true] {
        let (tx, _rx) = mpsc::channel();
        let client = ControlledPollClient::new();
        let ext = test_extension(client.clone(), tx.clone());
        ext.apply_config(cfg(), Some(temp_state_dir()))
            .expect("apply config");
        {
            let mut state = ext.state.lock();
            state.registered_agents.insert(agent_id("agent-1"));
            state
                .ensure_update_stream_locked(&cfg())
                .expect("stream lock");
            state.poller_drained_initial_backlog = drained;
        }
        let (wait_tx, wait_rx) = mpsc::channel();
        ext.state.observe_next_wait(wait_tx);
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::clone(&ext.state);
        let poll_shutdown = Arc::clone(&shutdown);
        let poll_client: Arc<dyn TelegramClient> = client.clone();
        let poller =
            std::thread::spawn(move || poll_loop(state, poll_client, tx.into(), poll_shutdown));

        client.wait_for_call_count(1);
        client.release_error(
            "Telegram getUpdates returned an update id outside the supported offset range",
        );
        wait_rx.recv().expect("poller entered error backoff");
        {
            let mut state = ext.state.lock();
            assert_eq!(*client.called.lock().expect("lock"), 1);
            state.shutdown_requested = true;
            state.mark_coordination_changed();
        }
        shutdown.store(true, Ordering::Relaxed);
        ext.state.notify_all();
        poller.join().expect("poller");
    }
}

/// Multiple registered agents without selection are ambiguous, so the bridge
/// replies with guidance instead of guessing a Tau target.
#[test]
fn multiple_agents_without_selection_do_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("Multiple"));
}

/// Bot-facing command replies must make `agent_id` the primary designator so
/// users copy stable ids into `/select` and `/to`, with display names only as
/// parenthetical context.
#[test]
fn bot_commands_show_agent_id_before_display_name() {
    let (ext, _rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state
            .agent_labels
            .insert(agent_id("agent-1"), "Alpha".to_owned());
        state
            .agent_labels
            .insert(agent_id("agent-2"), "Beta".to_owned());
    }

    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(2),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/select agent-2".to_owned()),
            }),
        },
    );

    let sent = client.sent.lock().expect("lock");
    assert_eq!(
        sent[0].1,
        "Registered Tau agents:\n- agent-1 (Alpha)\n- agent-2 (Beta)"
    );
    assert_eq!(sent[1].1, "Selected agent-2 (Beta)");
}

/// Agent ids should stand alone in `/agents` output when a display name is
/// missing, blank, or identical to the id, avoiding noisy duplicate context.
#[test]
fn agents_list_omits_empty_or_duplicate_display_names() {
    let (ext, _rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
        state.registered_agents.insert(agent_id("agent-3"));
        state
            .agent_labels
            .insert(agent_id("agent-2"), "   ".to_owned());
        state
            .agent_labels
            .insert(agent_id("agent-3"), "agent-3".to_owned());
    }

    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/agents".to_owned()),
            }),
        },
    );

    assert_eq!(
        client.sent.lock().expect("lock")[0].1,
        "Registered Tau agents:\n- agent-1\n- agent-2\n- agent-3"
    );
}

/// Unknown or malformed slash commands must get command feedback instead of
/// being routed as ordinary prompts across the external-input boundary.
#[test]
fn malformed_slash_commands_are_not_routed_as_prompts() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    for (update_id, text) in [(1, "/startx"), (2, "/select"), (3, "/to")] {
        process_update(
            &ext,
            TgUpdate {
                update_id: telegram_update_id(update_id),
                message: Some(TgMessage {
                    chat_id: 123,
                    chat_type: None,
                    user_id: 123,
                    from_name: None,
                    text: Some(text.to_owned()),
                }),
            },
        );
    }
    assert!(rx.try_recv().is_err());
    let sent = client.sent.lock().expect("lock");
    assert!(sent[0].1.contains("Unknown"));
    assert!(sent[1].1.contains("Usage: /select"));
    assert!(sent[2].1.contains("Usage: /to"));
}

/// `/select` stores a chat-local target so later plain text can be routed even
/// while multiple agents are registered.
#[test]
fn select_then_plain_text_routes_to_selected_agent() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state.registered_agents.insert(agent_id("agent-2"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("/select agent-2".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(2),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: None,
                user_id: 123,
                from_name: None,
                text: Some("hi".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.agent_id.as_str(), "agent-2");
}

/// Runtime argument validation must match the schema so a model cannot rely on
/// ignored extra fields that may later gain meaning.
#[test]
fn telegram_send_rejects_unknown_chat_id_argument() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.registered_agents.insert(agent_id("agent-1"));
        state
            .agent_labels
            .insert(agent_id("agent-1"), "Helper".to_owned());
    }
    let args = CborValue::Map(vec![
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
        (
            CborValue::Text("chat_id".to_owned()),
            CborValue::Integer(999.into()),
        ),
    ]);
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", args));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("unknown argument"));
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Group chats are refused unless the user explicitly configured that chat id;
/// this keeps the MVP private-chat oriented by default.
#[test]
fn unconfigured_group_chat_is_refused() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.learned_chat = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .1
            .contains("Group chats")
    );
}

/// Explicitly configured group chat ids are allowed, while the model still does
/// not get to choose a destination for outgoing messages.
#[test]
fn configured_group_chat_can_route() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = Some(-100);
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("prompt") else {
        panic!("emit")
    };
    assert!(matches!(*emit.event, Event::MessageDeliveredReported(_)));
}

/// When a fixed chat is configured, allowlisted messages from any other private
/// chat must not route into Tau because replies would go to the configured
/// chat.
#[test]
fn configured_chat_rejects_other_private_chat() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 456,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .1
            .contains("different Telegram chat")
    );
}

/// Without a configured chat, ordinary text must wait for an explicit `/start`
/// link so the extension has a single active reply destination.
#[test]
fn unconfigured_private_text_before_start_does_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("/start"));
}

/// Direct `/to` routing must also wait for a linked chat; otherwise a prompt
/// submitted before `/start` could later receive replies in a different chat.
#[test]
fn unconfigured_to_before_start_does_not_route() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/to agent-1 hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock")[0].1.contains("/start"));
}

/// A learned private chat is exclusive; another allowlisted private chat cannot
/// redirect future `telegram_send` output or route prompts through the bridge.
#[test]
fn linked_chat_rejects_other_private_chat() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state
            .config
            .as_mut()
            .expect("config")
            .allowed_user_ids
            .insert(456);
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(2),
            message: Some(TgMessage {
                chat_id: 456,
                chat_type: Some("private".to_owned()),
                user_id: 456,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let sent_report = expect_successful_send(&rx);

    let sent = client.sent.lock().expect("lock");
    assert_eq!(sent[0].0, 123);
    assert_eq!(sent[1].0, 456);
    assert_eq!(sent[2], (123, "[agent-1] reply".to_owned()));
    assert_eq!(sent_report.text, "reply");
    assert_eq!(
        sent_report.conversation.expect("conversation").stable_id,
        "123"
    );
}

/// Applying a new fixed chat invalidates registrations so replies for prompts
/// from the old active chat fail closed until agents explicitly re-register.
#[test]
fn reconfigured_chat_id_requires_reregistration_before_send() {
    let (ext, rx, client) = extension();
    {
        let mut state = ext.state.lock();
        state.config.as_mut().expect("config").configured_chat_id = None;
        state.registered_agents.insert(agent_id("agent-1"));
    }
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("/start".to_owned()),
            }),
        },
    );
    let mut new_cfg = cfg();
    new_cfg.configured_chat_id = Some(456);
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };

    assert!(error.message.contains("telegram_register"));
    assert!(
        !client
            .sent
            .lock()
            .expect("lock")
            .iter()
            .any(|sent| sent.0 == 456)
    );
}

/// Allowlist checks run before group handling, so an unallowed group user
/// cannot trigger either a Tau prompt or a Telegram reply from the bridge.
#[test]
fn unallowed_group_user_cannot_route() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: -100,
                chat_type: Some("supergroup".to_owned()),
                user_id: 999,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// The first poll after lazy startup drains Telegram backlog without side
/// effects so old pre-registration messages do not become fresh Tau prompts.
#[test]
fn initial_poller_drops_stale_backlog() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![vec![TgUpdate {
        update_id: telegram_update_id(10),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("old".to_owned()),
        }),
    }]]);
    let ext = test_extension(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    std::thread::sleep(Duration::from_millis(100));
    assert!(rx.try_recv().is_err());
}

/// Initial backlog draining must continue until Telegram returns an empty
/// batch; otherwise older messages split across batches could leak as fresh
/// prompts.
#[test]
fn initial_poller_drops_multiple_stale_batches_until_empty() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![
        vec![TgUpdate {
            update_id: telegram_update_id(10),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("old one".to_owned()),
            }),
        }],
        vec![TgUpdate {
            update_id: telegram_update_id(11),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("old two".to_owned()),
            }),
        }],
        Vec::new(),
        vec![TgUpdate {
            update_id: telegram_update_id(12),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("fresh".to_owned()),
            }),
        }],
    ]);
    let ext = test_extension(client, tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "fresh");
}

/// Telegram updates without a usable message still carry update ids and must be
/// represented so the poller can advance past them to later valid messages.
#[test]
fn decode_update_preserves_non_message_update_id() {
    let update = decode_update(&serde_json::json!({ "update_id": 42 }))
        .expect("update decoding should succeed")
        .expect("update id should be preserved");
    assert_eq!(update.update_id.as_i64(), 42);
    assert_eq!(update.message, None);
}

/// HTTP transport errors must not include Telegram Bot API URLs because those
/// URLs contain the bot token in their path.
#[test]
fn telegram_transport_errors_do_not_expose_bot_token() {
    let client = HttpTelegramClient::default();
    let mut cfg = cfg();
    cfg.bot_token = "secret-token-for-test".to_owned();
    cfg.api_base = "http://127.0.0.1:9".to_owned();
    let err = client
        .send_message(&cfg, 123, "hello")
        .expect_err("connection should fail");
    assert!(
        !err.to_string().contains("secret-token-for-test"),
        "err: {err}"
    );
}

/// A successful response declared at exactly 10 MiB must reach the production
/// JSON decoder, preventing a dependency default from restoring a strict limit.
#[test]
fn telegram_content_length_response_at_body_limit_is_accepted() {
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::ContentLength,
        successful_response_at_body_limit(),
        "application/json",
    );
    let client = HttpTelegramClient::default();
    let mut config = cfg();
    config.api_base = api_base;

    client
        .send_message(&config, 123, "hello")
        .expect("exactly 10 MiB response should succeed");
    fixture.join().expect("fixture should exit");
}

/// A Content-Length response one byte over 10 MiB must become the established
/// Protocol failure while its invalid payload remains unseen by JSON decoding.
#[test]
fn telegram_content_length_response_over_body_limit_is_protocol_failure() {
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::ContentLength,
        vec![
            b'x';
            usize::try_from(MAX_SUCCESSFUL_RESPONSE_BODY_BYTES + 1).expect("10 MiB fits usize")
        ],
        "application/json",
    );
    let client = HttpTelegramClient::default();
    let mut config = cfg();
    config.api_base = api_base;

    assert_oversized_successful_response(
        client
            .send_message(&config, 123, "hello")
            .expect_err("response above 10 MiB must fail"),
    );
    fixture.join().expect("fixture should exit");
}

/// Chunked framing must accept an aggregate payload at exactly 10 MiB, so
/// equality does not depend on a Content-Length header.
#[test]
fn telegram_chunked_response_at_body_limit_is_accepted() {
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::Chunked,
        successful_response_at_body_limit(),
        "application/json",
    );
    let client = HttpTelegramClient::default();
    let mut config = cfg();
    config.api_base = api_base;

    client
        .send_message(&config, 123, "hello")
        .expect("exactly 10 MiB chunked response should succeed");
    fixture.join().expect("fixture should exit");
}

/// Chunked framing must aggregate every chunk under the same cap and reject a
/// 10 MiB plus one payload before its invalid JSON reaches the decoder.
#[test]
fn telegram_chunked_response_over_body_limit_is_protocol_failure() {
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::Chunked,
        vec![
            b'x';
            usize::try_from(MAX_SUCCESSFUL_RESPONSE_BODY_BYTES + 1).expect("10 MiB fits usize")
        ],
        "application/json",
    );
    let client = HttpTelegramClient::default();
    let mut config = cfg();
    config.api_base = api_base;

    assert_oversized_successful_response(
        client
            .send_message(&config, 123, "hello")
            .expect_err("chunked response above 10 MiB must fail"),
    );
    fixture.join().expect("fixture should exit");
}

/// Successful text responses with malformed UTF-8 must retain the prior
/// replacement behavior while the explicit body-limit configuration is active.
#[test]
fn telegram_text_response_with_invalid_utf8_remains_lossy() {
    let (api_base, fixture) = telegram_response_fixture(
        HttpResponseFraming::ContentLength,
        b"{\"ok\":true,\"result\":\""
            .iter()
            .copied()
            .chain([0xff])
            .chain(b"\"}".iter().copied())
            .collect(),
        "text/plain",
    );
    let client = HttpTelegramClient::default();
    let mut config = cfg();
    config.api_base = api_base;

    client
        .send_message(&config, 123, "hello")
        .expect("invalid UTF-8 text response should remain lossy");
    fixture.join().expect("fixture should exit");
}

/// Registering starts a poller, and disconnect/EOF-facing shutdown must not
/// hang waiting for leaked sender clones held by that poller.
#[test]
fn run_exits_after_register_then_disconnect() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("tool");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: None,
        }))
        .expect("disconnect");
    writer.flush().expect("flush");

    run_with_client(
        path_std_io::Cursor::new(input),
        Vec::new(),
        FakeClient::new(),
    )
    .expect("run");
}

/// A routed ingress report must survive exhaustion of the production detached
/// FIFO, replay exactly once through checked output, and advance the Telegram
/// cursor only after the matching canonical echo.
#[test]
fn ingress_report_survives_production_fifo_saturation() {
    let _serial = SATURATION_TEST_LOCK
        .lock()
        .expect("telegram saturation test lock");
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (report_written_tx, report_written_rx) = mpsc::channel();
    let (saturated_tx, saturated_rx) = mpsc::channel();
    let expected_message_id = telegram_message_ref("123", "41");
    *SATURATION_HOOK.lock().expect("telegram saturation hook") =
        Some((expected_message_id.as_str().to_owned(), saturated_tx));
    let hook = SaturationHookGuard;
    let client = ControlledPollClient::new();
    let runner_client: Arc<dyn TelegramClient> = client.clone();
    let output_bytes = Arc::clone(&bytes);
    let output_gate = Arc::clone(&gate);
    let runner = std::thread::spawn(move || {
        run_with_client(
            extension_input,
            SaturationWriter {
                bytes: output_bytes,
                gate: output_gate,
                entered: entered_tx,
                report_written: report_written_tx,
                blocked: false,
                report_seen: false,
            },
            runner_client,
        )
        .map_err(|error| error.to_string())
    });
    let mut input = tau_proto::HarnessOutputWriter::new(harness_input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("extension name"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    input.flush().expect("flush startup");

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(41),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("sender".to_owned()),
            text: Some("saturated ingress".to_owned()),
        }),
    }]);
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("production writer blocked");
    saturated_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("detached FIFO exhausted");
    drop(hook);
    let (closed, wake) = &*gate;
    *closed.lock().expect("writer gate") = false;
    wake.notify_all();

    report_written_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("checked ingress report reached writer");
    let snapshot = bytes.lock().expect("output bytes").clone();
    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(snapshot));
    let report = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode output")
        .into_iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::MessageDeliveredReported(report)
                    if report.message_id == expected_message_id =>
                {
                    Some(report)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("checked ingress report flushed");
    client.wait_for_call_count(3);
    assert_eq!(
        client.offsets.lock().expect("offsets")[2],
        None,
        "cursor advanced before canonical echo"
    );
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(41),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("changed sender".to_owned()),
            text: Some("changed recomputation must not win".to_owned()),
        }),
    }]);
    client.wait_for_call_count(4);
    let replay_snapshot = bytes.lock().expect("output bytes").clone();
    let mut replay_reader = HarnessInputReader::new(path_std_io::Cursor::new(replay_snapshot));
    let matching = std::iter::from_fn(|| replay_reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode replay output")
        .into_iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::MessageDeliveredReported(candidate)
                    if candidate.message_id == expected_message_id =>
                {
                    Some(candidate)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(matching, vec![report.clone(), report.clone()]);

    input
        .write_message(&HarnessOutputMessage::deliver(Event::MessageDelivered(
            report.with_publisher(
                tau_proto::MessagePublisherId::parse("test-extension").expect("publisher"),
            ),
        )))
        .expect("canonical echo");
    input.flush().expect("flush canonical echo");
    client.release_first_response(Vec::new());
    client.wait_for_call_count(5);
    assert_eq!(
        client.offsets.lock().expect("offsets")[4],
        Some(42),
        "canonical echo did not advance the Telegram cursor"
    );
    input
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("fixture complete".to_owned()),
        }))
        .expect("disconnect");
    input.flush().expect("flush completion");
    runner.join().expect("runner").expect("clean disconnect");

    let snapshot = bytes.lock().expect("output bytes").clone();
    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(snapshot));
    let exact_reports = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode output")
        .into_iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::MessageDeliveredReported(report)
                        if report.text == "saturated ingress")
            )
        })
        .count();
    assert_eq!(exact_reports, 2);
}

/// Failure of a checked sent report must exit the production loop and suppress
/// the send call's paired terminal so harness disconnect cleanup owns
/// settlement.
#[test]
fn sent_report_writer_failure_exits_without_paired_terminal() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("extension name"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    let mut send = tool(
        SEND_TOOL_NAME,
        "agent-1",
        message_args("forced report failure"),
    );
    send.call_id = "gyf8-telegram-failed-send".into();
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(send)))
        .expect("send");
    writer.flush().expect("flush input");

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (failed_tx, failed_rx) = mpsc::channel();
    let result = run_with_client(
        path_std_io::Cursor::new(input),
        FailingWriter {
            bytes: Arc::clone(&bytes),
            target: b"message.sent_reported",
            failed: Some(failed_tx),
        },
        FakeClient::new(),
    );
    assert!(
        result.is_err(),
        "mandatory writer failure must exit the loop"
    );
    failed_rx
        .try_recv()
        .expect("selected writer failure occurred");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(
        bytes.lock().expect("bytes").clone(),
    ));
    while let Ok(Some(frame)) = reader.read_message() {
        assert!(
            !matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(),
                        Event::ToolResultReported(result)
                            if result.call_id.as_str() == "gyf8-telegram-failed-send"
                    ) || matches!(emit.event.as_ref(),
                        Event::ToolErrorReported(error)
                            if error.call_id.as_str() == "gyf8-telegram-failed-send"
                    )
            ),
            "sent-report failure published its paired terminal"
        );
    }
}

/// A poller-origin report failure must wake an otherwise idle protocol loop and
/// make process shutdown own cleanup.
#[test]
fn ingress_report_writer_failure_wakes_idle_production_loop() {
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let client = ControlledPollClient::new();
    let runner_client: Arc<dyn TelegramClient> = client.clone();
    let (failed_tx, failed_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_with_client(
            extension_input,
            FailingWriter {
                bytes: Arc::new(Mutex::new(Vec::new())),
                target: b"message.delivered_reported",
                failed: Some(failed_tx),
            },
            runner_client,
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    let mut input = tau_proto::HarnessOutputWriter::new(harness_input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("extension name"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    input.flush().expect("flush startup");

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(91),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("sender".to_owned()),
            text: Some("wake idle loop".to_owned()),
        }),
    }]);
    failed_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("worker report reached failing writer");
    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("idle loop exited")
            .is_err(),
        "worker output failure must fail the production runner"
    );
    assert_eq!(client.active_polls.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(client.poller_exits.load(AtomicOrdering::SeqCst), 1);
}

/// Explicit Disconnect returns promptly while a local poller report remains
/// blocked in the checked writer.
#[test]
fn disconnect_detaches_blocked_local_report() {
    let (disconnect_tx, disconnect_rx) = mpsc::channel();
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let client = ControlledPollClient::new();
    let runner_client: Arc<dyn TelegramClient> = client.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_with_client_observing_disconnect(
            extension_input,
            BlockingReportWriter {
                entered: entered_tx,
                release: release_rx,
            },
            runner_client,
            disconnect_tx,
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });
    let mut input = tau_proto::HarnessOutputWriter::new(harness_input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension").expect("name"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret":"bot","allowed_user_ids":[123],"chat_id":123,
                "poll_timeout_seconds":1
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("configure");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    input.flush().expect("startup");
    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(92),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: None,
            text: Some("blocked".to_owned()),
        }),
    }]);
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("blocked report");
    input
        .write_message(&HarnessOutputMessage::Disconnect(Default::default()))
        .expect("disconnect");
    input.flush().expect("flush disconnect");
    disconnect_rx
        .recv()
        .expect("manual runtime observed disconnect");
    result_rx
        .recv()
        .expect("prompt runner")
        .expect("disconnect");
    release_tx.send(()).expect("release detached writer");
}

/// Disconnect handling must not wait for an in-flight long poll to release its
/// channel sender before the extension process can exit.
#[test]
fn run_exits_promptly_when_disconnect_races_long_poll() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("tool");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: None,
        }))
        .expect("disconnect");
    writer.flush().expect("flush");

    let start = path_std_time::Instant::now();
    run_with_client(
        path_std_io::Cursor::new(input),
        Vec::new(),
        Arc::new(SlowPollClient),
    )
    .expect("run");
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// Replayed tool deliveries must be skipped so historical registrations do not
/// restart the Telegram bridge or authorize later live sends.
#[test]
fn run_ignores_replayed_tool_delivery_before_live_send() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1_700_000_000_000_000),
            Event::ToolStarted(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true))),
        ))
        .expect("replay register");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("live send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(path_std_io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut saw_unregistered_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Emit(emit) = frame
            && let Event::ToolErrorReported(error) = emit.event.as_ref()
            && error.tool_name.as_str() == SEND_TOOL_NAME
            && error.message.contains("telegram_register")
        {
            saw_unregistered_error = true;
        }
    }
    assert!(
        saw_unregistered_error,
        "live send should fail without live registration"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Initial malformed configuration must still surface as ConfigError and Ready
/// in deferred-startup mode, rather than becoming a silent extension startup
/// failure before the harness can publish a replayable notice.
#[test]
fn run_initial_malformed_config_emits_config_error_without_ready() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "unknown_field": true,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("invalid config");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(path_std_io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_ready = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("unknown_field") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Ready(_) => saw_ready = true,
            _ => {}
        }
    }
    assert!(saw_config_error, "initial config error should be reported");
    assert!(!saw_ready, "rejected initial config must withhold Ready");
}

/// The protocol startup path must publish and dispatch the dynamically computed
/// namespaced tools, not only construct the helper structs used by unit tests.
/// A custom initial Configure drives namespaced tool declarations and stamps
/// the same configured instance name on an outbound message report.
#[test]
fn run_custom_instance_registers_and_dispatches_namespaced_tools() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix")),
            instance_name: tau_proto::ExtensionName::parse("telegram-work")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            "work_telegram_register",
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            "work_telegram_send",
            "agent-1",
            message_args("hello"),
        ))))
        .expect("send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(path_std_io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut saw_register_tool = false;
    let mut saw_send_tool = false;
    let mut saw_register_result = false;
    let mut saw_delivery_subscription = false;
    let mut sent_publisher = None;
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Subscribe(subscribe) = &frame {
            saw_delivery_subscription = subscribe.live_selectors.iter().any(|selector| {
                selector
                    == &tau_proto::EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED)
            });
        }
        if let HarnessInputMessage::Emit(emit) = frame {
            match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == "work_telegram_register"
                        && register
                            .tool_group
                            .as_ref()
                            .is_some_and(|group| group.name.as_str() == "work_telegram") =>
                {
                    saw_register_tool = true;
                }
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == "work_telegram_send" =>
                {
                    saw_send_tool = true;
                }
                Event::ToolResultReported(result)
                    if result.tool_name.as_str() == "work_telegram_register" =>
                {
                    saw_register_result = true;
                }
                Event::MessageSentReported(report) => {
                    sent_publisher = Some(report.publisher_extension_id.as_str().to_owned());
                }
                _ => {}
            }
        }
    }
    assert!(
        saw_register_tool,
        "namespaced register tool should be published"
    );
    assert!(saw_send_tool, "namespaced send tool should be published");
    assert!(
        saw_register_result,
        "namespaced register invocation should dispatch"
    );
    assert!(
        saw_delivery_subscription,
        "canonical message delivery must be subscribed for local cursor acknowledgements"
    );
    assert_eq!(sent_publisher.as_deref(), Some("telegram-work"));
}

/// Manual deferred dispatch must preserve tau-client's previous named-handler
/// filtering: unrelated tool calls, including tools owned by another Telegram
/// instance, are not Telegram calls and must not receive Telegram progress or
/// errors.
#[test]
fn run_ignores_unrelated_tool_started_events() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            "other_tool",
            "agent-1",
            CborValue::Map(Vec::new()),
        ))))
        .expect("other tool");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(path_std_io::Cursor::new(input), output, FakeClient::new()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    while let Some(frame) = reader.read_message().expect("read output") {
        if let HarnessInputMessage::Emit(emit) = frame {
            match emit.event.as_ref() {
                Event::ToolProgressReported(progress)
                    if progress.tool_name.as_str() == "other_tool" =>
                {
                    panic!("unrelated tool should not receive Telegram progress");
                }
                Event::ToolErrorReported(error) if error.tool_name.as_str() == "other_tool" => {
                    panic!("unrelated tool should not receive Telegram error");
                }
                Event::ToolResultReported(result) if result.tool_name.as_str() == "other_tool" => {
                    panic!("unrelated tool should not receive Telegram result");
                }
                _ => {}
            }
        }
    }
}

/// Malformed reconfiguration that fails typed deserialization must fail closed:
/// emit `ConfigError`, clear registrations/config, and prevent later sends from
/// using stale Telegram routing state.
#[test]
fn run_malformed_reconfiguration_clears_active_bridge_state() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
                "poll_timeout_seconds": 1,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("valid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("live register");
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "unknown_field": true,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("invalid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("live send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(path_std_io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_unregistered_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("unknown_field") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::ToolErrorReported(error) = emit.event.as_ref()
                    && error.tool_name.as_str() == SEND_TOOL_NAME
                    && error.message.contains("telegram_register")
                {
                    saw_unregistered_error = true;
                }
            }
            _ => {}
        }
    }
    assert!(saw_config_error, "malformed config should emit ConfigError");
    assert!(
        saw_unregistered_error,
        "send should fail after malformed config clears registration"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Removed legacy `tool_namespace` configuration is rejected rather than
/// silently restoring the superseded Telegram-specific naming mechanism.
#[test]
fn run_legacy_tool_namespace_is_rejected() {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    let mut secrets = BTreeMap::new();
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("token"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets: secrets.clone(),
            settings_files: Default::default(),
        }))
        .expect("valid config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            REGISTER_TOOL_NAME,
            "agent-1",
            bool_args(true),
        ))))
        .expect("register");
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "tool_namespace": "tg_ops",
                "bot_token_secret": "bot",
                "allowed_user_ids": [123],
                "chat_id": 123,
            })),
            state_dir: Some(temp_state_dir()),
            secrets,
            settings_files: Default::default(),
        }))
        .expect("namespace config");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(tool(
            SEND_TOOL_NAME,
            "agent-1",
            message_args("reply"),
        ))))
        .expect("send");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let written = output.clone();
    let client = FakeClient::new();
    run_with_client(path_std_io::Cursor::new(input), output, client.clone()).expect("run");

    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let mut saw_config_error = false;
    let mut saw_send_error = false;
    while let Some(frame) = reader.read_message().expect("read output") {
        match frame {
            HarnessInputMessage::ConfigError(error) if error.message.contains("tool_namespace") => {
                saw_config_error = true;
            }
            HarnessInputMessage::Emit(emit) => {
                if let Event::ToolErrorReported(error) = emit.event.as_ref()
                    && error.tool_name.as_str() == SEND_TOOL_NAME
                    && error.message.contains("telegram_register")
                {
                    saw_send_error = true;
                }
            }
            _ => {}
        }
    }
    assert!(
        saw_config_error,
        "legacy tool_namespace should emit ConfigError"
    );
    assert!(
        saw_send_error,
        "send should fail after namespace config error"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Initial backlog drain must be a non-long-poll request. Otherwise a fresh
/// message arriving during the first long poll after registration could be
/// mistaken for stale backlog and dropped.
#[test]
fn initial_empty_drain_then_fresh_message_routes() {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::with_updates(vec![
        Vec::new(),
        vec![TgUpdate {
            update_id: telegram_update_id(11),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("fresh".to_owned()),
            }),
        }],
    ]);
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "fresh");
    assert_eq!(client.poll_timeouts.lock().expect("lock")[0], 0);
}

/// Switching to a different Telegram bot token changes the update stream, so
/// the extension must reset its offset and drain that bot's existing backlog
/// before routing fresh messages.
#[test]
fn reconfigured_bot_token_resets_update_backlog_drain() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(update_offset(99));
    }

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
}

/// Changing the Bot API endpoint also changes the update stream, so stale
/// offsets from the previous endpoint must be dropped.
#[test]
fn reconfigured_api_base_resets_update_backlog_drain() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(update_offset(99));
    }

    let mut new_cfg = cfg();
    new_cfg.api_base = "http://127.0.0.1:1234".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
}

/// Tuning poll timeout alone does not change the Telegram update stream, so the
/// extension should keep the acknowledged offset and avoid redraining already
/// processed updates.
#[test]
fn reconfigured_poll_timeout_keeps_update_offset() {
    let (ext, _rx, _client) = extension();
    {
        let mut state = ext.state.lock();
        state.poller_drained_initial_backlog = true;
        state.next_update_offset = Some(update_offset(99));
    }

    let mut new_cfg = cfg();
    new_cfg.poll_timeout_seconds = 5;
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");

    let state = ext.state.lock();
    assert!(state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, Some(update_offset(99)));
}

/// Poll responses captured under an older config generation must be discarded
/// after reconfiguration so old-stream updates cannot advance or drain the new
/// stream.
#[test]
fn old_generation_empty_poll_response_does_not_drain_new_stream() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.release_first_response(Vec::new());
    std::thread::sleep(Duration::from_millis(100));

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
    assert!(rx.try_recv().is_err());
}

/// Non-empty poll responses from an old config generation must also be
/// discarded, avoiding both stale offset updates and report submission under
/// the new config.
#[test]
fn old_generation_non_empty_poll_response_does_not_route_or_advance_offset() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    let _progress = rx.recv().expect("progress");
    let _result = rx.recv().expect("result");
    client.wait_for_call();

    let mut new_cfg = cfg();
    new_cfg.api_base = "http://127.0.0.1:1234".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(55),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("stale".to_owned()),
        }),
    }]);
    std::thread::sleep(Duration::from_millis(100));

    let state = ext.state.lock();
    assert!(!state.poller_drained_initial_backlog);
    assert_eq!(state.next_update_offset, None);
    assert!(rx.try_recv().is_err());
}

/// A period with no registered agents is a stale-backlog boundary: Telegram
/// messages observed while nobody is listening must advance offsets but must
/// not route after a later registration. Re-registration deliberately pauses
/// inside webhook preflight so the old poll must evaluate retirement while the
/// registration reservation is the stream's only remaining owner interest.
#[test]
fn zero_registered_agents_redrains_backlog_before_routing() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Arc::new(test_extension(client.clone(), tx));
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_success(&rx);

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);

    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_success(&rx);
    client.block_next_webhook_check();
    let registering_ext = Arc::clone(&ext);
    let registration = std::thread::spawn(move || {
        registering_ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    });
    client.wait_for_blocked_webhook_check();
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(20),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("stale while unregistered".to_owned()),
        }),
    }]);
    {
        let mut state = ext.state.lock();
        assert!(state.registered_agents.is_empty());
        assert_eq!(state.pending_local_registrations, 1);
        state.retire_update_stream_lock_if_idle();
        assert!(
            state.update_stream_lock.is_some(),
            "pending registration must retain update-stream ownership"
        );
    }
    client.release_webhook_check();
    registration.join().expect("registration thread");
    expect_tool_success(&rx);
    client.wait_for_call_count(3);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(4);
    client.release_first_response(vec![TgUpdate {
        update_id: telegram_update_id(21),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some("fresh after reregister".to_owned()),
        }),
    }]);

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("fresh prompt") else {
        panic!("emit")
    };
    let Event::MessageDeliveredReported(report) = *emit.event else {
        panic!("message.delivered_reported event")
    };
    assert_eq!(report.text, "fresh after reregister");
}

/// A re-registration made stale by same-stream reconfiguration must release
/// its pending owner interest and wake poller coordination so the idle stream
/// lock can retire instead of remaining pinned by a failed tool call.
#[test]
fn stale_reregistration_releases_pending_stream_interest() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = Arc::new(test_extension(client.clone(), tx));
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_success(&rx);
    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(false)));
    expect_tool_success(&rx);

    client.block_next_webhook_check();
    let registering_ext = Arc::clone(&ext);
    let registration = std::thread::spawn(move || {
        registering_ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    });
    client.wait_for_blocked_webhook_check();

    let mut reconfigured = cfg();
    reconfigured.poll_timeout_seconds = 2;
    ext.apply_config(reconfigured, Some(temp_state_dir()))
        .expect("apply same-stream config");
    let observed_generation = ext.state.lock().coordination_generation;
    let waiting_state = Arc::clone(&ext.state);
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (woke_tx, woke_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let state = waiting_state.lock();
        waiting_tx.send(()).expect("report waiter ready");
        let mut state = waiting_state.wait_while(state, |state| {
            state.coordination_generation == observed_generation
        });
        assert_ne!(state.coordination_generation, observed_generation);
        state.retire_update_stream_lock_if_idle();
        assert!(state.update_stream_lock.is_none());
        woke_tx.send(()).expect("report coordination wake");
    });
    waiting_rx.recv().expect("coordination waiter ready");

    client.release_first_response(Vec::new());
    client.release_webhook_check();
    registration.join().expect("registration thread");
    let message = expect_tool_error(&rx);
    assert!(
        message.contains("configuration changed while checking webhook status"),
        "{message}"
    );
    woke_rx.recv().expect("failed registration wakes poller");
    waiter.join().expect("coordination waiter");
    let state = ext.state.lock();
    assert_eq!(state.pending_local_registrations, 0);
    assert!(state.registered_agents.is_empty());
    assert!(state.update_stream_lock.is_none());
}

/// Error backoff uses the shared-state condvar so a config change wakes it
/// promptly instead of waiting for the full local retry delay.
#[test]
fn poll_error_backoff_wakes_on_config_change() {
    let (tx, rx) = mpsc::channel();
    let client = ControlledPollClient::new();
    let ext = test_extension(client.clone(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.dispatch_tool(tool(REGISTER_TOOL_NAME, "agent-1", bool_args(true)));
    expect_tool_finished(&rx);

    client.wait_for_call_count(1);
    client.release_first_response(Vec::new());
    client.wait_for_call_count(2);
    client.release_error("temporary failure");

    let mut new_cfg = cfg();
    new_cfg.poll_timeout_seconds = 2;
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    client.wait_for_call_count(3);
}

/// Shutdown is recorded under the same mutex used by the poller readiness
/// condvar, so a poller parked with no registered agents cannot miss the
/// wakeup.
#[test]
fn shutdown_wakes_poller_readiness_wait() {
    let (tx, _rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = test_extension(client.clone(), tx.clone());
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    let state = Arc::clone(&ext.state);
    let shutdown = Arc::clone(&ext.shutdown);
    let handle = std::thread::spawn(move || poll_loop(state, client, tx.into(), shutdown));

    std::thread::sleep(Duration::from_millis(50));
    let start = path_std_time::Instant::now();
    ext.request_shutdown();
    handle.join().expect("poller joins after shutdown");
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// Even after the poll loop's first generation check succeeds, a later config
/// change before per-update processing must stop stale updates from routing
/// through the new current config.
#[test]
fn stale_generation_update_processing_does_not_reread_current_config() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let old_generation = ext.state.lock().config_generation;
    assert!(ext.poll_response_matches_config(old_generation));

    let mut new_cfg = cfg();
    new_cfg.bot_token = "different-token".to_owned();
    ext.apply_config(new_cfg, Some(temp_state_dir()))
        .expect("apply config");
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    ext.process_update_for_generation(
        TgUpdate {
            update_id: telegram_update_id(55),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: Some("alice".to_owned()),
                text: Some("stale".to_owned()),
            }),
        },
        old_generation,
    );

    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// A failed report stops its provider batch before a later update can acquire
/// routing/checkpoint state.
#[test]
fn failed_report_stops_provider_batch_suffix() {
    let (tx, rx) = mpsc::channel();
    drop(rx);
    let ext = test_extension(FakeClient::new(), tx);
    ext.apply_config(cfg(), Some(temp_state_dir()))
        .expect("apply config");
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));
    let generation = ext.state.lock().config_generation;
    let update = |id, text: &str| TgUpdate {
        update_id: telegram_update_id(id),
        message: Some(TgMessage {
            chat_id: 123,
            chat_type: Some("private".to_owned()),
            user_id: 123,
            from_name: Some("alice".to_owned()),
            text: Some(text.to_owned()),
        }),
    };
    assert_eq!(
        ext.process_update_batch(vec![update(71, "first"), update(72, "suffix")], generation),
        ProcessingControl::Stop
    );
    let state = ext.state.lock();
    assert!(matches!(
        state
            .live_checkpoints
            .existing_update(telegram_update_id(72), state.next_update_offset),
        ExistingUpdate::New
    ));
}

/// Invalid reconfiguration fails closed: previous registrations and chat state
/// are cleared so neither old Telegram messages nor agent sends keep using the
/// previous access policy.
#[test]
fn invalid_reconfiguration_clears_active_bridge_state() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .registered_agents
        .insert(agent_id("agent-1"));

    ext.clear_config_after_error();
    process_update(
        &ext,
        TgUpdate {
            update_id: telegram_update_id(1),
            message: Some(TgMessage {
                chat_id: 123,
                chat_type: Some("private".to_owned()),
                user_id: 123,
                from_name: None,
                text: Some("hello".to_owned()),
            }),
        },
    );
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());

    ext.dispatch_tool(tool(SEND_TOOL_NAME, "agent-1", message_args("reply")));
    let _progress = rx.recv().expect("progress");
    let msg = rx.recv().expect("result");
    let HarnessInputMessage::Emit(emit) = msg else {
        panic!("emit")
    };
    let Event::ToolErrorReported(error) = *emit.event else {
        panic!("tool error")
    };
    assert!(error.message.contains("telegram_register"));
}
