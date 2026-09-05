use std::sync::{Arc, atomic as path_std_sync_atomic, mpsc as path_std_sync_mpsc};
use std::{io as path_std_io, net as path_std_net, sync as path_std_sync, time as path_std_time};

use super::*;

/// In-memory trace writer for runtime-level production assertions.
#[derive(Clone, Default)]
struct TraceWriter(path_std_sync::Arc<path_std_sync::Mutex<Vec<u8>>>);

impl path_std_io::Write for TraceWriter {
    /// Append formatted trace bytes.
    fn write(&mut self, bytes: &[u8]) -> path_std_io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// The in-memory sink has no external buffer.
    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Retry identity retains only an account digest: bearer rotation is invisible,
/// raw account content is absent, and missing identity stays fail-closed.
#[test]
fn chatgpt_retry_identity_is_closed_and_credential_free() {
    let provider = ProviderName::new("chatgpt");
    let model = ModelName::new("gpt-5.6-sol");
    let resolved = |bearer: &str, account_id: Option<&str>| {
        resolved_config_for_provider_model(
            &provider,
            &model,
            ResolvedCredentials::new(bearer.to_owned(), account_id.map(str::to_owned)),
            CodexMode::Standard,
        )
    };
    let account_canary = "private-account-canary";
    let identity =
        resolved("private-bearer-canary-a", Some(account_canary)).chatgpt_retry_identity();
    let rotated =
        resolved("private-bearer-canary-b", Some(account_canary)).chatgpt_retry_identity();
    assert!(
        identity == rotated,
        "bearer rotation preserves retry account"
    );
    assert!(
        !identity
            .account_digest
            .expect("non-empty account produces digest")
            .as_bytes()
            .windows(account_canary.len())
            .any(|window| window == account_canary.as_bytes()),
        "closed proof must not retain raw account content"
    );
    for missing_account in [None, Some(""), Some(" \t")] {
        let missing = resolved("private-bearer-canary-c", missing_account).chatgpt_retry_identity();
        assert!(missing.account_digest.is_none());
        assert!(
            !resolved("new-bearer", Some(account_canary)).matches_chatgpt_retry_identity(&missing)
        );
    }
}

/// Test cancellation source that reports when admission has entered its
/// mutex-protected pre-state check.
struct AdmissionEntered(path_std_sync_mpsc::Sender<()>);

impl TurnAbort for AdmissionEntered {
    fn is_aborted(&mut self) -> bool {
        self.0.send(()).expect("admission observer");
        false
    }

    fn register_waker(
        &mut self,
        _waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(NeverAbortWaker)
    }
}

/// A fresh runtime must not create capability evidence or contact the provider
/// until compaction explicitly asks for admission.
#[test]
fn compact_admission_does_not_probe_proactively() {
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));

    assert!(
        runtime
            .compact_routes
            .states
            .lock()
            .expect("compact admission lock")
            .is_empty()
    );
}

/// Negative evidence remains authoritative for an unchanged credential/account
/// identity and admits no new probe.
#[test]
fn compact_admission_retains_unchanged_identity_rejection() {
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let identity = InferenceProfileIdentity(11);
    assert!(runtime.mark_compact_route_unavailable(identity));

    assert!(matches!(
        runtime.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Unavailable
    ));
}

/// Rotating credential/account identity invalidates only the stale generation:
/// the old identity stays negative while the new identity owns one fresh probe.
#[test]
fn compact_admission_changed_identity_gets_one_fresh_probe() {
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let stale = InferenceProfileIdentity(21);
    let fresh = InferenceProfileIdentity(22);
    assert!(runtime.mark_compact_route_unavailable(stale));

    let CompactAdmissionResult::Probe(probe) =
        runtime.acquire_compact_probe(fresh, &mut NeverAbort)
    else {
        panic!("fresh identity must own one probe");
    };
    assert!(matches!(
        runtime.acquire_compact_probe(stale, &mut NeverAbort),
        CompactAdmissionResult::Unavailable
    ));
    probe.complete(CompactRouteState::Available);
}

