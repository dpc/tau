use std::{collections as path_std_collections, net as path_std_net};

use rustls::{crypto as path_rustls_crypto, pki_types as path_rustls_pki_types};
use tungstenite::http as path_tungstenite_http;

use crate::attempt_failure as path_crate_attempt_failure;
use crate::common::OutputItemAccumulator;

mod direct_target_canary;
mod scripted_tcp_server;
mod test_ca;
mod test_server;

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use direct_target_canary::DirectTargetCanary;
use scripted_tcp_server::ScriptedTcpServer;
use test_ca::TestCa;
use test_server::{ServerScript, TestWsServer};

use super::super::BorrowedContextItem;
use super::*;

fn outbound_error(result: Result<WsConn, LlmError>) -> tau_provider::OutboundError {
    let error = match result {
        Ok(_) => panic!("expected outbound failure"),
        Err(error) => error,
    };
    match error.root_error() {
        LlmError::Outbound(error) => error.clone(),
        other => panic!("expected typed outbound error, got {other:?}"),
    }
}

fn http_status_zero<T>(result: Result<T, LlmError>) -> String {
    let error = match result {
        Ok(_) => panic!("expected HTTP-status-zero failure"),
        Err(error) => error,
    };
    match error.root_error() {
        LlmError::HttpStatus(0, body) => body.clone(),
        other => panic!("expected HTTP-status-zero failure, got {other:?}"),
    }
}

/// Regression: only the first approved upgrade request-ID header crosses the
/// raw HTTP boundary into opaque failure evidence.
#[test]
fn upgrade_error_extracts_allowlisted_request_id_precedence() {
    let response = path_tungstenite_http::Response::builder()
        .status(503)
        .header("openai-request-id", "req-third")
        .header("request-id", "req-second")
        .header("x-request-id", "req-first")
        .header("x-secret-header", "must-not-cross")
        .body(Some(Vec::new()))
        .expect("upgrade response");
    let error = map_ws_connect_error(tungstenite::Error::Http(Box::new(response)));
    assert_eq!(
        error
            .evidence()
            .and_then(crate::attempt_failure::AttemptFailureEvidence::transport_request_id),
        Some("req-first")
    );
}
use crate::responses::ResponsesMode;
use crate::{NeverAbort, TurnAbortWaker};

/// Every allow-listed ChatGPT model must preserve the exact empirical minimum,
/// offset, and step boundaries rather than drifting to generic block rounding.
#[test]
fn cache_read_ceiling_uses_shared_provider_geometry() {
    assert_eq!(WsConn::cache_read_ceiling(0), 0);
    assert_eq!(WsConn::cache_read_ceiling(1_535), 0);
    assert_eq!(WsConn::cache_read_ceiling(1_536), 1_536);
    assert_eq!(WsConn::cache_read_ceiling(2_559), 1_536);
    assert_eq!(WsConn::cache_read_ceiling(2_560), 2_560);
}

/// Capability matching must accept only the documented model allow-list and
/// reject every other cache-geometry precondition.
#[test]
fn cache_read_ceiling_capability_is_narrow() {
    let mut config = test_responses_config();
    config.base_url = "https://chatgpt.com/backend-api".to_owned();
    config.mode = ResponsesMode::Standard;
    for model in CHATGPT_CACHE_READ_GEOMETRY_MODELS {
        config.model_id = (*model).to_owned();
        assert!(supports_cache_read_ceiling(&config, false));
        assert!(!supports_cache_read_ceiling(&config, true));
    }

    config.mode = ResponsesMode::LiteCompatibility;
    assert!(!supports_cache_read_ceiling(&config, false));
    config.mode = ResponsesMode::Standard;
    config.model_id = "gpt-5.6-sol-latest".to_owned();
    assert!(!supports_cache_read_ceiling(&config, false));
    config.model_id = "gpt-5.6-sol".to_owned();
    config.base_url = "https://chatgpt.com/backend-api/".to_owned();
    assert!(!supports_cache_read_ceiling(&config, false));
}

/// A matching live-socket anchor emits the ceiling on an ordinary completion,
/// while the same warm lineage suppresses it when the response compacts.
#[test]
fn warm_anchor_emits_ceiling_except_for_compaction() {
    for compaction in [false, true] {
        let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
        conn.cached_response_anchor =
            CachedResponseAnchor::new_with_input_tokens("resp_prev".to_owned(), &[], Some(2_000));
        let mut config = test_responses_config();
        config.model_id = "gpt-5.6-sol".to_owned();
        if compaction {
            inbound_tx
                .send_blocking(InboundEvent::Event {
                    text: r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","summary":"old history","input_items":[]}}"#.to_owned().into(),
                })
                .expect("queue compaction");
        }
        inbound_tx
            .send_blocking(InboundEvent::Event {
                text: r#"{"type":"response.completed","response":{"id":"resp_next","usage":{"input_tokens":2500,"output_tokens":10,"input_tokens_details":{"cached_tokens":1536}}}}"#.to_owned().into(),
            })
            .expect("queue completion");
        let mut fixture = PromptFixture::new();
        fixture
            .context
            .blocks
            .push(tau_proto::ContextBlock::AssistantResponse(
                tau_proto::AssistantResponseBlock {
                    provider_response_id: Some("resp_prev".to_owned()),
                    backend: None,
                    output_items: Vec::new(),
                    usage: None,
                },
            ));
        let result = conn
            .run_turn(
                &config,
                "ap-cache-ceiling",
                &fixture.payload(),
                None,
                None,
                &mut NeverAbort,
                &mut |_| {},
                &mut |_| {},
            )
            .expect("completed turn");
        assert_eq!(
            result
                .state
                .usage()
                .and_then(|usage| usage.prompt_cache_read_ceiling_tokens),
            (!compaction).then_some(1_536),
        );
    }
}

/// The live compact parser reports private state for repair accounting while
/// returning the completed item only after the full shape validates.
#[test]
fn compact_turn_validates_shape_while_reporting_private_progress() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","encrypted_content":"opaque"}}"#.into(),
        })
        .expect("queue compact item");
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.completed","response":{"id":"resp_compact","usage":{"input_tokens":4,"output_tokens":2}}}"#.into(),
        })
        .expect("queue compact terminal");
    let fixture = PromptFixture::new();
    let mut observed_semantic_output = false;
    let mut observed_sidecar_pointer = None;
    let fingerprint_sidecar_pointer = Rc::new(RefCell::new(None));
    let observed_fingerprint_pointer = Rc::clone(&fingerprint_sidecar_pointer);
    let result = super::super::with_fingerprint_item_observer(
        move |item| {
            if let BorrowedContextItem::Context(tau_proto::ContextItem::Compaction(item)) = item {
                *observed_fingerprint_pointer.borrow_mut() = Some(item.raw_json().as_ptr());
            }
        },
        || {
            conn.run_compact(
                &test_responses_config(),
                "ap-compact-shape",
                &fixture.payload(),
                None,
                None,
                &mut NeverAbort,
                &mut |_| {},
                &mut |state| {
                    if let Some(item) = state.single_compaction_item() {
                        observed_semantic_output = true;
                        observed_sidecar_pointer = Some(item.raw_json().as_ptr());
                    }
                },
            )
        },
    )
    .expect("valid compact response");

    assert!(
        observed_semantic_output,
        "the pool needs private semantic progress to prohibit replay"
    );
    assert_eq!(
        conn.cached_response_anchor
            .as_ref()
            .map(|anchor| anchor.response_id.as_str()),
        Some("resp_compact"),
        "a valid compact response must still publish its live-socket anchor"
    );
    assert_eq!(
        result
            .state
            .usage()
            .map(|usage| usage.response_received_tokens),
        Some(2),
    );
    let item = result
        .state
        .into_single_compaction_item()
        .expect("validated compact item");
    assert_eq!(
        *fingerprint_sidecar_pointer.borrow(),
        observed_sidecar_pointer,
        "production fingerprint serialization must borrow the original sidecar allocation"
    );
    assert_eq!(
        Some(item.raw_json().as_ptr()),
        observed_sidecar_pointer,
        "anchor fingerprinting must return the original opaque sidecar allocation"
    );
}

/// A malformed live compact response with an id must fail before
/// response-anchor publication, preserving the parser's primary shape
/// rejection.
#[test]
fn compact_anchor_rejects_malformed_output_shape() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    for text in [
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","encrypted_content":"opaque"}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"unexpected"}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_malformed","usage":{"input_tokens":4,"output_tokens":2}}}"#,
    ] {
        inbound_tx
            .send_blocking(InboundEvent::Event { text: text.into() })
            .expect("queue compact event");
    }
    let fixture = PromptFixture::new();
    let error = match conn.run_compact(
        &test_responses_config(),
        "ap-compact-malformed-anchor",
        &fixture.payload(),
        None,
        None,
        &mut NeverAbort,
        &mut |_| {},
        &mut |_| {},
    ) {
        Ok(_) => panic!("compact parser accepted extra semantic output"),
        Err(error) => error,
    };

    assert!(
        conn.cached_response_anchor.is_none(),
        "malformed compact output must not publish a response anchor"
    );
    assert!(
        matches!(error, LlmError::InvalidResponse(_)),
        "the malformed terminal must retain its invalid-response classification"
    );
}

