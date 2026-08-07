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

/// Public reports must render estimated cost as a readable dollar value while
/// retaining exact picodollars in the aggregation model.
#[test]
fn activity_counts_serialize_estimated_cost_as_rounded_dollars() {
    for (picodollars, expected_dollars) in [
        (18_728_643_000_000, serde_json::json!(18.728643)),
        (18_728_643_499_999, serde_json::json!(18.728643)),
        (18_728_643_500_000, serde_json::json!(18.728644)),
        (u64::MAX, serde_json::json!(18_446_744.073_71)),
    ] {
        let counts = super::ActivityCounts {
            estimated_api_cost: EstimatedApiCost::from_picodollars(picodollars),
            ..Default::default()
        };
        let serialized = serde_json::to_value(&counts).expect("serialize activity counts");

        assert_eq!(
            serialized["estimated_api_cost_dollars"], expected_dollars,
            "unexpected rounded dollar value for {picodollars} picodollars"
        );
        assert!(serialized.get("estimated_api_cost_picodollars").is_none());
        assert_eq!(counts.estimated_api_cost.as_picodollars(), picodollars);
    }

    let toon = serde_toon::to_string(&super::SessionStats {
        schema_version: 2,
        session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        complete: true,
        missing_data: Vec::new(),
        totals: super::ActivityCounts {
            estimated_api_cost: EstimatedApiCost::from_picodollars(18_728_643_500_000),
            ..Default::default()
        },
        agents: Vec::new(),
    })
    .expect("serialize activity counts as TOON");
    assert!(toon.contains("schema_version: 2"));
    assert!(toon.contains("estimated_api_cost_dollars: 18.728644"));
    assert!(!toon.contains("estimated_api_cost_picodollars"));
}

fn record(seq: u64, event: Event) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
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
    let prompt_id =
        AgentPromptId::parse("ap-engineer_0-0").expect("known-safe AgentPromptId must be valid");
    let model: tau_proto::ModelId = "openai/gpt-5".parse().expect("model id");
    let rates = EstimatedApiCostRates {
        uncached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
        cached_input: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
        cache_write_input: None,
        output: EstimatedUsdPerMillion::checked_from_usd(1).expect("rate"),
        storage_per_million_token_hour: None,
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
                session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
                outer_turn_id: test_agent_outer_turn_id("ot-ap-engineer_0-0"),
                agent_prompt_id: "ap-engineer_0-0"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                runtime_id: tau_proto::AccountingRuntimeId::parse("runtime-1")
                    .expect("test identifier must be valid"),
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
                session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
                model: model.clone(),
                model_params: Some(ModelParams {
                    effort: Effort::High,
                    ..ModelParams::default()
                }),
                outer_turn_id: Some(test_agent_outer_turn_id("ot-ap-engineer_0-0")),
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
                    prompt_cache_read_ceiling_tokens: None,
                    cache: None,
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
                session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
                outer_turn_id: test_agent_outer_turn_id("ot-ap-engineer_0-0"),
                disposition: tau_proto::AgentOuterTurnDisposition::Settled,
            }),
        ),
    ];
    events.insert(
        4,
        record(
            4,
            Event::ProviderToolResult(tau_proto::ToolResult {
                presentation: Default::default(),
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
            agent_prompt_id: "ap-engineer_0-foreign"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: AgentId::parse("engineer_0").expect("agent id"),
            session_id: SessionId::parse("s2").expect("known-safe SessionId must be valid"),
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
    foreign.agent_prompt_id = "ap-engineer_0-foreign"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    foreign.estimated_api_cost_increment = Some(EstimatedApiCost::from_picodollars(999_999));
    events.push(record(7, Event::ProviderResponseFinished(foreign)));
    events.push(record(
        8,
        Event::ProviderToolResult(tau_proto::ToolResult {
            presentation: Default::default(),
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
                agent_prompt_id: "ap-legacy_0-0"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
                agent_id,
                session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
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

/// Complete present-zero accounting contributes exact zero without reporting
/// an unavailable accounting gap.
#[test]
fn aggregation_treats_present_zero_accounting_as_complete() {
    let agent_id = AgentId::parse("accounting_0").expect("agent id");
    let model: tau_proto::ModelId = "openai/gpt-5".parse().expect("model");
    let prompt = |id: &str| {
        Event::AgentPromptStarted(AgentPromptStarted {
            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id.clone(),
            session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
            model: model.clone(),
            model_params: Some(ModelParams::default()),
            outer_turn_id: None,
            operation: PromptOperation::Inference,
            originator: Default::default(),
            ctx_id: None,
        })
    };
    let response = |id: &str,
                    usage: Option<ProviderTokenUsage>,
                    rates: Option<tau_proto::EstimatedApiCostRates>,
                    cost: Option<EstimatedApiCost>| {
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: agent_id.clone(),
            output_items: Vec::new(),
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: Default::default(),
            originator: Default::default(),
            usage,
            estimated_api_cost_rates: rates,
            estimated_api_cost_increment: cost,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        })
    };
    let zero_usage = ProviderTokenUsage::default();
    let zero_rates = tau_proto::ESTIMATED_API_COST_FALLBACK;
    let events = vec![
        record(0, prompt("zero")),
        record(
            1,
            response(
                "zero",
                Some(zero_usage),
                Some(zero_rates),
                Some(EstimatedApiCost::from_picodollars(0)),
            ),
        ),
    ];
    let mut missing = BTreeSet::new();
    let stats = aggregate_agent("s1", &agent_id, &events, &mut missing);

    assert_eq!(stats.totals.cached_input_tokens, 0);
    assert_eq!(stats.totals.uncached_input_tokens, 0);
    assert_eq!(stats.totals.output_tokens, 0);
    assert_eq!(stats.totals.estimated_api_cost.as_picodollars(), 0);
    assert!(
        !missing
            .iter()
            .any(|gap| gap.fact == super::MissingAccountingFact::ResponseEstimatedCost)
    );
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
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: SessionId::parse("s1").expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("missing_0").expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("membership");
    drop(sessions);

    let report = read_session_stats(
        &sessions_dir,
        &tau_proto::SessionId::parse("s1").expect("session id"),
    )
    .expect("stats")
    .expect("session");
    assert_eq!(report.schema_version, 2);
    assert!(!report.complete);
    assert_eq!(
        report.missing_data[0].fact,
        super::MissingAccountingFact::AgentJournalMissing
    );
    assert!(!temp.path().join("state").join("agents").exists());
}

/// Builds a validated agent outer turn id used by this test module.
fn test_agent_outer_turn_id(value: impl AsRef<str>) -> tau_proto::AgentOuterTurnId {
    tau_proto::AgentOuterTurnId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}
