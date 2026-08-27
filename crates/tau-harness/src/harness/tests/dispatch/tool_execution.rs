//! Tests for tool execution behavior.

use super::super::super::{
    PendingTerminalObservation, byte_free_embedded_tool_result, record_embedded_tool_calls,
};
use super::*;

/// A tool result from any connection other than the routed provider must not
/// close the call; otherwise a stale extension can spoof completion and make
/// the real owner look like a duplicate.
#[test]
fn provider_owner_validation_rejects_wrong_tool_result() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-result-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolResultReported(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "spoofed output",
        )),
    )
    .expect("wrong result ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-result-call")
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("owner-result-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolResultReported(result) if result.call_id.as_str() == "owner-result-call"
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolResultReported(final_tool_result(
            "owner-result-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");

    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-result-call")
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("owner-result-call")
    );
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolResult(result)
                if result.call_id.as_str() == "owner-result-call"
                    && matches!(&result.result, CborValue::Text(text) if text == "real output")
        )
    ));

    h.shutdown().expect("shutdown");
}

/// A tool error from a non-owner is also ignored so it cannot fail the pending
/// call or remove routing state before the owner reports the real failure.
#[test]
fn provider_owner_validation_rejects_wrong_tool_error() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-error-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolErrorReported(tool_error(
            "owner-error-call",
            "owned_tool",
            "spoofed failure",
        )),
    )
    .expect("wrong error ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-error-call")
    );
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("owner-error-call")
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );
    assert!(event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolErrorReported(error) if error.call_id.as_str() == "owner-error-call"
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolErrorReported(tool_error("owner-error-call", "owned_tool", "real failure")),
    )
    .expect("owner error accepted");

    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-error-call")
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("owner-error-call")
    );
    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolError(error)
                if error.call_id.as_str() == "owner-error-call" && error.message == "real failure"
        )
    ));

    h.shutdown().expect("shutdown");
}

/// Progress is non-terminal, but it still must come from the routed owner so a
/// wrong extension cannot publish spoofed output into the visible tool block.
#[test]
fn provider_owner_validation_rejects_wrong_tool_progress() {
    let (_td, mut h) = setup_routed_test_tool_call("owner-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-wrong"),
        Event::ToolProgressReported(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "spoofed progress",
        )),
    )
    .expect("wrong progress ignored");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "owner-progress-call"
                && progress.message.as_deref() == Some("spoofed progress")
    )));
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("owner-progress-call")
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolProgressReported(tool_progress(
            "owner-progress-call",
            "owned_tool",
            "real progress",
        )),
    )
    .expect("owner progress accepted");

    assert!(event_log_contains(
        &h,
        HARNESS_CONNECTION_ID,
        |event| matches!(
            event,
            Event::ToolProgress(progress)
                if progress.call_id.as_str() == "owner-progress-call"
                    && progress.message.as_deref() == Some("real progress")
        )
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_external_provider_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let _provider = connect_test_client(&mut h, "provider-spoof", tau_proto::ClientKind::Provider);

    h.handle_extension_event_inner(
        &crate::test_connection_id("provider-spoof"),
        Event::ProviderToolResult(final_tool_result(
            "provider-tool-call",
            "owned_tool",
            "spoofed provider result",
        )),
    )
    .expect("provider tool result ignored");

    assert!(!event_log_contains(&h, "provider-spoof", |event| matches!(
        event,
        Event::ProviderToolResult(result) if result.call_id.as_str() == "provider-tool-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_tool_event_message_emit() {
    let (_td, mut h) = setup_routed_test_tool_call("emit-cancelled-call", "owned_tool");

    h.handle_extension_event(
        "conn-wrong",
        TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
            event: Box::new(Event::ToolCancelled(tau_proto::ToolCancelled {
                presentation: Default::default(),
                call_id: "emit-cancelled-call".into(),
                tool_name: ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
                display: None,
            })),
            persist: true,
        })),
    )
    .expect("emitted cancellation ignored");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("emit-cancelled-call")
    );
    assert!(!event_log_contains(&h, "conn-wrong", |event| matches!(
        event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "emit-cancelled-call"
    )));

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_owner_validation_rejects_late_tool_progress_after_completion() {
    let (_td, mut h) = setup_routed_test_tool_call("late-progress-call", "owned_tool");

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolResultReported(final_tool_result(
            "late-progress-call",
            "owned_tool",
            "real output",
        )),
    )
    .expect("owner result accepted");
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("late-progress-call")
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-owner"),
        Event::ToolProgressReported(tool_progress(
            "late-progress-call",
            "owned_tool",
            "late progress",
        )),
    )
    .expect("late progress ignored");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "late-progress-call"
                && progress.message.as_deref() == Some("late progress")
    )));

    h.shutdown().expect("shutdown");
}

/// Completed-call diagnostics are also conversation-scoped so a caller cannot
/// probe whether another agent's guessed call id existed earlier in the
/// session.
#[test]
fn completed_tool_call_lookup_is_owner_scoped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let owner = AgentId::parse("owner").expect("owner id");
    let attacker = AgentId::parse("attacker").expect("attacker id");
    let target: ToolCallId = "owned-completed-call".into();

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(target.clone(), owner.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        target.clone(),
        PendingTool {
            name: ToolName::new("slow_tool"),
            internal_name: ToolName::new("slow_tool"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.clear_tool_call_tracking(target.as_str());

    assert!(h.is_completed_tool_call_for(&owner, &target));
    assert!(!h.is_completed_tool_call_for(&attacker, &target));

    h.shutdown().expect("shutdown");
}

/// Ensures read and edit dispatch immediately without harness-global
/// serialization because ext-shell coordinates their concurrent work.
///
/// This fixture explicitly selects the line-coordinate editor because untagged
/// models now default to exact-text editing.
#[test]
fn tool_turn_dispatches_provider_calls_without_global_locking() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.config.selected_model = Some("test/model".into());
    // This test needs the line-oriented schema below, not the untagged
    // exact-text default.
    h.config.tool_policy.default_shell_tool_style =
        Some(path_tau_config_settings::ShellToolStyle::Edit);
    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid);

    // A `read` of a nonexistent path returns a ToolError; a valid mutating
    // tool call returns ToolResult. Harness dispatch no longer serializes them
    // by execution mode; ext-shell owns update coordination.
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text("/nonexistent/tau-test-path".to_owned()),
    )]);
    let edit_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("w.txt").display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("start_line".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("end_line_exclusive".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("newText".to_owned()),
                    CborValue::Text("hi".to_owned()),
                ),
                (
                    CborValue::Text("context_line".to_owned()),
                    CborValue::Text(String::new()),
                ),
            ])]),
        ),
    ]);
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: read_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };

    h.handle_provider_response_finished(response)
        .expect("finished");

    for call_id in ["c1", "c2", "c3"] {
        assert!(
            h.tool_routing
                .tool_runtime
                .tool_turn
                .is_in_flight(&ToolCallId::from(call_id)),
            "{call_id} should dispatch immediately"
        );
    }
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.in_flight_len(), 3);

    drive_harness_until_tool_turn_empty(&mut h);
    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());

    h.shutdown().expect("shutdown");
}

/// A provider response remains lossless even when it returns multiple calls
/// after declaring parallel calls unsupported. Every sequentially completed
/// result must reach the follow-up prompt; otherwise a stale local tree head
/// can orphan a result and produce an upstream unbalanced-call rejection.
#[test]
fn multi_tool_turn_keeps_all_results_in_followup_prompt() {
    // When several tool calls complete in sequence, every
    // ToolResult must end up on the current branch so the follow-up
    // prompt sees a balanced tool_use ↔ tool_result set. A previous
    // bug let `publish_event` (used by the ToolResult/ToolError path)
    // leave the conversation's local head stale, so the next
    // ToolRequest's `publish_for_agent` emitted a
    // `UiNavigateTree` that bounced the tree head backward — orphaning
    // the just-published ToolResult onto a dead branch and triggering
    // OpenAI's "No tool output found for function call ..." 400.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    // This oracle exercises the legacy line editor's independent multi-call
    // lifecycle. Untagged models now use the exact-text implementation.
    h.config.tool_policy.default_shell_tool_style =
        Some(path_tau_config_settings::ShellToolStyle::Edit);
    let model_id: tau_proto::ModelId = "test/model".parse().expect("model id");
    let mut model_info = provider_model_info(model_id.clone(), 1_000);
    model_info.supports_parallel_tool_calls = false;
    h.provider_runtime.model_info.insert(model_id, model_info);

    append_user_message_via_event(&mut h, "s1", "go");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = test_agent_prompt_id(format!("ap-{agent_id}-0"));
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);

    let edit_args = |name: &str| {
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join(name).display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("start_line".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("end_line_exclusive".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text(name.to_owned()),
                    ),
                    (
                        CborValue::Text("context_line".to_owned()),
                        CborValue::Text(String::new()),
                    ),
                ])]),
            ),
        ])
    };
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id,
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("a.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("b.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c3".into(),
                name: tau_proto::ToolName::new("edit"),
                tool_type: tau_proto::ToolType::Function,
                arguments: edit_args("c.txt"),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(response)
        .expect("finished");

    drive_harness_until_tool_turn_empty(&mut h);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "c1"
                && result.kind == tau_proto::ToolResultKind::Final
    )));

    // After all three tools complete, the harness has auto-dispatched
    // a follow-up prompt. Read its context items and check that every
    // tool call has a matching tool result on the same branch.
    let prompt = read_nth_prompt_created(&h, 0);
    let tool_use_ids: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_result_ids: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert_eq!(
        tool_use_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "follow-up prompt must keep every tool_use; got {tool_use_ids:?}"
    );
    assert_eq!(
        tool_result_ids,
        vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
        "every tool_use must be paired with a tool_result on the current branch; \
         got {tool_result_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures a prompt submitted during a tool call steers the tool-result
/// follow-up instead of starting a later turn.
///
/// This fixture explicitly selects the line-coordinate editor rather than
/// silently relying on the ordinary exact-text default.
#[test]
fn queued_prompt_is_steered_into_next_round_after_tool_result() {
    // While the agent is mid-turn (a tool is in flight), a fresh user
    // prompt must queue rather than dispatch. When the tool result
    // arrives and the harness is about to issue the next-round prompt,
    // it should drain the queued prompt onto this conversation's
    // branch as a `AgentPromptSteered` event so it rides the same
    // `AgentPromptCreated` as the tool results — instead of waiting
    // for full `Idle` and starting a separate turn.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    // This test supplies line-coordinate edit arguments, so retain that
    // implementation rather than the untagged exact-text default.
    h.config.tool_policy.default_shell_tool_style =
        Some(path_tau_config_settings::ShellToolStyle::Edit);

    let cid = ensure_test_user_agent(&mut h);
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(test_agent_prompt_id("sp-x"), cid.clone());

    let edit_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("a.txt").display().to_string()),
        ),
        (
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("start_line".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("end_line_exclusive".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("newText".to_owned()),
                    CborValue::Text("a".to_owned()),
                ),
                (
                    CborValue::Text("context_line".to_owned()),
                    CborValue::Text(String::new()),
                ),
            ])]),
        ),
    ]);
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("sp-x"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            arguments: edit_args,
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("agent response with tool call");

    // The conversation must be in `ToolsRunning` so `submit_user_prompt`
    // takes the queued path rather than dispatching.
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default")
            .turn
            .turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "redirect".to_owned(),
            agent_id,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("submit interactive UI prompt");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default")
            .dispatch
            .pending_prompts
            .len(),
        1,
        "the steering message should sit in pending_prompts until the next-round seam",
    );

    drive_harness_until_call_completes(&mut h, "c1");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default")
            .dispatch
            .pending_prompts
            .is_empty(),
        "queued prompt must be drained when folded as a steer",
    );

    // Walk the event log and verify ordering: the AgentPromptSteered
    // is published before the next-round AgentPromptCreated, and the
    // latter's `context_items` includes the steered text alongside the
    // original user prompt.
    let mut cursor = path_crate_event_log::EventLogSeq::new(0);
    let mut saw_steered = false;
    let mut saw_next_round = false;
    while let Some(entry) = h.runtime_io.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        match &entry.event {
            Event::AgentPromptSteered(steered) => {
                assert_eq!(steered.text, "redirect");
                assert_eq!(
                    steered.submission_source,
                    tau_proto::PromptSubmissionSource::HumanUi
                );
                assert!(
                    !saw_next_round,
                    "steered event must precede the prompt it folds into",
                );
                saw_steered = true;
            }
            Event::AgentPromptCreated(p) if saw_steered => {
                assert!(
                    saw_steered,
                    "next-round prompt must follow the AgentPromptSteered",
                );
                saw_next_round = true;

                let user_texts: Vec<String> = p
                    .context
                    .flatten()
                    .iter()
                    .filter_map(|item| match item {
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) => text_part(item).map(str::to_owned),
                        _ => None,
                    })
                    .collect();
                assert!(
                    user_texts.iter().any(|t| t == "<user>redirect</user>"),
                    "next-round prompt should fold the steered message into messages; \
                     user texts were {user_texts:?}",
                );

                // The steered message must land *after* the tool result
                // on the same branch — otherwise the model sees its
                // tool_use replied to with a steer instead of the
                // ToolResult, which providers reject.
                let last_tool_result_idx = p
                    .context
                    .flatten()
                    .iter()
                    .rposition(|item| matches!(item, ContextItem::ToolResult(_)));
                let last_user_idx = p.context.flatten().iter().rposition(|item| {
                    matches!(
                        item,
                        ContextItem::Message(MessageItem {
                            role: ContextRole::User,
                            ..
                        }) if text_part(item) == Some("<user>redirect</user>")
                    )
                });
                assert!(
                    last_tool_result_idx.is_some(),
                    "next-round prompt must include the tool result"
                );
                assert!(
                    matches!((last_tool_result_idx, last_user_idx),
                        (Some(t), Some(u)) if u > t),
                    "steered user message must follow the tool result, not precede it",
                );
            }
            _ => {}
        }
    }
    assert!(saw_steered, "expected a AgentPromptSteered event");
    assert!(saw_next_round, "expected the next-round AgentPromptCreated");

    h.shutdown().expect("shutdown");
}

