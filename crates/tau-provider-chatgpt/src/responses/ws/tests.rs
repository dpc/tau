use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use super::*;
use crate::responses::{ResponsesMode, ResponsesSurface};
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
        mode: ResponsesMode::Standard,
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
    let error = map_connect_wait_error(ConnectWaitError::Timeout);
    assert!(matches!(
        error,
        LlmError::HttpStatus(0, ref body)
            if body == "stream error: websocket connect timeout"
    ));
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
        map_connect_wait_error(ConnectWaitError::Canceled),
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
        .send(InboundEvent::Event {
            text: r#"{"type":"response.output_text.delta","delta":"hello"}"#.into(),
        })
        .expect("queue partial WS frame");
    let result = conn.run_envelope_with_timeouts(
        "ap-stalled-ws",
        envelope,
        None,
        &mut abort,
        EnvelopeTimeouts {
            idle: Duration::from_millis(50),
            absolute: None,
        },
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

/// Prewarm has an absolute response bound independent of provider frame
/// activity, so a peer cannot keep supervised work alive with nonterminal data.
#[test]
fn prewarm_absolute_timeout_bounds_nonterminal_frame_stream() {
    let (mut conn, inbound_tx, _outbound_rx) = test_ws_conn();
    let config = test_responses_config();
    let fixture = PromptFixture::new();
    let request = fixture.payload();
    let envelope = build_ws_envelope(&config, &request, None, Some(false));
    let mut abort = NeverAbort;
    for _ in 0..4 {
        inbound_tx
            .send(InboundEvent::Event {
                text: r#"{"type":"response.output_text.delta","delta":"x"}"#.into(),
            })
            .expect("queue nonterminal frame");
    }

    let result = conn.run_envelope_with_timeouts(
        "<prewarm>",
        envelope,
        None,
        &mut abort,
        EnvelopeTimeouts {
            idle: Duration::from_secs(1),
            absolute: Some(Duration::from_millis(20)),
        },
        &mut |_| std::thread::sleep(Duration::from_millis(10)),
    );

    assert!(matches!(
        result,
        Err(LlmError::HttpStatus(0, body))
            if body == "websocket prewarm response timeout"
    ));
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
                .send(InboundEvent::Event { text: text.into() })
                .expect("queue WS fixture frame");
        }

        conn.run_turn(
            &config,
            "ap-ws-quota",
            &request,
            None,
            &mut abort,
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
