use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use super::*;
use crate::TurnAbortWaker;
use crate::responses::ResponsesSurface;

struct CapturingAbort {
    aborted: Arc<AtomicBool>,
    registered_tx: std_mpsc::Sender<()>,
    waker: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>>,
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

/// Ensure WebSocket turns wake promptly from registered cancellation rather
/// than waiting for the 120 second provider-event timeout.
#[test]
fn ws_turn_abort_waker_returns_499_promptly() {
    let (outbound_tx, _outbound_rx) = mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = std_mpsc::channel();
    let runtime = ws_runtime::handle();
    let reader_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    let writer_abort = runtime.spawn(std::future::pending::<()>()).abort_handle();
    let mut conn = WsConn {
        outbound_tx,
        inbound_tx,
        inbound_rx,
        reader_abort,
        writer_abort,
        opened_at: Instant::now(),
        bearer: "test-token".to_owned(),
        cached_response_id: None,
    };
    let config = ResponsesConfig {
        surface: ResponsesSurface::ChatGpt,
        base_url: "https://chatgpt.com/backend-api".to_owned(),
        api_key: "test-token".to_owned(),
        model_id: "gpt-test".to_owned(),
        context_window: 128_000,
        account_id: None,
        supports_reasoning_effort: true,
        supports_reasoning_summary: true,
        supports_verbosity: true,
        supports_phase: true,
        supports_encrypted_reasoning: true,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: true,
    };
    let context = tau_proto::PromptContext::default();
    let session_id = tau_proto::SessionId::new("session-test");
    let agent_id = tau_proto::AgentId::parse("agent-test").expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    let request = PromptPayload {
        system_prompt: "",
        context: &context,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        share_user_cache_key: false,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: false,
    };
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