/// A watch notification received behind inference must activate after the
/// terminal folds both the tool result and deferred message, even if the live
/// tool projection retains a stale call absent from the durable round.
#[test]
fn watch_notification_folded_by_tool_terminal_starts_continuation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config.tool_policy.default_shell_tool_style =
        Some(path_tau_config_settings::ShellToolStyle::Edit);

    let watcher_cid = ensure_test_user_agent(&mut h);
    let watched_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        watched_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.dispatch_prompt_for_agent(
        &watcher_cid,
        PendingPrompt::user("start watcher tool round".to_owned()),
    )
    .expect("dispatch watcher prompt");
    let initial = read_nth_prompt_created(&h, 0);
    h.report_agent_work_status(
        &watched_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "changed while watcher inference runs".to_owned(),
        )
        .expect("valid status"),
    )
    .expect("publish watch notification");
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        1
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: initial.agent_prompt_id,
        agent_id: initial.agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "watcher-tool-call".into(),
            name: ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(td.path().join("watched.txt").display().to_string()),
                ),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        (
                            CborValue::Text("start_line".to_owned()),
                            CborValue::Integer(1.into()),
                        ),
                        (
                            CborValue::Text("end_line_exclusive".to_owned()),
                            CborValue::Integer(1.into()),
                        ),
                        (
                            CborValue::Text("newText".to_owned()),
                            CborValue::Text("done".to_owned()),
                        ),
                        (
                            CborValue::Text("context_line".to_owned()),
                            CborValue::Text(String::new()),
                        ),
                    ])]),
                ),
            ]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("dispatch watcher tool");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&watcher_cid]
            .turn
            .turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));

    drive_harness_until_call_completes(&mut h, "watcher-tool-call");

    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        2,
        "the first terminal must start a continuation containing the folded watch notification"
    );
    let continuation = read_nth_prompt_created(&h, 1);
    let first_items = continuation.context.flatten();
    let first_result = first_items
        .iter()
        .position(|item| {
            matches!(
                item,
                ContextItem::ToolResult(result)
                    if result.call_id.as_str() == "watcher-tool-call"
            )
        })
        .expect("first tool result");
    let first_watch = first_items
        .iter()
        .position(|item| {
            text_part(item)
                .is_some_and(|text| text.contains("changed while watcher inference runs"))
        })
        .expect("first folded watch status");
    assert!(first_result < first_watch);
    assert_eq!(
        serde_json::to_string(&continuation.context)
            .expect("serialize first continuation")
            .matches("changed while watcher inference runs")
            .count(),
        1
    );
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "watcher-tool-call"
        )),
        1
    );
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id
        )),
        1
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.report_agent_work_status(
        &watched_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Done,
            "changed again while watcher inference runs".to_owned(),
        )
        .expect("valid status"),
    )
    .expect("publish second watch notification");
    h.handle_provider_response_finished(provider_tool_response(
        &continuation,
        "watcher-second-tool-call",
        "edit",
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(td.path().join("watched-again.txt").display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("start_line".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("end_line_exclusive".to_owned()),
                        CborValue::Integer(1.into()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text("done again".to_owned()),
                    ),
                    (
                        CborValue::Text("context_line".to_owned()),
                        CborValue::Text(String::new()),
                    ),
                ])]),
            ),
        ]),
    ))
    .expect("dispatch second watcher tool");
    let AgentTurnState::ToolsRunning { remaining_calls } = &mut h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state
    else {
        panic!("second tool round must be running");
    };
    remaining_calls.push("stale-runtime-only-call".into());
    drive_harness_until_call_completes(&mut h, "watcher-second-tool-call");
    assert_eq!(
        event_log_count(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        3,
        "the second terminal must not strand the next folded watch notification"
    );
    let second_continuation = read_nth_prompt_created(&h, 2);
    let second_items = second_continuation.context.flatten();
    let second_result = second_items
        .iter()
        .position(|item| {
            matches!(
                item,
                ContextItem::ToolResult(result)
                    if result.call_id.as_str() == "watcher-second-tool-call"
            )
        })
        .expect("second tool result");
    let second_watch = second_items
        .iter()
        .position(|item| {
            text_part(item)
                .is_some_and(|text| text.contains("changed again while watcher inference runs"))
        })
        .expect("second folded watch status");
    assert!(second_result < second_watch);
    assert_eq!(
        serde_json::to_string(&second_continuation.context)
            .expect("serialize second continuation")
            .matches("changed again while watcher inference runs")
            .count(),
        1
    );
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == second_continuation.agent_prompt_id
        )),
        1
    );
    assert_eq!(
        agent_event_count(&h, |event| matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "watcher-second-tool-call"
        )),
        1,
        "repair must not duplicate the canonical terminal"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty(),
        "successful repair must not retain a publication completion"
    );
    h.shutdown().expect("shutdown");
}

#[test]
fn tool_calls_stop_reason_without_tool_items_does_not_wedge_turn() {
    // Providers can disagree between their terminal stop reason and
    // emitted item list. With no concrete tool-call items, there is no
    // round Tau can execute, so the harness must finish this model call
    // instead of entering an empty ToolsRunning state.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "hello".to_owned())
        .expect("submit");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: read_nth_prompt_created(&h, 0).agent_prompt_id,
        agent_id: read_nth_prompt_created(&h, 0).agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish");

    let cid = ensure_test_user_agent(&mut h);
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);

    h.submit_user_prompt(test_session_id("s1"), "again".to_owned())
        .expect("submit again");
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default")
            .turn
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));

    h.shutdown().expect("shutdown");
}

/// A tool registering mid-conversation must bust the chain — the
/// upstream stored its reasoning state against the *previous* tools
/// list, and chaining a request whose `tools` field grew (or shrank)
/// would silently mix new affordances into reasoning that never saw
/// them. Realistic trigger: an extension hot-registers a tool while
/// the user is mid-task.
#[test]
fn tools_drift_invalidates_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.submit_user_prompt(test_session_id("s1"), "first".to_owned())
        .expect("submit first");
    let prompt1 = read_nth_prompt_created(&h, 0);
    let spid1 = prompt1.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1,
        agent_id: prompt1.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_tools".to_owned()),
        ws_pool_delta: None,
    })
    .expect("finish first");

    // A new tool appears between turns — same shape as an extension
    // hot-registering. `gather_tool_definitions` reads from the
    // registry on every send, so the next prompt's `tools` field
    // grows by one.
    h.tool_routing.registry.register(
        &crate::test_connection_id("test-ext"),
        ToolSpec {
            name: ToolName::new("late_tool"),
            model_visible_name: None,
            description: Some("appeared between turns".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    h.submit_user_prompt(test_session_id("s1"), "second".to_owned())
        .expect("submit second");
    let prompt2 = read_nth_prompt_created(&h, 1);

    assert_eq!(
        prompt2.context.flatten().last().and_then(text_part),
        Some("second")
    );
}

/// A peer-created endpoint executes ordinary role-authorized tool calls and is
/// not completed or unloaded as a one-shot extension query.
#[test]
fn peer_auto_start_endpoint_dispatches_tools_and_remains_loaded() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let _tool = connect_test_tool(&mut h, "peer-tool-owner");
    h.tool_routing.registry.register(
        &crate::test_connection_id("peer-tool-owner"),
        shared_test_tool_spec("peer_test_tool"),
    );
    let result = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "peer-tool".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("peer-tool-message")
                .expect("test identifier must satisfy its grammar"),
            capability: "test-only".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "use the test tool".to_owned(),
        },
    );
    let recipient = result.recipient_id.expect("peer recipient");
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(recipient.as_str())
        .cloned()
        .expect("peer route");
    let prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("peer prompt in flight");
    let originator = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .originator
        .clone();
    assert!(
        !h.complete_failed_compaction_side_conversation(&cid, None),
        "peer endpoint compaction failure follows ordinary blocked-agent recovery"
    );
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt_id,
        agent_id: recipient.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "peer-tool-call".into(),
            name: ToolName::new("peer_test_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("dispatch peer tool call");

    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(recipient.as_str())
    );
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("peer-tool-call")
            .map(|connection| connection.as_str()),
        Some("peer-tool-owner")
    );
}

/// A completed agent used to be collapsed with a typo as an unknown recipient.
/// Keep the error distinct so callers can decide whether to retry or fix the
/// id.
#[test]
fn message_tool_stopped_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let stopped_cid: AgentId = crate::parse_agent_id("stopped-recipient");
    h.agent_runtime.agent_registry.agents.insert(
        stopped_cid.clone(),
        Agent::new(
            stopped_cid.clone(),
            1,
            test_session_id("s1"),
            tau_proto::PromptOriginator::User,
            None,
            None,
        ),
    );
    let recipient_id = h.ensure_agent_id_for_agent(&stopped_cid).expect("agent id");
    h.remove_agent(&stopped_cid);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-stopped", &recipient_id, "hello"),
        ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());
    let errors: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-stopped" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("stopped message recipient"));
    assert!(errors[0].message.contains("stopped"));

    h.shutdown().expect("shutdown");
}

/// A self-send keeps one outbound and one inbound canonical occurrence, one
/// payload-free receive wake, and no delivery-created prompt copy.
#[test]
fn message_tool_to_agent_uses_canonical_projection_and_payload_free_wake() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.ensure_agent_id_for_agent(&cid).expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("conversation")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-message-target"),
    };

    h.handle_message_tool_call(
        &cid,
        &message_tool_call(
            "msg-agent",
            &recipient_id,
            "secret <message>&</message> payload >",
        ),
        ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let sent = session_agent_message_sent_events(&h);
    let received = session_agent_message_received_events(&h);
    assert_eq!(sent.len(), 1);
    assert_eq!(received.len(), 1);
    assert_eq!(sent[0].message_id, received[0].message_id);
    assert_eq!(sent[0].sender_id.as_str(), recipient_id);
    assert_eq!(received[0].sender_id.as_str(), recipient_id);
    assert_eq!(received[0].recipient_id.as_str(), recipient_id);
    assert_eq!(received[0].kind, tau_proto::AgentMessageKind::Message);

    let durable_sent = durable_agent_message_sent_events(&h);
    let durable_received = durable_agent_message_received_events(&h);
    assert_eq!(durable_sent.len(), 1);
    assert_eq!(durable_received.len(), 1);
    assert_eq!(durable_sent[0].message_id, sent[0].message_id);
    assert_eq!(durable_received[0].message_id, received[0].message_id);

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("conversation");
    assert!(conv.dispatch.pending_prompts.is_empty());
    assert_eq!(conv.dispatch.pending_message_wakes.len(), 1);
    let tree = h
        .session_runtime
        .agent_store
        .agent(&recipient_id)
        .expect("self-send tree");
    let entries: Vec<_> = tree
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            tau_core::AgentEntry::AgentMessage {
                durable_event_seq,
                direction,
                ..
            } => Some((*durable_event_seq, *direction)),
            _ => None,
        })
        .collect();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].0, entries[1].0);
    assert_eq!(entries[0].1, tau_core::AgentMessageDirection::Outbound);
    assert_eq!(entries[1].1, tau_core::AgentMessageDirection::Inbound);
    let context = crate::prompt::assemble_prompt_context_from(tree, conv.identity.head)
        .context
        .flatten();
    assert!(context.iter().any(|item| {
        text_part(item).is_some_and(|text| {
            text.contains(&format!(
                "<tau_internal>You have received a message from {recipient_id}"
            )) && text
                .contains("<message>\nsecret <message>&&lt;/message&gt; payload >\n</message>")
        })
    }));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.text.contains("secret <message>")
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(prompt) if prompt.text.contains("secret <message>")
    )));

    h.shutdown().expect("shutdown");
}

/// A racing sibling response cannot persist a second foreground tool round or
/// launch any of its calls after another branch has opened the tree-global
/// round.
#[test]
fn second_tool_bearing_response_is_rejected_before_persistence_and_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("racing prompt".to_owned()))
        .expect("dispatch real racing prompt");
    let active_prompt = read_nth_prompt_created(&h, 0);
    let second_prompt_id = active_prompt.agent_prompt_id.clone();
    let request_count_before = h
        .session_runtime
        .current_session_state
        .token_usage
        .total
        .requests;
    let model_request_counts_before = h
        .session_runtime
        .current_session_state
        .token_usage
        .by_model
        .clone();
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .as_ref(),
        Some(&second_prompt_id)
    );
    assert!(matches!(
        &h.agent_runtime.agent_registry.agents[&cid].dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            agent_prompt_id,
            ..
        } if *agent_prompt_id == second_prompt_id
    ));
    let parent = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .map(tau_core::AgentEventParent::Under)
        .unwrap_or(tau_core::AgentEventParent::Root);
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            parent,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: test_agent_prompt_id("ap-first-round"),
                agent_id: agent_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-first-round".into(),
                    name: ToolName::new("first_tool"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("open first round without terminalizing active racing prompt");
    assert!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .has_open_foreground_tool_round()
    );

    let provider_responses_before = loaded_agent_events(&h, "s1")
        .iter()
        .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
        .count();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: second_prompt_id.clone(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-must-not-launch".into(),
            name: ToolName::new("forbidden_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::RepetitionDetected,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("racing response is rejected in band");

    assert_eq!(
        loaded_agent_events(&h, "s1")
            .iter()
            .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
            .count(),
        provider_responses_before
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("call-must-not-launch")
    );
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::ToolRequest(request)
            if request.call_id.as_str() == "call-must-not-launch")
    }));
    assert!(event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id == second_prompt_id
                    && terminated.reason
                        == tau_proto::AgentPromptTerminationReason::Canceled
        )
    }));
    let agent = &h.agent_runtime.agent_registry.agents[&cid];
    assert!(agent.dispatch.in_flight_prompt.is_none());
    assert!(agent.dispatch.last_prompt_id.is_none());
    assert!(matches!(
        agent.dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(matches!(agent.turn.turn_state, AgentTurnState::Idle));
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&second_prompt_id)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .models
            .contains_key(&second_prompt_id)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .operations
            .contains_key(&second_prompt_id)
    );
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        request_count_before
    );
    assert_eq!(
        h.session_runtime.current_session_state.token_usage.by_model,
        model_request_counts_before
    );
}

