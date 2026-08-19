use std::sync::{Arc, atomic as path_std_sync_atomic};
use std::{io as path_std_io, net as path_std_net, sync as path_std_sync, time as path_std_time};

use super::*;

/// Ensures the enabled real Codex response producer submits typed response
/// metadata through the shared compressed-capture boundary.
#[test]
fn debug_response_producer_submits_typed_compressed_capture_job() {
    let response = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: tau_proto::AgentPromptId::parse("prompt-test").expect("prompt id"),
        agent_id: tau_proto::AgentId::parse("agent-test").expect("agent id"),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        output_items: Vec::new(),
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::Responses,
            base_url: "https://example.invalid".to_owned(),
            transport: tau_proto::ProviderBackendTransport::Websocket,
            stale_chain_fallback: false,
        }),
        provider_attempt: Default::default(),
        provider_response_id: Some("response-test".to_owned()),
        ws_pool_delta: None,
    };
    let mut submitted = None;

    let session_id = tau_proto::SessionId::parse("session-test").expect("session id");
    submit_response_debug_with(&session_id, true, &response, None, |capture| {
        submitted = Some(capture);
    });

    let capture = submitted.expect("capture submitted");
    assert_eq!(capture.session_id().as_str(), "session-test");
    assert_eq!(capture.agent_prompt_id(), &response.agent_prompt_id);
    assert_eq!(
        capture.class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::WebsocketResponse
    );
    let metadata: serde_json::Value = serde_json::from_slice(capture.json()).expect("capture JSON");
    assert_eq!(metadata["provider_response_id"], "response-test");
    assert_eq!(
        metadata["provider_response_finished"]["agent_prompt_id"],
        "prompt-test"
    );
}

/// Published ChatGPT models carry public ordinary/output comparison prices but
/// leave private-route cache billing to the non-authoritative central fallback.
#[test]
fn chatgpt_models_publish_basic_non_cache_prices() {
    let models = models_for_provider(&ProviderName::new("chatgpt"));
    let sol = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.6-sol")
        .expect("sol model")
        .estimated_api_cost_rates();
    let terra = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.6-terra")
        .expect("terra model")
        .estimated_api_cost_rates();
    let luna = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.6-luna")
        .expect("luna model")
        .estimated_api_cost_rates();
    let gpt_5_4 = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.4")
        .expect("gpt-5.4 model")
        .estimated_api_cost_rates();
    let mini = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.4-mini")
        .expect("mini model")
        .estimated_api_cost_rates();

    assert_eq!(sol.uncached_input.as_micro_usd(), 5_000_000);
    assert_eq!(sol.cached_input.as_micro_usd(), 500_000);
    assert_eq!(sol.output.as_micro_usd(), 30_000_000);
    assert_eq!(terra.uncached_input.as_micro_usd(), 2_000_000);
    assert_eq!(
        terra.cached_input,
        tau_proto::ESTIMATED_API_COST_FALLBACK.cached_input
    );
    assert_eq!(terra.output.as_micro_usd(), 12_000_000);
    assert_eq!(luna.uncached_input.as_micro_usd(), 200_000);
    assert_eq!(
        luna.cached_input,
        tau_proto::ESTIMATED_API_COST_FALLBACK.cached_input
    );
    assert_eq!(luna.output.as_micro_usd(), 1_200_000);
    assert_eq!(gpt_5_4.uncached_input.as_micro_usd(), 2_500_000);
    assert_eq!(
        gpt_5_4.cached_input,
        tau_proto::ESTIMATED_API_COST_FALLBACK.cached_input
    );
    assert_eq!(gpt_5_4.output.as_micro_usd(), 15_000_000);
    assert_eq!(mini.uncached_input.as_micro_usd(), 750_000);
    assert_eq!(
        mini.cached_input,
        tau_proto::ESTIMATED_API_COST_FALLBACK.cached_input
    );
    assert_eq!(mini.output.as_micro_usd(), 4_500_000);
}