/// Retiring a superseded credential generation removes only its negative
/// capability observation, bounding live runtime state without disturbing
/// another generation's admission result.
#[test]
fn compact_admission_retires_superseded_negative_identity() {
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let stale = InferenceProfileIdentity(23);
    let current = InferenceProfileIdentity(24);
    assert!(runtime.mark_compact_route_unavailable(stale));
    assert!(runtime.mark_compact_route_unavailable(current));

    runtime.retire_compact_identity(stale);

    assert!(matches!(
        runtime.acquire_compact_probe(stale, &mut NeverAbort),
        CompactAdmissionResult::Probe(_)
    ));
    assert!(matches!(
        runtime.acquire_compact_probe(current, &mut NeverAbort),
        CompactAdmissionResult::Unavailable
    ));
}

/// A waiter bound to a retired probe cannot adopt a replacement generation
/// that installs and completes before the waiter reacquires the admission lock.
#[test]
fn compact_admission_waiter_rejects_replacement_probe_generation() {
    let runtime = Arc::new(CodexRuntime::new(Arc::new(crate::test_network_policy())));
    let identity = InferenceProfileIdentity(26);
    let CompactAdmissionResult::Probe(stale_probe) =
        runtime.acquire_compact_probe(identity, &mut NeverAbort)
    else {
        panic!("first probe");
    };
    let waiter_runtime = Arc::clone(&runtime);
    let (entered_tx, entered_rx) = path_std_sync_mpsc::channel();
    let waiter = std::thread::spawn(move || {
        matches!(
            waiter_runtime.acquire_compact_probe(identity, &mut AdmissionEntered(entered_tx)),
            CompactAdmissionResult::Unavailable
        )
    });
    entered_rx.recv().expect("waiter entered compact admission");

    let replacement_generation = stale_probe.generation.saturating_add(1);
    {
        let mut routes = runtime
            .compact_routes
            .states
            .lock()
            .expect("compact admission lock");
        assert_eq!(
            routes.remove(&identity),
            Some(CompactRouteObservation {
                generation: stale_probe.generation,
                state: CompactRouteState::Probing,
            })
        );
        routes.insert(
            identity,
            CompactRouteObservation {
                generation: replacement_generation,
                state: CompactRouteState::Available,
            },
        );
        runtime.compact_routes.changed.notify_all();
    }

    assert!(
        waiter.join().expect("retired probe waiter"),
        "a waiter for retired work must not adopt a replacement result"
    );
    assert!(!stale_probe.complete(CompactRouteState::Unavailable));
    assert!(matches!(
        runtime.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Admitted
    ));
}

/// A late unavailable result created before main-loop retirement cannot remain
/// after the stale worker message observes that the identity is superseded.
#[test]
fn compact_admission_cleans_late_negative_after_retirement() {
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let identity = InferenceProfileIdentity(27);
    assert!(runtime.mark_compact_route_unavailable(identity));
    runtime.retire_compact_identity(identity);

    assert!(
        runtime.mark_compact_route_unavailable(identity),
        "simulate a late worker result installed before main-loop arbitration"
    );
    runtime.retire_compact_identity(identity);

    assert!(matches!(
        runtime.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Probe(_)
    ));
}

/// Negative capability observations are runtime-only: a cold runtime after
/// restart admits one fresh probe instead of restoring a prior generation.
#[test]
fn compact_admission_restart_drops_negative_identity() {
    let identity = InferenceProfileIdentity(25);
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    assert!(runtime.mark_compact_route_unavailable(identity));
    drop(runtime);

    let restarted = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    assert!(matches!(
        restarted.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Probe(_)
    ));
}