/// Standalone terminal classification precedes the ordinary global-round guard,
/// so a rejected telemetry-bearing compact response records only its permitted
/// terminal diagnostic, cannot launch the malformed tool call, and does not
/// project already-finished provider work as blocked.
#[test]
fn standalone_tool_response_with_telemetry_is_rejected_before_persistence() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&tau_proto::ModelId::from("test/model"))
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    }
    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-standalone-race-tool",
        "configured-standalone-race-tool",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-standalone-race-tool"),
        shared_test_tool_spec("forbidden_compaction_tool"),
    );
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&watcher_cid)
        .expect("watcher")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("busy-standalone-race-watcher"),
    };
    h.set_agent_watch(
        watcher_id.as_str(),
        agent_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let compact = read_nth_prompt_created(&h, 0);
    assert!(
        compact
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "forbidden_compaction_tool"),
        "the rejected call must name a dispatchable tool offered to the provider"
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&compact.agent_prompt_id),
        "standalone dispatch must capture harness-owned telemetry evidence"
    );

    let parent = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .map(tau_core::AgentEventParent::Under)
        .unwrap_or(tau_core::AgentEventParent::Root);
    h.session_runtime
        .agent_store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            parent,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: test_agent_prompt_id("ap-racing-open-round"),
                agent_id: agent_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-existing-round".into(),
                    name: ToolName::new("existing_tool"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("open racing round without moving the runtime branch cursor");
    let context_before = (
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_cached_tokens,
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_model
            .clone(),
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_usage_head,
    );
    let watcher_receives_before = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.recipient_id == watcher_id)
        .count();
    let context_publications_before = event_log_events(&h)
        .into_iter()
        .filter(|event| matches!(event, Event::HarnessAgentContextUsageChanged(_)))
        .count();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: compact.agent_prompt_id.clone(),
        agent_id: agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-standalone-must-not-launch".into(),
            name: ToolName::new("forbidden_compaction_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::RepetitionDetected,
        error: Some("standalone context rejection with semantic output".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: Some(tau_proto::ProviderTokenUsage {
            model: None,
            prompt_sent_tokens: 999,
            prompt_cached_tokens: 111,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens: 7,
            stats: Default::default(),
        }),
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("racing standalone response is rejected in band");

    assert!(!loaded_agent_events(&h, "s1").iter().any(|event| {
        matches!(event, Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == compact.agent_prompt_id)
    }));
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::ToolRequest(request)
            if request.call_id.as_str() == "call-standalone-must-not-launch")
    }));
    assert_eq!(
        (
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_input_tokens,
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_cached_tokens,
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_usage_model
                .clone(),
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_usage_head,
        ),
        context_before
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .get(agent_id.as_str())
            .is_some_and(|status| matches!(
                status.state,
                tau_proto::AgentWatchProviderState::Blocked {
                    category: tau_proto::AgentWatchProviderCategory::Compaction
                }
            ))
    );
    let watcher_receives_after: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.recipient_id == watcher_id)
        .collect();
    assert_eq!(
        watcher_receives_after.len(),
        watcher_receives_before,
        "{watcher_receives_after:#?}"
    );
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(event, Event::HarnessAgentContextUsageChanged(_)))
            .count(),
        context_publications_before
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .models
            .contains_key(&compact.agent_prompt_id)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .context_limits
            .contains_key(&compact.agent_prompt_id)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .contains_key(&compact.agent_prompt_id)
    );
    assert!(event_log_events(&h).iter().any(|event| {
        matches!(event, Event::HarnessNotice(notice)
            if notice.message.contains("provider failed standalone compaction"))
    }));
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::HarnessNotice(notice)
            if notice.message.contains("used repetition_detected with output items"))
    }));
}

#[test]
fn tool_group_overrides_apply_before_individual_tool_overrides() {
    // Role group toggles are coarse-grained defaults. Individual tool toggles
    // must run after them so a role can enable a whole group and exclude one
    // dangerous tool, or disable a group and keep one explicit exception.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.available_roles.insert(
        h.config.selected_role.clone(),
        tau_config::settings::AgentRole {
            enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
            disable_tools: vec![ToolName::new("email_trash")],
            disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
            enable_tools: vec![ToolName::new("shell_safe")],
            ..Default::default()
        },
    );

    for (name, group) in [
        ("email_read", "pim"),
        ("email_trash", "pim"),
        ("shell_exec", "shell"),
        ("shell_safe", "shell"),
    ] {
        h.tool_routing.registry.register_with_prompt_fragment(
            &crate::test_connection_id("conn-grouped"),
            tau_core::ToolRegistration {
                tool: ToolSpec {
                    name: ToolName::new(name),
                    model_visible_name: None,
                    description: Some(name.to_owned()),
                    tool_type: tau_proto::ToolType::Function,
                    parameters: None,
                    format: None,
                    tags: Vec::new(),
                    enabled_by_default: false,
                    background_support: None,
                    examples: Vec::new(),
                },
                tool_group: Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new(group),
                    prompt_fragment: (name == "email_trash").then(|| {
                        tau_proto::PromptFragment::new(
                            format!("{group}.instructions"),
                            tau_proto::PromptPriority::new(10),
                            format!("{group} GROUP PROMPT"),
                        )
                    }),
                }),
                prompt_fragment: None,
            },
        );
    }

    let defs = h.gather_tool_definitions_for_role(&h.config.selected_role);
    let names = defs.iter().map(|def| def.name.as_str()).collect::<Vec<_>>();
    assert!(
        names.contains(&"email_read"),
        "expected group-enabled tool: {names:?}"
    );
    assert!(
        !names.contains(&"email_trash"),
        "individual disable must win: {names:?}"
    );
    assert!(
        !names.contains(&"shell_exec"),
        "group disable should hide tool: {names:?}"
    );
    assert!(
        names.contains(&"shell_safe"),
        "individual enable must win: {names:?}"
    );
    let prompt_fragments = h.gather_prompt_fragments();
    let pim_group_prompts = prompt_fragments
        .iter()
        .filter(|fragment| fragment.template.as_str() == "pim GROUP PROMPT")
        .count();
    assert_eq!(pim_group_prompts, 1, "group prompt renders once");
}

/// An embedded tool round retains provider-requested arguments and observed
/// terminal metadata while stripping directed image bytes from the trace.
#[test]
fn embedded_tool_trace_records_call_and_byte_free_result() {
    let call = ToolCallItem {
        call_id: "call-image".into(),
        name: ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("mode".to_owned()),
            CborValue::Text("overview".to_owned()),
        )]),
        raw_arguments_json: None,
        responses_envelope: None,
    };
    let mut calls = Vec::new();
    record_embedded_tool_calls(&[ContextItem::ToolCall(call.clone())], &mut calls);
    assert_eq!(calls, vec![call]);

    let result = ToolResult {
        presentation: Default::default(),
        call_id: "call-image".into(),
        tool_name: ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![(
            CborValue::Text("mode".to_owned()),
            CborValue::Text("overview".to_owned()),
        )]),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: vec![1, 2, 3].into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };
    let observed = byte_free_embedded_tool_result(&result);
    assert_eq!(observed.result, result.result);
    assert!(observed.provider_content.is_empty());
}

/// Regression: `agent_start` display names should remain the requested topic.
/// Parent lineage is represented by generic watch state, not encoded into
/// `agent.started.display_name`, because display names nest and are
/// user-visible wherever the agent is referenced.
#[test]
fn tool_backed_start_agent_display_name_does_not_include_parent_lineage() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        ext_query("q-parent"),
    )
    .expect("parent query");
    let parent_cid = ext_query_cid(&h, "q-parent").expect("parent started");

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("child-call".into(), parent_cid);
    let mut child = ext_query("q-child");
    child.tool_call_id = Some("child-call".into());
    child.task_name = Some("fix streaming ellipsis".to_owned());
    h.handle_start_agent_request(&crate::test_connection_id("conn-delegate"), child)
        .expect("child query");

    let child_cid = ext_query_cid(&h, "q-child").expect("child started");
    let display_name = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.agent_id.as_str() == child_cid.as_str() => {
                started.display_name
            }
            _ => None,
        })
        .expect("child display name");

    assert_eq!(display_name, "fix streaming ellipsis");
    assert!(!display_name.contains("child of"));

    h.shutdown().expect("shutdown");
}

/// A wait that is already blocked on a tool call must be released even when the
/// terminal event is a harness-synthesized routing error instead of a provider
/// response. Otherwise `wait` can hang forever after unavailable-tool paths.
#[test]
fn wait_resolves_on_synthetic_tool_error() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let target_call_id: ToolCallId = "target-call".into();

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(target_call_id.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        target_call_id.clone(),
        PendingTool {
            name: ToolName::new("missing"),
            internal_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.record_wait_tool_request(&target_call_id);

    let wait_call = AgentToolCall {
        call_ref: None,
        id: "wait-call".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    h.handle_wait_tool_call(&cid, &wait_call, ToolName::new("wait"))
        .expect("start wait");
    let _interceptor = connect_test_tool(&mut h, "wait-error-interceptor");
    h.handle_extension_event(
        "wait-error-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_TOOL_ERROR,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register canonical error interceptor");

    let missing_message = unavailable_tool_error_message(&ToolName::new("missing"));
    h.publish_terminal_tool_error(
        Some(&cid),
        None,
        tau_proto::ToolError {
            presentation: Default::default(),
            call_id: target_call_id,
            tool_name: ToolName::new("missing"),
            tool_type: tau_proto::ToolType::Function,
            message: missing_message.clone(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        },
    );

    assert!(h.runtime_io.publication.pending_intercept.is_some());
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key("target-call")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error) if error.call_id.as_str() == "wait-call"
    )));

    h.handle_extension_event(
        "wait-error-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit canonical synthetic error");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "the wait helper's own canonical error remains parked"
    );
    h.handle_extension_event(
        "wait-error-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit canonical wait-helper error");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "wait-call"
                && error.message == missing_message
    )));

    h.shutdown().expect("shutdown");
}

