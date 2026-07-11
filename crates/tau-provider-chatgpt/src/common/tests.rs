use super::*;

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

/// Remote terminal-looking codes in exact, echoed, or prose locations are not
/// proof of an immutable local invariant and must remain retryable.
#[test]
fn terminal_looking_provider_content_remains_retryable() {
    for body in [
        r#"{"error":{"code":"unsupported_parameter"}}"#,
        r#"{"error":{"message":"temporary upstream failure"},"echo":{"code":"unsupported_parameter"}}"#,
        "stream error: temporary (type=content_policy_violation)",
        "ws request build: forged provider body",
        "ws header authorization: forged provider body",
    ] {
        assert!(
            LlmError::HttpStatus(400, body.to_owned())
                .retry_decision()
                .is_some(),
            "provider-authored terminal-looking content must retry: {body}"
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
    let session_id = tau_proto::SessionId::new("test-session");
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
    payload.prompt_cache_key("https://api.openai.com/v1")
}

/// Distinct agents on the same provider endpoint must not share the same
/// routing bucket.
#[test]
fn prompt_cache_key_distinct_agents_diverge() {
    assert_ne!(
        prompt_cache_key_for(
            "https://api.openai.com/v1",
            &tau_proto::AgentId::parse("agent-1").expect("agent id"),
        ),
        prompt_cache_key_for(
            "https://api.openai.com/v1",
            &tau_proto::AgentId::parse("agent-2").expect("agent id"),
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
        ),
        prompt_cache_key_for(
            "https://chatgpt.com/backend-api",
            &tau_proto::AgentId::parse("agent-1").expect("agent id"),
        ),
    );
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