/// Concurrent requests for one fresh identity wait behind the first probe and
/// all become admitted after its successful capability result.
#[test]
fn compact_admission_coalesces_successful_concurrent_requests() {
    let runtime = Arc::new(CodexRuntime::new(Arc::new(crate::test_network_policy())));
    let identity = InferenceProfileIdentity(31);
    let CompactAdmissionResult::Probe(probe) =
        runtime.acquire_compact_probe(identity, &mut NeverAbort)
    else {
        panic!("first request must own the probe");
    };
    let waiter_runtime = Arc::clone(&runtime);
    let (entered_tx, entered_rx) = path_std_sync_mpsc::channel();
    let waiter = std::thread::spawn(move || {
        matches!(
            waiter_runtime.acquire_compact_probe(identity, &mut AdmissionEntered(entered_tx)),
            CompactAdmissionResult::Admitted
        )
    });

    entered_rx.recv().expect("waiter entered compact admission");
    probe.complete(CompactRouteState::Available);

    assert!(waiter.join().expect("compact waiter"));
    assert!(matches!(
        runtime.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Admitted
    ));
}

/// A compaction-specific rejection publishes one negative result to every
/// waiter, so no waiter can start a redundant probe for that generation.
#[test]
fn compact_admission_coalesces_rejection_for_all_waiters() {
    let runtime = Arc::new(CodexRuntime::new(Arc::new(crate::test_network_policy())));
    let identity = InferenceProfileIdentity(41);
    let CompactAdmissionResult::Probe(probe) =
        runtime.acquire_compact_probe(identity, &mut NeverAbort)
    else {
        panic!("first request must own the probe");
    };
    let waiter_runtime = Arc::clone(&runtime);
    let (entered_tx, entered_rx) = path_std_sync_mpsc::channel();
    let waiter = std::thread::spawn(move || {
        matches!(
            waiter_runtime.acquire_compact_probe(identity, &mut AdmissionEntered(entered_tx)),
            CompactAdmissionResult::Unavailable
        )
    });

    entered_rx.recv().expect("waiter entered compact admission");
    probe.complete(CompactRouteState::Unavailable);

    assert!(waiter.join().expect("compact waiter"));
    assert!(matches!(
        runtime.acquire_compact_probe(identity, &mut NeverAbort),
        CompactAdmissionResult::Unavailable
    ));
}

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
        compaction_output_tokens: None,
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
struct WsLoopbackServer {
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

impl WsLoopbackServer {
    /// Returns the synthetic provider base URL.
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Returns captured request counts.
    fn counts(&self) -> &TransportCounts {
        &self.counts
    }
}

impl Drop for WsLoopbackServer {
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
enum LoopbackResponseMode {
    Upgrade426,
    CloseWithoutResponse,
    ContextWindowExceeded,
    DirectContextWindowExceeded,
    SemanticThenClose,
    CompactErrorWithEmbeddedItem,
    CompactItemThenClose,
    CompactExactSuccess,
    CompactItemThenCloseThenSuccess,
}

fn spawn_ws_426_server() -> WsLoopbackServer {
    spawn_loopback_server(LoopbackResponseMode::Upgrade426)
}

fn spawn_ws_disconnect_server() -> WsLoopbackServer {
    spawn_loopback_server(LoopbackResponseMode::CloseWithoutResponse)
}

fn spawn_ws_context_error_server() -> WsLoopbackServer {
    spawn_loopback_server(LoopbackResponseMode::ContextWindowExceeded)
}

fn spawn_direct_ws_context_error_server() -> WsLoopbackServer {
    spawn_loopback_server(LoopbackResponseMode::DirectContextWindowExceeded)
}

fn spawn_ws_semantic_close_server() -> WsLoopbackServer {
    spawn_loopback_server(LoopbackResponseMode::SemanticThenClose)
}

fn spawn_loopback_server(mode: LoopbackResponseMode) -> WsLoopbackServer {
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
                LoopbackResponseMode::ContextWindowExceeded
                    | LoopbackResponseMode::DirectContextWindowExceeded
                    | LoopbackResponseMode::SemanticThenClose
                    | LoopbackResponseMode::CompactErrorWithEmbeddedItem
                    | LoopbackResponseMode::CompactItemThenClose
                    | LoopbackResponseMode::CompactExactSuccess
                    | LoopbackResponseMode::CompactItemThenCloseThenSuccess
            ) {
                let Ok(mut socket) = tungstenite::accept(stream) else {
                    continue;
                };
                thread_counts
                    .ws_upgrade_requests
                    .fetch_add(1, path_std_sync_atomic::Ordering::SeqCst);
                let _ = socket.read();
                if matches!(mode, LoopbackResponseMode::DirectContextWindowExceeded) {
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
                if matches!(mode, LoopbackResponseMode::SemanticThenClose) {
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
                if matches!(mode, LoopbackResponseMode::CompactErrorWithEmbeddedItem) {
                    let _ = socket.send(tungstenite::Message::Text(
                        serde_json::json!({
                            "type": "error",
                            "code": "overloaded_error",
                            "message": "busy",
                            "output_index": 0,
                            "item": {
                                "type": "compaction",
                                "id": "cmp_ignored",
                                "encrypted_content": "must-not-be-accepted"
                            }
                        })
                        .to_string()
                        .into(),
                    ));
                    continue;
                }
                if matches!(
                    mode,
                    LoopbackResponseMode::CompactItemThenClose
                        | LoopbackResponseMode::CompactItemThenCloseThenSuccess
                ) && !(matches!(mode, LoopbackResponseMode::CompactItemThenCloseThenSuccess)
                    && 0 < request_index)
                {
                    let _ = socket.send(tungstenite::Message::Text(
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {
                                "type": "compaction",
                                "id": "cmp_uncommitted",
                                "encrypted_content": "discard-me"
                            }
                        })
                        .to_string()
                        .into(),
                    ));
                    let _ = socket.close(None);
                    continue;
                }
                if matches!(
                    mode,
                    LoopbackResponseMode::CompactExactSuccess
                        | LoopbackResponseMode::CompactItemThenCloseThenSuccess
                ) {
                    let _ = socket.send(tungstenite::Message::Text(
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {
                                "type": "compaction",
                                "id": "cmp_committed",
                                "encrypted_content": "opaque"
                            }
                        })
                        .to_string()
                        .into(),
                    ));
                    let _ = socket.send(tungstenite::Message::Text(
                        serde_json::json!({
                            "type": "response.completed",
                            "response": {
                                "usage": {
                                    "input_tokens": 3,
                                    "output_tokens": 1
                                }
                            }
                        })
                        .to_string()
                        .into(),
                    ));
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
                LoopbackResponseMode::Upgrade426 => {
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
                LoopbackResponseMode::CloseWithoutResponse => {}
                LoopbackResponseMode::ContextWindowExceeded
                | LoopbackResponseMode::DirectContextWindowExceeded
                | LoopbackResponseMode::SemanticThenClose
                | LoopbackResponseMode::CompactErrorWithEmbeddedItem
                | LoopbackResponseMode::CompactItemThenClose
                | LoopbackResponseMode::CompactExactSuccess
                | LoopbackResponseMode::CompactItemThenCloseThenSuccess => {
                    unreachable!("handled above")
                }
            }
            assert!(
                request_index + 1 < MAX_REQUESTS,
                "fake provider request bound exhausted"
            );
        }
    });
    WsLoopbackServer {
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
        ResponseMode::Ordinary,
        &mut correlation,
        &mut abort,
        &mut |_| {},
        &mut None,
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

/// Builds a standalone prompt whose canonical historical prefix has exactly the
/// requested JSON byte length.
fn context_with_historical_prefix_bytes(target: tau_proto::ByteCount) -> tau_proto::PromptContext {
    let mut context = tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![
                    tau_proto::ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: String::new(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    }),
                    tau_proto::ContextItem::CompactionTrigger,
                ],
            },
        )],
    };
    let empty_bytes =
        tau_provider::local_summary_compaction::historical_prefix_json_bytes(&context)
            .expect("well-formed standalone prompt has a historical prefix");
    let padding = target
        .checked_sub(empty_bytes)
        .expect("target admits the fixed prompt envelope")
        .get();
    let tau_proto::ContextBlock::UserInput(block) = &mut context.blocks[0] else {
        unreachable!("fixture contains one user block")
    };
    let tau_proto::ContextItem::Message(message) = &mut block.items[0] else {
        unreachable!("fixture starts with one message")
    };
    let tau_proto::ContentPart::Text { text } = &mut message.content[0] else {
        unreachable!("fixture message contains text")
    };
    *text = "x".repeat(usize::try_from(padding).expect("fixture padding fits usize"));
    assert_eq!(
        tau_provider::local_summary_compaction::historical_prefix_json_bytes(&context),
        Some(target)
    );
    context
}