/// The live compact transport rejects the inference-only legacy terminal even
/// after receiving an otherwise valid compaction item.
#[test]
fn compact_turn_rejects_response_done_terminal() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction"}}"#.into(),
        })
        .expect("queue compact item");
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.done"}"#.into(),
        })
        .expect("queue wrong terminal");
    let fixture = PromptFixture::new();
    let error = match conn.run_compact(
        &test_responses_config(),
        "ap-compact-wrong-terminal",
        &fixture.payload(),
        None,
        None,
        &mut NeverAbort,
        &mut |_| {},
        &mut |_| {},
    ) {
        Ok(_) => panic!("response.done must be compact-invalid"),
        Err(error) => error,
    };

    assert!(matches!(error, LlmError::InvalidResponse(_)));
}

type TestAbortWakerSlot = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>>;

struct CapturingAbort {
    aborted: Arc<AtomicBool>,
    registered_tx: std_mpsc::Sender<()>,
    waker: TestAbortWakerSlot,
}

impl TurnAbort for CapturingAbort {
    fn is_aborted(&mut self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn register_waker(
        &mut self,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        *self.waker.lock().expect("waker slot lock") = Some(waker);
        self.registered_tx.send(()).expect("registered receiver");
        Box::new(TestAbortWaker)
    }
}

struct TestAbortWaker;

impl TurnAbortWaker for TestAbortWaker {}

/// Shared-flag abort source used to place cancellation exactly around request
/// serialization.
struct FlagAbort {
    /// Authoritative cancellation state.
    aborted: Arc<AtomicBool>,
}

impl TurnAbort for FlagAbort {
    fn is_aborted(&mut self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn register_waker(
        &mut self,
        _waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(TestAbortWaker)
    }
}

/// Shared trace writer for private dispatch-accounting assertions.
#[derive(Clone, Default)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for TraceWriter {
    /// Append one formatted trace fragment.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// The in-memory sink has no external buffer.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Test envelope that records serialization and can cancel its owning turn
/// before serialization returns.
struct CancellationSeamEnvelope {
    /// Number of serializer entries.
    serialization_count: Arc<AtomicUsize>,
    /// Cancellation authority shared with the abort source.
    aborted: Arc<AtomicBool>,
    /// Whether serialization should publish cancellation.
    cancel_during_serialization: bool,
}

impl serde::Serialize for CancellationSeamEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.serialization_count.fetch_add(1, Ordering::SeqCst);
        if self.cancel_during_serialization {
            self.aborted.store(true, Ordering::SeqCst);
        }
        serializer.serialize_str("exact-wire")
    }
}

/// A turn canceled before request serialization must retain typed cancellation
/// policy and perform neither serialization, dispatch publication, nor enqueue.
#[test]
fn websocket_enqueue_rejects_cancellation_before_serialization() {
    let aborted = Arc::new(AtomicBool::new(true));
    let serialization_count = Arc::new(AtomicUsize::new(0));
    let envelope = CancellationSeamEnvelope {
        serialization_count: Arc::clone(&serialization_count),
        aborted: Arc::clone(&aborted),
        cancel_during_serialization: false,
    };
    let mut abort = FlagAbort { aborted };
    let mut dispatched = false;
    let mut enqueued = None;

    let error =
        serialize_and_enqueue_envelope(&envelope, &mut abort, &mut |_| dispatched = true, |text| {
            enqueued = Some(text);
            Ok(())
        })
        .expect_err("cancellation must prevent request build");

    assert!(matches!(error, LlmError::Canceled));
    assert_eq!(error.retry_decision(), None);
    assert_eq!(serialization_count.load(Ordering::SeqCst), 0);
    assert!(!dispatched);
    assert_eq!(enqueued, None);
}

/// Cancellation published by request serialization must win at the final
/// pre-enqueue check without changing terminal or retry classification.
#[test]
fn websocket_enqueue_rechecks_cancellation_after_serialization() {
    let aborted = Arc::new(AtomicBool::new(false));
    let serialization_count = Arc::new(AtomicUsize::new(0));
    let envelope = CancellationSeamEnvelope {
        serialization_count: Arc::clone(&serialization_count),
        aborted: Arc::clone(&aborted),
        cancel_during_serialization: true,
    };
    let mut abort = FlagAbort { aborted };
    let mut dispatched = false;
    let mut enqueued = None;

    let error =
        serialize_and_enqueue_envelope(&envelope, &mut abort, &mut |_| dispatched = true, |text| {
            enqueued = Some(text);
            Ok(())
        })
        .expect_err("post-serialization cancellation must prevent enqueue");

    assert!(matches!(error, LlmError::Canceled));
    assert_eq!(error.retry_decision(), None);
    assert_eq!(serialization_count.load(Ordering::SeqCst), 1);
    assert!(!dispatched);
    assert_eq!(enqueued, None);
}

/// Cancellation published by the about-to-dispatch callback must prevent
/// private wire-dispatch accounting and the writer handoff.
#[test]
fn websocket_enqueue_rechecks_cancellation_after_dispatch_callback() {
    let aborted = Arc::new(AtomicBool::new(false));
    let serialization_count = Arc::new(AtomicUsize::new(0));
    let envelope = CancellationSeamEnvelope {
        serialization_count: Arc::clone(&serialization_count),
        aborted: Arc::clone(&aborted),
        cancel_during_serialization: false,
    };
    let mut abort = FlagAbort {
        aborted: Arc::clone(&aborted),
    };
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let mut enqueued = None;
    let output = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let output = output.clone();
            move || output.clone()
        })
        .finish();

    let error = tracing::subscriber::with_default(subscriber, || {
        let mut trace = private_trace::AttemptTrace::selected(
            private_trace::Backend::Codex,
            private_trace::Transport::Websocket,
        );
        let error = serialize_and_enqueue_envelope_observed(
            &envelope,
            &mut abort,
            &mut |_| {
                dispatch_count.fetch_add(1, Ordering::SeqCst);
                aborted.store(true, Ordering::SeqCst);
            },
            &mut trace,
            |text| {
                enqueued = Some(text);
                Ok(())
            },
        )
        .expect_err("callback cancellation must prevent enqueue");
        trace
            .take()
            .expect("enabled trace")
            .finish(private_trace::Outcome::Canceled);
        error
    });

    assert!(matches!(error, LlmError::Canceled));
    assert_eq!(error.retry_decision(), None);
    assert_eq!(serialization_count.load(Ordering::SeqCst), 1);
    assert_eq!(dispatch_count.load(Ordering::SeqCst), 1);
    assert_eq!(enqueued, None);
    let trace =
        String::from_utf8(output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert!(trace.contains("dispatch_count=0"), "{trace}");
}

/// A live turn must preserve the serializer's exact bytes and publish dispatch
/// exactly once immediately before the sole enqueue.
#[test]
fn websocket_enqueue_preserves_live_request_bytes_and_order() {
    let aborted = Arc::new(AtomicBool::new(false));
    let serialization_count = Arc::new(AtomicUsize::new(0));
    let envelope = CancellationSeamEnvelope {
        serialization_count: Arc::clone(&serialization_count),
        aborted: Arc::clone(&aborted),
        cancel_during_serialization: false,
    };
    let mut abort = FlagAbort { aborted };
    let events = RefCell::new(Vec::new());

    serialize_and_enqueue_envelope(
        &envelope,
        &mut abort,
        &mut |_| events.borrow_mut().push(("dispatch", String::new())),
        |text| {
            events.borrow_mut().push(("enqueue", text));
            Ok(())
        },
    )
    .expect("live request must enqueue");

    assert_eq!(serialization_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        events.into_inner(),
        [
            ("dispatch", String::new()),
            ("enqueue", "\"exact-wire\"".to_owned()),
        ]
    );
}

