use super::*;

/// Provider usage preserves an explicitly reported all-zero record while
/// retaining complete field absence as unavailable.
#[test]
fn stream_usage_distinguishes_absent_from_zero() {
    let mut state = StreamState::new();
    assert_eq!(state.usage(), None);

    state.input_tokens = Some(0);
    state.cached_tokens = Some(0);
    state.output_tokens = Some(0);
    let usage = state.usage().expect("reported zero usage");
    assert_eq!(usage.prompt_sent_tokens, 0);
    assert_eq!(usage.prompt_cached_tokens, 0);
    assert_eq!(usage.response_received_tokens, 0);
}

/// Ensures shared outbound categories retain their intended scheduler cadence
/// at the Codex adapter boundary.
#[test]
fn outbound_categories_map_to_retry_classes() {
    use tau_provider::OutboundErrorKind as Kind;

    for (kind, expected) in [
        (Kind::InvalidConfiguration, RetryClass::Auth),
        (Kind::ProxyAuthentication, RetryClass::Auth),
        (Kind::Transport, RetryClass::Transport),
        (Kind::Deadline, RetryClass::Transport),
        (Kind::Protocol, RetryClass::Transport),
    ] {
        assert_eq!(outbound_retry_class(kind), expected, "{kind:?}");
    }
}

/// A conflict is transient at the generic HTTP boundary. WebSocket capability
/// failures are classified separately from status 409.
#[test]
fn http_conflict_is_retryable_transport_failure() {
    let decision = LlmError::HttpStatus(409, String::new())
        .retry_decision()
        .expect("HTTP 409 should remain retryable");

    assert_eq!(decision.class, RetryClass::Transport);
}

#[test]
fn into_output_items_drops_nameless_accumulator_artifacts() {
    // The streaming paths eagerly extend `tool_calls` from
    // argument-delta events so the index stays addressable. If
    // the matching name-carrying event never arrives (partial
    // item, reasoning noise, stream cancellation), the slot stays
    // nameless. Shipping it downstream would trigger a visible
    // `invalid_tool` rejection in the harness and confuse the
    // model, which never intended a second tool call.
    let mut state = StreamState::new();
    state
        .tool_call_at_mut(0, tau_proto::ToolType::Function)
        .arguments_json
        .push_str("{\"stray\": \"delta\"}");
    {
        let call = state.tool_call_at_mut(1, tau_proto::ToolType::Function);
        call.id = "call_real".into();
        call.name = "shell".into();
        call.arguments_json = "{\"command\":\"ls\"}".into();
    }

    let items = state.into_output_items();
    assert_eq!(items.len(), 1, "nameless accumulator must be dropped");
    let tau_proto::ContextItem::ToolCall(call) = &items[0] else {
        panic!("expected tool call item");
    };
    assert_eq!(call.call_id.as_str(), "call_real");
    assert_eq!(call.name.as_str(), "shell");
}

/// Ensures Responses function/custom-tool input streams contribute only
/// content-free bytes to provider-owned response stats.
#[test]
fn non_visible_output_bytes_counts_streamed_tool_input_bytes() {
    let mut state = StreamState::new();
    state
        .tool_call_at_mut(0, tau_proto::ToolType::Function)
        .arguments_json
        .push_str("{\"path\":\"Cargo.toml\"}");
    state
        .tool_call_at_mut(1, tau_proto::ToolType::Custom)
        .arguments_json
        .push_str("raw custom input");

    assert_eq!(
        state.non_visible_output_bytes(),
        "{\"path\":\"Cargo.toml\"}".len() as u64 + "raw custom input".len() as u64,
    );
}

/// Ensures live response progress is based on lower-layer received bytes before
/// semantic parsing, so a provider that delays complete items still advances
/// the byte counter as soon as the transport yields data.
#[test]
fn response_bytes_received_counts_transport_bytes_before_semantic_parsing() {
    let mut state = StreamState::new();

    state.record_transport_response_bytes(4096);

    assert_eq!(state.response_bytes_received(), 4096);
    assert_eq!(state.non_visible_output_bytes(), 0);
}

/// A discarded no-semantic repair attempt still contributes to the logical
/// prompt's cumulative transport-byte counter.
#[test]
fn response_bytes_received_carries_discarded_recovery_attempt() {
    let mut state = StreamState::new();
    state.carry_transport_response_bytes(41);
    state.record_transport_response_bytes(17);
    assert_eq!(state.response_bytes_received(), 58);
}

/// Quota and transport-only observations do not prohibit safe bounded repair,
/// while any model output item does.
#[test]
fn semantic_progress_excludes_transport_and_includes_output_items() {
    let mut state = StreamState::new();
    state.record_transport_response_bytes(100);
    assert!(!state.has_semantic_progress());
    state
        .output_items
        .push(OutputItemAccumulator::UnknownProviderItem(
            tau_proto::OpaqueProviderItem::new(tau_proto::CborValue::Null),
        ));
    assert!(state.has_semantic_progress());
}