/// Builds the smallest valid standalone-compaction context for production-path
/// retry and completion tests.
fn compact_trigger_context() -> tau_proto::PromptContext {
    tau_proto::PromptContext {
        blocks: vec![tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: vec![tau_proto::ContextItem::CompactionTrigger],
            },
        )],
    }
}

/// Builds varied closed history before the final compaction trigger so the
/// production success path proves it copies no compacted-prefix item.
fn compact_mixed_prefix_context() -> tau_proto::PromptContext {
    let call_id = tau_proto::ToolCallId::from("call-before-compaction");
    tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![
                    tau_proto::ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: "ordinary user input".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    }),
                    tau_proto::ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::HarnessInternalText {
                            text: "<tau_internal>Watched agent worker is working</tau_internal>"
                                .to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    }),
                ],
            }),
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: call_id.clone(),
                    name: tau_proto::ToolName::new("status"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Map(Vec::new()),
                    raw_arguments_json: Some("{}".to_owned()),
                    responses_envelope: None,
                })],
                usage: None,
            }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id,
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
                    provider_content: Vec::new(),
                }],
            }),
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![tau_proto::ContextItem::CompactionTrigger],
            }),
        ],
    }
}

/// A retryable transport failure before any compact item remains eligible for
/// the outer logical-request scheduler after the one transparent repair.
#[test]
fn compact_pre_progress_failure_remains_retryable() {
    let server = spawn_ws_disconnect_server();
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-compact-pre-progress").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-pre-progress").expect("agent");
    let context = compact_trigger_context();
    let request = test_prompt_payload(&session_id, &agent_id, &context);

    let CompactOutcome::Retry {
        backend_reached, ..
    } = runtime.compact(
        "ap-compact-pre-progress",
        &config,
        &request,
        &mut NeverAbort,
    )
    else {
        panic!("pre-dispatch compact failure must remain retryable");
    };
    assert!(!backend_reached);
    assert!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1,
        "the retryable outcome follows an upgrade that failed before response.create dispatch"
    );
}