/// The production response path must preserve correlated debug capture while
/// callback cancellation suppresses private dispatch and writer-channel
/// enqueue.
#[test]
fn websocket_response_callback_cancellation_preserves_capture_without_enqueue() {
    let (mut conn, _inbound_tx, mut outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let mut request = fixture.payload();
    request.debug_provider_requests = true;
    let mut correlation =
        path_crate_attempt_failure::AttemptCaptureCorrelation::new(crate::LogicalAttempt::new(7));
    let dispatch = correlation.next_dispatch();
    let aborted = Arc::new(AtomicBool::new(false));
    let mut abort = FlagAbort {
        aborted: Arc::clone(&aborted),
    };
    let mut capture = None;
    let dispatch_count = Arc::new(AtomicUsize::new(0));

    let error = match conn.run_response_with_capture_submit(
        &config,
        "ap-cancel-after-build",
        &request,
        Some(dispatch),
        None,
        ResponseMode::Ordinary,
        &mut abort,
        &mut |_| {
            dispatch_count.fetch_add(1, Ordering::SeqCst);
            aborted.store(true, Ordering::SeqCst);
        },
        &mut |_| {},
        |submitted| capture = Some(submitted),
    ) {
        Ok(_) => panic!("callback cancellation must prevent enqueue"),
        Err(error) => error,
    };

    assert!(matches!(error, LlmError::Canceled));
    assert_eq!(error.retry_decision(), None);
    assert_eq!(dispatch_count.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outbound_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let capture = capture.expect("debug capture remains at its existing boundary");
    let metadata: serde_json::Value =
        serde_json::from_slice(capture.json()).expect("capture metadata");
    assert_eq!(metadata["logical_attempt"], 7);
    assert_eq!(metadata["wire_dispatch_index"], 1);
}

/// The production response path must submit correlated debug capture, publish
/// dispatch before the actual enqueue, and preserve the exact live wire bytes.
#[test]
fn websocket_response_live_path_preserves_capture_enqueue_bytes_and_order() {
    let (mut conn, inbound_tx, mut outbound_rx) = test_ws_conn();
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.completed","response":{"id":"resp-live"}}"#
                .to_owned()
                .into(),
        })
        .expect("queue completion");
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let mut request = fixture.payload();
    request.debug_provider_requests = true;
    let expected = serde_json::to_string(&build_ws_envelope(&config, &request, None, None))
        .expect("wire JSON");
    let mut correlation =
        path_crate_attempt_failure::AttemptCaptureCorrelation::new(crate::LogicalAttempt::new(9));
    let dispatch = correlation.next_dispatch();
    let events = RefCell::new(Vec::new());
    let mut abort = NeverAbort;

    conn.run_response_with_capture_submit(
        &config,
        "ap-live-enqueue",
        &request,
        Some(dispatch),
        None,
        ResponseMode::Ordinary,
        &mut abort,
        &mut |_| {
            assert!(matches!(
                outbound_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            events.borrow_mut().push("dispatch");
        },
        &mut |_| {},
        |capture| {
            let metadata: serde_json::Value =
                serde_json::from_slice(capture.json()).expect("capture metadata");
            assert_eq!(metadata["logical_attempt"], 9);
            assert_eq!(metadata["wire_dispatch_index"], 1);
            events.borrow_mut().push("capture");
        },
    )
    .expect("live response completes");

    let WsCommand::SendText(enqueued) = outbound_rx.try_recv().expect("writer enqueue");
    assert_eq!(enqueued, expected);
    events.borrow_mut().push("enqueue");
    assert_eq!(
        events.into_inner(),
        ["capture", "dispatch", "enqueue"],
        "capture policy and dispatch-before-enqueue ordering must not drift"
    );
}

/// Synthetic abort source that becomes canceled after a fixed number of checks.
struct AbortAfterChecks {
    /// Number of initial negative cancellation checks.
    remaining_false: usize,
}

impl TurnAbort for AbortAfterChecks {
    fn is_aborted(&mut self) -> bool {
        if self.remaining_false == 0 {
            true
        } else {
            self.remaining_false -= 1;
            false
        }
    }

    fn register_waker(
        &mut self,
        _waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(TestAbortWaker)
    }
}

fn test_ws_conn() -> (WsConn, InboundSender, UnboundedReceiver<WsCommand>) {
    let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = mpsc::channel(16);
    let inbound_control = Arc::new(InboundControl::new());
    let inbound_sender = InboundSender {
        tx: inbound_tx,
        control: Arc::clone(&inbound_control),
    };
    let runtime = ws_runtime::handle();
    let reader_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    let writer_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    (
        WsConn {
            outbound_tx,
            inbound_rx,
            inbound_control,
            reader_abort,
            writer_abort,
            opened_at: Instant::now(),
            bearer: "test-token".to_owned(),
            cached_response_anchor: None,
            prewarm_baseline: None,
            carried_response_bytes: 0,
        },
        inbound_sender,
        _outbound_rx,
    )
}

fn test_responses_config() -> ResponsesConfig {
    ResponsesConfig {
        profile_namespace: tau_proto::ProviderName::new("chatgpt"),
        mode: ResponsesMode::Standard,
        base_url: "https://chatgpt.com/backend-api".to_owned(),
        api_key: "test-token".to_owned(),
        model_id: "gpt-test".to_owned(),
        raw_context_window: tau_proto::TokenCount::new(128_000),
        account_id: None,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: true,
        supports_phase: true,
        supports_encrypted_reasoning: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    }
}

fn padded_json(json: &str, len: usize) -> String {
    assert!(json.len() <= len);
    let mut padded = String::with_capacity(len);
    padded.push_str(json);
    padded.extend(std::iter::repeat_n(' ', len - json.len()));
    padded
}

fn run_local_resource_script(script: ServerScript) -> (Result<WsTurnResult, LlmError>, bool, bool) {
    let server = TestWsServer::spawn(script);
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let fixture = PromptFixture::new();
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-resource",
        &crate::test_network_policy(),
        &mut abort,
    )
    .expect("connect resource-test WebSocket");
    let result = conn.run_turn(
        &config,
        "ap-resource",
        &fixture.payload(),
        None,
        None,
        &mut abort,
        &mut |_| {},
        &mut |_| {},
    );
    server.wait_for_request();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !(conn.reader_abort.is_finished() && conn.writer_abort.is_finished())
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    let retired = (
        conn.reader_abort.is_finished(),
        conn.writer_abort.is_finished(),
    );
    drop(conn);
    server.join();
    (result, retired.0, retired.1)
}

/// The live turn owner semantically decodes and lexically indexes each frame
/// once while preserving an opaque replay sidecar exactly.
#[test]
fn live_turn_decodes_each_frame_once_and_preserves_opaque_sidecar() {
    let raw_item = r#"{ "type":"compaction", "summary":{"n":1.2300} }"#;
    let item_event =
        format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{raw_item}}}"#);
    let quota = r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"reset_at":1700000000}}}"#.to_owned();
    let terminal = r#"{"type":"response.completed","response":{"id":"once"}}"#.to_owned();
    crate::decoded_event::reset_test_counts();
    let state = run_local_resource_script(ServerScript::Frames(vec![quota, item_event, terminal]))
        .0
        .expect("live turn")
        .state;
    assert_eq!(crate::decoded_event::test_counts(), (3, 3));
    assert!(state.quota_observation.is_some());
    let Some(OutputItemAccumulator::Compaction(Some(item))) = state.output_items.first() else {
        panic!("opaque compaction output");
    };
    assert_eq!(item.raw_json(), raw_item);
}

/// Ensures plain WebSocket traffic uses an HTTP proxy's absolute-form upgrade,
/// carries the shared HTTP response-coding policy, and offers no WebSocket
/// extension while avoiding a direct target connection.
#[test]
fn websocket_upgrade_uses_selected_http_proxy_absolute_form() {
    use std::io::Write;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let (request_tx, request_rx) = std_mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("proxy connection");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("upgrade request");
            request.push(byte[0]);
            assert!(request.len() < 32 * 1024, "upgrade head is bounded");
        }
        let request_text = String::from_utf8(request).expect("ASCII upgrade");
        let key = request_text
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        stream.flush().expect("flush upgrade");
        request_tx.send(request_text).expect("request capture");
    });
    let environment = path_std_collections::BTreeMap::from([(
        "http_proxy".to_owned(),
        format!("http://{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = "http://unresolvable.invalid/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-proxy", &network, &mut abort)
        .expect("proxied WebSocket upgrade");
    drop(connection);
    let request = request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured request");
    assert!(
        request.starts_with(
            "GET http://unresolvable.invalid/backend-api/codex/responses HTTP/1.1\r\n"
        ),
        "request was {request:?}",
    );
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("accept-encoding: zstd,gzip")),
        "upgrade must carry the shared HTTP response-coding policy: {request:?}"
    );
    assert!(
        !request.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-extensions"))
        }),
        "HTTP response decoding must not offer WebSocket PMCE: {request:?}"
    );
    proxy.join().expect("proxy thread");
}

/// Ensures a proxy cannot negotiate an extension the client did not request.
#[test]
fn websocket_upgrade_rejects_unsolicited_proxy_extension() {
    use std::io::Write;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let proxy = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("proxy connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::from([(
            "http_proxy".to_owned(),
            format!("http://{address}"),
        )]),
        None,
    );
    let mut config = test_responses_config();
    config.base_url = "http://unresolvable.invalid/backend-api".to_owned();
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-extension",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    proxy.join().expect("proxy thread");
}

