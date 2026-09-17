//! Exact standalone outcome and accounting preparation before terminal effects.

use super::super::dispatch::{
    enable_remote_compaction_for_test_model, standalone_compaction_success_response,
};
use super::*;
use crate::harness::prepared_standalone_accounting::StandaloneAccountingFact;
use crate::harness::prepared_standalone_terminal::PreparedStandaloneTerminal;
use crate::harness::provider_terminal_plan::ProviderTerminalPlan;

/// Start a real standalone owner so preparation exercises the production
/// accounting and transaction identities rather than an uncorrelated DTO.
fn start_preparation_compaction(h: &mut Harness) -> (AgentId, tau_proto::AgentPromptCreated) {
    enable_remote_compaction_for_test_model(h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(h);
    let agent_id = durable_agent_id_for_conversation(h, &cid);
    let cut = h
        .selected_head_for_agent(&cid)
        .unwrap_or(tau_proto::AgentHead::Root);
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id,
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-preparation")
                .expect("transaction"),
            compact_prompt_id: test_agent_prompt_id("ap-preparation"),
            cut,
            resume_through: None,
            model: "test/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    let prompt = event_log_events(h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .expect("standalone prompt");
    (cid, prompt)
}

/// Both the accepted replacement and length-failure sidecar must be shaped
/// exactly before usage, ownership, or successor identity effects occur.
#[test]
fn standalone_preparation_matches_accounting_and_outcome_publication() {
    for length in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("start");
        let (cid, prompt) = start_preparation_compaction(&mut h);
        let mut raw = standalone_compaction_success_response(&prompt, "replacement");
        raw.usage = Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 31,
            response_received_tokens: 7,
            ..Default::default()
        });
        if length {
            raw.stop_reason = tau_proto::ProviderStopReason::Length;
            raw.backend = reasoning_only_length_response(&prompt, 7).backend;
        }
        let ProviderTerminalPlan::StandaloneCompaction(plan) =
            h.classify_standalone_compaction_terminal(&cid, &raw)
        else {
            panic!("standalone classification");
        };
        let before_events = event_log_events(&h).len();
        let before_stats = h.session_runtime.current_session_state.token_usage.clone();
        let before_index = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .next_prompt_index;
        let mut candidate = raw.clone();
        let _accounting = h.prepare_provider_accounting(&mut candidate, false);
        let prepared_accounting = h
            .prepare_standalone_accounting(&candidate, !length)
            .expect("accounting");
        let StandaloneAccountingFact::Initial(accounted) = prepared_accounting.fact else {
            panic!("initial accounting");
        };
        let outcome = h.prepare_standalone_terminal(&cid, &candidate, plan);
        let expected_outcome = match outcome {
            PreparedStandaloneTerminal::Accepted(Some((_, event))) => {
                assert!(!length);
                *event
            }
            PreparedStandaloneTerminal::Rejected {
                failure: Some(failure),
                ..
            } => {
                assert!(length);
                assert!(failure.failed.incomplete_response.is_some());
                let successor = failure
                    .failed
                    .output_length_continuation
                    .as_ref()
                    .expect("length successor");
                assert_eq!(
                    successor.transaction_id,
                    tau_proto::CompactionTransactionId::parse(format!("ct-{before_index}"))
                        .expect("transaction")
                );
                Event::AgentStandaloneCompactionFailed(failure.failed)
            }
            _ => panic!("expected output-bearing outcome"),
        };
        assert_eq!(event_log_events(&h).len(), before_events);
        assert_eq!(
            h.session_runtime.current_session_state.token_usage,
            before_stats
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .next_prompt_index,
            before_index
        );
        let state = &h.prompt_coordination.standalone_accounting;
        assert!(state.owners.contains_key(&prompt.agent_prompt_id));
        assert!(
            !state
                .observed_attempts
                .contains(&(prompt.agent_prompt_id.clone(), candidate.provider_attempt))
        );
        h.handle_provider_response_finished(raw).expect("terminal");
        let events = event_log_events(&h);
        let expected_accounting = Event::ProviderStandaloneExecutionAccounted(accounted);
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == expected_accounting)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == expected_outcome)
                .count(),
            1
        );
        h.shutdown().expect("shutdown");
    }
}

/// A cancellation-time unknown observation retains a correction owner.
/// Preparing the final numeric accounting must not consume it or duplicate the
/// request-counting initial fact.
#[test]
fn standalone_correction_preparation_does_not_consume_accounting_owner() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    let (_, prompt) = start_preparation_compaction(&mut h);
    h.publish_awaiting_cancelled_standalone_accounting(&prompt.agent_prompt_id);
    let mut response = standalone_compaction_success_response(&prompt, "discarded");
    response.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 19,
        response_received_tokens: 3,
        ..Default::default()
    });
    let _accounting = h.prepare_provider_accounting(&mut response, false);
    let before_events = event_log_events(&h).len();
    let first = h
        .prepare_standalone_accounting(&response, false)
        .expect("correction");
    let repeated = h
        .prepare_standalone_accounting(&response, false)
        .expect("repeat");
    let StandaloneAccountingFact::Correction(first_fact) = first.fact else {
        panic!("correction");
    };
    let StandaloneAccountingFact::Correction(repeated_fact) = &repeated.fact else {
        panic!("correction");
    };
    assert_eq!(&first_fact, repeated_fact);
    assert_eq!(event_log_events(&h).len(), before_events);
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .owners
            .contains_key(&prompt.agent_prompt_id)
    );
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .observed_corrections
            .is_empty()
    );
    h.apply_standalone_accounting(&response, Some(repeated));
    assert!(
        !h.prompt_coordination
            .standalone_accounting
            .owners
            .contains_key(&prompt.agent_prompt_id)
    );
    let expected = Event::ProviderStandaloneExecutionAccountingCorrected(first_fact);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| **event == expected)
            .count(),
        1
    );
    assert!(h.prepare_standalone_accounting(&response, false).is_none());
    h.shutdown().expect("shutdown");
}
