use super::*;

#[derive(Default)]
struct TransportCounts {
    ws_upgrade_requests: std::sync::atomic::AtomicUsize,
    http_post_requests: std::sync::atomic::AtomicUsize,
}

#[derive(Clone, Copy)]
enum WsFailureMode {
    Upgrade426,
    CloseWithoutResponse,
}

fn spawn_ws_426_server() -> (String, std::sync::Arc<TransportCounts>) {
    spawn_ws_failure_server(WsFailureMode::Upgrade426)
}

fn spawn_ws_disconnect_server() -> (String, std::sync::Arc<TransportCounts>) {
    spawn_ws_failure_server(WsFailureMode::CloseWithoutResponse)
}

fn spawn_ws_failure_server(mode: WsFailureMode) -> (String, std::sync::Arc<TransportCounts>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    listener
        .set_nonblocking(true)
        .expect("set fake provider nonblocking");
    let addr = listener.local_addr().expect("fake provider addr");
    let counts = std::sync::Arc::new(TransportCounts::default());
    let thread_counts = std::sync::Arc::clone(&counts);
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let mut request = [0_u8; 1024];
            let read = std::io::Read::read(&mut stream, &mut request).unwrap_or(0);
            if request[..read].starts_with(b"POST ") {
                thread_counts
                    .http_post_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            } else {
                thread_counts
                    .ws_upgrade_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            match mode {
                WsFailureMode::Upgrade426 => {
                    let response = concat!(
                        "HTTP/1.1 426 Upgrade Required\r\n",
                        "Content-Length: 21\r\n",
                        "Connection: close\r\n",
                        "\r\n",
                        "upgrade unavailable\n"
                    );
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                }
                WsFailureMode::CloseWithoutResponse => {}
            }
        }
    });
    (format!("http://{addr}/backend-api"), counts)
}

fn test_config(base_url: String) -> responses::ResponsesConfig {
    responses::ResponsesConfig {
        surface: responses::ResponsesSurface::ChatGpt,
        base_url,
        api_key: "token".to_owned(),
        model_id: "gpt-5.3-codex".to_owned(),
        context_window: CONTEXT_WINDOW,
        account_id: Some("account".to_owned()),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_verbosity: false,
        supports_phase: true,
        supports_encrypted_reasoning: false,
        supports_websocket: true,
        supports_compaction: true,
        supports_prompt_cache_key: false,
    }
}

fn test_prompt_payload<'a>(
    session_id: &'a tau_proto::SessionId,
    agent_id: &'a tau_proto::AgentId,
    context: &'a tau_proto::PromptContext,
) -> common::PromptPayload<'a> {
    common::PromptPayload {
        system_prompt: "",
        context,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        compaction: None,
        originator: &tau_proto::PromptOriginator::User,
        session_id,
        agent_id,
        share_user_cache_key: false,
        debug_provider_requests: false,
    }
}

#[test]
fn publishes_chatgpt_model_metadata() {
    // ChatGPT account profiles do not store models; this crate is the
    // source of truth for their published model IDs and capabilities.
    let models = models_for_provider(&ProviderName::new("work-chatgpt"));
    let ids = models
        .iter()
        .map(|model| model.id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            "work-chatgpt/gpt-5.6-sol",
            "work-chatgpt/gpt-5.6-terra",
            "work-chatgpt/gpt-5.6-luna",
            "work-chatgpt/gpt-5.5",
            "work-chatgpt/gpt-5.4",
            "work-chatgpt/gpt-5.4-mini",
            "work-chatgpt/gpt-5.3-codex",
        ],
    );
    assert!(models.iter().all(|model| model.supports_compaction));
    assert!(
        models
            .iter()
            .all(|model| model.tags.iter().any(|tag| tag.as_str() == "shell:chatgpt"))
    );
    assert!(models.iter().all(|model| {
        model
            .tags
            .iter()
            .any(|tag| tag.as_str() == "tools:custom-text")
    }));
}

#[test]
fn config_for_model_enables_codex_responses_capabilities() {
    // The builtin registry only supplies account credentials; ChatGPT owns
    // the Responses feature matrix for its model IDs.
    let config = config_for_model(
        &ModelName::new("gpt-5.3-codex"),
        "token".to_owned(),
        Some("account".to_owned()),
    );

    assert_eq!(config.surface, responses::ResponsesSurface::ChatGpt);
    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.api_key, "token");
    assert_eq!(config.account_id.as_deref(), Some("account"));
    assert!(config.supports_websocket);
    assert!(config.supports_compaction);
    assert!(config.supports_phase);
    assert!(config.supports_encrypted_reasoning);
}

#[test]
fn websocket_capability_error_does_not_fallback_to_http_sse() {
    // Regression for tau-agent-y8vc: WebSocket-capable ChatGPT/Codex models
    // treat WS support as a routing commitment. An upgrade/capability failure
    // must be surfaced to the caller instead of silently POSTing the same turn
    // through HTTP/SSE.
    let (base_url, counts) = spawn_ws_426_server();
    let config = test_config(base_url);
    let runtime = ChatGptRuntime::new();
    let session_id = tau_proto::SessionId::new("session-ws-426");
    let agent_id = tau_proto::AgentId::parse("agent-ws-426").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut turn_state = ChatGptTurnState::new(2);
    let mut abort = NeverAbort;
    let mut on_update = |_: &common::StreamState| {};

    let error = match runtime.stream(
        "ap-ws-426",
        &config,
        &request,
        &mut turn_state,
        &mut abort,
        &mut on_update,
    ) {
        Ok(_) => panic!("WS upgrade failure should surface"),
        Err(error) => error,
    };

    assert!(matches!(error, common::LlmError::HttpStatus(426, _)));
    assert_eq!(
        counts
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        counts
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "WS failure must not fall back to HTTP/SSE POST"
    );
}

#[test]
fn retryable_websocket_exhaustion_does_not_fallback_to_http_sse() {
    // Regression for tau-agent-y8vc: even when the per-turn WS retry budget is
    // exhausted, retryable transport failures must return a terminal provider
    // error for that turn rather than disabling WS and using HTTP/SSE.
    let (base_url, counts) = spawn_ws_disconnect_server();
    let config = test_config(base_url);
    let runtime = ChatGptRuntime::new();
    let session_id = tau_proto::SessionId::new("session-ws-retry");
    let agent_id = tau_proto::AgentId::parse("agent-ws-retry").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut turn_state = ChatGptTurnState::new(0);
    let mut abort = NeverAbort;
    let mut on_update = |_: &common::StreamState| {};

    let error = match runtime.stream(
        "ap-ws-retry",
        &config,
        &request,
        &mut turn_state,
        &mut abort,
        &mut on_update,
    ) {
        Ok(_) => panic!("retryable WS failure should surface after budget exhaustion"),
        Err(error) => error,
    };

    assert!(
        error.retry_after().is_some(),
        "fake disconnect should remain a retryable WS error"
    );
    assert_eq!(
        counts
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "retry exhaustion must not fall back to HTTP/SSE POST"
    );
}