/// Ensures a direct target cannot negotiate an extension the client did not
/// request.
#[test]
fn websocket_upgrade_rejects_unsolicited_target_extension() {
    use std::io::Write;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("target listener");
    let address = listener.local_addr().expect("target address");
    let target = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("target connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = crate::test_network_policy();
    let mut config = test_responses_config();
    config.base_url = format!("http://{address}/backend-api");
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-extension",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    target.join().expect("target thread");
}

/// Ensures a direct target cannot select a WebSocket subprotocol the client did
/// not offer.
#[test]
fn websocket_upgrade_rejects_unsolicited_target_subprotocol() {
    use std::io::Write;

    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("target listener");
    let address = listener.local_addr().expect("target address");
    let target = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("target connection");
        let request = read_http_head(&mut stream);
        let key = request
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: unoffered\r\n\r\n"
        )
        .expect("upgrade response");
    });
    let network = crate::test_network_policy();
    let mut config = test_responses_config();
    config.base_url = format!("http://{address}/backend-api");
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-subprotocol",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Request);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Protocol);
    target.join().expect("target thread");
}

/// Ensures secure WebSocket traffic uses CONNECT, performs target TLS with the
/// additive custom CA, and sends provider credentials only inside the tunnel.
#[test]
fn secure_websocket_proxy_connects_before_target_tls_and_upgrade() {
    use std::io::Write;

    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("CA key");
    let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .expect("leaf params")
        .signed_by(&leaf_key, &ca, &ca_key)
        .expect("leaf certificate");
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        path_rustls_crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("target TLS versions")
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        path_rustls_pki_types::PrivateKeyDer::Pkcs8(
            path_rustls_pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
        ),
    )
    .expect("target TLS");
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let (capture_tx, capture_rx) = std_mpsc::channel();
    let proxy = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("proxy connection");
        let connect = read_http_head(&mut socket);
        assert!(
            connect.starts_with("CONNECT localhost:4443 HTTP/1.1\r\n"),
            "CONNECT was {connect:?}",
        );
        assert!(
            !connect
                .to_ascii_lowercase()
                .contains("authorization: bearer"),
            "target credential escaped into CONNECT"
        );
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let upgrade = read_http_head(&mut tunnel);
        let key = upgrade
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("websocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            tunnel,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        tunnel.flush().expect("flush upgrade");
        capture_tx.send((connect, upgrade)).expect("capture");
    });
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("ca.pem");
    std::fs::write(&ca_path, ca.pem()).expect("write CA");
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = "https://localhost:4443/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-connect", &network, &mut abort)
        .expect("tunneled secure WebSocket");
    drop(connection);
    let (_, upgrade) = capture_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured tunnel");
    assert!(
        upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"),
        "upgrade was {upgrade:?}",
    );
    assert!(upgrade.contains("authorization: Bearer test-token"));
    proxy.join().expect("proxy thread");
}

/// Ensures WSS through an HTTPS proxy performs proxy TLS, one authenticated
/// CONNECT, target TLS, and WebSocket upgrade without leaking either layer's
/// credentials into the other.
#[test]
fn secure_websocket_through_https_proxy_uses_nested_tls_and_scoped_auth() {
    use std::io::Write;

    let proxy_ca = TestCa::new();
    let target_ca = TestCa::new();
    let proxy_tls = proxy_ca.server_config("localhost");
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let outer =
            rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("proxy TLS connection");
        let mut outer = rustls::StreamOwned::new(outer, socket);
        let connect = read_http_head(&mut outer);
        outer
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let inner =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut inner = rustls::StreamOwned::new(inner, outer);
        let upgrade = read_http_head(&mut inner);
        let key = upgrade
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
            })
            .expect("WebSocket key");
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        write!(
            inner,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .expect("upgrade response");
        inner.flush().expect("flush upgrade");
        (connect, upgrade)
    });
    let address = proxy.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("nested-ca.pem");
    std::fs::write(&ca_path, format!("{}\n{}", proxy_ca.pem(), target_ca.pem()))
        .expect("write CA bundle");
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = "https://localhost:4444/backend-api".to_owned();
    let mut abort = NeverAbort;
    let connection = WsConn::connect(&config, "thread-nested", &network, &mut abort)
        .expect("nested TLS WebSocket");
    drop(connection);
    let (connect, upgrade) = proxy.finish();
    assert!(connect.starts_with("CONNECT localhost:4444 HTTP/1.1\r\n"));
    assert_eq!(
        connect
            .lines()
            .filter(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:"))
            .collect::<Vec<_>>(),
        ["Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz"]
    );
    assert!(!connect.contains("test-token"));
    assert!(upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(upgrade.contains("authorization: Bearer test-token\r\n"));
    assert!(
        !upgrade
            .to_ascii_lowercase()
            .contains("proxy-authorization:")
    );
}

/// Ensures an untrusted HTTPS proxy certificate fails before CONNECT and never
/// reaches the otherwise available direct WSS target.
#[test]
fn wss_proxy_tls_failure_has_no_direct_fallback() {
    let target = DirectTargetCanary::new();
    let proxy_ca = TestCa::new();
    let proxy_tls = proxy_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |socket| {
        let connection =
            rustls::ServerConnection::new(Arc::new(proxy_tls)).expect("proxy TLS connection");
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        let _ = stream.read(&mut byte);
    });
    let address = proxy.address();
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("https://proxy-user:proxy-pass@localhost:{}", address.port()),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-proxy-tls",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    for canary in ["proxy-user", "proxy-pass", "localhost", "test-token"] {
        assert!(
            !projection.contains(canary),
            "leaked {canary}: {projection}"
        );
    }
    proxy.finish();
    target.assert_untouched();
}

/// Ensures a hidden CONNECT rejection remains generic Proxy/Transport and does
/// not trigger a direct WSS fallback, preserving the approved reqwest boundary.
#[test]
fn wss_connect_rejection_is_generic_and_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let authority = target.authority();
    let proxy = ScriptedTcpServer::spawn(move |mut stream| {
        let connect = read_http_head(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("CONNECT rejection");
        connect
    });
    let address = proxy.address();
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://proxy-user:proxy-pass@{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-connect-reject",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let connect = proxy.finish();
    assert!(connect.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
    assert_eq!(
        connect
            .lines()
            .filter(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:"))
            .collect::<Vec<_>>(),
        ["Proxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNz"]
    );
    assert!(!connect.contains("test-token"));
    target.assert_untouched();
}

/// Ensures inner target TLS rejection after a successful CONNECT remains a
/// redacted generic proxy transport failure and cannot fall back direct.
#[test]
fn wss_target_tls_failure_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let authority = target.authority();
    let target_ca = TestCa::new();
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |mut socket| {
        let connect = read_http_head(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let mut byte = [0_u8; 1];
        let _ = tunnel.read(&mut byte);
        connect
    });
    let address = proxy.address();
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, None);
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    let error = outbound_error(WsConn::connect(
        &config,
        "thread-target-tls",
        &network,
        &mut abort,
    ));
    assert_eq!(error.route(), tau_provider::OutboundRouteKind::Proxy);
    assert_eq!(error.phase(), tau_provider::OutboundPhase::Proxy);
    assert_eq!(error.kind(), tau_provider::OutboundErrorKind::Transport);
    let projection = format!("{error:?} {error}");
    assert!(!projection.contains("localhost"));
    assert!(!projection.contains("test-token"));
    let connect = proxy.finish();
    assert!(connect.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
    target.assert_untouched();
}

/// Ensures a target-authored WebSocket upgrade failure after successful CONNECT
/// never causes a direct transport fallback.
#[test]
fn wss_upgrade_failure_has_no_direct_fallback() {
    use std::io::Write;

    let target = DirectTargetCanary::new();
    let target_ca = TestCa::new();
    let target_tls = target_ca.server_config("localhost");
    let proxy = ScriptedTcpServer::spawn(move |mut socket| {
        let _connect = read_http_head(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .expect("CONNECT response");
        let connection =
            rustls::ServerConnection::new(Arc::new(target_tls)).expect("target TLS connection");
        let mut tunnel = rustls::StreamOwned::new(connection, socket);
        let upgrade = read_http_head(&mut tunnel);
        tunnel
            .write_all(
                b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("upgrade rejection");
        upgrade
    });
    let address = proxy.address();
    let directory = tempfile::tempdir().expect("CA directory");
    let ca_path = directory.path().join("target-ca.pem");
    std::fs::write(&ca_path, target_ca.pem()).expect("write target CA");
    let environment = path_std_collections::BTreeMap::from([(
        "https_proxy".to_owned(),
        format!("http://{address}"),
    )]);
    let network = tau_provider::OutboundNetworkPolicy::from_environment(environment, Some(ca_path));
    let mut config = test_responses_config();
    config.base_url = target.base_url();
    let mut abort = NeverAbort;
    assert!(matches!(
        WsConn::connect(&config, "thread-upgrade-fail", &network, &mut abort),
        Err(LlmError::WsUpgradeRequired)
    ));
    let upgrade = proxy.finish();
    assert!(upgrade.starts_with("GET /backend-api/codex/responses HTTP/1.1\r\n"));
    assert!(upgrade.contains("authorization: Bearer test-token\r\n"));
    target.assert_untouched();
}

fn read_http_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("HTTP head");
        head.push(byte[0]);
        assert!(head.len() < 32 * 1024, "HTTP head is bounded");
    }
    String::from_utf8(head).expect("ASCII HTTP head")
}

/// Mutable credential/account header failures retry as auth/config and a
/// corrected profile builds successfully on the next attempt.
#[test]
fn mutable_ws_header_configuration_can_be_repaired() {
    for invalid_account in [false, true] {
        let mut config = test_responses_config();
        if invalid_account {
            config.account_id = Some("bad\naccount".to_owned());
        } else {
            config.api_key = "bad\ntoken".to_owned();
        }
        let error = build_request(&config, "thread-id").expect_err("invalid mutable header");
        assert!(matches!(error, LlmError::ReloadableConfig(_)));
        assert_eq!(
            error.retry_decision().map(|decision| decision.class),
            Some(tau_provider::retry_policy::RetryClass::Auth)
        );

        config.api_key = "repaired-token".to_owned();
        config.account_id = Some("repaired-account".to_owned());
        build_request(&config, "thread-id").expect("repaired profile request");
    }
}

/// Unsupported configured schemes remain retryable because profile reload can
/// repair the endpoint before a later attempt.
#[test]
fn unsupported_ws_scheme_is_reloadable() {
    let mut config = test_responses_config();
    config.base_url = "file:///tmp/provider".to_owned();
    let error = build_request(&config, "thread-id").expect_err("unsupported WS scheme");
    assert!(matches!(error, LlmError::ReloadableConfig(_)));
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Auth)
    );
    config.base_url = "https://chatgpt.com/backend-api".to_owned();
    build_request(&config, "thread-id").expect("repaired WS URL");
}