/// Proves the private route publishes only its conservative response-chain
/// capability and leaves undocumented TTL, quota, and privacy facts unknown.
#[test]
fn chatgpt_models_publish_conservative_runtime_cache_contract() {
    let model = models_for_provider(&ProviderName::new("chatgpt"))
        .into_iter()
        .next()
        .expect("published ChatGPT model");
    let policy = model.cache_policy.expect("private cache policy");

    assert_eq!(policy.kind, tau_proto::ProviderCacheKind::ResponseChain);
    assert_eq!(policy.ttl, tau_proto::ProviderCacheTtl::Unknown);
    assert_eq!(policy.renewal, tau_proto::ProviderCacheRenewal::Recreate);
    assert_eq!(
        policy.output_floor,
        tau_proto::ProviderCacheOutputFloor::Zero
    );
    assert_eq!(
        policy.quota.requests,
        tau_proto::ProviderCacheQuotaCharge::Unknown
    );
    assert_eq!(
        policy.quota.read_tokens,
        tau_proto::ProviderCacheQuotaCharge::Unknown
    );
    assert_eq!(
        policy.quota.write_tokens,
        tau_proto::ProviderCacheQuotaCharge::Unknown
    );
    assert_eq!(
        policy.quota.output_tokens,
        tau_proto::ProviderCacheQuotaCharge::Unknown
    );
    assert_eq!(
        policy.privacy.storage,
        tau_proto::ProviderCacheStorageMode::Unknown
    );
    assert_eq!(
        policy.privacy.zero_data_retention,
        tau_proto::ProviderCacheZeroDataRetentionCompatibility::Unknown
    );
    assert_eq!(
        policy.privacy.data_residency,
        tau_proto::ProviderCacheDataResidencyEffect::Unknown
    );
    assert_eq!(
        policy.privacy.manual_deletion,
        tau_proto::ProviderCacheDeletionAvailability::Unavailable
    );
    assert_eq!(model.est_cached_input_cost_1m_usd, None);
    assert_eq!(model.est_cache_write_input_cost_1m_usd, None);
    assert_eq!(model.est_cache_storage_cost_1m_token_hour_usd, None);
}

#[derive(Default)]
struct TransportCounts {
    ws_upgrade_requests: path_std_sync::atomic::AtomicUsize,
    http_post_requests: path_std_sync::atomic::AtomicUsize,
}

/// Bounded loopback peer used to prove WebSocket routing never falls back.
struct WsFailureServer {
    /// Provider base URL targeting the loopback listener.
    base_url: String,
    /// Captured transport attempt counts.
    counts: std::sync::Arc<TransportCounts>,
    /// Signals the blocking accept loop to stop.
    shutdown: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
    /// Listener address used to wake accept during teardown.
    addr: std::net::SocketAddr,
    /// Joined server worker.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WsFailureServer {
    /// Returns the synthetic provider base URL.
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Returns captured request counts.
    fn counts(&self) -> &TransportCounts {
        &self.counts
    }
}

impl Drop for WsFailureServer {
    fn drop(&mut self) {
        self.shutdown
            .store(true, path_std_sync_atomic::Ordering::SeqCst);
        let _ = path_std_net::TcpStream::connect(self.addr);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("join fake provider");
            }
        }
    }
}

#[derive(Clone, Copy)]
enum WsFailureMode {
    Upgrade426,
    CloseWithoutResponse,
    ContextWindowExceeded,
    SemanticThenClose,
}

fn spawn_ws_426_server() -> WsFailureServer {
    spawn_ws_failure_server(WsFailureMode::Upgrade426)
}

fn spawn_ws_disconnect_server() -> WsFailureServer {
    spawn_ws_failure_server(WsFailureMode::CloseWithoutResponse)
}

fn spawn_ws_context_error_server() -> WsFailureServer {
    spawn_ws_failure_server(WsFailureMode::ContextWindowExceeded)
}

fn spawn_ws_semantic_close_server() -> WsFailureServer {
    spawn_ws_failure_server(WsFailureMode::SemanticThenClose)
}