/// An error event wins before item-shaped fields in that same event can become
/// semantic progress, so the failed logical request remains retryable.
#[test]
fn compact_same_event_error_first_remains_retryable() {
    let server = spawn_loopback_server(LoopbackResponseMode::CompactErrorWithEmbeddedItem);
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-compact-error-first").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-error-first").expect("agent");
    let context = compact_trigger_context();
    let request = test_prompt_payload(&session_id, &agent_id, &context);

    let CompactOutcome::Retry {
        decision,
        backend_reached,
    } = runtime.compact("ap-compact-error-first", &config, &request, &mut NeverAbort)
    else {
        panic!("error-first event must remain retryable");
    };
    assert_eq!(
        decision.class,
        tau_provider::retry_policy::RetryClass::Overload,
        "the embedded overloaded frame, not a later socket close, owns retry"
    );
    assert!(backend_reached);
}

/// Once the parser accepts a canonical compact item, a later retryable
/// transport failure terminalizes this paid request instead of scheduling it
/// again.
#[test]
fn compact_post_progress_failure_is_terminal() {
    let server = spawn_loopback_server(LoopbackResponseMode::CompactItemThenClose);
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-compact-post-progress").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-post-progress").expect("agent");
    let context = compact_trigger_context();
    let request = test_prompt_payload(&session_id, &agent_id, &context);

    let CompactOutcome::Terminal {
        error,
        backend_reached: true,
    } = runtime.compact(
        "ap-compact-post-progress",
        &config,
        &request,
        &mut NeverAbort,
    )
    else {
        panic!("post-progress transport failure must terminalize");
    };
    assert_eq!(
        error
            .retry_decision()
            .expect("underlying failure remains retry-classified")
            .class,
        tau_provider::retry_policy::RetryClass::Transport
    );
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "semantic progress prohibits both transparent repair and logical retry"
    );
}

