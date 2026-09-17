//! Pure canonical-field preparation and effectful-wrapper equivalence.

use tau_config::settings::{CompactionPolicyThreshold, ContextPolicyPoint};

use super::*;
use crate::harness::AgentToolCall;

/// Preparing output-length authority must not spend its prompt identity or
/// install ownership; attachment must use exactly the prepared checkpoint.
#[test]
fn output_length_preparation_preserves_identity_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let mut response = reasoning_only_length_response(&source, 7);
    let before = &h.agent_runtime.agent_registry.agents[&cid];
    let index = before.dispatch.next_prompt_index;
    let state = before.turn.output_length_continuation.clone();
    let events = event_log_events(&h).len();

    let prepared = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    let repeated = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(prepared.disposition, repeated.disposition);
    assert_eq!(prepared.plan, repeated.plan);
    assert_eq!(prepared.next_prompt_index, Some(index + 1));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        index
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        state
    );
    assert_eq!(event_log_events(&h).len(), events);
    assert_eq!(
        response.output_length_disposition,
        tau_proto::OutputLengthDisposition::None
    );
    let plan = prepared.plan.expect("checkpoint-backed plan");
    assert_eq!(plan.owner.source_agent_prompt_id, source.agent_prompt_id);
    assert_eq!(plan.dispatch.model, source.model);

    h.derive_output_length_continuation(
        &cid,
        &mut response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(response.output_length_disposition, prepared.disposition);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        index + 1
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        OutputLengthContinuationState::Planned(plan)
    );
    let spent = h.prepare_output_length_continuation(
        &cid,
        &response,
        tau_proto::PromptOperation::Inference,
        false,
    );
    assert_eq!(spent.disposition, tau_proto::OutputLengthDisposition::None);
    assert_eq!(spent.next_prompt_index, None);
    assert!(spent.plan.is_none());
    h.shutdown().expect("shutdown");
}

/// The existing derivation consumes an identity before discovering a missing
/// checkpoint. Preserve that unusual behavior, including saturated cursors,
/// while keeping preparation itself free of mutations.
#[test]
fn output_length_preparation_preserves_missing_checkpoint_identity_consumption() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let mut response = reasoning_only_length_response(&source, 7);
    response.agent_prompt_id = test_agent_prompt_id("unmarked-source");
    for index in [37, u64::MAX] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .next_prompt_index = index;
        let prepared = h.prepare_output_length_continuation(
            &cid,
            &response,
            tau_proto::PromptOperation::Inference,
            false,
        );
        assert_eq!(
            prepared.disposition,
            tau_proto::OutputLengthDisposition::None
        );
        assert!(prepared.plan.is_none());
        assert_eq!(prepared.next_prompt_index, Some(index.saturating_add(1)));
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index
        );
        h.derive_output_length_continuation(
            &cid,
            &mut response,
            tau_proto::PromptOperation::Inference,
            false,
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index.saturating_add(1)
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .output_length_continuation,
            OutputLengthContinuationState::None
        );
    }
    h.shutdown().expect("shutdown");
}

/// Eager decision preparation must preserve exact policy coalescing and
/// identity values, without allocating or publishing before its wrapper
/// applies.
#[test]
fn automatic_compaction_preparation_is_read_only_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_id_for_prompt(&source.agent_prompt_id)
        .expect("owner");
    let policies = [("higher", 90), ("lower", 50), ("zero", 0)]
        .into_iter()
        .map(|(name, tokens)| {
            (
                name.to_owned(),
                tau_config::settings::CompactionPolicy {
                    enable: true,
                    threshold: CompactionPolicyThreshold::Tokens(tokens),
                    when: tau_config::settings::ContextPolicyWhen {
                        at: ContextPolicyPoint::OuterTurnFinished,
                        statuses: None,
                    },
                },
            )
        })
        .collect();
    let events = event_log_events(&h).len();
    for index in [23, u64::MAX] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .next_prompt_index = index;
        let prepared = h.prepare_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            Some(source.agent_prompt_id.clone()),
            &policies,
        );
        let repeated = h.prepare_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            Some(source.agent_prompt_id.clone()),
            &policies,
        );
        assert_eq!(prepared.decision, repeated.decision);
        let diagnostic = prepared.diagnostic.expect("coalesced policies");
        assert_eq!(diagnostic.names, "higher,lower");
        assert_eq!(diagnostic.threshold, tau_proto::TokenCount::new(50));
        let decision = prepared.decision.expect("eligible decision");
        assert_eq!(
            decision.transaction_id,
            tau_proto::CompactionTransactionId::parse(format!("ct-{index}")).expect("transaction")
        );
        assert_eq!(decision.threshold, diagnostic.threshold);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index
        );
        assert_eq!(event_log_events(&h).len(), events);
        assert_eq!(
            h.eager_automatic_compaction_decision(
                &cid,
                source.model.clone(),
                Some(tau_proto::TokenCount::new(100)),
                Some(source.agent_prompt_id.clone()),
                &policies,
            ),
            Some(decision)
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            index.saturating_add(1)
        );
    }
    // Historical evidence has not committed. Matching policies still produce
    // the old diagnostic, but neither preparation nor attachment spends an id.
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .context_usage_prompt_id = Some(source.agent_prompt_id.clone());
    let rejected = h.prepare_automatic_compaction_decision(
        &cid,
        source.model.clone(),
        Some(tau_proto::TokenCount::new(100)),
        None,
        &policies,
    );
    assert!(rejected.decision.is_none());
    assert!(rejected.diagnostic.is_some());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .next_prompt_index = 41;
    assert!(
        h.eager_automatic_compaction_decision(
            &cid,
            source.model.clone(),
            Some(tau_proto::TokenCount::new(100)),
            None,
            &policies,
        )
        .is_none()
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index,
        41
    );
    let no_usage = h.prepare_automatic_compaction_decision(
        &cid,
        source.model,
        None,
        Some(source.agent_prompt_id),
        &policies,
    );
    assert!(no_usage.decision.is_none());
    assert!(no_usage.diagnostic.is_none());
    h.shutdown().expect("shutdown");
}