fn spawn_ws_failure_server(mode: WsFailureMode) -> WsFailureServer {
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let addr = listener.local_addr().expect("fake provider addr");
    let counts = path_std_sync::Arc::new(TransportCounts::default());
    let thread_counts = path_std_sync::Arc::clone(&counts);
    let shutdown = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let thread_shutdown = path_std_sync::Arc::clone(&shutdown);
    let worker = std::thread::spawn(move || {
        const MAX_REQUESTS: usize = 8;
        for request_index in 0..MAX_REQUESTS {
            let (mut stream, _) = listener.accept().expect("accept fake provider request");
            if thread_shutdown.load(path_std_sync_atomic::Ordering::SeqCst) {
                return;
            }
            let _ = stream.set_read_timeout(Some(path_std_time::Duration::from_secs(2)));
            if matches!(
                mode,
                WsFailureMode::ContextWindowExceeded | WsFailureMode::SemanticThenClose
            ) {
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    continue;
                };
                thread_counts
                    .ws_upgrade_requests
                    .fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
                let _ = socket.read();
                if matches!(mode, WsFailureMode::SemanticThenClose) {
                    let _ = socket.send(tungstenite::Message::Text(
                        serde_json::json!({
                            "type": "response.output_text.delta",
                            "delta": "tentative"
                        })
                        .to_string()
                        .into(),
                    ));
                    let _ = socket.close(None);
                    continue;
                }
                let _ = socket.send(tungstenite::Message::Text(
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_prewarm",
                            "usage": {
                                "input_tokens": 1,
                                "output_tokens": 0,
                                "input_tokens_details": { "cached_tokens": 0 }
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ));
                let _ = socket.read();
                let _ = socket.send(tungstenite::Message::Text(
                    serde_json::json!({
                        "type": "error",
                        "code": "context_length_exceeded",
                        "message": "maximum context reached"
                    })
                    .to_string()
                    .into(),
                ));
                continue;
            }
            let request = read_bounded_http_request_head(&mut stream);
            let request_text = std::str::from_utf8(&request).expect("ASCII HTTP request head");
            let request_line = request_text.lines().next().expect("HTTP request line");
            if request_line == "POST /backend-api/codex/responses HTTP/1.1" {
                thread_counts
                    .http_post_requests
                    .fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
                let response = concat!(
                    "HTTP/1.1 500 Internal Server Error\r\n",
                    "Content-Length: 0\r\n",
                    "Connection: close\r\n",
                    "\r\n"
                );
                path_std_io::Write::write_all(&mut stream, response.as_bytes())
                    .expect("reject unexpected HTTP fallback");
                continue;
            }
            let lower = request_text.to_ascii_lowercase();
            if request_line == "GET /backend-api/codex/responses HTTP/1.1"
                && lower.contains("\r\nupgrade: websocket\r\n")
                && lower.contains("\r\nconnection: upgrade\r\n")
            {
                thread_counts
                    .ws_upgrade_requests
                    .fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
            } else {
                panic!("unexpected fake-provider request: {request_line}");
            }
            match mode {
                WsFailureMode::Upgrade426 => {
                    let response = concat!(
                        "HTTP/1.1 426 Upgrade Required\r\n",
                        "Retry-After: 999999\r\n",
                        "Content-Length: 21\r\n",
                        "Connection: close\r\n",
                        "\r\n",
                        "upgrade unavailable\n"
                    );
                    let _ = path_std_io::Write::write_all(&mut stream, response.as_bytes());
                }
                WsFailureMode::CloseWithoutResponse => {}
                WsFailureMode::ContextWindowExceeded => unreachable!("handled above"),
                WsFailureMode::SemanticThenClose => unreachable!("handled above"),
            }
            assert!(
                request_index + 1 < MAX_REQUESTS,
                "fake provider request bound exhausted"
            );
        }
    });
    WsFailureServer {
        base_url: format!("http://{addr}/backend-api"),
        counts,
        shutdown,
        addr,
        worker: Some(worker),
    }
}

/// Reads one complete bounded HTTP request head from the loopback peer.
fn read_bounded_http_request_head(stream: &mut std::net::TcpStream) -> Vec<u8> {
    const MAX_HEAD_BYTES: usize = 16 * 1024;
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while head.len() < MAX_HEAD_BYTES {
        let read =
            path_std_io::Read::read(stream, &mut byte).expect("read complete HTTP request head");
        assert_ne!(read, 0, "HTTP request ended before complete head");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return head;
        }
    }
    panic!("HTTP request head exceeds test bound");
}

/// Reproduces the incident: an exact WebSocket context rejection under an
/// unlimited outer budget terminates after one upstream request without replay
/// or HTTP/SSE fallback.
#[test]
fn websocket_context_rejection_bypasses_unlimited_retry_budget() {
    let server = spawn_ws_context_error_server();
    let config = test_config(server.base_url());
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-ws-context")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-ws-context").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut abort = crate::NeverAbort;
    assert!(matches!(
        runtime.prewarm(
            &ResolvedConfig {
                inner: config.clone(),
            },
            session_id.as_str(),
            &request,
            &mut abort,
        ),
        PrewarmOutcome::Installed
    ));
    let mut abort = NeverAbort;
    let mut correlation = attempt_failure::AttemptCaptureCorrelation::new(LogicalAttempt::new(1));

    let error = match runtime.stream(
        "ap-ws-context",
        &config,
        &request,
        &mut correlation,
        &mut abort,
        &mut |_| {},
    ) {
        Ok(_) => panic!("context rejection must terminate"),
        Err(error) => error,
    };
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        server
            .counts()
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