/// A fresh WebSocket upgrade that never resolves must return a retryable,
/// content-free transport timeout instead of holding the prompt forever.
#[test]
fn ws_connect_wait_is_bounded() {
    let mut abort = NeverAbort;
    let result = wait_for_connect(
        &ws_runtime::handle(),
        &mut abort,
        Duration::from_millis(20),
        std::future::pending::<Result<(), ()>>(),
    );
    assert!(matches!(result, Err(ConnectWaitError::Timeout)));
    let network = crate::test_network_policy();
    let error = map_connect_wait_error(
        ConnectWaitError::Timeout,
        &network,
        "wss://target.example/codex/responses",
    );
    let LlmError::Outbound(outbound) = error.root_error() else {
        panic!("expected typed deadline");
    };
    assert!(matches!(
        error.evidence(),
        Some(crate::attempt_failure::AttemptFailureEvidence::Transport {
            phase: crate::attempt_failure::TransportPhase::PreUpgrade,
            established: false,
            kind: crate::attempt_failure::TransportFailureKind::Outbound,
            ..
        })
    ));
    assert_eq!(outbound.route(), tau_provider::OutboundRouteKind::Direct);
    assert_eq!(outbound.phase(), tau_provider::OutboundPhase::Connect);
    assert_eq!(outbound.kind(), tau_provider::OutboundErrorKind::Deadline);
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Transport)
    );
}

/// Cancellation must wake a fresh WebSocket upgrade before its connection
/// deadline, matching the cooperative wake contract used by active turns.
#[test]
fn ws_connect_wait_is_cancellation_aware() {
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = wait_for_connect(
                &ws_runtime::handle(),
                &mut abort,
                Duration::from_secs(30),
                std::future::pending::<Result<(), ()>>(),
            );
            result_tx.send(result).expect("result receiver");
        });
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("connect abort waker registration");

        aborted.store(true, Ordering::SeqCst);
        waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered connect waker")
            .clone()();
        assert!(matches!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("connect cancellation result"),
            Err(ConnectWaitError::Canceled)
        ));
    });
}

/// An abort source may become canceled while registering without invoking a
/// late waker; the mandatory post-registration check must still win
/// immediately.
#[test]
fn ws_connect_rechecks_cancellation_after_waker_registration() {
    struct AbortDuringRegistration(bool);
    impl TurnAbort for AbortDuringRegistration {
        fn is_aborted(&mut self) -> bool {
            self.0
        }

        fn register_waker(
            &mut self,
            _waker: Arc<dyn Fn() + Send + Sync + 'static>,
        ) -> Box<dyn TurnAbortWaker> {
            self.0 = true;
            Box::new(TestAbortWaker)
        }
    }

    let mut abort = AbortDuringRegistration(false);
    let result = wait_for_connect(
        &ws_runtime::handle(),
        &mut abort,
        Duration::from_secs(30),
        std::future::pending::<Result<(), ()>>(),
    );
    assert!(matches!(result, Err(ConnectWaitError::Canceled)));
    assert!(matches!(
        map_connect_wait_error(
            ConnectWaitError::Canceled,
            &crate::test_network_policy(),
            "wss://target.example/codex/responses",
        ),
        LlmError::Canceled
    ));
}

struct PromptFixture {
    context: tau_proto::PromptContext,
    session_id: tau_proto::SessionId,
    agent_id: tau_proto::AgentId,
    originator: tau_proto::PromptOriginator,
}

impl PromptFixture {
    fn new() -> Self {
        Self {
            context: tau_proto::PromptContext::default(),
            session_id: tau_proto::SessionId::parse("session-test")
                .expect("known-safe SessionId must be valid"),
            agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn payload(&self) -> PromptPayload<'_> {
        PromptPayload {
            system_prompt: "",
            context: &self.context,
            hosted_tools: &[],
            tools: &[],
            params: tau_proto::ModelParams::default(),
            tool_choice: tau_proto::ToolChoice::default(),
            compaction: None,
            originator: &self.originator,
            share_user_cache_key: false,
            session_id: &self.session_id,
            agent_id: &self.agent_id,
            debug_provider_requests: false,
        }
    }
}

/// A localhost WebSocket round trip must use the production upgrade, request
/// lowering, reader task, frame parser, and typed stream-state accumulation.
#[test]
fn localhost_ws_round_trip_lowers_and_parses_production_frames() {
    let frames = [
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "hello",
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc_local",
                "call_id": "call_local",
                "name": "shell",
                "status": "in_progress",
            },
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"command\":\"pwd\"}",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_local",
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 7,
                    "input_tokens_details": { "cached_tokens": 3 },
                },
            },
        }),
    ]
    .map(|frame| frame.to_string())
    .to_vec();
    let server = TestWsServer::spawn(ServerScript::Frames(frames));
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    config.account_id = Some("account-local".to_owned());
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-local",
        &crate::test_network_policy(),
        &mut abort,
    )
    .expect("connect localhost WebSocket");

    let result = conn
        .run_turn(
            &config,
            "ap-local-round-trip",
            &request,
            None,
            None,
            &mut abort,
            &mut |_| {},
            &mut |_| {},
        )
        .expect("complete localhost WebSocket turn");
    server.wait_for_request();
    let capture = server.capture();
    let capture = capture.lock().expect("localhost WebSocket capture");
    assert_eq!(capture.requests.len(), 1);
    let envelope: serde_json::Value =
        serde_json::from_str(&capture.requests[0]).expect("production request envelope");
    assert_eq!(envelope["type"], "response.create");
    assert_eq!(envelope["model"], "gpt-test");
    assert_eq!(
        capture.headers.get("thread-id").map(String::as_str),
        Some("thread-local")
    );
    assert_eq!(
        capture.headers.get("session-id").map(String::as_str),
        Some("thread-local")
    );
    assert_eq!(
        capture
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("account-local")
    );
    drop(capture);

    assert_eq!(result.state.aggregate_assistant_text(), "hello");
    assert_eq!(result.state.response_id.as_deref(), Some("resp_local"));
    assert_eq!(result.state.input_tokens, Some(11));
    assert_eq!(result.state.cached_tokens, Some(3));
    assert_eq!(result.state.output_tokens, Some(7));
    let output = result.state.into_output_items();
    assert_eq!(output.len(), 2);
    let tau_proto::ContextItem::ToolCall(call) = &output[1] else {
        panic!("expected parsed function call");
    };
    assert_eq!(call.call_id.as_str(), "call_local");
    assert_eq!(call.name.as_str(), "shell");
    assert_eq!(
        call.raw_arguments_json.as_deref(),
        Some("{\"command\":\"pwd\"}")
    );

    drop(conn);
    server.join();
}

/// A silent upgraded localhost peer must be interrupted through the production
/// connection's abort-waker path and return typed cancellation.
#[test]
fn localhost_ws_silent_turn_returns_typed_cancellation() {
    let server = TestWsServer::spawn(ServerScript::Silent);
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let mut connect_abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-cancel",
        &crate::test_network_policy(),
        &mut connect_abort,
    )
    .expect("connect localhost WS");
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = conn.run_turn(
                &config,
                "ap-local-cancel",
                &request,
                None,
                None,
                &mut abort,
                &mut |_| {},
                &mut |_| {},
            );
            result_tx
                .send(result)
                .expect("cancellation result receiver");
        });
        server.wait_for_request();
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("turn abort-waker registration");
        aborted.store(true, Ordering::SeqCst);
        waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered turn waker")();
        assert!(matches!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("typed cancellation result"),
            Err(LlmError::Canceled)
        ));
    });

    drop(conn);
    server.join();
}

