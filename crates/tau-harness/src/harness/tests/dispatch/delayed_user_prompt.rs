//! Harness admission and replay coverage for delayed direct user input.

use super::*;

/// Submit through authenticated UI admission rather than manufacturing
/// provenance.
fn submit_ui(h: &mut Harness, agent_id: &tau_proto::AgentId, text: &str) {
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            text: text.to_owned(),
            literal: true,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("submit authenticated UI input");
}

/// Read the accepted annotation, independently of submission versus steering.
fn accepted_delayed(h: &Harness, text: &str) -> bool {
    event_log_events(h)
        .iter()
        .find_map(|event| match event {
            Event::AgentPromptSubmitted(prompt) if prompt.text == text => Some(prompt.delayed),
            Event::AgentPromptSteered(prompt) if prompt.text == text => Some(prompt.delayed),
            _ => None,
        })
        .expect("prompt was admitted")
}

/// Queued input is annotated only for a completed visible response, including
/// commentary and retained canceled text; empty terminals do not qualify.
/// These cancellation-shaped terminals exercise classification, not
/// arbitration.
#[test]
fn delayed_ui_input_requires_completed_visible_response() {
    for (text, phase, stop_reason, expected) in [
        ("answer", None, tau_proto::ProviderStopReason::EndTurn, true),
        (
            "commentary",
            Some(tau_proto::MessagePhase::Commentary),
            tau_proto::ProviderStopReason::EndTurn,
            true,
        ),
        ("", None, tau_proto::ProviderStopReason::EndTurn, false),
        ("partial", None, tau_proto::ProviderStopReason::Error, true),
        ("", None, tau_proto::ProviderStopReason::Error, false),
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        submit_ui(&mut h, &agent_id, "first");
        assert!(!accepted_delayed(&h, "first"));
        let first = read_nth_prompt_created(&h, 0);
        // Submission during a streamed response must still compare against its
        // completion, not against the first token.
        h.publish_event(
            None,
            Event::ProviderResponseUpdated(ProviderResponseUpdated {
                agent_prompt_id: first.agent_prompt_id.clone(),
                agent_id: agent_id.clone(),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "streaming".to_owned(),
                    phase,
                }],
                compaction: None,
                status: None,
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        submit_ui(&mut h, &agent_id, "queued");
        let mut response = provider_text_response(&first.agent_prompt_id, agent_id.clone(), text);
        response.stop_reason = stop_reason;
        if stop_reason == tau_proto::ProviderStopReason::Error {
            response.error = Some("cancelled".to_owned());
        }
        let ContextItem::Message(message) = &mut response.output_items[0] else {
            unreachable!()
        };
        message.phase = phase;
        h.handle_provider_response_finished(response)
            .expect("complete response");
        assert_eq!(accepted_delayed(&h, "queued"), expected, "{text:?}");

        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("records");
        let mut encoded = Vec::new();
        ciborium::into_writer(&records, &mut encoded).expect("encode durable facts");
        let records: Vec<tau_core::PersistedAgentEvent> =
            ciborium::from_reader(encoded.as_slice()).expect("decode durable facts");
        let replay = tau_core::AgentTree::from_events(agent_id.clone(), &records);
        let live = h
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("live tree");
        let live_context = crate::prompt::assemble_prompt_context_from(live, live.head()).context;
        let replay_context =
            crate::prompt::assemble_prompt_context_from(&replay, replay.head()).context;
        assert_eq!(live_context, replay_context);
        let rendered = serde_json::to_string(&live_context).expect("render context");
        assert_eq!(rendered.contains("delayed=\\\"true\\\""), expected);
    }
}

/// A response completed before submission must not annotate fresh input.
#[test]
fn delayed_ui_input_after_completed_response_is_unmarked() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    submit_ui(&mut h, &agent_id, "first");
    let first = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_text_response(
        &first.agent_prompt_id,
        agent_id.clone(),
        "already complete",
    ))
    .expect("finish before new input");
    submit_ui(&mut h, &agent_id, "fresh input");
    assert!(!accepted_delayed(&h, "fresh input"));
}

/// A completed private standalone summary is not a user-visible response,
/// even when its replacement contains nonempty assistant text.
#[test]
fn delayed_ui_input_waiting_for_private_compaction_is_unmarked() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    compaction::append_byte_fit_text(&mut h, &cid, "history to compact".to_owned());
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        h.session_runtime.current_session_id.clone(),
        Some(&agent_id),
    );
    let compact = read_nth_prompt_created(&h, 0);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    submit_ui(&mut h, &agent_id, "waiting for private summary");
    h.handle_provider_response_finished(standalone_compaction_success_response(
        &compact,
        "private assistant summary",
    ))
    .expect("complete private compaction");
    assert!(!accepted_delayed(&h, "waiting for private summary"));
}

/// Tool-only waiting and partial commentary must not imply a completed
/// response. The same captured pending input can be steered without gaining an
/// annotation.
#[test]
fn delayed_ui_input_steering_without_completed_response_is_unmarked() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    submit_ui(&mut h, &agent_id, "first");
    let first = read_nth_prompt_created(&h, 0);
    submit_ui(&mut h, &agent_id, "waiting");
    h.handle_provider_response_finished(provider_tool_response(
        &first,
        "tool-only",
        "unavailable_fixture_tool",
        tau_proto::CborValue::Map(Vec::new()),
    ))
    .expect("complete tool-only response");
    h.fold_pending_prompts_as_steered_with_completion(&cid, None);
    assert!(!accepted_delayed(&h, "waiting"));
}
