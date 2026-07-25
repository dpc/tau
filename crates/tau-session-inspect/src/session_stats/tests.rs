use std::collections::BTreeSet;

use tau_core::{AgentEventParent, PersistedAgentEvent, PersistedAgentEventSeq, SessionStore};
use tau_proto::{
    AgentHead, AgentId, AgentOuterTurnFinished, AgentOuterTurnStarted, AgentPromptId,
    AgentPromptStarted, ContextItem, Effort, EstimatedApiCost, EstimatedApiCostRates,
    EstimatedUsdPerMillion, Event, ModelParams, PromptOperation, ProviderResponseFinished,
    ProviderStopReason, ProviderTokenUsage, SessionId, ToolCallItem, ToolName, ToolType,
    UnixMicros,
};

use super::{aggregate_agent, read_session_stats};

fn record(seq: u64, event: Event) -> PersistedAgentEvent {
    PersistedAgentEvent {
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        recorded_at: UnixMicros::new(seq),
    }
}

/// Exact aggregation must use response-local counters and the captured cost
/// increment rather than the much larger cumulative usage snapshot.
#[test]
fn aggregation_uses_response_local_usage_and_captured_dispatch_fields() {
    let agent_id = AgentId::parse("engineer_0").expect("agent id");
    let prompt_id = AgentPromptId::from("ap-engineer_0-0");
    let model: tau_proto::ModelId = "openai/gpt-5".parse().expect("model id");
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
        output: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
    };
    let mut events = vec![
        record(
            0,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: Some("primary".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
        ),
        record(
            1,
            Event::AgentOuterTurnStarted(AgentOuterTurnStarted {
                agent_id: agent_id.clone(),
                session_id: SessionId::from("s1"),
                outer_turn_id: "ot-ap-engineer_0-0".into(),
                agent_prompt_id: "ap-engineer_0-0".into(),
                runtime_id: tau_proto::AccountingRuntimeId::new("runtime-1"),
                activation: tau_proto::AgentOuterTurnActivation::Journal {
                    occurrence: AgentHead::Node(tau_core::NodeId::new(0)),
                },
            }),
        ),
        record(
            2,
            Event::AgentPromptStarted(AgentPromptStarted {
                agent_prompt_id: prompt_id.clone(),
                agent_id: agent_id.clone(),
                session_id: SessionId::from("s1"),
                model: model.clone(),
                model_params: Some(ModelParams {
                    effort: Effort::High,
                    ..ModelParams::default()
                }),
                outer_turn_id: Some("ot-ap-engineer_0-0".into()),
                operation: PromptOperation::Inference,
                originator: Default::default(),
                ctx_id: None,
            }),
        ),
        record(
            3,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: prompt_id,
                agent_id: agent_id.clone(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-1".into(),
                    name: ToolName::new("shell"),
                    tool_type: ToolType::Function,
                    arguments: tau_proto::CborValue::Null,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: Default::default(),
                originator: Default::default(),
                usage: Some(ProviderTokenUsage {
                    model: Some(model),
                    prompt_sent_tokens: 100,
                    prompt_cached_tokens: 60,
                    response_received_tokens: 20,
                    stats: tau_proto::TokenUsageStats {
                        total: tau_proto::TokenUsageCounts {
                            sent_tokens: 99_999,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                }),
                estimated_api_cost_rates: Some(rates),
                estimated_api_cost_increment: Some(EstimatedApiCost::from_picodollars(123)),
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        ),
        record(
            4,
            Event::AgentOuterTurnFinished(AgentOuterTurnFinished {
                agent_id,
                session_id: SessionId::from("s1"),
                outer_turn_id: "ot-ap-engineer_0-0".into(),
                disposition: tau_proto::AgentOuterTurnDisposition::Settled,
            }),
        ),
    ];
    events.insert(
        4,
        record(
            4,
            Event::ProviderToolResult(tau_proto::ToolResult {
                call_id: "call-1".into(),
                tool_name: ToolName::new("shell"),
                tool_type: ToolType::Function,
                result: tau_proto::CborValue::Null,
                provider_content: Vec::new(),
                kind: Default::default(),
                display: None,
                originator: Default::default(),
            }),
        ),
    );
    events.push(record(
        6,
        Event::AgentPromptStarted(AgentPromptStarted {
            agent_prompt_id: "ap-engineer_0-foreign".into(),
            agent_id: AgentId::parse("engineer_0").expect("agent id"),
            session_id: SessionId::from("s2"),
            model: "openai/gpt-5".parse().expect("model id"),
            model_params: Some(ModelParams::default()),
            outer_turn_id: None,
            operation: PromptOperation::StandaloneCompaction,
            originator: Default::default(),
            ctx_id: None,
        }),
    ));
    let mut foreign = match events[3].event.clone() {
        Event::ProviderResponseFinished(response) => response,
        _ => unreachable!("selected response fixture"),
    };
    foreign.agent_prompt_id = "ap-engineer_0-foreign".into();
    foreign.estimated_api_cost_increment = Some(EstimatedApiCost::from_picodollars(999_999));
    events.push(record(7, Event::ProviderResponseFinished(foreign)));
    events.push(record(
        8,
        Event::ProviderToolResult(tau_proto::ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            tool_type: ToolType::Function,
            result: tau_proto::CborValue::Null,
            provider_content: Vec::new(),
            kind: Default::default(),
            display: None,
            originator: Default::default(),
        }),
    ));
    let mut missing = BTreeSet::new();
    let stats = aggregate_agent(
        "s1",
        &AgentId::parse("engineer_0").expect("agent id"),
        &events,
        &mut missing,
    );

    assert!(missing.is_empty());
    assert_eq!(stats.totals.outer_turns_started, 1);
    assert_eq!(stats.totals.outer_turns_finished, 1);
    assert_eq!(stats.totals.inner_turns, 1);
    assert_eq!(stats.totals.cached_input_tokens, 60);
    assert_eq!(stats.totals.uncached_input_tokens, 40);
    assert_eq!(stats.totals.output_tokens, 20);
    assert_eq!(stats.totals.estimated_api_cost.as_picodollars(), 123);
    assert_eq!(stats.models[0].effort, Effort::High);
    assert_eq!(stats.tools[0].calls, 1);
    assert_eq!(stats.totals.tool_results, 1);
}

/// Pre-contract prompt and response records must remain visible as incomplete
/// instead of receiving inferred creator, turn, model-parameter, or cost facts.
#[test]
fn aggregation_reports_legacy_accounting_gaps_without_inference() {
    let agent_id = AgentId::parse("legacy_0").expect("agent id");
    let events = vec![
        record(
            0,
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                creator: None,
                parent_agent: None,
                role: "legacy".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        ),
        record(
            1,
            Event::AgentPromptStarted(AgentPromptStarted {
                agent_prompt_id: "ap-legacy_0-0".into(),
                agent_id,
                session_id: SessionId::from("s1"),
                model: "openai/gpt-5".parse().expect("model id"),
                model_params: None,
                outer_turn_id: None,
                operation: PromptOperation::Inference,
                originator: Default::default(),
                ctx_id: None,
            }),
        ),
    ];
    let mut missing = BTreeSet::new();
    let stats = aggregate_agent(
        "s1",
        &AgentId::parse("legacy_0").expect("agent id"),
        &events,
        &mut missing,
    );

    assert_eq!(stats.totals.inner_turns, 1);
    assert_eq!(stats.totals.outer_turns_started, 0);
    assert!(stats.models.is_empty());
    assert_eq!(missing.len(), 3);
}

/// A membership reference without its authoritative agent journal must produce
/// an explicitly incomplete public report without creating an agents directory.
#[test]
fn persisted_traversal_reports_missing_member_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions_dir = temp.path().join("state").join("sessions");
    let mut sessions = SessionStore::open(&sessions_dir).expect("session store");
    sessions
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: SessionId::from("s1"),
                agent_id: AgentId::parse("missing_0").expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("membership");
    drop(sessions);

    let report = read_session_stats(&sessions_dir, "s1")
        .expect("stats")
        .expect("session");
    assert!(!report.complete);
    assert_eq!(
        report.missing_data[0].fact,
        super::MissingAccountingFact::AgentJournalMissing
    );
    assert!(!temp.path().join("state").join("agents").exists());
}
