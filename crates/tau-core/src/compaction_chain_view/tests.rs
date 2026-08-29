use tau_proto::{
    AgentCompacted, AgentHead, AgentId, AgentStandaloneCompactionFailed,
    AgentStandaloneCompactionStarted, ModelId, PromptOperation, PromptOriginator,
    ProviderStandaloneExecutionAccounted, ProviderStandaloneExecutionAccountingCorrected,
    StandaloneCompactionFailureReason, StandaloneExecutionAccountingFinality,
    StandaloneExecutionOutput,
};

use super::*;

/// Build one canonical-looking start for the view-only reducer tests.
fn start(
    id: &str,
    predecessor: Option<&str>,
    trigger: Option<StandaloneCompactionTrigger>,
) -> AgentStandaloneCompactionStarted {
    AgentStandaloneCompactionStarted {
        agent_id: AgentId::parse("chain-agent").expect("agent id"),
        transaction_id: CompactionTransactionId::parse(id).expect("transaction id"),
        compact_prompt_id: format!("prompt-{id}").parse().expect("prompt id"),
        cut: AgentHead::Root,
        resume_through: None,
        model: ModelId::from("provider/model"),
        operation: PromptOperation::StandaloneCompaction,
        originator: PromptOriginator::User,
        supersedes: predecessor
            .map(|id| CompactionTransactionId::parse(id).expect("predecessor transaction id")),
        trigger: trigger.unwrap_or_default(),
    }
}

/// Build one exact accounting observation for an attempt.
fn accounting(
    started: &AgentStandaloneCompactionStarted,
    attempt: u32,
    cost: Option<u64>,
    awaiting: bool,
) -> ProviderStandaloneExecutionAccounted {
    let usage = cost.map_or(StandaloneExecutionUsage::Unknown, |_| {
        StandaloneExecutionUsage::Known(tau_proto::ProviderTokenUsage {
            model: Some(started.model.clone()),
            ..Default::default()
        })
    });
    ProviderStandaloneExecutionAccounted {
        session_id: "chain-session".parse().expect("session id"),
        agent_id: started.agent_id.clone(),
        agent_prompt_id: started.compact_prompt_id.clone(),
        logical_attempt: ProviderAttempt::new(attempt).expect("finite attempt"),
        transaction_id: started.transaction_id.clone(),
        model: started.model.clone(),
        backend: None,
        usage,
        estimated_api_cost_rates: Some(tau_proto::ESTIMATED_API_COST_FALLBACK),
        estimated_api_cost_increment: cost.map(EstimatedApiCost::from_picodollars),
        output: StandaloneExecutionOutput::Rejected,
        finality: if awaiting {
            StandaloneExecutionAccountingFinality::AwaitingCancelledTerminal
        } else {
            StandaloneExecutionAccountingFinality::Final
        },
    }
}

/// Observe one event at an exact sequence and wall-clock timestamp.
fn observe(index: &mut CompactionChainIndex, seq: u64, at: u64, event: Event) {
    index.observe(
        PersistedAgentEventSeq::new(seq),
        UnixMicros::new(at),
        &event,
    );
}

/// Explicit predecessor links must isolate unrelated branch work and
/// corrections must replace, rather than add to, their awaiting observations.
#[test]
fn explicit_lineage_is_branch_independent_and_correction_replaces_cost() {
    let mut index = CompactionChainIndex::default();
    let first = start("chain-first", None, None);
    observe(
        &mut index,
        0,
        100,
        Event::AgentStandaloneCompactionStarted(first.clone()),
    );
    observe(
        &mut index,
        1,
        110,
        Event::ProviderStandaloneExecutionAccounted(accounting(&first, 1, None, true)),
    );
    let initial = accounting(&first, 1, None, true);
    observe(
        &mut index,
        2,
        120,
        Event::ProviderStandaloneExecutionAccountingCorrected(
            ProviderStandaloneExecutionAccountingCorrected {
                session_id: initial.session_id,
                agent_id: initial.agent_id,
                agent_prompt_id: initial.agent_prompt_id,
                logical_attempt: initial.logical_attempt,
                transaction_id: initial.transaction_id,
                model: initial.model,
                backend: None,
                usage: StandaloneExecutionUsage::Known(tau_proto::ProviderTokenUsage {
                    model: Some(first.model.clone()),
                    ..Default::default()
                }),
                estimated_api_cost_rates: initial.estimated_api_cost_rates,
                estimated_api_cost_increment: Some(EstimatedApiCost::from_picodollars(7)),
                output: StandaloneExecutionOutput::Rejected,
            },
        ),
    );
    let unrelated = start("branch-away", None, None);
    observe(
        &mut index,
        3,
        130,
        Event::AgentStandaloneCompactionStarted(unrelated),
    );
    let second = start("chain-second", Some("chain-first"), None);
    observe(
        &mut index,
        4,
        140,
        Event::AgentStandaloneCompactionStarted(second.clone()),
    );
    observe(
        &mut index,
        5,
        150,
        Event::ProviderStandaloneExecutionAccounted(accounting(&second, 1, Some(11), false)),
    );
    let lineage = index
        .lineage(&second.transaction_id)
        .expect("explicit lineage");
    assert_eq!(lineage.len(), 2, "unrelated branch start must be excluded");
    assert_eq!(
        estimated_cost(&lineage),
        CompactionChainEstimatedCost::Known(EstimatedApiCost::from_picodollars(18)),
        "correction replaces Unknown initial and contributes only one attempt"
    );
    assert_eq!(
        elapsed(&lineage),
        CompactionChainElapsed::Observed {
            duration: std::time::Duration::from_micros(50)
        }
    );
}