/// Native compact success remains exactly one canonical compaction item
/// followed by `response.completed`.
#[test]
fn compact_exact_success_returns_one_item() {
    let server = spawn_loopback_server(LoopbackResponseMode::CompactExactSuccess);
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id = tau_proto::SessionId::parse("session-compact-exact-success").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-exact-success").expect("agent");
    let context = compact_mixed_prefix_context();
    let request = test_prompt_payload(&session_id, &agent_id, &context);

    let trace_output = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace_output = trace_output.clone();
            move || trace_output.clone()
        })
        .finish();
    let outcome = tracing::subscriber::with_default(subscriber, || {
        runtime.compact(
            "ap-compact-exact-success",
            &config,
            &request,
            &mut NeverAbort,
        )
    });
    let CompactOutcome::Finished {
        output_items,
        usage,
    } = outcome
    else {
        panic!("exact native compact response must finish");
    };
    assert!(matches!(
        output_items.as_slice(),
        [tau_proto::ContextItem::Compaction(_)]
    ));
    assert!(usage.is_some(), "provider usage survives exact success");
    let trace =
        String::from_utf8(trace_output.0.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert_eq!(
        trace.matches("provider backend stage observation").count(),
        1
    );
    assert!(trace.contains("outcome=\"completed\""), "{trace}");
    assert!(trace.contains("transport=\"websocket\""), "{trace}");
}

/// Terminalizing one post-progress failure does not poison explicit recovery:
/// a later user-owned request pays for and dispatches a distinct successful
/// compact operation.
#[test]
fn compact_explicit_new_request_dispatches_after_post_progress_failure() {
    let server = spawn_loopback_server(LoopbackResponseMode::CompactItemThenCloseThenSuccess);
    let config = ResolvedConfig {
        inner: test_config(server.base_url()),
    };
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let session_id =
        tau_proto::SessionId::parse("session-compact-explicit-retry").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-explicit-retry").expect("agent");
    let context = compact_trigger_context();
    let request = test_prompt_payload(&session_id, &agent_id, &context);

    assert!(matches!(
        runtime.compact(
            "ap-compact-explicit-first",
            &config,
            &request,
            &mut NeverAbort
        ),
        CompactOutcome::Terminal { .. }
    ));
    assert!(matches!(
        runtime.compact(
            "ap-compact-explicit-second",
            &config,
            &request,
            &mut NeverAbort
        ),
        CompactOutcome::Finished { .. }
    ));
    assert_eq!(
        server
            .counts()
            .ws_upgrade_requests
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "explicit recovery owns a distinct second paid request"
    );
}