/// A silent upgraded localhost peer must traverse the production reader and
/// request writer before the test-sized provider-frame idle deadline fires.
#[test]
fn localhost_ws_silent_turn_returns_typed_idle_timeout() {
    let server = TestWsServer::spawn(ServerScript::Silent);
    let mut config = test_responses_config();
    config.base_url = server.base_url();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, None);
    let mut abort = NeverAbort;
    let mut conn = WsConn::connect(
        &config,
        "thread-timeout",
        &crate::test_network_policy(),
        &mut abort,
    )
    .expect("connect localhost WS");

    let result = conn.run_envelope_with_timeouts(
        "ap-local-timeout",
        envelope,
        EnvelopeExecution {
            recording_stream: None,
            evidence_mode: path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
            timeouts: EnvelopeTimeouts {
                idle: Duration::from_millis(20),
                absolute: None,
            },
            response_mode: ResponseMode::Ordinary,
        },
        &mut abort,
        &mut |_| {},
        &mut |_| {},
        &mut None,
    );
    server.wait_for_request();
    let body = http_status_zero(result);
    assert!(body.contains("provider stream idle timeout"), "{body}");
    assert!(body.contains("transport=Websocket"), "{body}");
    assert!(body.contains("agent_prompt_id=ap-local-timeout"), "{body}");
    assert!(body.contains("partial_output=false"), "{body}");
    let error = LlmError::HttpStatus(0, body);
    assert_eq!(
        error.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Transport)
    );

    drop(conn);
    server.join();
}

/// Ensure WebSocket turns wake promptly from registered cancellation rather
/// than waiting for the five-minute provider-stream idle timeout.
#[test]
fn ws_turn_abort_waker_returns_typed_cancellation_promptly() {
    let (mut conn, _inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let aborted = Arc::new(AtomicBool::new(false));
    let (registered_tx, registered_rx) = std_mpsc::channel();
    let waker = Arc::new(Mutex::new(None));
    let mut abort = CapturingAbort {
        aborted: Arc::clone(&aborted),
        registered_tx,
        waker: Arc::clone(&waker),
    };
    let (result_tx, result_rx) = std_mpsc::channel();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = conn.run_turn(
                &config,
                "ap-ws-abort",
                &request,
                None,
                None,
                &mut abort,
                &mut |_| {},
                &mut |_| {},
            );
            result_tx.send(result).expect("result receiver");
        });
        registered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("abort waker registration");

        let start = Instant::now();
        aborted.store(true, Ordering::SeqCst);
        let wake = waker
            .lock()
            .expect("waker slot lock")
            .as_ref()
            .expect("registered waker")
            .clone();
        wake();

        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("prompt cancellation result");
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(matches!(result, Err(LlmError::Canceled)));
    });
}

/// Regression for tau-agent-jx2z: a WebSocket turn that never receives a
/// terminal provider frame must trip the per-turn idle watchdog instead of
/// waiting forever on a quiet pooled socket.
#[test]
fn ws_turn_returns_idle_timeout_error_after_stalled_frame_stream() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, None);
    let mut abort = NeverAbort;

    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.output_text.delta","delta":"hello"}"#.into(),
        })
        .expect("queue partial WS frame");
    let result = conn.run_envelope_with_timeouts(
        "ap-stalled-ws",
        envelope,
        EnvelopeExecution {
            recording_stream: None,
            evidence_mode: path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
            timeouts: EnvelopeTimeouts {
                idle: Duration::from_millis(50),
                absolute: None,
            },
            response_mode: ResponseMode::Ordinary,
        },
        &mut abort,
        &mut |_| {},
        &mut |_| {},
        &mut None,
    );

    let body = http_status_zero(result);
    assert!(body.contains("provider stream idle timeout"), "{body}");
    assert!(body.contains("transport=Websocket"), "{body}");
    assert!(body.contains("agent_prompt_id=ap-stalled-ws"), "{body}");
    assert!(body.contains("elapsed="), "{body}");
    assert!(body.contains("idle="), "{body}");
    assert!(body.contains("idle_timeout="), "{body}");
    assert!(body.contains("partial_output=true"), "{body}");
}

/// Prewarm's elapsed absolute deadline wins even when nonterminal provider
/// frames are already queued for processing.
#[test]
fn prewarm_absolute_timeout_preempts_queued_nonterminal_frames() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, Some(false));
    let mut abort = NeverAbort;
    for _ in 0..4 {
        inbound_tx
            .send_blocking(InboundEvent::Event {
                text: r#"{"type":"response.output_text.delta","delta":"x"}"#.into(),
            })
            .expect("queue nonterminal frame");
    }

    let result = conn.run_envelope_with_timeouts(
        "<prewarm>",
        envelope,
        EnvelopeExecution {
            recording_stream: None,
            evidence_mode: path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
            timeouts: EnvelopeTimeouts {
                idle: Duration::from_secs(1),
                absolute: Some(Duration::ZERO),
            },
            response_mode: ResponseMode::Ordinary,
        },
        &mut abort,
        &mut |_| {},
        &mut |_| {},
        &mut None,
    );

    assert!(matches!(
        result,
        Err(LlmError::HttpStatus(0, body))
            if body == "websocket prewarm response timeout"
    ));
}

/// Rejected provider data frames still contribute to cumulative transport bytes
/// before the socket is retired.
#[test]
fn malformed_text_frame_counts_bytes_before_protocol_error() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let malformed = "{not-json";
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: malformed.into(),
        })
        .expect("queue malformed frame");
    let mut observed_bytes = 0;
    let error = match conn.run_turn(
        &config,
        "ap-malformed",
        &request,
        None,
        None,
        &mut NeverAbort,
        &mut |_| {},
        &mut |state| observed_bytes = state.response_bytes_received(),
    ) {
        Ok(_) => panic!("malformed frame must retire socket"),
        Err(error) => error,
    };
    assert!(matches!(error.root_error(), LlmError::HttpStatus(0, _)));
    assert_eq!(observed_bytes, malformed.len() as u64);
}

/// Tau must own the exact one-MiB limits instead of inheriting tungstenite's
/// larger dependency defaults.
#[test]
fn websocket_config_caps_frames_and_complete_messages_at_one_mibibyte() {
    let config = websocket_config();
    assert_eq!(config.max_frame_size, Some(1024 * 1024));
    assert_eq!(config.max_message_size, Some(1024 * 1024));
}

/// A complete one-MiB text message assembled from smaller wire fragments is
/// accepted by the production transport, while its first excess byte is
/// terminal.
#[test]
fn production_fragmented_message_accepts_equality_and_retires_on_excess() {
    let completion = r#"{"type":"response.completed","response":{"id":"fragmented"}}"#;
    let exact = padded_json(completion, MAX_WS_EVENT_BYTES);
    let midpoint = MAX_WS_EVENT_BYTES / 2;
    let (result, _, _) = run_local_resource_script(ServerScript::FragmentedText {
        first: exact[..midpoint].to_owned(),
        second: exact[midpoint..].to_owned(),
    });
    result.expect("exact complete-message limit");

    let excess = padded_json(completion, MAX_WS_EVENT_BYTES + 1);
    let (result, reader_retired, writer_retired) =
        run_local_resource_script(ServerScript::FragmentedText {
            first: excess[..midpoint].to_owned(),
            second: excess[midpoint..].to_owned(),
        });
    assert!(matches!(
        result,
        Err(LlmError::InvalidResponse(message)) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    ));
    assert!(reader_retired && writer_retired);
}

/// One unfragmented frame exercises the independent production frame cap at
/// exact equality and at the first excess byte.
#[test]
fn production_frame_accepts_equality_and_retires_on_excess() {
    let completion = r#"{"type":"response.completed","response":{"id":"frame"}}"#;
    run_local_resource_script(ServerScript::Frames(vec![padded_json(
        completion,
        MAX_WS_EVENT_BYTES,
    )]))
    .0
    .expect("exact frame limit");
    let (result, reader_retired, writer_retired) = run_local_resource_script(ServerScript::Frames(
        vec![padded_json(completion, MAX_WS_EVENT_BYTES + 1)],
    ));
    assert!(matches!(
        result,
        Err(LlmError::InvalidResponse(message)) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    ));
    assert!(reader_retired && writer_retired);
}