fn test_config(base_url: String) -> responses::ResponsesConfig {
    responses::ResponsesConfig {
        profile_namespace: "chatgpt".to_owned(),
        mode: responses::ResponsesMode::Standard,
        base_url,
        api_key: "token".to_owned(),
        model_id: "gpt-5.3-codex".to_owned(),
        raw_context_window: DEFAULT_RAW_CONTEXT_WINDOW,
        account_id: Some("account".to_owned()),
        supports_reasoning_effort: false,
        supports_reasoning_summary: false,
        supports_verbosity: false,
        supports_phase: true,
        supports_encrypted_reasoning: false,
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

/// Ensures the provider publishes the complete hardcoded ChatGPT model list
/// with model-specific capabilities and effective context limits.
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
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| !model.supports_compaction)
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| {
                model.supports_standalone_compaction
                    && model.standalone_compaction_threshold == Some(334_800)
            })
    );
    assert!(
        models
            .iter()
            .filter(|model| !model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| model.supports_compaction)
    );
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
    assert_eq!(
        models
            .iter()
            .find(|model| model.id.model.as_str() == "gpt-5.6-sol")
            .expect("gpt-5.6-sol model")
            .context_window,
        GPT_5_6_RAW_CONTEXT_WINDOW * EFFECTIVE_CONTEXT_WINDOW_PERCENT / 100
    );
    assert_eq!(
        models
            .iter()
            .find(|model| model.id.model.as_str() == "gpt-5.5")
            .expect("gpt-5.5 model")
            .context_window,
        DEFAULT_RAW_CONTEXT_WINDOW * EFFECTIVE_CONTEXT_WINDOW_PERCENT / 100
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| model.efforts.contains(&Effort::Max))
    );
    assert!(
        models
            .iter()
            .filter(|model| !model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| !model.efforts.contains(&Effort::Max))
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| {
                model
                    .input_modalities
                    .contains(&tau_proto::InputModality::Image)
                    && model
                        .tool_result_modalities
                        .contains(&tau_proto::InputModality::Image)
            })
    );
    assert!(
        models
            .iter()
            .filter(|model| !model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| {
                !model
                    .input_modalities
                    .contains(&tau_proto::InputModality::Image)
                    && !model
                        .tool_result_modalities
                        .contains(&tau_proto::InputModality::Image)
            })
    );
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

    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.api_key, "token");
    assert_eq!(config.account_id.as_deref(), Some("account"));
    assert!(config.supports_compaction);
    assert!(config.supports_phase);
    assert!(config.supports_encrypted_reasoning);
}

/// Exact audited model IDs, rather than a name prefix, control GPT-5.6 image
/// and standalone-compaction capabilities.
#[test]
fn unaudited_gpt_5_6_suffix_does_not_gain_audited_route_capabilities() {
    let model = "gpt-5.6-experimental";
    let info = model_info(
        &ProviderName::new("chatgpt"),
        model,
        responses::ResponsesMode::Standard,
    );
    let config = config_for_model(&ModelName::new(model), "token".to_owned(), None);

    assert!(
        !info
            .input_modalities
            .contains(&tau_proto::InputModality::Image)
    );
    assert!(
        !info
            .tool_result_modalities
            .contains(&tau_proto::InputModality::Image)
    );
    assert!(config.supports_compaction);
}

/// Ensures request configuration retains each model's raw context window
/// independently of the effective window published to the harness.
#[test]
fn config_uses_model_specific_context_window() {
    let gpt_5_6 = config_for_model(&ModelName::new("gpt-5.6-terra"), "token".to_owned(), None);
    let gpt_5_5 = config_for_model(&ModelName::new("gpt-5.5"), "token".to_owned(), None);

    assert_eq!(gpt_5_6.raw_context_window, GPT_5_6_RAW_CONTEXT_WINDOW);
    assert_eq!(gpt_5_5.raw_context_window, DEFAULT_RAW_CONTEXT_WINDOW);
}

/// Ensures GPT-5.6 advertises standalone rather than inline compaction in every
/// mode while older models retain inline compaction.
#[test]
fn config_scopes_inline_compaction_away_from_gpt_5_6() {
    let gpt_5_6 = config_for_model(&ModelName::new("gpt-5.6-terra"), "token".to_owned(), None);
    let gpt_5_5 = config_for_model(&ModelName::new("gpt-5.5"), "token".to_owned(), None);

    assert!(!gpt_5_6.supports_compaction);
    assert!(gpt_5_5.supports_compaction);
}