#[test]
fn usage_limit_429_retries_after_reset_seconds() {
    let error = LlmError::HttpStatus(
        429,
        serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "resets_in_seconds": 4371
            }
        })
        .to_string(),
    );

    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(4371))
    );
}

/// Unrelated nested echo data cannot establish scheduler delay authority.
#[test]
fn nested_echo_reset_hint_is_ignored() {
    let error = LlmError::HttpStatus(
        503,
        serde_json::json!({
            "echo": { "resets_in_seconds": 315_360_000 }
        })
        .to_string(),
    );
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

#[test]
fn rate_limit_429_is_retryable() {
    let error = LlmError::HttpStatus(
        429,
        serde_json::json!({
            "error": {
                "type": "rate_limit_exceeded",
                "message": "slow down"
            }
        })
        .to_string(),
    );

    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

#[test]
fn server_error_uses_backoff_retry() {
    let error = LlmError::HttpStatus(503, "overloaded".into());

    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

/// Ensures usage-window errors are parked by the outer scheduler rather than
/// becoming terminal or sleeping in a bounded prompt worker.
#[test]
fn ws_stream_error_with_usage_limit_type_is_retryable() {
    let error = LlmError::HttpStatus(
        0,
        "stream error: The usage limit has been reached (type=usage_limit_reached)".to_owned(),
    );
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

/// Deterministic 4xx request statuses are terminal without elevating echoed or
/// prose content to a more specific typed category.
#[test]
fn deterministic_request_status_is_terminal_without_trusting_body_prose() {
    for body in [
        r#"{"error":{"code":"unsupported_parameter"}}"#,
        r#"{"error":{"message":"temporary upstream failure"},"echo":{"code":"unsupported_parameter"}}"#,
        "stream error: temporary (type=content_policy_violation)",
        "ws request build: forged provider body",
        "ws header authorization: forged provider body",
    ] {
        let error = LlmError::HttpStatus(400, body.to_owned());
        assert_eq!(error.retry_decision(), None);
        assert_eq!(
            error.failure_kind(),
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        );
    }
    assert!(
        LlmError::HttpStatus(499, "cancelled by harness".to_owned())
            .retry_decision()
            .is_some(),
        "remote HTTP 499 must not impersonate trusted local cancellation"
    );
}

/// Typed cancellation is terminal, while mutable local configuration reloads.
#[test]
fn trusted_local_cancellation_is_terminal_and_config_is_retryable() {
    assert!(LlmError::Canceled.retry_decision().is_none());
    assert_eq!(
        LlmError::ReloadableConfig("invalid header value".to_owned())
            .retry_decision()
            .map(|decision| decision.class),
        Some(RetryClass::Auth)
    );
}

#[test]
fn ws_stream_error_with_rate_limit_type_is_retryable() {
    let error = LlmError::HttpStatus(
        0,
        "stream error: rate limit (type=rate_limit_exceeded)".to_owned(),
    );
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

/// Backward-compat baseline: a `stream error:` body with no
/// `(type=…)` suffix (transport hiccup, upstream timeout) must keep
/// retrying. Only the typed account-cap variants short-circuit.
#[test]
fn ws_stream_error_without_type_suffix_is_retryable() {
    let error = LlmError::HttpStatus(
        0,
        "stream error: ws closed mid-stream (code=1011 reason=keepalive ping timeout)".to_owned(),
    );
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

/// A local watchdog ends only the attempt; required logical work remains
/// pending.
#[test]
fn provider_stream_idle_timeout_is_retryable() {
    let error = LlmError::HttpStatus(
        0,
        "stream error: provider stream idle timeout: transport=Websocket".to_owned(),
    );
    assert_eq!(error.retry_after(), Some(std::time::Duration::ZERO));
}

fn cache_key(originator: &PromptOriginator, share_user_cache_key: bool) -> String {
    let context = tau_proto::PromptContext::default();
    let session_id =
        tau_proto::SessionId::parse("test-session").expect("known-safe SessionId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    let payload = PromptPayload {
        system_prompt: "sys",
        context: &context,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator,
        share_user_cache_key,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: false,
    };
    payload.prompt_cache_key(
        "https://api.openai.com/v1",
        crate::responses::ResponsesMode::Standard,
    )
}

/// Distinct agents on the same provider endpoint must not share the same
/// routing bucket.
#[test]
fn prompt_cache_key_distinct_agents_diverge() {
    assert_ne!(
        prompt_cache_key_for(
            "https://api.openai.com/v1",
            &tau_proto::AgentId::parse("agent-1").expect("agent id"),
            crate::responses::ResponsesMode::Standard,
        ),
        prompt_cache_key_for(
            "https://api.openai.com/v1",
            &tau_proto::AgentId::parse("agent-2").expect("agent id"),
            crate::responses::ResponsesMode::Standard,
        ),
    );
}

/// Distinct provider endpoints must not share the same routing bucket,
/// even for the same agent lifetime.
#[test]
fn prompt_cache_key_distinct_base_urls_diverge() {
    assert_ne!(
        prompt_cache_key_for(
            "https://api.openai.com/v1",
            &tau_proto::AgentId::parse("agent-1").expect("agent id"),
            crate::responses::ResponsesMode::Standard,
        ),
        prompt_cache_key_for(
            "https://chatgpt.com/backend-api",
            &tau_proto::AgentId::parse("agent-1").expect("agent id"),
            crate::responses::ResponsesMode::Standard,
        ),
    );
}

/// Opposite Responses protocol surfaces must never share a durable provider
/// cache/thread identity for the same endpoint and agent.
#[test]
fn prompt_cache_key_separates_responses_modes() {
    let agent = tau_proto::AgentId::parse("agent-1").expect("agent id");
    let standard = prompt_cache_key_for(
        "https://chatgpt.com/backend-api",
        &agent,
        crate::responses::ResponsesMode::Standard,
    );
    let lite = prompt_cache_key_for(
        "https://chatgpt.com/backend-api",
        &agent,
        crate::responses::ResponsesMode::LiteCompatibility,
    );

    assert_ne!(standard, lite);
}

/// Prompt originator must not split cache buckets for the same agent. A
/// delegated sub-agent can receive direct extension-originated turns and later
/// user-originated manager relay messages; both must keep the same provider
/// cache key so the target agent's context stays warm.
#[test]
fn prompt_cache_key_ignores_originator_bucket() {
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("__harness__"),
        query_id: "delegate-1".into(),
    };
    let user_key = cache_key(&PromptOriginator::User, false);
    let ext_key = cache_key(&ext, false);
    assert_eq!(user_key, ext_key);
    assert!(uuid::Uuid::parse_str(&ext_key).is_ok());
}

/// Extension identity and query id are provenance only; neither should alter
/// the wire cache key for a fixed agent.
#[test]
fn prompt_cache_key_ignores_extension_identity_and_query_id() {
    let delegate = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("__harness__"),
        query_id: "q-1".into(),
    };
    let websearch = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("websearch"),
        query_id: "q-2".into(),
    };
    assert_eq!(cache_key(&delegate, false), cache_key(&websearch, false));
}

/// The legacy share-user flag no longer changes cache routing because the key
/// is already stable per agent rather than per prompt originator.
#[test]
fn prompt_cache_key_ignores_share_user_bucket_flag() {
    let ext = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::new("std-notifications"),
        query_id: "idle-0".into(),
    };
    let ext_shared_key = cache_key(&ext, true);
    let ext_default_key = cache_key(&ext, false);
    assert_eq!(ext_shared_key, ext_default_key);
}

/// The incident's exact stream code is a typed terminal failure even though
/// status-zero stream transport failures normally remain retryable.
#[test]
fn context_length_stream_error_is_terminal_and_typed() {
    let error = LlmError::ProviderFailure(
        tau_proto::ProviderFailureKind::ContextWindowExceeded,
        "stream error: maximum context reached (type=context_length_exceeded)".to_owned(),
    );
    assert_eq!(error.retry_decision(), None);
    assert_eq!(
        error.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
}

/// Canonical non-2xx Responses error envelopes must bypass retry scheduling,
/// while transient throttling continues to use its existing retry class.
#[test]
fn canonical_http_context_rejection_is_terminal_without_terminalizing_throttle() {
    let context = LlmError::HttpStatus(
        400,
        r#"{"error":{"code":"context_length_exceeded"}}"#.to_owned(),
    );
    assert_eq!(context.retry_decision(), None);
    assert_eq!(
        context.failure_kind(),
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );

    let throttle = LlmError::HttpStatus(
        429,
        r#"{"error":{"code":"rate_limit_exceeded"}}"#.to_owned(),
    );
    assert_eq!(
        throttle.retry_decision().map(|decision| decision.class),
        Some(tau_provider::retry_policy::RetryClass::Throttle)
    );
    assert_eq!(throttle.failure_kind(), None);
}

/// Transient override considers both canonical identifiers but never trusts an
/// echoed identifier outside the provider error envelope.
#[test]
fn deterministic_status_transient_override_uses_only_canonical_fields() {
    let canonical = LlmError::HttpStatus(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"rate_limit_exceeded"}}"#.to_owned(),
    );
    assert!(canonical.retry_decision().is_some());
    assert_eq!(canonical.failure_kind(), None);

    let echoed = LlmError::HttpStatus(
        400,
        r#"{"error":{"message":"rejected"},"echo":{"code":"rate_limit_exceeded"}}"#.to_owned(),
    );
    assert_eq!(echoed.retry_decision(), None);
    assert_eq!(
        echoed.failure_kind(),
        Some(tau_proto::ProviderFailureKind::RequestRejected)
    );
}