/// The production reader accepts exactly 64 MiB across a finite attempt and
/// rejects byte 64 MiB + 1 before parsing, even under a queued frame flood.
#[test]
fn production_attempt_budget_accepts_equality_and_rejects_first_excess() {
    let terminal = padded_json(
        r#"{"type":"response.completed","response":{"id":"exact-attempt"}}"#,
        MAX_WS_EVENT_BYTES,
    );
    let mut expected = String::new();
    let mut exact = Vec::new();
    for sequence in 0..63 {
        let delta = format!("{sequence:02},");
        expected.push_str(&delta);
        exact.push(padded_json(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "output_index": 0,
                "delta": delta,
            })
            .to_string(),
            MAX_WS_EVENT_BYTES,
        ));
    }
    exact.push(terminal);
    let state = run_local_resource_script(ServerScript::Frames(exact))
        .0
        .expect("exact cumulative attempt limit")
        .state;
    assert_eq!(
        state.aggregate_assistant_text(),
        expected,
        "flood must not drop or reorder data"
    );

    let mut excess = vec![padded_json("{}", MAX_WS_EVENT_BYTES); 64];
    excess.push("{}".to_owned());
    let (result, reader_retired, writer_retired) =
        run_local_resource_script(ServerScript::Frames(excess));
    assert!(matches!(
        result,
        Err(LlmError::InvalidResponse(message)) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    ));
    assert!(reader_retired && writer_retired);
}

/// Equality at the cumulative attempt limit is accepted, including carried
/// repair bytes, while the first byte beyond it is rejected.
#[test]
fn cumulative_attempt_bytes_accept_exact_limit_and_reject_first_excess() {
    assert_eq!(
        checked_attempt_response_bytes(MAX_ATTEMPT_RESPONSE_BYTES - 1, 1),
        Some(MAX_ATTEMPT_RESPONSE_BYTES)
    );
    assert_eq!(
        checked_attempt_response_bytes(MAX_ATTEMPT_RESPONSE_BYTES, 1),
        None
    );
    assert_eq!(
        checked_attempt_response_bytes(MAX_ATTEMPT_RESPONSE_BYTES - 17, 17),
        Some(MAX_ATTEMPT_RESPONSE_BYTES)
    );
}

/// Bytes carried from a discarded transparent repair dispatch participate in
/// the live turn-owner check at exact equality and first excess.
#[test]
fn carried_repair_bytes_share_the_production_attempt_budget() {
    let completion = r#"{"type":"response.completed","response":{"id":"repair-budget"}}"#;
    for excess in [false, true] {
        let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
        let carried = MAX_ATTEMPT_RESPONSE_BYTES - completion.len() as u64 + u64::from(excess);
        conn.carry_response_bytes(carried);
        inbound_tx
            .send_blocking(InboundEvent::Event {
                text: completion.into(),
            })
            .expect("queue completion");
        let config = test_responses_config();
        let fixture = PromptFixture::new();
        let result = conn.run_turn(
            &config,
            "ap-repair-budget",
            &fixture.payload(),
            None,
            None,
            &mut NeverAbort,
            &mut |_| {},
            &mut |_| {},
        );
        if excess {
            assert!(matches!(
                result,
                Err(LlmError::InvalidResponse(message))
                    if message == RESPONSE_RESOURCE_LIMIT_ERROR
            ));
        } else {
            assert_eq!(
                result
                    .expect("exact carried-repair equality")
                    .state
                    .transport_response_bytes(),
                MAX_ATTEMPT_RESPONSE_BYTES
            );
        }
    }
}

/// Retained-state admission is transactional: equality commits the event, but
/// the first excess returns the fixed error without mutating semantic state.
#[test]
fn retained_state_budget_accepts_equality_and_rejects_before_mutation() {
    let event = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": "bounded text"
    });
    let mut admitted = StreamState::new();
    apply_parsed_json_event(&mut admitted, &event, None, &mut |_| {})
        .expect("measure admitted state");
    let exact = admitted.logical_retained_bytes();

    let mut equality = StreamState::new();
    apply_ws_json_event_with_limit(&mut equality, &event, None, exact, &mut |_| {})
        .expect("equality is accepted");
    assert_eq!(equality.logical_retained_bytes(), exact);

    let mut excess = StreamState::new();
    let mut updates = 0;
    let error =
        apply_ws_json_event_with_limit(&mut excess, &event, None, exact - 1, &mut |_| updates += 1)
            .expect_err("first excess is terminal");
    assert!(matches!(
        error,
        LlmError::InvalidResponse(message) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    ));
    assert!(
        response_resource_limit_error().retry_decision().is_none(),
        "resource excess must not enter logical retry"
    );
    assert_eq!(
        super::super::pool::recovery_decision(&response_resource_limit_error(), false),
        super::super::pool::RecoveryDecision::Surface,
        "resource excess must not spend transparent repair"
    );
    assert_eq!(excess.logical_retained_bytes(), 0);
    assert_eq!(updates, 0);
}

/// Missing raw JSON on completed opaque output is a shape error before retained
/// state accounting, even when the same event would exceed a zero byte limit.
#[test]
fn missing_opaque_raw_json_precedes_websocket_resource_limit() {
    let event = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {"type": "compaction", "encrypted_content": "opaque"}
    });
    let mut state = StreamState::new();
    let mut updates = 0;

    assert!(matches!(
        apply_ws_json_event_with_limit(&mut state, &event, None, 0, &mut |_| updates += 1),
        Err(LlmError::InvalidResponse(_))
    ));
    assert_eq!(updates, 0);
    assert!(state.output_items.is_empty());
    assert_eq!(state.admitted_retained_state_bytes(), 0);
}

/// The production 64 MiB retained-state comparison accepts exact equality and
/// rejects its first excess before changing live semantic output.
#[test]
fn production_retained_state_limit_accepts_equality_and_rejects_first_excess() {
    let event = serde_json::json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": "x"
    });
    let growth = projected_retained_state_bytes(&StreamState::new(), &event, None)
        .expect("project one-byte delta");
    for excess in [false, true] {
        let mut state = StreamState::new();
        state.commit_retained_state_bytes(MAX_RETAINED_STATE_BYTES - growth + u64::from(excess));
        let result = apply_ws_json_event(&mut state, &event, None, &mut |_| {});
        if excess {
            assert!(matches!(
                result,
                Err(LlmError::InvalidResponse(message))
                    if message == RESPONSE_RESOURCE_LIMIT_ERROR
            ));
            assert!(state.output_items.is_empty());
            assert!(state.assistant_text_bytes() == 0);
        } else {
            result.expect("exact production retained-state equality");
            assert_eq!(
                state.admitted_retained_state_bytes(),
                MAX_RETAINED_STATE_BYTES
            );
        }
    }
}

/// A real socket stream reaches the production retained-state limit exactly;
/// one additional semantic byte fails before mutation and retires both tasks.
#[test]
fn production_stream_reaches_exact_retained_limit_and_retires_on_excess() {
    let response_id = "retained";
    let terminal = serde_json::json!({
        "type": "response.completed",
        "response": {"id": response_id}
    })
    .to_string();
    let fixed = std::mem::size_of::<crate::common::OutputItemAccumulator>() as u64
        + serde_json::to_vec(
            &serde_json::from_str::<serde_json::Value>(&terminal).expect("terminal value"),
        )
        .expect("terminal serialization")
        .len() as u64
        + response_id.len() as u64;
    let remaining = MAX_RETAINED_STATE_BYTES - fixed;
    assert_eq!(remaining % 2, 0, "fixture must admit exact duplicated text");
    let exact_text = usize::try_from(remaining / 2).expect("test text fits usize");

    let frames = |text_len: usize| {
        let mut frames = Vec::new();
        let mut remaining = text_len;
        let mut random = 0x9e37_79b9_7f4a_7c15_u64;
        while remaining != 0 {
            let chunk_len = remaining.min(MAX_WS_EVENT_BYTES - 128);
            let mut chunk = String::with_capacity(chunk_len);
            for _ in 0..chunk_len {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                chunk.push(char::from(b'a' + (random % 26) as u8));
            }
            frames.push(
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": chunk,
                })
                .to_string(),
            );
            remaining -= chunk_len;
        }
        frames.push(terminal.clone());
        frames
    };

    let exact = run_local_resource_script(ServerScript::Frames(frames(exact_text)))
        .0
        .expect("exact production retained-state limit")
        .state;
    assert_eq!(
        exact.admitted_retained_state_bytes(),
        MAX_RETAINED_STATE_BYTES
    );

    let (result, reader_retired, writer_retired) =
        run_local_resource_script(ServerScript::Frames(frames(exact_text + 1)));
    assert!(matches!(
        result,
        Err(LlmError::InvalidResponse(message)) if message == RESPONSE_RESOURCE_LIMIT_ERROR
    ));
    assert!(reader_retired && writer_retired);
}

/// Existing sparse-gap validation keeps precedence over the cumulative
/// retained-state policy, even when state is already at the resource ceiling.
#[test]
fn sparse_reasoning_gap_precedes_retained_state_excess() {
    let mut state = StreamState::new();
    state.commit_retained_state_bytes(MAX_RETAINED_STATE_BYTES);
    let event = serde_json::json!({
        "type": "response.reasoning_summary_text.delta",
        "output_index": super::super::MAX_OUTPUT_SLOT_GROWTH,
        "delta": "would exceed retained state"
    });
    let error = apply_ws_json_event(&mut state, &event, None, &mut |_| {})
        .expect_err("forbidden sparse reasoning gap");
    assert!(matches!(
        error,
        LlmError::InvalidResponse(message) if message == super::super::INVALID_OUTPUT_INDEX
    ));
    assert!(state.thinking.is_none());
}