/// The compatibility flag affects only the exact audited GPT-5.6 family and
/// never disables parallel calls or inline compaction for older models.
#[test]
fn lite_compatibility_is_scoped_to_audited_gpt_5_6_models() {
    let provider = ProviderName::new("chatgpt");
    let older = config_for_model_mode(
        &ModelName::new("gpt-5.5"),
        "token".to_owned(),
        None,
        responses::ResponsesMode::LiteCompatibility,
    );
    let lite_models =
        models_for_provider_mode(&provider, responses::ResponsesMode::LiteCompatibility);
    let lite_sol = lite_models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.6-sol")
        .expect("Lite Sol");
    let older_info = lite_models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-5.5")
        .expect("older model");

    assert_eq!(older.mode, responses::ResponsesMode::Standard);
    assert!(older.supports_compaction);
    assert!(!lite_sol.supports_parallel_tool_calls);
    assert!(older_info.supports_parallel_tool_calls);
}

/// A WebSocket capability rejection surfaces after one upgrade and never
/// retries the logical turn through HTTP/SSE.
#[test]
fn websocket_capability_error_does_not_fallback_to_http_sse() {
    let server = spawn_ws_426_server();
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id =
        tau_proto::SessionId::parse("session-ws-426").expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-ws-426").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut abort = NeverAbort;
    let outcome = runtime.run_attempt("ap-ws-426", &config, &request, &mut abort, &mut |_| {});
    let AttemptOutcome::Terminal { error, progress } = outcome else {
        panic!("WS 426 must be terminal");
    };
    assert_eq!(progress, SemanticProgress::None);
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
    assert_eq!(
        error.to_string(),
        "Codex requires WebSocket; Tau has no HTTP/SSE fallback"
    );
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        server
            .counts()
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "WS failure must not fall back to HTTP/SSE POST"
    );
}

/// Exhausting the per-turn WebSocket retry budget surfaces the transport
/// failure without disabling WebSocket or replaying over HTTP/SSE.
#[test]
fn retryable_websocket_exhaustion_does_not_fallback_to_http_sse() {
    let server = spawn_ws_disconnect_server();
    let config = test_config(server.base_url());
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-ws-retry")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-ws-retry").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut abort = NeverAbort;
    let mut dispatched = 0;
    let outcome = runtime.run_attempt(
        "ap-ws-retry",
        &ResolvedConfig { inner: config },
        &request,
        &mut abort,
        &mut |update| {
            if matches!(update, StreamUpdate::Dispatched(_)) {
                dispatched += 1;
            }
        },
    );
    let AttemptOutcome::Retry {
        decision, progress, ..
    } = outcome
    else {
        panic!("retryable WS failure should surface after budget exhaustion");
    };
    assert_eq!(
        decision.class,
        tau_provider::retry_policy::RetryClass::Transport
    );
    assert_eq!(progress, SemanticProgress::None);
    assert_eq!(
        dispatched, 0,
        "a socket that dies before request enqueue has no dispatch origin"
    );
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "pre-dispatch connection failure returns to the outer scheduler"
    );
    assert_eq!(
        server
            .counts()
            .http_post_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "retry exhaustion must not fall back to HTTP/SSE POST"
    );
}

/// Semantic output followed by a dead socket must surface as parsed retry
/// without spending the internal replay budget.
#[test]
fn run_attempt_does_not_replay_after_semantic_progress() {
    let server = spawn_ws_semantic_close_server();
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-semantic-close")
        .expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-semantic-close").expect("agent id");
    let context = tau_proto::PromptContext::default();
    let request = test_prompt_payload(&session_id, &agent_id, &context);
    let mut abort = NeverAbort;
    let mut dispatched = 0;
    let outcome = runtime.run_attempt(
        "ap-semantic-close",
        &config,
        &request,
        &mut abort,
        &mut |update| {
            if matches!(update, StreamUpdate::Dispatched(_)) {
                dispatched += 1;
            }
        },
    );
    let AttemptOutcome::Retry { progress, .. } = outcome else {
        panic!("semantic close must return scheduler-owned retry");
    };
    assert_eq!(progress, SemanticProgress::Parsed);
    assert_eq!(dispatched, 1);
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "semantic output prohibits a replacement connection"
    );
}