/// Regression: `wait` is harness-owned and publishes its answer inline, but the
/// answer still must be folded as a provider-terminal tool output. Otherwise
/// the next full replay contains the `wait` ToolCall without a matching
/// ToolResult, which OpenAI rejects with `No tool output found for function
/// call …`.
#[test]
fn wait_tool_reply_is_folded_into_followup_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let cid = ensure_test_user_agent(&mut h);
    append_user_message_via_event(&mut h, "s1", "wait on missing call");
    seed_agent_thinking(&mut h, &cid, "sp-wait");
    let spid: AgentPromptId = test_agent_prompt_id("sp-wait");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "wait-call".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("missing-target".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("wait response");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error) if error.call_id.as_str() == "wait-call"
    )));
    let followup_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, prompt_cid)| {
            (prompt_id != &spid && prompt_cid == &cid).then_some(prompt_id.clone())
        })
        .expect("follow-up prompt id");
    let prompt = read_prompt_created(&h, &followup_spid);
    let prompt_items = prompt.context.flatten();
    let tool_uses: Vec<&str> = prompt_items.iter().filter_map(tool_call_id).collect();
    let tool_results: Vec<&str> = prompt_items.iter().filter_map(tool_result_id).collect();

    assert!(
        tool_uses.contains(&"wait-call"),
        "follow-up prompt must include the wait ToolCall; got: {tool_uses:?}",
    );
    assert!(
        tool_results.contains(&"wait-call"),
        "follow-up prompt must include the matching wait ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression for `tau-agent-ral6kd`: a parent `agent_start` call is only a
/// side-agent launcher. It must not keep a normal tool from the same agent turn
/// queued behind it; filesystem locking is handled by ext-shell, not the
/// harness tool-turn queue.
#[test]
fn delegate_launcher_does_not_block_same_turn_exclusive_tool() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-mutate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-mutate"),
        ToolSpec {
            name: ToolName::new("mutate"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-call".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "mutate-call".into(),
                name: ToolName::new("mutate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&ToolCallId::from("delegate-call")),
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_turn.pending_len(),
        0,
        "mutating tool must not remain queued behind the delegate launcher",
    );

    h.shutdown().expect("shutdown");
}

/// Mutating tool calls in distinct side conversations dispatch independently.
/// Any real filesystem coordination must happen inside the tool extension.
#[test]
fn mutating_tools_in_distinct_side_conversations_dispatch_concurrently() {
    use tau_proto::CborValue;

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let _ = connect_test_tool(&mut h, "conn-mutate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-mutate"),
        ToolSpec {
            name: ToolName::new("mutate"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // The parent creates two realistic side agents concurrently. The assertion
    // below is about mutating tools owned by those distinct side agents.
    let parent_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &parent_cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), parent_cid.clone());
    h.publish_for_agent(
        &parent_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "fan out".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    let delegate_args = CborValue::Map(Vec::new());
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-A".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: delegate_args.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "delegate-B".into(),
                name: ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: delegate_args,
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-A".to_owned(),
            instruction: "side task A".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-A".into()),
            task_name: Some("A".to_owned()),
        },
    )
    .expect("query A");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-B".to_owned(),
            instruction: "side task B".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-B".into()),
            task_name: Some("B".to_owned()),
        },
    )
    .expect("query B");

    let cid_a = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-A"
            )
            .then_some(cid.clone())
        })
        .expect("conversation A");
    let cid_b = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. } if query_id == "q-B"
            )
            .then_some(cid.clone())
        })
        .expect("conversation B");
    assert_ne!(cid_a, cid_b, "side agents must be distinct");

    let spid_a = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_a).then_some(spid.clone()))
        .expect("prompt A");
    let spid_b = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid_b).then_some(spid.clone()))
        .expect("prompt B");

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid_a,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-A".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-A".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response A");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid_b,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "mut-B".into(),
            name: ToolName::new("mutate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-B".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response B");

    let mut_a_id: ToolCallId = "mut-A".to_owned().into();
    let mut_b_id: ToolCallId = "mut-B".to_owned().into();
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&mut_a_id),
        "conversation A's mutating call should be in flight",
    );
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&mut_b_id),
        "conversation B's mutating call should be in flight too",
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get("mut-A"),
        Some(&cid_a)
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_agents.get("mut-B"),
        Some(&cid_b)
    );
    assert_ne!(
        h.tool_routing.tool_runtime.tool_agents.get("mut-A"),
        h.tool_routing.tool_runtime.tool_agents.get("mut-B"),
        "mutating calls must be attributed to different agents",
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_turn.pending_len(),
        0,
        "cross-conversation mutating calls should not queue behind each other",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn agent_stats_snapshots_cover_tool_and_context_transitions_and_replay() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    set_available_provider_models(&mut h, [provider_model_info("test/model".into(), 1_000)]);
    h.config.selected_model = Some("test/model".into());
    let stats = connect_test_tool(&mut h, "stats-live");
    h.complete_subscription(
        &crate::test_connection_id("stats-live"),
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_stats_updated(&stats);

    let cid = ensure_test_user_agent(&mut h);
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("sp-stats"),
        },
    );
    h.bump_tools_started_for(&cid);
    h.update_agent_context_usage(
        &cid,
        None,
        Some(&"test/model".into()),
        Some(250),
        Some(50),
        None,
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("counted-call".into(), cid.clone());
    h.finish_tool_call_runtime_state("counted-call");

    let snapshots = drain_stats_updated(&stats);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.runtime_state == tau_proto::AgentRuntimeState::Running
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 1
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.context.input_tokens == Some(250)
            && snapshot.context.cached_tokens == Some(50)
            && snapshot.context.context_window == Some(1_000)
            && snapshot.context.percent_used == Some(25)
    }));

    let replay = connect_test_tool(&mut h, "stats-replay");
    h.complete_subscription(
        &crate::test_connection_id("stats-replay"),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
        Vec::new(),
    )
    .expect("stats replay");
    let replayed = drain_stats_updated(&replay);
    assert!(replayed.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.context.input_tokens == Some(250)
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn rejected_pre_dispatch_tool_attempt_counts_once_in_agent_stats() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let stats = connect_test_tool(&mut h, "stats-reject");
    h.complete_subscription(
        &crate::test_connection_id("stats-reject"),
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_stats_updated(&stats);
    let cid = ensure_test_user_agent(&mut h);
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    let call = AgentToolCall {
        call_ref: None,
        id: "bad-call".into(),
        name: ToolName::new("missing_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };

    h.reject_agent_tool_call_before_dispatch(
        &cid,
        &call,
        ToolName::new("missing_tool"),
        "missing tool".to_owned(),
    );

    let snapshots = drain_stats_updated(&stats);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 1
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.tools.started_total == 1
            && snapshot.tools.in_flight == 0
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn explicit_agent_start_role_controls_side_agent_prompt_model_and_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    set_available_provider_models(
        &mut h,
        [provider_model_info("test/role-model".into(), 8_000)],
    );
    let _provider = connect_test_client(
        &mut h,
        "explicit-role-provider",
        tau_proto::ClientKind::Provider,
    );
    h.provider_runtime.model_routes.insert(
        "test/role-model".into(),
        tau_proto::ConnectionId::parse("explicit-role-provider")
            .expect("test connection id must satisfy the identifier grammar"),
    );
    h.prompt_coordination
        .context_discovery
        .system_prompt_templates
        .insert(
            "explicit-template".to_owned(),
            "role={{role.name}} agent={{agent_id}}".to_owned(),
        );
    h.config.available_roles.insert(
        "explicit-role".to_owned(),
        tau_config::settings::AgentRole {
            model: Some("test/role-model".into()),
            prompt_override: Some("explicit-template".to_owned()),
            tools: Some(vec![ToolName::new("agent_watch")]),
            effort: Some(tau_proto::Effort::High),
            ..Default::default()
        },
    );
    let _delegate = connect_test_tool(&mut h, "conn-delegate");
    let parent = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("explicit-call".into(), parent);
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_watch"),
            model_visible_name: None,
            description: Some("watch agent".to_owned()),
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-explicit".to_owned(),
            instruction: "side task".to_owned(),
            role: Some("explicit-role".to_owned()),
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("explicit-call".into()),
            task_name: Some("explicit task".to_owned()),
        },
    )
    .expect("start explicit role");

    let cid = ext_query_cid(&h, "q-explicit").expect("side conversation");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|conversation| conversation.identity.role.as_deref()),
        Some("explicit-role")
    );
    let spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid).then_some(spid.clone()))
        .expect("side prompt");
    let prompt = read_prompt_created(&h, &spid);
    assert_eq!(prompt.model.to_string(), "test/role-model");
    assert_eq!(prompt.model_params.effort, tau_proto::Effort::High);
    assert!(
        prompt
            .system_prompt
            .starts_with("role=explicit-role agent=")
    );
    assert_eq!(
        prompt
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["agent_watch"]
    );
    h.shutdown().expect("shutdown");
}

/// Regression: when one side conversation tears down (running
/// `snap_to_default_agent`) before another's tool result
/// arrives, the result must still fold onto the *originating*
/// conversation's branch. Before this fix, the result landed at
/// `tree.head` (which `snap_to_default` had moved to the parent
/// branch), producing orphan ToolUse blocks in subsequent prompts —
/// the exact `No tool output found for function call …` 400 we hit
/// in `tau-agent-yvxco1`'s log.
#[test]
fn sibling_side_conv_teardown_does_not_misplace_other_side_conv_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // Set up the main agent's turn that emits a single delegate call.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    // Spawn the outer side conversation.
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    // Have the outer sub-agent emit a *nested* delegate. The harness
    // should issue another StartAgentRequest for it, which we then ack
    // with a fresh side conversation. This is the exact pattern that
    // produced the misplacement: outer side conv runs teardown
    // (snap_to_default) before nested side conv's tool result lands.
    let outer_side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: outer_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    // Nested sub-agent finishes with a final answer. This triggers
    // side teardown: `snap_to_default_agent` runs, moving
    // tree.head back to the main branch. The delegate ext then
    // publishes a ToolResult for `nested-call` — which must fold on
    // the *outer* conv's branch (since outer issued nested-call), not
    // wherever tree.head happens to be.
    let nested_side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: nested_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "nested answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-nested".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("nested final");

    // The delegate extension would route the nested StartAgentResult
    // back as a ToolResult — simulate that here.
    mark_connected_test_extension_configured(
        &mut h,
        "conn-delegate",
        "configured-delegate",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "conn-delegate",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: "nested-call".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("nested answer".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("nested tool result");

    // Now re-prompt the outer sub-agent and inspect the assembled
    // messages. The `outer-call` tool_use must NOT appear in the
    // outer sub-agent's branch — the only ToolUse the outer
    // sub-agent should see is its own `nested-call` (with a
    // matching ToolResult).
    let outer_resume_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() == outer_side_cid_str(&h)).then_some(spid.clone())
        })
        .expect("outer resume prompt id");
    let prompt = read_prompt_created(&h, &outer_resume_spid);

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    let tool_results: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "outer sub-agent's prompt must not include the parent's `outer-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_uses.iter().any(|id| id == "nested-call"),
        "outer sub-agent's prompt must include its own `nested-call` ToolUse; got: {tool_uses:?}",
    );
    assert!(
        tool_results.iter().any(|id| id == "nested-call"),
        "outer sub-agent must see the matching ToolResult for `nested-call`; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: nested extension-agent queries must branch from the
/// conversation that issued the nested tool call. Branching from the
/// default conversation can replay unrelated in-flight ToolUse blocks
/// from the main branch into the nested sub-agent prompt, which OpenAI
/// rejects with `No tool output found for function call …`.
#[test]
fn nested_start_agent_request_branches_from_tool_owner_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let default_cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &default_cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), default_cid.clone());
    h.publish_for_agent(
        &default_cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("outer query");

    let outer_side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("outer side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: outer_side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "nested-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("outer response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-nested".to_owned(),
            instruction: "nested task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("nested-call".into()),
            task_name: Some("nested".to_owned()),
        },
    )
    .expect("nested query");

    let nested_side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default" && prompt_cid.as_str() != outer_side_cid_str(&h))
                .then_some(spid.clone())
        })
        .expect("nested side prompt id");
    let prompt = read_prompt_created(&h, &nested_side_spid);

    let tool_uses: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_call_id)
        .map(str::to_owned)
        .collect();
    assert!(
        !tool_uses.iter().any(|id| id == "outer-call"),
        "nested sub-agent's prompt must not include the default branch's unresolved `outer-call`; got: {tool_uses:?}",
    );
    assert!(
        !tool_uses.iter().any(|id| id == "nested-call"),
        "nested sub-agent starts before its parent call has a result, so it must not include `nested-call`; got: {tool_uses:?}",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn completed_side_conversation_tool_result_reprompts_parent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: target_agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "outer-call".into(),
            name: ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-outer".to_owned(),
            instruction: "outer task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("outer-call".into()),
            task_name: Some("outer".to_owned()),
        },
    )
    .expect("query");

    let side_cid = ext_query_cid(&h, "q-outer").expect("side conversation");
    let side_agent_id = durable_agent_id_for_conversation(&h, &side_cid);
    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: side_agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "outer answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q-outer".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side final");

    mark_connected_test_extension_configured(
        &mut h,
        "conn-delegate",
        "configured-delegate",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "conn-delegate",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: "outer-call".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("outer answer".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("delegate result");

    let main_resume_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &cid).then_some(spid.clone()))
        .expect("main resume prompt id");
    let prompt = read_prompt_created(&h, &main_resume_spid);
    let tool_results: Vec<String> = prompt
        .context
        .flatten()
        .iter()
        .filter_map(tool_result_id)
        .map(str::to_owned)
        .collect();
    assert!(
        tool_results.iter().any(|id| id == "outer-call"),
        "parent conversation must be re-prompted with agent_start ToolResult; got: {tool_results:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a delayed response for an older prompt in the same conversation
/// must not be allowed to append fresh tool calls after a newer prompt is
/// already in flight. That creates orphan `function_call` items with no
/// matching output in later full replays, which OpenAI rejects with `No tool
/// output found for function call …`.
#[test]
fn stale_same_conversation_tool_call_response_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let old_spid: AgentPromptId = test_agent_prompt_id("sp-old");
    let new_spid: AgentPromptId = test_agent_prompt_id("sp-new");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(old_spid.clone(), cid.clone());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(new_spid.clone(), cid.clone());
    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("default conversation");
        conv.dispatch.in_flight_prompt = Some(new_spid.clone());
        conv.dispatch.last_prompt_id = Some(new_spid.clone());
    }

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: old_spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "stale-call".into(),
            name: ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("stale response ignored");

    assert!(
        !event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolRequest(request) if request.call_id.as_str() == "stale-call"
        )),
        "stale tool call must not be dispatched",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id.as_str() == old_spid.as_str()
                    && terminated.reason == tau_proto::AgentPromptTerminationReason::Stale
        )),
        "stale prompt must publish a terminal lifecycle event",
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(old_spid.as_str())
    );
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conversation");
    assert_eq!(conv.dispatch.in_flight_prompt.as_ref(), Some(&new_spid));
    assert!(matches!(conv.turn.turn_state, AgentTurnState::Idle));

    h.shutdown().expect("shutdown");
}

/// The removed `user` recipient must fail before any projection is published,
/// so a tool call cannot report successful but invisible user delivery.
#[test]
fn message_tool_to_user_is_rejected_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-user", "user", "hello user"),
        ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    let sent = session_agent_message_sent_events(&h);
    let received = session_agent_message_received_events(&h);
    assert!(sent.is_empty());
    assert!(received.is_empty());

    let durable_sent = durable_agent_message_sent_events(&h);
    let durable_received = durable_agent_message_received_events(&h);
    assert!(durable_sent.is_empty());
    assert!(durable_received.is_empty());
    let errors: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-user" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "unsupported message recipient: `user`");

    h.shutdown().expect("shutdown");
}

