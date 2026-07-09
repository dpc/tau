use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use super::*;
use crate::responses::ResponsesSurface;
use crate::{NeverAbort, TurnAbortWaker};

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

fn test_ws_conn() -> (
    WsConn,
    std_mpsc::Sender<InboundEvent>,
    UnboundedReceiver<WsCommand>,
) {
    let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = std_mpsc::channel();
    let runtime = ws_runtime::handle();
    let reader_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    let writer_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    (
        WsConn {
            outbound_tx,
            inbound_tx: inbound_tx.clone(),
            inbound_rx,
            reader_abort,
            writer_abort,
            opened_at: Instant::now(),
            bearer: "test-token".to_owned(),
            cached_response_id: None,
        },
        inbound_tx,
        _outbound_rx,
    )
}

fn test_responses_config() -> ResponsesConfig {
    ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".to_owned(),
        api_key: "test-token".to_owned(),
        model_id: "gpt-test".to_owned(),
        raw_context_window: 128_000,
        account_id: None,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: true,
        supports_phase: true,
        supports_encrypted_reasoning: true,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    }
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
            session_id: tau_proto::SessionId::new("session-test"),
            agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn payload(&self) -> PromptPayload<'_> {
        PromptPayload {
            system_prompt: "",
            context: &self.context,
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

/// Ensure WebSocket turns wake promptly from registered cancellation rather
/// than waiting for the five-minute provider-stream idle timeout.
#[test]
fn ws_turn_abort_waker_returns_499_promptly() {
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
                &mut abort,
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
        assert!(matches!(
            result,
            Err(LlmError::HttpStatus(499, ref body)) if body == "cancelled by harness"
        ));
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
        .send(InboundEvent::Event {
            text: r#"{"type":"response.output_text.delta","delta":"hello"}"#.into(),
        })
        .expect("queue partial WS frame");
    let result = conn.run_envelope_with_idle_timeout(
        "ap-stalled-ws",
        envelope,
        None,
        &mut abort,
        Duration::from_millis(50),
        &mut |_| {},
    );

    let Err(LlmError::HttpStatus(0, body)) = result else {
        panic!("expected timeout stream error");
    };
    assert!(body.contains("provider stream idle timeout"), "{body}");
    assert!(body.contains("transport=Websocket"), "{body}");
    assert!(body.contains("agent_prompt_id=ap-stalled-ws"), "{body}");
    assert!(body.contains("elapsed="), "{body}");
    assert!(body.contains("idle="), "{body}");
    assert!(body.contains("idle_timeout="), "{body}");
    assert!(body.contains("partial_output=true"), "{body}");
}