/// Incremental admission must exactly match the independently measured state
/// across slots, assistant/reasoning/tool text, opaque raw replay, and terminal
/// data without copying the full accumulator for each event.
#[test]
fn retained_state_accounting_covers_every_charged_field_family() {
    let events = [
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"assistant"}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"same-slot","name":"shell"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","summary":"retained opaque"}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"compaction"}}"#,
        r#"{"type":"response.reasoning_summary_text.delta","output_index":1,"delta":"reasoning"}"#,
        r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"/tmp\"}"}"#,
        r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"sealed","summary":[]}}"#,
        r#"{"type":"response.output_item.done","output_index":4,"item":{"type":"future_item","payload":{"keep":"opaque"}}}"#,
        r#"{"type":"response.output_text.delta","output_index":5,"delta":"replace me"}"#,
        r#"{"type":"response.function_call_arguments.delta","output_index":5,"delta":"{\"replacement\":true}"}"#,
        r#"{"type":"response.completed","response":{"id":"terminal","usage":{"input_tokens":1,"output_tokens":2}}}"#,
    ];
    let mut state = StreamState::new();
    for raw in events {
        let event: serde_json::Value = serde_json::from_str(raw).expect("fixture event");
        let exact =
            projected_retained_state_bytes(&state, &event, super::super::raw_output_item_json(raw))
                .expect("project retained state");
        apply_ws_json_event_with_limit(
            &mut state,
            &event,
            super::super::raw_output_item_json(raw),
            exact,
            &mut |_| {},
        )
        .expect("exact per-event admission");
        assert_eq!(
            state.admitted_retained_state_bytes(),
            state.logical_retained_bytes(),
            "accounting drift after {raw}"
        );
    }
}

/// An assistant done event without an extracted raw sidecar must clear and stop
/// charging the older same-slot sidecar while retaining its semantic text.
#[test]
fn retained_state_accounting_matches_message_without_raw_sidecar() {
    let event = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "semantic only"}]
        }
    });
    let mut state = StreamState::new();
    let prior_raw = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"prior"}],"old_sidecar":true}"#;
    let prior = serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": serde_json::from_str::<serde_json::Value>(prior_raw).expect("prior item")
    });
    apply_ws_json_event_with_limit(
        &mut state,
        &prior,
        Some(prior_raw),
        MAX_RETAINED_STATE_BYTES,
        &mut |_| {},
    )
    .expect("seed prior raw sidecar");
    let exact =
        projected_retained_state_bytes(&state, &event, None).expect("project message without raw");
    apply_ws_json_event_with_limit(&mut state, &event, None, exact, &mut |_| {})
        .expect("exact no-sidecar equality");
    assert_eq!(
        state.admitted_retained_state_bytes(),
        state.logical_retained_bytes()
    );
    let Some(OutputItemAccumulator::Message(message)) = state.output_items.first() else {
        panic!("assistant message output");
    };
    assert!(message.responses_raw_json.is_none());
}

/// The provider-data lane has exactly one queued event. Saturation reports
/// backpressure, and retrying after one dequeue preserves both events' order.
#[test]
fn inbound_data_lane_backpressures_without_drop_or_reorder() {
    let (tx, mut rx) = mpsc::channel(1);
    let control = Arc::new(InboundControl::new());
    let sender = InboundSender {
        tx,
        control: Arc::clone(&control),
    };
    let first = InboundEvent::Event {
        text: r#"{"sequence":1}"#.into(),
    };
    let second = InboundEvent::Event {
        text: r#"{"sequence":2}"#.into(),
    };
    sender.tx.try_send(first).expect("sole queue slot");
    let second = sender
        .tx
        .try_send(second)
        .expect_err("second event is backpressured")
        .into_inner();
    let InboundEvent::Event { text: first } = rx.blocking_recv().expect("first event") else {
        panic!("expected first text event");
    };
    sender.tx.try_send(second).expect("slot reopened");
    let InboundEvent::Event { text: second } = rx.blocking_recv().expect("second event") else {
        panic!("expected second text event");
    };
    assert_eq!(first.as_str(), r#"{"sequence":1}"#);
    assert_eq!(second.as_str(), r#"{"sequence":2}"#);
}

/// Coalesced cancellation wins over writer failure and provider data without
/// growing a FIFO of local controls.
#[test]
fn cancellation_control_has_priority_over_writer_failure() {
    let control = InboundControl::new();
    control.notify_writer_failure(
        path_crate_attempt_failure::TransportFailureKind::WebSocketControlPing,
    );
    control.notify_abort();
    assert!(matches!(control.take(), Some(InboundControlEvent::Abort)));
    assert!(matches!(
        control.take(),
        Some(InboundControlEvent::WriterFailure(
            path_crate_attempt_failure::TransportFailureKind::WebSocketControlPing
        ))
    ));
    assert!(control.take().is_none(), "each coalesced fact drains once");
}

/// A local writer failure is read from the independent control path before an
/// already queued provider completion can mutate or successfully end the turn.
#[test]
fn writer_failure_preempts_queued_provider_data() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.completed","response":{"id":"must-not-commit"}}"#.into(),
        })
        .expect("queue provider completion");
    conn.inbound_control
        .notify_writer_failure(path_crate_attempt_failure::TransportFailureKind::Send);
    conn.inbound_control.notify_abort();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let mut updates = 0;
    let error = match conn.run_turn(
        &config,
        "ap-writer-priority",
        &fixture.payload(),
        None,
        None,
        &mut NeverAbort,
        &mut |_| {},
        &mut |_| updates += 1,
    ) {
        Ok(_) => panic!("writer failure must preempt completion"),
        Err(error) => error,
    };
    assert!(matches!(error.root_error(), LlmError::HttpStatus(0, _)));
    assert_eq!(updates, 0);
}

/// Confirmed cancellation from the independent control path must beat provider
/// data that was already waiting in the bounded lane.
#[test]
fn cancellation_preempts_queued_provider_data() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    inbound_tx
        .send_blocking(InboundEvent::Event {
            text: r#"{"type":"response.completed","response":{"id":"must-not-commit"}}"#.into(),
        })
        .expect("queue provider completion");
    conn.inbound_control.notify_abort();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let mut abort = AbortAfterChecks { remaining_false: 2 };
    let mut updates = 0;
    let result = conn.run_turn(
        &config,
        "ap-cancel-priority",
        &fixture.payload(),
        None,
        None,
        &mut abort,
        &mut |_| {},
        &mut |_| updates += 1,
    );
    assert!(matches!(result, Err(LlmError::Canceled)));
    assert_eq!(updates, 0);
}

/// Quota parsing is mode-independent: standard and Lite WebSocket turns both
/// surface the official nameless default-pool event.
#[test]
fn ws_turn_surfaces_nameless_default_quota_in_both_modes() {
    for (model_id, mode) in [
        ("gpt-test", ResponsesMode::Standard),
        ("gpt-5.6-sol", ResponsesMode::LiteCompatibility),
    ] {
        let (mut conn, inbound_tx, mut outbound_rx) = test_ws_conn();
        let mut config = test_responses_config();
        config.model_id = model_id.to_owned();
        config.mode = mode;
        let fixture = PromptFixture::new();
        let request = fixture.payload();
        let mut abort = NeverAbort;
        let mut observed_limit = None;

        for text in [
            r#"{"type":"codex.rate_limits","plan_type":"plus","rate_limits":{"secondary":{"used_percent":45,"window_minutes":10080,"reset_at":1700600000}}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_quota"}}"#,
        ] {
            inbound_tx
                .send_blocking(InboundEvent::Event { text: text.into() })
                .expect("queue WS fixture frame");
        }

        conn.run_turn(
            &config,
            "ap-ws-quota",
            &request,
            None,
            None,
            &mut abort,
            &mut |_| {},
            &mut |state| {
                if let Some(observation) = state.quota_observation.as_ref() {
                    observed_limit = observation
                        .active_limit_id
                        .as_ref()
                        .map(ToString::to_string);
                }
            },
        )
        .expect("completed WS turn");
        assert_eq!(observed_limit.as_deref(), Some("codex"), "{model_id}");
        let WsCommand::SendText(request_text) =
            outbound_rx.try_recv().expect("sent WS request envelope");
        let request_json: serde_json::Value =
            serde_json::from_str(&request_text).expect("valid WS request envelope");
        let lite_marker = request_json
            .pointer("/client_metadata/ws_request_header_x_openai_internal_codex_responses_lite")
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            lite_marker,
            mode.is_lite_compatibility().then_some("true"),
            "{model_id}"
        );
    }
}