/// Unknown agent recipients must fail the tool call before publishing any
/// message projection, so a typo cannot create forged transcript state.
#[test]
fn message_tool_unknown_recipient_errors_without_agent_message() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.handle_message_tool_call(
        &cid,
        &message_tool_call("msg-bad", "missing_agent", "hello"),
        ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
    )
    .expect("message tool");

    assert!(session_agent_message_sent_events(&h).is_empty());
    assert!(session_agent_message_received_events(&h).is_empty());
    assert!(durable_agent_message_sent_events(&h).is_empty());
    assert!(durable_agent_message_received_events(&h).is_empty());
    let errors: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolError(error) if error.call_id.as_str() == "msg-bad" => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("unknown message recipient"));
    assert!(errors[0].message.contains("unknown"));

    h.shutdown().expect("shutdown");
}
/// While a parent's `agent_start` tool call is in flight, the harness
/// must still dispatch the spawned side conversation's prompt
/// immediately — the parent's `ToolsRunning` turn state is logically
/// independent from the side conv's own turn. The two failure modes
/// this test pins down: (1) the side prompt gets queued behind the
/// parent's pending tool result and never goes out (deadlock), and
/// (2) the parent's `ToolsRunning` state gets clobbered when the
/// side conv finishes, leaving the parent unable to receive its
/// `ToolResult`. Uses the real delegate shape (`tool_call_id: Some`).
#[test]
fn start_agent_request_dispatches_while_tool_is_running_and_restores_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("side_source"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    let target_agent_id = durable_agent_id_for_conversation(&h, &cid);
    let spid: AgentPromptId = test_agent_prompt_id("sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: target_agent_id,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("side_source"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));
    let default_turn = &h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .expect("default conversation")
        .turn
        .turn_state;
    assert!(matches!(default_turn, AgentTurnState::ToolsRunning { .. }));
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_cid = ext_query_cid(&h, "q1").expect("side conversation");
    let side_agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.identity.agent_id.clone())
        .expect("side agent id");
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|conv| conv.dispatch.pending_prompts.is_empty()),
        "side prompt must dispatch immediately"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.text == "side task"
                && prompt.agent_id.as_str() == side_agent_id.as_str()
    )));
    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: crate::parse_agent_id(&side_agent_id),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "delegated answer".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-delegate"),
            query_id: "q1".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish side agent");

    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_in_flight(&ToolCallId::from("delegate-call")),
        "parent agent_start tool must remain in flight until its ToolResult arrives"
    );
    let events = delegate_events.lock().expect("delegate events");
    let results = events
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q1" => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [result] = results.as_slice() else {
        panic!(
            "expected exactly one delegated result, got {}",
            results.len()
        );
    };
    assert_eq!(result.text, "delegated answer");

    let side_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(&side_agent_id)
        .expect("completed delegate remains targetable")
        .clone();
    let side_conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .expect("completed delegate conversation is kept");
    // Tool-backed delegates are detached rather than removed after their tool
    // result is returned so a resumed UI agent can receive follow-up prompts on
    // the same branch without being treated as an extension side query again.
    assert!(matches!(
        side_conv.identity.originator,
        tau_proto::PromptOriginator::User
    ));
    assert!(side_conv.identity.source_connection.is_none());
    assert!(side_conv.identity.parent_tool_call_id.is_none());
    assert!(side_conv.identity.parent_agent_id.is_none());
    assert_eq!(
        side_conv.identity.agent_id.as_deref(),
        Some(side_agent_id.as_str())
    );
    h.shutdown().expect("shutdown");
}

/// Publishing an internal delegate's terminal result can synchronously dispatch
/// a canceled-tool completion prompt before the delegate detaches. Detachment
/// must preserve that replacement turn, block overlap, and treat its stale
/// extension originator as ordinary post-delegation work.
#[test]
fn detached_delegate_preserves_reentrant_tool_completion_turn() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let target_agent_id = path_std_sync::Arc::new(path_std_sync::Mutex::new(None));
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(
        ReentrantDelegateCompletionPrompt {
            target_agent_id: target_agent_id.clone(),
        },
    )]);

    let mut query = ext_query("q-reentrant");
    query.tool_call_id = Some("delegate-call".into());
    let parent = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("delegate-call".into(), parent);
    let side_agent_id = h
        .enqueue_internal_start_agent_request_without_draining(query)
        .expect("enqueue query");
    h.drain_pending_start_agent_requests()
        .expect("dispatch query");
    let side_cid = ext_query_cid(&h, "q-reentrant").expect("side conversation");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .identity
            .agent_id
            .as_deref(),
        Some(side_agent_id.as_str())
    );
    *target_agent_id.lock().expect("target agent id") = Some(side_agent_id.clone());
    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid == &side_cid).then_some(spid.clone()))
        .expect("side prompt id");
    let mut terminal =
        provider_text_response(&side_spid, crate::parse_agent_id(&side_agent_id), "done");
    terminal.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name(HARNESS_CONNECTION_ID),
        query_id: "q-reentrant".to_owned(),
    };
    h.handle_provider_response_finished(terminal)
        .expect("side finished");
    assert!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .identity
            .originator
            .is_user()
    );
    let replacement_spid = h.agent_runtime.agent_registry.agents[&side_cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("reentrant completion prompt remains owned");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .turn
            .turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert!(h.dispatch_blocked_for(&side_cid));
    assert!(matches!(
        h.submit_prompt_to_agent(test_session_id("s1"), &side_agent_id, "overlap".to_owned())
            .expect("queue overlapping prompt"),
        PromptSubmission::Queued
    ));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .dispatch
            .in_flight_prompt
            .as_ref(),
        Some(&replacement_spid)
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&side_cid)
        .expect("side conversation")
        .dispatch
        .pending_prompts
        .clear();

    let mut late_response =
        provider_text_response(&replacement_spid, crate::parse_agent_id(&side_agent_id), "");
    late_response.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name(HARNESS_CONNECTION_ID),
        query_id: "q-reentrant".to_owned(),
    };
    h.handle_provider_response_finished(late_response)
        .expect("late completion response");

    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&side_agent_id),
        "a stale extension originator must not tear down the detached delegate"
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    let result_count = event_log_count(&h, |event| {
        matches!(
            event,
            Event::StartAgentResult(result) if result.query_id == "q-reentrant"
        )
    });
    assert_eq!(
        result_count, 1,
        "the detached delegate must produce only one StartAgentResult"
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.kind == tau_proto::notice_kind::HARNESS_FAILURE
                && notice.message.contains("had no source connection")
    )));
    h.shutdown().expect("shutdown");
}

/// A tool-backed `StartAgentRequest` (`tool_call_id: Some(...)`) is the
/// `agent_start` path: it dispatches *while the parent's tool call is
/// still in flight*, so the parent conv's tip is a `ToolUse` block
/// with no matching `ToolResult` yet. The side conv must therefore
/// fork off the tree root with `head: None`, NOT inherit the
/// parent's branch — otherwise (a) the assembled prompt would carry
/// an orphan `ToolUse` block (provider 400s on unmatched tool_use),
/// and (b) the sub-agent would see the user's framing and might
/// recursively re-delegate the same task. (Contrast with the
/// non-tool path, where `tool_call_id: None` deliberately inherits
/// the parent — see `non_tool_start_agent_request_inherits_parent_branch`.)
#[test]
fn start_agent_request_during_tool_call_branches_off_unresolved_tool_use() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);

    // Tool-backed sub-agents (`tool_call_id: Some(...)`) get a fresh
    // context regardless of whether the parent is mid-tool-call: they
    // see only their own `query.instruction`, never the parent's
    // unresolved `agent_start` tool_use (which would be an orphan ToolUse
    // the provider rejects), and never the user's task framing (which
    // would invite recursive re-delegation).
    let saw_orphan_tool_use = prompt
        .context
        .flatten()
        .iter()
        .any(|item| tool_call_id(item) == Some("delegate-call"));
    assert!(
        !saw_orphan_tool_use,
        "side prompt must not replay the parent's unresolved agent_start tool_use"
    );

    let saw_user_framing = prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("delegate something"))
        )
    });
    assert!(
        !saw_user_framing,
        "side prompt must NOT inherit the user's task framing — sub-agents start with a fresh context"
    );

    let saw_own_instruction = prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("side task"))
        )
    });
    assert!(
        saw_own_instruction,
        "side prompt should contain the delegated instruction"
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool `StartAgentRequest` (`tool_call_id: None`, e.g.
/// `std-notifications`' idle summary) is **not** a delegate, but it still
/// starts an independent agent log. The child prompt contains only its own
/// instruction and selected role/system/tool setup; it must not inherit the
/// parent transcript branch.
#[test]
fn non_tool_start_agent_request_starts_fresh_agent_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    connect_test_tool(&mut h, "conn-notifications");

    // Drive the user's main conversation through one full
    // user-message → agent-final-response turn so the parent conv has
    // a non-empty history when the idle summary fires.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "find the bug in foo.rs".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");
    let parent_head_before = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conv")
        .identity
        .head;
    assert!(
        parent_head_before.is_some(),
        "parent conv should have advanced its head after the agent's reply",
    );

    // std-notifications-shaped query: no tool_call_id, just an
    // instruction asking the model to summarize.
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-notifications"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    // A non-tool start-agent request creates an independent agent log.
    // Parent transcript nodes belong to the parent agent, so the side
    // prompt starts from its own instruction instead of inheriting the
    // parent branch.
    let user_task_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("find the bug in foo.rs"))
        )
    });
    let agent_answer_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                ..
            }) if text_part(item).is_some_and(|text| text.contains("I fixed the off-by-one"))
        )
    });
    let instruction_present = side_prompt.context.flatten().iter().any(|item| {
        matches!(
            item,
            ContextItem::Message(MessageItem {
                role: ContextRole::User,
                ..
            }) if text_part(item)
                == Some("Summarize in one sentence.")
        )
    });
    assert!(
        !user_task_present,
        "side prompt must not inherit parent user message: {:?}",
        side_prompt.context.flatten(),
    );
    assert!(
        !agent_answer_present,
        "side prompt must not inherit parent assistant reply: {:?}",
        side_prompt.context.flatten(),
    );
    assert!(
        instruction_present,
        "side prompt must contain the summarize-instruction itself: {:?}",
        side_prompt.context.flatten(),
    );

    // Tool execution is blocked locally by the harness. The provider
    // request must still keep `tool_choice: Auto` so the side query's
    // non-input fields match the parent conv's cached chain.
    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "non-tool start-agent request must preserve wire tool_choice for cache compatibility",
    );

    // The parent conv's head must not have moved sideways because of
    // the side conv's publish — both convs are now downstream of the
    // parent's previous tip, but the side conv folded onto its own
    // child node.
    let parent_head_after = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("default conv")
        .identity
        .head;
    assert_eq!(
        parent_head_before, parent_head_after,
        "side conv's UserMessage must not advance the parent conv's head",
    );

    h.shutdown().expect("shutdown");
}

/// A non-tool start-agent request (idle-summary path) must not execute
/// tools, but it also must not mutate provider-visible request fields
/// to enforce that policy. It must preserve `tool_choice: Auto`; flipping it
/// to `None` changes the wire request and can defeat provider cache reuse.
#[test]
fn non_tool_start_agent_request_preserves_tool_choice_without_parent_chain_anchor() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    connect_test_tool(&mut h, "conn-notifications");

    // Drive one full main-conv turn through the normal dispatch path
    // so `prompt_fingerprints`/`prompt_models` are populated and
    // `handle_provider_response_finished` actually mints the anchor.
    h.submit_user_prompt(test_session_id("s1"), "find the bug in foo.rs".to_owned())
        .expect("submit main");
    let main_prompt = read_nth_prompt_created(&h, 0);
    let main_spid = main_prompt.agent_prompt_id.clone();
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: main_prompt.agent_id.clone(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "I fixed the off-by-one in foo.rs".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(responses_backend()),
        provider_attempt: Default::default(),
        provider_response_id: Some("resp_parent".to_owned()),
        ws_pool_delta: None,
    })
    .expect("main response");

    // std-notifications-shaped query — `tool_call_id: None` triggers
    // the `tool_choice: None` branch in `send_prompt_to_agent_for`.
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-notifications"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let side_prompt = read_prompt_created(&h, &side_spid);

    assert_eq!(
        side_prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "idle-summary query must preserve the parent's wire tool_choice; the harness enforces no-tool execution locally",
    );
    assert!(
        side_prompt.share_user_cache_key,
        "idle-summary side conv keeps setting the legacy cache-sharing hint for \
         older providers; first-party ChatGPT/Codex ignores it and uses the \
         target agent cache bucket",
    );
}

/// Counterpart to `non_tool_start_agent_request_starts_fresh_agent_branch`.
/// The harness picks `tool_choice` per conversation in
/// `send_prompt_to_agent_for`; if that discriminator ever
/// over-matches (e.g. flips on `originator.is_extension()` alone),
/// delegate sub-agents would receive `tool_choice: "none"` and be
/// unable to call any tool — silently turning every delegated task
/// into a one-shot text response. Asserts the inverse leg: when
/// `tool_call_id: Some(...)`, `ToolChoice::Auto` is preserved.
#[test]
fn delegate_start_agent_request_keeps_tool_choice_auto() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "go".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    let prompt = read_prompt_created(&h, &side_spid);
    assert_eq!(
        prompt.tool_choice,
        tau_proto::ToolChoice::Auto,
        "delegated sub-agent must keep tool access (ToolChoice::Auto)",
    );
    assert!(
        !prompt.share_user_cache_key,
        "delegate sub-agents leave the legacy cache-sharing hint unset; \
         first-party ChatGPT/Codex still uses the target agent cache bucket",
    );

    h.shutdown().expect("shutdown");
}

/// Regression for the `tau-agent-bsjr7t` stall: an in-flight
/// non-tool extension side conversation (idle-summary stuck on a
/// usage-limit retry) must be preempted as soon as the user submits
/// a fresh prompt. Otherwise the agent's single prompt slot keeps
/// burning backoff retries on the side conv while the user waits.
#[test]
fn user_prompt_preempts_in_flight_non_tool_ext_side_conversation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    connect_test_tool(&mut h, "conn-notifications");

    // Seed an in-flight idle-summary side conv with a previously
    // dispatched spid that's notionally still being retried by the
    // agent.
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-notifications"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "idle-0".to_owned(),
            instruction: "Summarize in one sentence.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start-agent request");

    let (side_cid, side_spid) = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find(|(_, prompt_cid)| {
            h.agent_runtime
                .agent_registry
                .agents
                .get(*prompt_cid)
                .is_some_and(|conv| !conv.identity.originator.is_user())
        })
        .map(|(spid, cid)| (cid.clone(), spid.clone()))
        .expect("side conv must exist");
    let side_conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .expect("side conv present");
    assert_eq!(
        side_conv.dispatch.in_flight_prompt.as_ref(),
        Some(&side_spid),
        "sanity: side conv is mid-flight before user submits",
    );

    // User submits a real prompt — the harness must preempt the
    // side conv (cancel it, free the agent slot) before queueing or
    // dispatching the user's turn.
    h.submit_user_prompt(test_session_id("s1"), "interrupting prompt".to_owned())
        .expect("submit user");

    let side_conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .expect("side conv still tracked");
    assert!(
        side_conv.dispatch.in_flight_prompt.is_none(),
        "user prompt must clear the side conv's in-flight spid so the agent's \
         prompt slot is free; still set to {:?}",
        side_conv.dispatch.in_flight_prompt,
    );
    assert!(
        h.prompt_coordination.canceled_prompts.contains(&side_spid),
        "side conv's spid must be marked canceled so a late response is dropped",
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&side_spid),
        "side conv's spid must be unrouted so the agent's eventual abort \
         doesn't try to publish a finished event into a stale slot",
    );
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptTerminated(terminated)
                if terminated.agent_prompt_id.as_str() == side_spid.as_str()
                    && terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
        )),
        "preempted side prompt must publish a terminal lifecycle event",
    );

    h.shutdown().expect("shutdown");
}