/// Candidate normalization must freeze repaired IDs and declarations without
/// registering any tool ownership; the existing wrapper installs exactly those
/// calls, preserving prior ownership and duplicate-call rejection.
#[test]
fn tool_field_preparation_is_read_only_and_matches_attachment() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let prior = test_agent_prompt_id("prior-owner");
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt("reserved".into(), prior.clone());
    h.tool_routing
        .tool_runtime
        .completed_tool_calls
        .insert("reserved".into());
    let mut raw = reasoning_only_length_response(&source, 7);
    raw.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    let raw_calls = ["valid", "valid", "reserved", ""]
        .map(|id| AgentToolCall {
            call_ref: None,
            id: id.into(),
            name: ToolName::new("missing_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })
        .to_vec();
    raw.output_items = raw_calls
        .iter()
        .map(|call| {
            ContextItem::ToolCall(ToolCallItem {
                call_id: call.id.clone(),
                name: call.name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                raw_arguments_json: None,
                responses_envelope: None,
            })
        })
        .collect();
    let declaration = tau_proto::ObservationId::random();
    let events = event_log_events(&h).len();
    let mut candidate = raw.clone();
    let mut calls = raw_calls.clone();
    let prepared = h.prepare_finished_response_tool_calls(
        &mut candidate,
        &mut calls,
        false,
        false,
        declaration,
    );
    assert_eq!(prepared.invalid_errors.len(), 3);
    assert_eq!(prepared.calls.len(), 4);
    for (index, entry) in prepared.calls.iter().enumerate() {
        assert_eq!(
            entry.call.call_ref,
            Some(tau_proto::ToolCallRef {
                declaration,
                item_index: index as u32
            })
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .tool_call_prompt(&entry.call.id)
                .is_none()
        );
    }
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompt(&"reserved".into()),
        Some(&prior)
    );
    assert_eq!(event_log_events(&h).len(), events);
    let mut repeated_response = raw.clone();
    let mut repeated_calls = raw_calls.clone();
    let repeated = h.prepare_finished_response_tool_calls(
        &mut repeated_response,
        &mut repeated_calls,
        false,
        false,
        declaration,
    );
    assert_eq!(candidate, repeated_response);
    assert_eq!(prepared.invalid_errors, repeated.invalid_errors);

    let applied = h.normalize_finished_response_tool_calls(
        &mut raw,
        &mut raw_calls.clone(),
        false,
        false,
        declaration,
    );
    assert_eq!(raw, candidate);
    assert_eq!(applied.invalid_errors, prepared.invalid_errors);
    for (actual, expected) in applied.calls.iter().zip(&prepared.calls) {
        assert_eq!(actual.call.id, expected.call.id);
        assert_eq!(actual.call.call_ref, expected.call.call_ref);
        assert_eq!(actual.background_support, expected.background_support);
        assert_eq!(actual.turn_categories, expected.turn_categories);
        assert_eq!(
            h.prompt_coordination
                .prompt_runtime
                .tool_call_prompt(&actual.call.id),
            Some(&source.agent_prompt_id)
        );
    }
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .tool_call_prompt(&"reserved".into()),
        Some(&prior)
    );
    h.shutdown().expect("shutdown");
}