/// Large fixed system material may make the actual wire frame numerically
/// larger than token-window metadata; only the provider owns token rejection.
#[test]
fn compact_large_fixed_material_reaches_canonical_context_rejection_once() {
    let server = spawn_direct_ws_context_error_server();
    let mut config = test_config(server.base_url());
    config.raw_context_window = tau_proto::TokenCount::new(1_000);
    let session_id =
        tau_proto::SessionId::parse("session-compact-provider-limit").expect("session id");
    let agent_id = tau_proto::AgentId::parse("agent-compact-provider-limit").expect("agent id");
    let context = context_with_historical_prefix_bytes(tau_proto::ByteCount::new(500));
    let system_prompt = "s".repeat(8_000);
    let mut request = test_prompt_payload(&session_id, &agent_id, &context);
    request.system_prompt = &system_prompt;
    assert!(
        responses::compact_ws_request_bytes(&config, &request)
            .expect("measure actual compact frame")
            > config.raw_context_window.get()
    );
    let runtime = CodexRuntime::new(Arc::new(crate::test_network_policy()));
    let mut abort = NeverAbort;

    let CompactOutcome::Terminal {
        error,
        backend_reached,
    } = runtime.compact(
        "ap-compact-provider-limit",
        &ResolvedConfig { inner: config },
        &request,
        &mut abort,
    )
    else {
        panic!("canonical context rejection must terminate compact dispatch")
    };
    assert!(backend_reached);
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
        profile_namespace: tau_proto::ProviderName::new("chatgpt"),
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
        hosted_tools: &[],
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
            "work-chatgpt/gpt-6-astra",
            "work-chatgpt/gpt-5.5",
            "work-chatgpt/gpt-5.4",
            "work-chatgpt/gpt-5.4-mini",
            "work-chatgpt/gpt-5.3-codex",
        ],
    );
    let astra = models
        .iter()
        .find(|model| model.id.model.as_str() == "gpt-6-astra")
        .expect("gpt-6-astra model");
    assert_eq!(astra.default_affinity, 0);
    assert_eq!(
        models
            .iter()
            .max_by_key(|model| model.default_affinity)
            .expect("published model")
            .id
            .model
            .as_str(),
        "gpt-5.6-sol"
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
                    && model.standalone_compaction_threshold
                        == Some(GPT_5_6_STANDALONE_COMPACTION_TOKEN_THRESHOLD)
                    && model.standalone_compaction_prefix_budget.is_none()
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
        GPT_5_6_RAW_CONTEXT_WINDOW
    );
    assert_eq!(
        models
            .iter()
            .find(|model| model.id.model.as_str() == "gpt-5.6-sol")
            .expect("gpt-5.6-sol model")
            .max_input_tokens,
        Some(effective_context_window_for_model("gpt-5.6-sol"))
    );
    assert_eq!(
        models
            .iter()
            .find(|model| model.id.model.as_str() == "gpt-5.5")
            .expect("gpt-5.5 model")
            .context_window,
        DEFAULT_RAW_CONTEXT_WINDOW
    );
    assert_eq!(
        models
            .iter()
            .find(|model| model.id.model.as_str() == "gpt-5.5")
            .expect("gpt-5.5 model")
            .max_input_tokens,
        Some(effective_context_window_for_model("gpt-5.5"))
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| model.efforts.contains(NativeReasoningEffort::Max))
    );
    assert!(
        models
            .iter()
            .filter(|model| !model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| !model.efforts.contains(NativeReasoningEffort::Max))
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

/// A configured non-default provider namespace must reach the private wire
/// configuration as the same validated identity, rather than a raw or default
/// profile string.
#[test]
fn resolved_provider_config_retains_typed_profile_namespace() {
    let provider = ProviderName::new("work-chatgpt");
    let config = resolved_config_for_provider_model(
        &provider,
        &ModelName::new("gpt-5.3-codex"),
        ResolvedCredentials::new("token".to_owned(), Some("account".to_owned())),
        CodexMode::Standard,
    );

    assert_eq!(config.inner.profile_namespace, provider);
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
    let AttemptOutcome::Terminal {
        error,
        progress,
        backend_reached,
    } = outcome
    else {
        panic!("WS 426 must be terminal");
    };
    assert!(!backend_reached);
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