/// Missing or explicit Unknown accounting must remain unknown, while a durable
/// preflight-only start proves known zero provider cost.
#[test]
fn cost_knowledge_distinguishes_dispatch_ambiguity_from_preflight() {
    let mut index = CompactionChainIndex::default();
    let dispatched = start("missing-accounting", None, None);
    observe(
        &mut index,
        0,
        10,
        Event::AgentStandaloneCompactionStarted(dispatched.clone()),
    );
    assert_eq!(
        estimated_cost(&index.lineage(&dispatched.transaction_id).expect("lineage")),
        CompactionChainEstimatedCost::Unknown
    );
    observe(
        &mut index,
        1,
        15,
        Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
            agent_id: dispatched.agent_id.clone(),
            transaction_id: dispatched.transaction_id.clone(),
            cut: dispatched.cut,
            reason: StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    let generic_failure = index
        .derive(&dispatched.transaction_id)
        .expect("failed view");
    assert_eq!(
        generic_failure.completion,
        CompactionChainCompletion::Complete
    );
    assert_eq!(
        generic_failure.estimated_cost,
        CompactionChainEstimatedCost::Unknown,
        "generic terminal cannot invent missing provider accounting"
    );

    let preflight = start(
        "preflight-only",
        None,
        Some(StandaloneCompactionTrigger::ReactivePreflightFailure {
            failed_agent_prompt_id: "failed-prompt".parse().expect("prompt id"),
            reason: StandaloneCompactionFailureReason::PrefixTooLarge,
        }),
    );
    observe(
        &mut index,
        2,
        20,
        Event::AgentStandaloneCompactionStarted(preflight.clone()),
    );
    assert_eq!(
        estimated_cost(&index.lineage(&preflight.transaction_id).expect("lineage")),
        CompactionChainEstimatedCost::Known(EstimatedApiCost::default())
    );

    let route_failed = start("route-failed", None, None);
    observe(
        &mut index,
        3,
        30,
        Event::AgentStandaloneCompactionStarted(route_failed.clone()),
    );
    observe(
        &mut index,
        4,
        40,
        Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
            agent_id: route_failed.agent_id.clone(),
            transaction_id: route_failed.transaction_id.clone(),
            cut: route_failed.cut,
            reason: StandaloneCompactionFailureReason::RouteFailed,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert_eq!(
        index
            .derive(&route_failed.transaction_id)
            .expect("route failure view")
            .estimated_cost,
        CompactionChainEstimatedCost::Known(EstimatedApiCost::default()),
        "explicit no-route terminal proves zero dispatch cost"
    );
}

/// A durable automatic decision has zero passes until start, and its explicit
/// stale closure completes that same zero-pass chain.
#[test]
fn unstarted_decision_and_stale_terminal_have_explicit_completion() {
    let mut index = CompactionChainIndex::default();
    let transaction_id = CompactionTransactionId::parse("decision-only").expect("transaction id");
    let prompt_id = "decision-prompt"
        .parse::<tau_proto::AgentPromptId>()
        .expect("prompt id");
    observe(
        &mut index,
        0,
        10,
        Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
            agent_id: AgentId::parse("chain-agent").expect("agent id"),
            agent_prompt_id: prompt_id.clone(),
            reason: tau_proto::AgentPromptTerminationReason::Canceled,
            originator: PromptOriginator::User,
            automatic_compaction_decision: Some(tau_proto::AutomaticCompactionDecision {
                transaction_id: transaction_id.clone(),
                outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&prompt_id),
                model: ModelId::from("provider/model"),
                threshold: tau_proto::TokenCount::new(1),
                evidence: None,
            }),
        }),
    );
    let awaiting = index.derive(&transaction_id).expect("decision view");
    assert_eq!(awaiting.pass_count, 0);
    assert_eq!(
        awaiting.completion,
        CompactionChainCompletion::AwaitingStart
    );
    observe(
        &mut index,
        1,
        20,
        Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
            agent_id: AgentId::parse("chain-agent").expect("agent id"),
            transaction_id: transaction_id.clone(),
            cut: AgentHead::Root,
            reason: StandaloneCompactionFailureReason::StaleBranch,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert_eq!(
        index
            .derive(&transaction_id)
            .expect("closed decision")
            .completion,
        CompactionChainCompletion::Complete
    );
}