/// Regression: a sub-agent's `Shared` tool call must not be gated by the
/// parent's still-in-flight `Exclusive` `agent_start` call. The parent's
/// delegate only resolves once the sub-agent's tools have run, so a
/// global execution-mode gate produces a self-deadlock — the main
/// symptom we hit in `tau-agent-m2dpw4`'s event log.
#[test]
fn side_conversation_shared_tool_dispatches_through_parent_exclusive_delegate() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.config.selected_model = Some("test/model".into());
    let _delegate_events = connect_test_tool(&mut h, "conn-delegate");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-delegate"),
        ToolSpec {
            name: tau_proto::ToolName::new("agent_start"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    let websearch_events = connect_test_tool(&mut h, "conn-websearch");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-websearch"),
        ToolSpec {
            name: tau_proto::ToolName::new("websearch"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );

    // Main agent issues `agent_start`, putting an Exclusive call in flight
    // on the default conversation.
    let cid = ensure_test_user_agent(&mut h);
    let main_spid: AgentPromptId = test_agent_prompt_id("sp-main");
    seed_agent_thinking(&mut h, &cid, "sp-main");
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(main_spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "delegate something".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: main_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("main response");

    // Delegate extension turns it into an StartAgentRequest; the harness
    // spawns a side conversation and dispatches its prompt.
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q1".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    // Sub-agent now responds with a Shared `websearch` call. Without
    // per-conversation gating this would queue forever behind the
    // parent's still-in-flight Exclusive `agent_start`.
    let side_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| (prompt_cid.as_str() != "default").then_some(spid.clone()))
        .expect("side prompt id");
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "websearch-call".into(),
            name: tau_proto::ToolName::new("websearch"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("core-subagents"),
            query_id: "q1".to_owned(),
        },

        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side response");

    // The Shared call must have been accepted for the websearch
    // extension. The harness broadcasts `ToolStarted`; the
    // subscribed provider sees that event and starts the tool.
    let saw_routed = websearch_events.lock().expect("ws").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolStarted(invoke)) if invoke.call_id.as_str() == "websearch-call"
        )
    });
    assert!(
        saw_routed,
        "side conversation's Shared tool must dispatch despite parent's in-flight Exclusive delegate"
    );
    assert_eq!(
        h.tool_routing.tool_runtime.tool_turn.pending_len(),
        0,
        "no entries should be left queued"
    );

    h.shutdown().expect("shutdown");
}

/// An alert crossed by a response that starts tools must stay queued until the
/// terminal tool completion gate folds it into the continuation.
#[test]
fn context_size_alert_waits_for_tool_round_completion() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .context_size_alerts
        .insert(
            "compact-soon".to_owned(),
            tau_config::settings::ContextSizeAlert {
                threshold: 100,
                enable: true,
                message: "compact after tools".to_owned(),
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                    statuses: None,
                },
            },
        );
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::ToolsRunning {
            remaining_calls: vec!["alert-tool".into()],
        },
    );
    let alerts = h.config.available_roles[&h.config.selected_role]
        .context_size_alerts
        .clone();

    h.queue_crossed_context_size_alerts(&cid, Some(101), &alerts);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "compact after tools"
    )));

    h.maybe_complete_agent_turn_for(&cid, "alert-tool");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.text == "compact after tools"
                && steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.internal_kind
                    == Some(tau_proto::InternalPromptKind::ContextSizeAlert)
    )));
    h.shutdown().expect("shutdown");
}

/// Operational snapshots must retain outer runtime activity while tool-round
/// bookkeeping briefly makes the inner turn state idle.
#[test]
fn agent_stats_keep_outer_turn_running_across_inner_tool_continuation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("first-inner-round"),
        },
    );
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::ToolsRunning {
            remaining_calls: vec!["tool-round".into()],
        },
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::Idle;

    assert_eq!(
        h.agent_stats_snapshot(&cid).expect("stats").runtime_state,
        tau_proto::AgentRuntimeState::Running
    );

    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    assert_eq!(
        h.agent_stats_snapshot(&cid).expect("stats").runtime_state,
        tau_proto::AgentRuntimeState::Idle
    );

    h.shutdown().expect("shutdown");
}

/// The central detailed-activity reducer must apply the approved precedence
/// without changing the binary runtime projection.
#[test]
fn agent_turn_activity_reduces_provider_tools_and_timer_in_order() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|agent| agent.identity.agent_id.as_deref())
        .map(crate::parse_agent_id)
        .expect("stable agent id");
    let source = tau_proto::ConnectionId::parse("timer-source").expect("connection id");
    h.agent_runtime.agent_runtime_indicators.insert(
        source,
        path_std_collections::HashMap::from([(
            agent_id.clone(),
            path_std_collections::BTreeSet::from([
                tau_proto::AgentRuntimeIndicator::TimerScheduled,
            ]),
        )]),
    );
    assert_eq!(
        h.agent_turn_activity(&agent_id, &cid),
        tau_proto::AgentTurnActivity::TimerScheduled
    );

    let wait_id = ToolCallId::from("wait");
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cid.clone(),
            wait_id.clone(),
            ToolTurnCategories::from_tags(&[tau_proto::ToolTag::new(
                tau_proto::TURN_WAIT_TOOL_TAG,
            )]),
        );
    assert_eq!(
        h.agent_turn_activity(&agent_id, &cid),
        tau_proto::AgentTurnActivity::Waiting
    );
    let fetch_id = ToolCallId::from("fetch");
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cid.clone(),
            fetch_id.clone(),
            ToolTurnCategories::from_tags(&[tau_proto::ToolTag::new(
                tau_proto::TURN_DATA_FETCH_TOOL_TAG,
            )]),
        );
    assert_eq!(
        h.agent_turn_activity(&agent_id, &cid),
        tau_proto::AgentTurnActivity::Fetching
    );
    let manipulator_id = ToolCallId::from("manipulator");
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            cid.clone(),
            manipulator_id.clone(),
            ToolTurnCategories::default(),
        );
    assert_eq!(
        h.agent_turn_activity(&agent_id, &cid),
        tau_proto::AgentTurnActivity::Manipulating
    );

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .in_flight_prompt = Some(test_agent_prompt_id("responding"));
    assert_eq!(
        h.agent_turn_activity(&agent_id, &cid),
        tau_proto::AgentTurnActivity::Responding
    );
    assert_eq!(
        h.agent_stats_snapshot(&cid).expect("stats").runtime_state,
        tau_proto::AgentRuntimeState::Idle
    );

    h.shutdown().expect("shutdown");
}

/// A duplicate tool surface terminalizes a real create request's exact initial
/// activation; removing the collision permits an independently correlated
/// prompt.
#[test]
fn duplicate_tool_surface_does_not_resurrect_failed_create_prompt() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let provider = crate::test_connection_id("duplicate-create-tools");
    for internal_name in ["first_internal", "second_internal"] {
        h.tool_routing.registry.register(
            &provider,
            ToolSpec {
                name: ToolName::new(internal_name),
                model_visible_name: Some(ToolName::new("duplicate_visible")),
                description: None,
                tool_type: tau_proto::ToolType::Function,
                parameters: None,
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
                background_support: None,
                examples: Vec::new(),
            },
        );
    }

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("duplicate-tools-ui"),
        tau_proto::UiCreateAgent {
            request_id: "create-duplicate-tools".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some("initial prompt".to_owned()),
            literal: false,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("prompt-duplicate-tools".to_owned()),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("create agent");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .session_loaded
        .iter()
        .next()
        .cloned()
        .expect("created agent");
    let cid = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, agent)| {
            (agent.identity.agent_id.as_deref() == Some(agent_id.as_str())).then(|| cid.clone())
        })
        .expect("created runtime agent");

    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentPromptFailed(failed)
            if failed.request_id == "create-duplicate-tools"
                && failed.ctx_id == "prompt-duplicate-tools"
    )));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_ctx_id
            .is_none()
    );

    h.tool_routing.registry.unregister_connection(&provider);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::user("prompt after tool repair".to_owned())
            .with_ctx_id(Some("prompt-after-tool-repair".to_owned())),
    )
    .expect("dispatch independent prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.ctx_id.as_deref(), Some("prompt-after-tool-repair"));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.ctx_id.as_deref() == Some("prompt-duplicate-tools")
    )));
    h.shutdown().expect("shutdown");
}

/// One visible default shell instance must get concise, actionable persistent
/// workdir guidance and its current dynamic path.
#[test]
fn shell_workdir_prompt_guides_one_default_instance() {
    let (_td, mut h, agent_id) = shell_workdir_prompt_fixture();
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "core-shell",
        "default",
        "/srv/project",
        "available",
    );

    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert!(prompt.contains("### Shell workdirs"));
    assert!(prompt.contains("default shell tools (`workdir`): `/srv/project` [available]"));
    assert!(prompt.contains("there is no global shell cwd"));
    assert!(prompt.contains("project root before project work"));
    assert!(prompt.contains("configured directory-scoped wrappers"));
    assert!(prompt.contains("notably `direnv exec .`"));
    assert!(prompt.contains("in a later tool turn after success"));
    assert!(prompt.contains("sibling calls have no workdir-first ordering"));
}

/// Prefix-associated shell families must render independent paths and matching
/// workdir names without multiplying the shared guidance fragment.
#[test]
fn shell_workdir_prompt_renders_prefixed_instances_independently() {
    let (_td, mut h, agent_id) = shell_workdir_prompt_fixture();
    let fragment = h
        .prompt_coordination
        .context_discovery
        .prompt_fragments
        .values()
        .find_map(|fragments| fragments.get("shell.workdir"))
        .cloned()
        .expect("core shell workdir fragment");
    h.prompt_coordination
        .context_discovery
        .prompt_fragments
        .insert(
            crate::test_connection_id("shell-prod"),
            path_std_collections::BTreeMap::from([("shell.workdir".to_owned(), fragment)]),
        );
    let mut prod_workdir = shared_test_tool_spec("prod_workdir");
    prod_workdir.tags = vec![tau_proto::ToolTag::new("shell:workdir")];
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("shell-prod"), prod_workdir);
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "core-shell",
        "default",
        "/srv/default-project",
        "available",
    );
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "prod_workdir",
        "prod-shell",
        "prod",
        "/srv/prod-project",
        "available",
    );

    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert_eq!(prompt.matches("### Shell workdirs").count(), 1);
    assert_eq!(prompt.matches("there is no global shell cwd").count(), 1);
    assert!(prompt.contains("default shell tools (`workdir`): `/srv/default-project` [available]"));
    assert!(
        prompt.contains("`prod_*` shell tools (`prod_workdir`): `/srv/prod-project` [available]")
    );

    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("prod_workdir"));
    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert_eq!(prompt.matches("### Shell workdirs").count(), 1);
    assert!(prompt.contains("/srv/default-project"));
    assert!(!prompt.contains("/srv/prod-project"));
}

/// A role that hides the persistent setter must not retain guidance for a tool
/// the model cannot call, even while other shell tools remain visible.
#[test]
fn shell_workdir_prompt_is_absent_when_workdir_is_hidden() {
    let (_td, mut h, agent_id) = shell_workdir_prompt_fixture();
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "core-shell",
        "default",
        "/srv/hidden-workdir",
        "available",
    );
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("selected role")
        .disable_tools
        .push(ToolName::new("workdir"));

    let role_name = h.config.selected_role.as_str();
    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        role_name,
    );
    let specs = h.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
    assert!(!specs.iter().any(|spec| spec.name.as_str() == "workdir"));
    assert!(specs.iter().any(|spec| {
        spec.tags
            .iter()
            .any(|tag| tag.as_str().starts_with("shell:"))
    }));
    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert!(!prompt.contains("### Shell workdirs"));
    assert!(!prompt.contains("/srv/hidden-workdir"));
}

/// An unavailable remembered directory remains visible as state that the model
/// must repair; prompt rendering must not silently present it as usable.
#[test]
fn shell_workdir_prompt_marks_unavailable_state() {
    let (_td, mut h, agent_id) = shell_workdir_prompt_fixture();
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "core-shell",
        "default",
        "/srv/missing-project",
        "unavailable",
    );

    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert!(
        prompt.contains("default shell tools (`workdir`): `/srv/missing-project` [unavailable]")
    );
}

/// A stale extension fragment cannot advertise shell workdir behavior after
/// every shell tool provider has disappeared.
#[test]
fn shell_workdir_prompt_is_absent_without_a_shell_instance() {
    let (_td, mut h, agent_id) = shell_workdir_prompt_fixture();
    publish_shell_workdir_context(
        &mut h,
        &agent_id,
        "workdir",
        "stale-shell",
        "default",
        "/srv/stale-shell",
        "available",
    );
    let shell_connections = h
        .tool_routing
        .registry
        .all_tool_providers()
        .into_iter()
        .filter(|provider| {
            provider
                .tool
                .tags
                .iter()
                .any(|tag| tag.as_str().starts_with("shell:"))
        })
        .map(|provider| provider.connection_id.clone())
        .collect::<std::collections::HashSet<_>>();
    for connection_id in shell_connections {
        h.tool_routing
            .registry
            .unregister_connection(&connection_id);
    }

    let prompt = render_shell_workdir_prompt(&h, &agent_id);
    assert!(!prompt.contains("### Shell workdirs"));
    assert!(!prompt.contains("/srv/stale-shell"));
}

/// Multiple shell instances publish one shared coalescing fragment rather than
/// rendering every prefix-associated workdir once per instance.
#[test]
fn duplicate_shell_workdir_fragments_are_coalesced_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    h.prompt_coordination
        .context_discovery
        .prompt_fragments
        .clear();
    let fragment = tau_proto::PromptFragment::new(
        "shell.workdir",
        tau_proto::PromptPriority::new(900),
        "{{#each agent_context.workdir}}{{value.label}}={{value.path}}{{/each}}",
    );
    h.prompt_coordination
        .context_discovery
        .prompt_fragments
        .insert(
            crate::test_connection_id("shell-a"),
            path_std_collections::BTreeMap::from([("shell.workdir".to_owned(), fragment.clone())]),
        );
    h.prompt_coordination
        .context_discovery
        .prompt_fragments
        .insert(
            crate::test_connection_id("shell-b"),
            path_std_collections::BTreeMap::from([("shell.workdir".to_owned(), fragment)]),
        );
    let fragments = h.gather_prompt_fragments();
    assert_eq!(
        fragments
            .iter()
            .filter(|fragment| fragment.name == "shell.workdir")
            .count(),
        1
    );
}

/// Invalid model arguments must be rejected before tool dispatch. The harness
/// still emits a logical `ToolError` for user-visible UI state and a
/// provider-facing error for the model, but no `ToolRequest`/`ToolStarted`.
#[test]
fn invalid_tool_arguments_are_rejected_before_logical_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-strict-tool");
    let mut spec = shared_test_tool_spec("strict_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "allowed": { "type": "string" }
        },
        "required": ["allowed"],
        "additionalProperties": false
    }));
    spec.examples = vec![tau_proto::ToolExample {
        id: "allowed-string".to_owned(),
        title: Some("Allowed string".to_owned()),
        arguments: CborValue::Map(vec![(
            CborValue::Text("allowed".to_owned()),
            CborValue::Text("ok".to_owned()),
        )]),
        note: Some("Only include schema-declared fields.".to_owned()),
        subcommand: None,
    }];
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("conn-strict-tool"), spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-invalid-tool-args");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "bad-args".into(),
            name: ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("allowed".to_owned()),
                    CborValue::Text("ok".to_owned()),
                ),
                (
                    CborValue::Text("extra".to_owned()),
                    CborValue::Text("nope".to_owned()),
                ),
            ]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "bad-args" => {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.request");
            }
            Event::ToolStarted(invoke) if invoke.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.started");
            }
            Event::ToolError(error) if error.call_id.as_str() == "bad-args" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    let provider_error = provider_error.expect("provider tool error");
    assert!(provider_error.contains("invalid arguments for tool `strict_tool`"));
    assert!(provider_error.contains("unexpected argument(s): `extra`"));
    assert!(provider_error.contains("allowed fields: `allowed`"));
    assert!(provider_error.contains("Example valid call:"));
    assert!(provider_error.contains("\"allowed\":\"ok\""));
    assert_eq!(logical_events, vec!["tool.error"]);
    assert!(tool_invoke_call_ids(&tool_events).is_empty());

    h.execute_agent_tool_call(
        &cid,
        &crate::harness::AgentToolCall {
            call_ref: None,
            id: "bad-args-repeat".into(),
            name: ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("allowed".to_owned()),
                    CborValue::Text("ok".to_owned()),
                ),
                (
                    CborValue::Text("extra".to_owned()),
                    CborValue::Text("nope".to_owned()),
                ),
            ]),
        },
    )
    .expect("repeat tool call handled");

    let mut repeat_error = None;
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "bad-args-repeat"
        {
            repeat_error = Some(error.message.clone());
        }
    }
    let repeat_error = repeat_error.expect("repeat provider tool error");
    assert!(repeat_error.contains("invalid arguments for tool `strict_tool`"));
    assert!(!repeat_error.contains("Example valid call:"));

    h.shutdown().expect("shutdown");
}

#[test]
fn invalid_tool_arguments_are_repaired_and_revalidated_before_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-repair-tool");
    let mut spec = shared_test_tool_spec("repair_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer" },
            "flags": { "type": "array", "items": { "type": "boolean" } },
            "optional": { "type": "string" }
        },
        "required": ["count", "flags"],
        "additionalProperties": false
    }));
    h.tool_routing
        .registry
        .register(&crate::test_connection_id("conn-repair-tool"), spec);

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-repair-tool-args");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "repair-args".into(),
            name: ToolName::new("repair_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("count".to_owned()),
                    CborValue::Text("42".to_owned()),
                ),
                (CborValue::Text("flags".to_owned()), CborValue::Bool(true)),
                (CborValue::Text("optional".to_owned()), CborValue::Null),
            ]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut published_request = None;
    let mut provider_error = None;
    let mut notice = None;
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ToolRequest(request) if request.call_id.as_str() == "repair-args" => {
                published_request = Some(request.clone());
            }
            Event::ProviderToolError(error) if error.call_id.as_str() == "repair-args" => {
                provider_error = Some(error.message.clone());
            }
            Event::HarnessNotice(harness_notice)
                if harness_notice
                    .message
                    .contains("Repaired arguments for tool `repair_tool`") =>
            {
                notice = Some(harness_notice.clone());
            }
            _ => {}
        }
    }

    let published_request = published_request.expect("repaired tool request");
    assert_eq!(
        published_request.arguments,
        CborValue::Map(vec![
            (
                CborValue::Text("count".to_owned()),
                CborValue::Integer(42.into()),
            ),
            (
                CborValue::Text("flags".to_owned()),
                CborValue::Array(vec![CborValue::Bool(true)]),
            ),
        ])
    );
    assert!(provider_error.is_none());
    assert!(notice.is_some());

    h.shutdown().expect("shutdown");
}

#[test]
fn repaired_tool_arguments_are_rejected_when_revalidation_fails() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-repair-revalidate-tool");
    let mut spec = shared_test_tool_spec("repair_revalidate_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "count": { "type": "integer", "minimum": 1 }
        },
        "required": ["count"],
        "additionalProperties": false
    }));
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-repair-revalidate-tool"),
        spec,
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-repair-revalidate-tool-args");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "repair-revalidate-args".into(),
            name: ToolName::new("repair_revalidate_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("count".to_owned()),
                CborValue::Text("0".to_owned()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut logical_events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "repair-revalidate-args" =>
            {
                provider_error = Some(error.message.clone());
            }
            Event::ToolRequest(request) if request.call_id.as_str() == "repair-revalidate-args" => {
                logical_events.push("tool.request");
            }
            Event::ToolError(error) if error.call_id.as_str() == "repair-revalidate-args" => {
                logical_events.push("tool.error");
            }
            _ => {}
        }
    }

    let provider_error = provider_error.expect("provider error");
    assert!(provider_error.contains("invalid arguments for tool `repair_revalidate_tool`"));
    assert!(provider_error.contains("$.count: expected integer, got string"));
    assert_eq!(logical_events, vec!["tool.error"]);

    h.shutdown().expect("shutdown");
}

#[test]
fn rendered_tool_definitions_do_not_include_examples() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");
    let mut spec = shared_test_tool_spec("example_tool");
    spec.description = Some("visible description".to_owned());
    spec.examples = vec![tau_proto::ToolExample {
        id: "repair".to_owned(),
        title: Some("Repair".to_owned()),
        arguments: CborValue::Map(vec![(
            CborValue::Text("field".to_owned()),
            CborValue::Text("value".to_owned()),
        )]),
        note: Some("This should not be in prompt tool definitions.".to_owned()),
        subcommand: None,
    }];

    let definitions = h.tool_definitions_from_specs(&[spec]);
    let rendered = serde_json::to_string(&definitions).expect("definitions serialize");

    assert!(rendered.contains("visible description"));
    assert!(!rendered.contains("examples"));
    assert!(!rendered.contains("Repair"));
    assert!(!rendered.contains("This should not be in prompt"));
}

#[test]
fn invalid_tool_example_registration_is_rejected_with_notice() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let _events = connect_ready_configured_extension(
        &mut h,
        "conn-invalid-example",
        "invalid-example",
        tau_proto::ClientKind::Tool,
    );
    let mut spec = shared_test_tool_spec("invalid_example_tool");
    spec.parameters = Some(serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
        "additionalProperties": false
    }));
    spec.examples = vec![tau_proto::ToolExample {
        id: "bad".to_owned(),
        title: None,
        arguments: CborValue::Map(vec![(
            CborValue::Text("path".to_owned()),
            CborValue::Integer(1.into()),
        )]),
        note: None,
        subcommand: None,
    }];

    h.handle_extension_event_inner(
        &crate::test_connection_id("conn-invalid-example"),
        Event::ToolRegistrationDeclared(tau_proto::ToolRegistrationDeclared {
            tool: spec,
            tool_group: None,
            prompt_fragment: None,
        }),
    )
    .expect("registration handled");

    assert!(
        h.tool_routing
            .registry
            .providers_for("invalid_example_tool")
            .is_empty()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Critical
                && notice.purpose == tau_proto::NoticePurpose::Alert
                && notice.message.contains("Rejected tool registration")
                && notice.message.contains("invalid example `bad`")
                && notice.message.contains("$.path: expected string")
    )));
}

#[test]
fn shown_tool_failure_examples_are_session_and_agent_scoped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    h.prompt_coordination
        .prompt_runtime
        .shown_tool_failure_examples
        .insert((
            cid.clone(),
            ToolName::new("example_tool"),
            "hint".to_owned(),
        ));

    h.remove_agent(&cid);

    assert!(
        h.prompt_coordination
            .prompt_runtime
            .shown_tool_failure_examples
            .is_empty()
    );

    let cid = ensure_test_user_agent(&mut h);
    h.prompt_coordination
        .prompt_runtime
        .shown_tool_failure_examples
        .insert((cid, ToolName::new("example_tool"), "hint".to_owned()));
    h.switch_session(
        test_session_id("session-after-example-hint"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");

    assert!(
        h.prompt_coordination
            .prompt_runtime
            .shown_tool_failure_examples
            .is_empty()
    );
}

/// A rejected tool call can synchronously dispatch the side agent's automatic
/// continuation. The rejection path must preserve that new running state so
/// request completion cannot unload the agent before the continuation finishes.
#[test]
fn rejected_side_agent_tool_call_preserves_dispatched_continuation() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _frames = connect_test_client(&mut h, "conn-side-invalid", tau_proto::ClientKind::External);
    h.submit_user_prompt(test_session_id("s1"), "parent prompt".to_owned())
        .expect("submit parent");
    let parent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&test_user_agent(&h))
        .and_then(|agent| agent.identity.agent_id.as_deref())
        .and_then(|agent_id| tau_proto::AgentId::parse(agent_id).ok())
        .expect("parent agent id");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-side-invalid"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: Some(parent),
            query_id: "q-invalid-tool".to_owned(),
            instruction: "review the change".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
        },
    )
    .expect("start side agent");
    let side_cid = ext_query_cid(&h, "q-invalid-tool").expect("side conversation");
    let _interceptor = connect_test_tool(&mut h, "side-invalid-terminal-interceptor");
    h.handle_extension_event(
        "side-invalid-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_TOOL_ERROR,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register side terminal interceptor");
    let first_prompt_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .and_then(|agent| agent.dispatch.in_flight_prompt.clone())
        .expect("first side prompt");
    let mut response = provider_text_response(
        &first_prompt_id,
        durable_agent_id_for_conversation(&h, &side_cid),
        "",
    );
    response.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "invalid-side-call".into(),
        name: ToolName::new("tool_that_does_not_exist"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    response.originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("conn-side-invalid"),
        query_id: "q-invalid-tool".to_owned(),
    };

    h.handle_provider_response_finished(response)
        .expect("reject invalid side-agent tool call");

    assert!(h.runtime_io.publication.pending_intercept.is_some());
    let parked_call_id = h
        .tool_routing
        .tool_runtime
        .tool_agents
        .iter()
        .find_map(|(call_id, owner)| (owner == &side_cid).then(|| call_id.clone()))
        .expect("normalized side call remains live");
    let parked_side_agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .expect("side agent remains parked");
    assert_eq!(parked_side_agent.execution.tools_in_flight, 1);
    assert!(parked_side_agent.dispatch.in_flight_prompt.is_none());
    assert!(
        !h.tool_routing
            .tool_runtime
            .completed_tool_calls
            .contains(&parked_call_id)
    );

    h.handle_extension_event(
        "side-invalid-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit side canonical error");

    let side_agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .expect("side agent remains loaded");
    let continuation = side_agent
        .dispatch
        .in_flight_prompt
        .as_ref()
        .expect("automatic continuation remains in flight");
    assert_ne!(continuation, &first_prompt_id);
    assert!(matches!(
        side_agent.turn.turn_state,
        AgentTurnState::AgentThinking { .. }
    ));
    assert_eq!(side_agent.execution.tools_in_flight, 0);
    assert!(
        h.tool_routing
            .tool_runtime
            .completed_tool_calls
            .contains(&parked_call_id)
    );
    assert_eq!(
        event_log_events(&h)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::ProviderToolError(error) | Event::ToolError(error)
                    if error.call_id == parked_call_id
            ))
            .count(),
        2
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::SessionAgentUnloaded(unloaded)
            if unloaded.agent_id == durable_agent_id_for_conversation(&h, &side_cid)
    )));
}