/// Timestamp absence and clock reversal must remain visible instead of being
/// rendered as an ordinary completed duration.
#[test]
fn elapsed_reports_missing_and_reversed_clock_quality() {
    let mut missing = CompactionChainIndex::default();
    let missing_start = start("missing-clock", None, None);
    observe(
        &mut missing,
        0,
        0,
        Event::AgentStandaloneCompactionStarted(missing_start.clone()),
    );
    assert_eq!(
        elapsed(
            &missing
                .lineage(&missing_start.transaction_id)
                .expect("lineage")
        ),
        CompactionChainElapsed::UnknownMissingTimestamp
    );

    let mut reversed = CompactionChainIndex::default();
    let reversed_start = start("reversed-clock", None, None);
    observe(
        &mut reversed,
        0,
        100,
        Event::AgentStandaloneCompactionStarted(reversed_start.clone()),
    );
    observe(
        &mut reversed,
        1,
        90,
        Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
            agent_id: reversed_start.agent_id.clone(),
            transaction_id: reversed_start.transaction_id.clone(),
            cut: AgentHead::Root,
            reason: StandaloneCompactionFailureReason::Cancelled,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    assert_eq!(
        elapsed(
            &reversed
                .lineage(&reversed_start.transaction_id)
                .expect("lineage")
        ),
        CompactionChainElapsed::SaturatedClockReversal {
            duration: std::time::Duration::ZERO
        }
    );
}

/// Accepted and rejected terminals affect neither pass count nor accounting
/// multiplicity; each durable start remains exactly one pass.
#[test]
fn terminal_boundaries_do_not_create_passes_or_cost() {
    let mut index = CompactionChainIndex::default();
    let started = start("terminal-boundary", None, None);
    observe(
        &mut index,
        0,
        10,
        Event::AgentStandaloneCompactionStarted(started.clone()),
    );
    for attempt in 1..=65 {
        observe(
            &mut index,
            u64::from(attempt),
            10 + u64::from(attempt),
            Event::ProviderStandaloneExecutionAccounted(accounting(
                &started,
                attempt,
                Some(u64::MAX),
                false,
            )),
        );
    }
    observe(
        &mut index,
        66,
        76,
        Event::AgentCompacted(AgentCompacted {
            agent_id: started.agent_id.clone(),
            transaction_id: Some(started.transaction_id.clone()),
            cut: Some(AgentHead::Root),
            suffix_end: Some(AgentHead::Root),
            compact_prompt_id: Some(started.compact_prompt_id.clone()),
            model: Some(started.model.clone()),
            operation: Some(PromptOperation::StandaloneCompaction),
            original_input_tokens: None,
            compaction_output_tokens: None,
            replacement_window: Vec::new(),
        }),
    );
    let lineage = index.lineage(&started.transaction_id).expect("lineage");
    assert_eq!(
        lineage.iter().filter(|facts| facts.start.is_some()).count(),
        1,
        "attempt 65 and accepted boundary cannot add passes"
    );
    assert_eq!(
        estimated_cost(&lineage),
        CompactionChainEstimatedCost::Known(EstimatedApiCost::from_picodollars(u64::MAX)),
        "all 65 bounded attempts fold saturating cost without adding passes"
    );
}