/// Ensures unavailable-tool diagnostics distinguish a misspelled/unknown tool
/// from a registered tool disabled by policy, and give a near-name suggestion
/// only for the unknown-tool case where the model can retry mechanically.
#[test]
fn unavailable_tool_errors_are_actionable_for_unknown_and_disabled_tools() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config.selected_role = "restricted".to_owned();
    h.config.available_roles.insert(
        "restricted".to_owned(),
        tau_config::settings::AgentRole {
            disable_tools: vec![ToolName::new("disabled_tool")],
            ..Default::default()
        },
    );

    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-readable"),
        shared_test_tool_spec("readable"),
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-disabled"),
        shared_test_tool_spec("disabled_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-unavailable-tool-errors");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "unknown-tool".into(),
                name: ToolName::new("readble"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "disabled-tool".into(),
                name: ToolName::new("disabled_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut unknown_error = None;
    let mut disabled_error = None;
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        match &entry.event {
            Event::ProviderToolError(error) if error.call_id.as_str() == "unknown-tool" => {
                unknown_error = Some(error.message.clone());
            }
            Event::ProviderToolError(error) if error.call_id.as_str() == "disabled-tool" => {
                disabled_error = Some(error.message.clone());
            }
            _ => {}
        }
    }

    let unknown_error = unknown_error.expect("unknown tool provider error");
    assert!(unknown_error.contains("Tool `readble` is not available."));
    assert!(unknown_error.contains("Did you mean `readable`?"));
    let disabled_error = disabled_error.expect("disabled tool provider error");
    assert!(
        disabled_error
            .contains("Tool `disabled_tool` was not in the tool set advertised for this prompt."),
        "unexpected disabled error: {disabled_error}"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures a registered tool that is disabled by the current live role/model is
/// reported as disabled, not as unknown, when the call is not tied to a prompt
/// snapshot.
#[test]
fn current_role_disabled_tool_error_names_role_model_authority() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    h.config.selected_role = "restricted".to_owned();
    h.config.available_roles.insert(
        "restricted".to_owned(),
        tau_config::settings::AgentRole {
            disable_tools: vec![ToolName::new("disabled_tool")],
            ..Default::default()
        },
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-disabled"),
        shared_test_tool_spec("disabled_tool"),
    );
    let cid = ensure_test_user_agent(&mut h);

    h.execute_agent_tool_call(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: "disabled-current-role".into(),
            name: ToolName::new("disabled_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
    )
    .expect("disabled direct call handled");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == "disabled-current-role"
                && error.message.contains(
                    "Tool `disabled_tool` exists, but is disabled for the current role/model."
                )
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures unknown-tool suggestions for a provider response use the prompt's
/// advertised tool snapshot rather than the current role surface, which can
/// change while the model is thinking.
#[test]
fn unknown_tool_suggestion_uses_prompt_tool_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-current"),
        shared_test_tool_spec("current_tool"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-snapshot-tool-suggestion");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(spid.clone(), vec![shared_test_tool_spec("snapshot_tool")]);
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert("snapshot-typo".into(), spid.clone());
    h.prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert("registered-not-snapshot".into(), spid.clone());

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "snapshot-typo".into(),
                name: ToolName::new("snapshot_too"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "registered-not-snapshot".into(),
                name: ToolName::new("current_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("provider response handled");

    let mut provider_error = None;
    let mut registered_error = None;
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "snapshot-typo"
        {
            provider_error = Some(error.message.clone());
        }
        if let Event::ProviderToolError(error) = &entry.event
            && error.call_id.as_str() == "registered-not-snapshot"
        {
            registered_error = Some(error.message.clone());
        }
    }

    let provider_error = provider_error.expect("provider tool error");
    assert!(provider_error.contains("Did you mean `snapshot_tool`?"));
    assert!(!provider_error.contains("current_tool"));
    let registered_error = registered_error.expect("registered tool outside snapshot error");
    assert!(
        registered_error
            .contains("Tool `current_tool` was not in the tool set advertised for this prompt.")
    );
    assert!(!registered_error.contains("current role/model"));

    h.shutdown().expect("shutdown");
}

/// Disconnect cleanup unregisters the provider first, commits every in-flight
/// terminal as one batch, and only then drains queued work.
#[test]
fn disconnect_with_multiple_inflight_tools_cleans_up_all_calls() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let tool_events = connect_test_tool(&mut h, "conn-dead-tool");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-dead-tool"),
        exclusive_test_tool_spec("dead_slow"),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id("sp-dead-tool");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.publish_for_agent(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "run two slow tools".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "running-call".into(),
                name: ToolName::new("dead_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "queued-call".into(),
                name: ToolName::new("dead_slow"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned(), "queued-call".to_owned()]
    );
    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 0);
    h.tool_routing.tool_runtime.tool_turn.push(
        cid.clone(),
        AgentToolCall {
            call_ref: None,
            id: "queued-after-disconnect".into(),
            name: ToolName::new("dead_slow"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        tau_proto::BackgroundSupport::Never,
    );
    let _interceptor = connect_test_tool(&mut h, "disconnect-terminal-interceptor");
    h.handle_extension_event(
        "disconnect-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_TOOL_ERROR,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register disconnect terminal interceptor");

    h.handle_disconnect(&crate::test_connection_id("conn-dead-tool"));

    assert_eq!(h.tool_routing.tool_runtime.tool_turn.pending_len(), 1);
    h.handle_extension_event(
        "disconnect-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit first disconnect terminal");
    assert_eq!(
        h.tool_routing.tool_runtime.tool_turn.pending_len(),
        1,
        "first foreground commit must not drain work before the batch completes"
    );
    h.handle_extension_event(
        "disconnect-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second disconnect terminal");
    assert_eq!(
        h.tool_routing.tool_runtime.tool_turn.pending_len(),
        0,
        "queued work drains only after the full disconnect batch commits"
    );
    if h.runtime_io.publication.pending_intercept.is_some() {
        h.handle_extension_event(
            "disconnect-terminal-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit queued-call rejection projection");
    }

    assert_eq!(
        tool_invoke_call_ids(&tool_events),
        vec!["running-call".to_owned(), "queued-call".to_owned()]
    );
    assert!(
        h.tool_routing
            .registry
            .providers_for("dead_slow")
            .is_empty()
    );
    assert!(h.tool_routing.tool_runtime.tool_turn.is_empty());
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("running-call")
    );
    assert!(
        !h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .contains_key("queued-call")
    );

    let running: ToolCallId = "running-call".into();
    let queued: ToolCallId = "queued-call".into();
    let running_message = extension_disconnected_tool_call_error_message(&running);
    let queued_message = extension_disconnected_tool_call_error_message(&queued);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "running-call"
                && error.message == running_message
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "queued-call"
                && error.message == queued_message
    )));

    h.shutdown().expect("shutdown");
}
/// Completed calls must release every process-local correlation before a
/// provider reuses the same display call ID.
#[test]
fn completed_call_clears_all_runtime_observation_correlation_before_id_reuse() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(td.path()).expect("harness");
    let call_id = ToolCallId::from("reused-call");
    let observation = tau_proto::ObservationId::from_bytes([7; 16]);
    harness
        .tool_routing
        .tool_runtime
        .pending_terminal_observations
        .insert(
            call_id.clone(),
            PendingTerminalObservation {
                observation_id: observation,
                cause: tau_proto::ToolTerminalCause::Completed,
            },
        );
    harness
        .tool_routing
        .tool_runtime
        .pending_cancellation_observations
        .insert(call_id.clone(), observation);
    harness
        .tool_routing
        .tool_runtime
        .pending_wait_settlements
        .insert(
            call_id.clone(),
            path_crate_harness::subagents_tool::PendingWaitSettlement {
                wait_observation: observation,
                wait_call: tau_proto::ToolCallRef {
                    declaration: observation,
                    item_index: 0,
                },
                registration: None,
                outcome: tau_proto::ToolWaitOutcome::TimedOut,
            },
        );

    harness.clear_tool_call_tracking(call_id.as_str());

    assert!(
        !harness
            .tool_routing
            .tool_runtime
            .pending_terminal_observations
            .contains_key(&call_id)
    );
    assert!(
        !harness
            .tool_routing
            .tool_runtime
            .pending_cancellation_observations
            .contains_key(&call_id)
    );
    assert!(
        !harness
            .tool_routing
            .tool_runtime
            .pending_wait_settlements
            .contains_key(&call_id)
    );
}

/// Post-tool lifecycle recovery repairs exactly the currently owed automatic
/// suffix in stages: terminal/finish cuts first add the protected start, the
/// next reopen records the now-interrupted in-flight transaction, and only the
/// following reopen is quiescent. A start cut owes that interruption on its
/// first reopen; a continuation-start cut owes no compaction repair.
#[test]
fn post_tool_automatic_policy_crash_cuts_repair_owed_suffix_once() {
    for cut_name in [
        "continuation_started",
        "terminal_decision",
        "outer_finished",
        "standalone_started",
    ] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (agent_id, transaction_id, records, cut);
        {
            let mut h = quiet_provider_harness(&state).expect("start");
            enable_remote_compaction_for_test_model(&mut h);
            let info = h
                .provider_runtime
                .model_info
                .get_mut(&"test/model".into())
                .expect("test model");
            info.supports_compaction = false;
            info.supports_standalone_compaction = true;
            let cid = ensure_test_user_agent(&mut h);
            h.config
                .available_roles
                .get_mut(&h.config.selected_role)
                .expect("selected role")
                .compactions
                .insert(
                    "crash-cut".to_owned(),
                    tau_config::settings::CompactionPolicy {
                        threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(100),
                        enable: true,
                        when: tau_config::settings::ContextPolicyWhen {
                            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
                        },
                    },
                );
            h.dispatch_prompt_for_agent(
                &cid,
                PendingPrompt::user(format!("post-tool crash cut {cut_name}")),
            )
            .expect("dispatch inference");
            let initial = read_nth_prompt_created(&h, 0);
            h.handle_provider_response_finished(provider_tool_response(
                &initial,
                &format!("crash-cut-call-{cut_name}"),
                "self_info",
                CborValue::Map(Vec::new()),
            ))
            .expect("finish tool round");
            let continuation = read_nth_prompt_created(&h, 1);
            h.config
                .available_roles
                .get_mut(&h.config.selected_role)
                .expect("selected role")
                .compactions
                .clear();
            let mut response = provider_text_response(
                &continuation.agent_prompt_id,
                continuation.agent_id.clone(),
                "done",
            );
            response.usage = Some(tau_proto::ProviderTokenUsage {
                prompt_sent_tokens: 250,
                response_received_tokens: 1,
                ..Default::default()
            });
            h.handle_provider_response_finished(response)
                .expect("finish continuation");
            agent_id = continuation.agent_id.clone();
            records = h
                .session_runtime
                .agent_store
                .agent_events(agent_id.as_str())
                .expect("records");
            let terminal_index = records
                .iter()
                .position(|record| {
                    matches!(
                        &record.event,
                        Event::ProviderResponseFinished(response)
                            if response.agent_prompt_id == continuation.agent_prompt_id
                                && response.automatic_compaction_decision.is_some()
                    )
                })
                .expect("decision terminal");
            transaction_id = match &records[terminal_index].event {
                Event::ProviderResponseFinished(response) => response
                    .automatic_compaction_decision
                    .as_ref()
                    .expect("decision")
                    .transaction_id
                    .clone(),
                _ => unreachable!("located response"),
            };
            let continuation_index = records
                .iter()
                .position(|record| {
                    matches!(
                        &record.event,
                        Event::AgentPromptStarted(started)
                            if started.agent_prompt_id == continuation.agent_prompt_id
                    )
                })
                .expect("continuation start");
            let finish_index = records
                .iter()
                .position(|record| {
                    matches!(
                        &record.event,
                        Event::AgentOuterTurnFinished(finished)
                            if finished.automatic_compaction_decision.as_ref()
                                == Some(&transaction_id)
                    )
                })
                .expect("finish");
            let start_index = records
                .iter()
                .position(|record| {
                    matches!(
                        &record.event,
                        Event::AgentStandaloneCompactionStarted(started)
                            if started.transaction_id == transaction_id
                    )
                })
                .expect("start");
            cut = match cut_name {
                "continuation_started" => continuation_index + 1,
                "terminal_decision" => terminal_index + 1,
                "outer_finished" => finish_index + 1,
                "standalone_started" => start_index + 1,
                _ => unreachable!(),
            };
            h.shutdown().expect("shutdown seed");
        }
        wait_for_session_unlock(&state, "s1");
        rewrite_agent_records(&state, &agent_id, &records[..cut]);

        let mut previous_reopen_records: Option<Vec<tau_core::PersistedAgentEvent>> = None;
        for reopen in 1..=3 {
            let expected = match (cut_name, reopen) {
                ("continuation_started", _) => (1, 0, 0, 0),
                ("terminal_decision" | "outer_finished", 1) => (2, 1, 1, 0),
                ("terminal_decision" | "outer_finished", _) => (2, 1, 1, 1),
                ("standalone_started", _) => (2, 1, 1, 1),
                _ => unreachable!(),
            };
            let mut restored = quiet_provider_harness_with_start_reason(
                &state,
                tau_proto::SessionStartReason::Resume,
            )
            .unwrap_or_else(|error| panic!("cut={cut_name} reopen={reopen}: {error}"));
            let restored_records = restored
                .session_runtime
                .agent_store
                .agent_events(agent_id.as_str())
                .expect("restored records");
            let counts = automatic_policy_recovery_counts(&restored_records, &transaction_id);
            assert_eq!(
                counts, expected,
                "cut={cut_name} reopen={reopen} repaired a non-owed fact"
            );
            assert!(
                restored
                    .prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty(),
                "cut={cut_name} reopen={reopen} retained a completion"
            );
            let quiescent = match cut_name {
                "continuation_started" | "standalone_started" => 2 <= reopen,
                "terminal_decision" | "outer_finished" => 3 <= reopen,
                _ => unreachable!(),
            };
            if quiescent {
                let previous = previous_reopen_records
                    .as_ref()
                    .expect("quiescent reopen has predecessor");
                assert_eq!(
                    automatic_policy_recovery_events(&restored_records),
                    automatic_policy_recovery_events(previous),
                    "cut={cut_name} final reopen appended an unowed lifecycle record"
                );
            }
            previous_reopen_records = Some(restored_records);
            restored.shutdown().expect("shutdown reopen");
            drop(restored);
            wait_for_session_unlock(&state, "s1");
        }
    }
}
