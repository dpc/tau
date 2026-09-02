use super::*;

fn id(byte: u8) -> ObservationId {
    ObservationId::from_bytes([byte; 16])
}

fn fact(byte: u8, seq: u64, kind: FactKind) -> Fact {
    Fact {
        seq: PersistedAgentEventSeq::new(seq),
        observation_id: id(byte),
        at: UnixMicros::new(seq + 10),
        clock_regressions: 0,
        kind,
    }
}

fn wait_facts(activation_kind: FactKind) -> Vec<Fact> {
    let wait_call = ToolCallRef {
        declaration: id(1),
        item_index: 0,
    };
    vec![
        fact(
            1,
            0,
            FactKind::Declaration(vec![(wait_call, ToolCallId::from("wait-call"))]),
        ),
        fact(
            2,
            1,
            FactKind::WaitObserved(tau_proto::AgentToolWaitObserved {
                wait_call,
                mode: ToolWaitMode::ActivatingInput {
                    effective_timeout_minutes: 5,
                },
            }),
        ),
        fact(
            3,
            2,
            FactKind::WaitRegistered(tau_proto::AgentToolWaitRegistered {
                wait_observation: id(2),
                wait_call,
                mode: ToolWaitMode::ActivatingInput {
                    effective_timeout_minutes: 5,
                },
            }),
        ),
        fact(4, 3, activation_kind),
        fact(
            5,
            4,
            FactKind::CanonicalTerminal {
                call_id: ToolCallId::from("wait-call"),
                phase: Some(ToolSourcePhase::Foreground),
            },
        ),
        fact(
            6,
            5,
            FactKind::WaitSettled(tau_proto::AgentToolWaitSettled {
                wait_observation: id(2),
                wait_call,
                registration: Some(id(3)),
                wait_terminal: id(5),
                outcome: ToolWaitOutcome::InputAvailable { activation: id(4) },
            }),
        ),
    ]
}

fn call(byte: u8) -> ToolCallRef {
    ToolCallRef {
        declaration: id(byte),
        item_index: 0,
    }
}

fn settlement(registration: bool, outcome: ToolWaitOutcome) -> tau_proto::AgentToolWaitSettled {
    tau_proto::AgentToolWaitSettled {
        wait_observation: id(2),
        wait_call: call(1),
        registration: registration.then(|| id(3)),
        wait_terminal: id(5),
        outcome,
    }
}

/// Typed activating-input waits expose only effective timeout and validated
/// activation metadata with an exact top-level key set.
#[test]
fn wait_projection_is_content_free_and_exact() {
    let agent_id = AgentId::parse("agent-wait").expect("agent id");
    let facts = wait_facts(FactKind::Activation(
        tau_proto::ActivationKind::WatchNotification,
    ));
    let by_id = observation_index(&agent_id, &facts).expect("observation index");
    let rows = wait_rows(&agent_id, Some(UnixMicros::new(10)), &facts, &by_id).expect("wait rows");
    let row = rows[0].value.as_object().expect("wait row");
    assert_eq!(row["mode"], "activating_input");
    assert_eq!(row["effective_timeout_minutes"], 5);
    assert_eq!(row["registration"], "active");
    assert_eq!(row["outcome"], "input_available");
    assert_eq!(row["activation_kind"], "watch_notification");
    assert!(!row.contains_key("requested_timeout_minutes"));
    assert_eq!(
        row.keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "record_type",
            "agent_id",
            "journal_seq",
            "observed_at_us",
            "wait_call",
            "mode",
            "effective_timeout_minutes",
            "registration",
            "terminal_journal_seq",
            "terminal_at_us",
            "outcome",
            "activation",
            "activation_kind",
            "active_wait_us",
            "activation_to_wait_terminal_us",
        ])
    );
}

/// A selected unrelated event cannot masquerade as an activation endpoint.
#[test]
fn wait_projection_rejects_wrong_activation_event_type() {
    let agent_id = AgentId::parse("agent-wait").expect("agent id");
    let facts = wait_facts(FactKind::Other);
    let by_id = observation_index(&agent_id, &facts).expect("observation index");
    let error = match wait_rows(&agent_id, Some(UnixMicros::new(10)), &facts, &by_id) {
        Err(error) => error,
        Ok(_) => panic!("wrong activation type must fail"),
    };
    assert!(error.to_string().contains("wrong event type"));
}

/// The performance projection shares compact trace's exact mode, envelope,
/// phase, rejection, cancellation, and registration settlement matrix.
#[test]
fn wait_mode_outcome_matrix_matches_runtime_semantics() {
    use tau_proto::{
        ToolOutputEnvelope as Envelope, ToolSourcePhase as Phase, WaitRejectionReason as Reject,
    };
    let exact = ToolWaitMode::Exact { target: call(9) };
    let completion = |source_call, source_phase, envelope| ToolWaitOutcome::CompletionDelivered {
        source_call,
        source_terminal: id(8),
        source_phase,
        envelope,
    };
    assert!(wait_mode_allows_outcome(
        &exact,
        &settlement(
            false,
            completion(call(9), Phase::Foreground, Envelope::Identity),
        )
    ));
    assert!(!wait_mode_allows_outcome(
        &exact,
        &settlement(
            false,
            completion(
                call(8),
                Phase::Foreground,
                Envelope::OriginalToolCallIdHeader,
            ),
        )
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::NextBackground,
        &settlement(
            false,
            completion(
                call(9),
                Phase::Background,
                Envelope::OriginalToolCallIdHeader,
            ),
        )
    ));
    assert!(!wait_mode_allows_outcome(
        &ToolWaitMode::NextBackground,
        &settlement(
            false,
            completion(call(9), Phase::Foreground, Envelope::Identity),
        )
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::ActivatingInput {
            effective_timeout_minutes: 5,
        },
        &settlement(true, ToolWaitOutcome::TimedOut)
    ));
    assert!(!wait_mode_allows_outcome(
        &ToolWaitMode::ActivatingInput {
            effective_timeout_minutes: 5,
        },
        &settlement(
            true,
            ToolWaitOutcome::InterruptedByActivation { activation: id(4) },
        )
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::InvalidArguments,
        &settlement(
            false,
            ToolWaitOutcome::Rejected {
                reason: Reject::InvalidArguments,
            },
        )
    ));
    assert!(!wait_mode_allows_outcome(
        &ToolWaitMode::InvalidArguments,
        &settlement(
            true,
            ToolWaitOutcome::Rejected {
                reason: Reject::InvalidArguments,
            },
        )
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::ExactUnresolved,
        &settlement(
            false,
            ToolWaitOutcome::Rejected {
                reason: Reject::UnknownTarget,
            },
        )
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::NextBackground,
        &settlement(
            false,
            ToolWaitOutcome::Rejected {
                reason: Reject::NoBackgroundCandidate,
            },
        )
    ));
    assert!(!wait_mode_allows_outcome(
        &exact,
        &settlement(false, ToolWaitOutcome::Cancelled)
    ));
    assert!(wait_mode_allows_outcome(
        &exact,
        &settlement(true, ToolWaitOutcome::Cancelled)
    ));
    assert!(wait_mode_allows_outcome(
        &ToolWaitMode::ActivatingInput {
            effective_timeout_minutes: 5,
        },
        &settlement(true, ToolWaitOutcome::LifecycleAborted)
    ));
}

/// Performance JSONL preserves exact-all target and delivered-source request
/// order rather than reducing plural correlation to an unordered set.
#[test]
fn plural_wait_performance_fields_preserve_request_order() {
    let targets = vec![call(2), call(1)];
    let sources = vec![
        tau_proto::WaitDeliveredSource {
            source_call: call(2),
            source_terminal: id(4),
            source_phase: ToolSourcePhase::Background,
            envelope: tau_proto::ToolOutputEnvelope::Identity,
        },
        tau_proto::WaitDeliveredSource {
            source_call: call(1),
            source_terminal: id(3),
            source_phase: ToolSourcePhase::Foreground,
            envelope: tau_proto::ToolOutputEnvelope::Identity,
        },
    ];
    let mut row = Map::new();
    add_wait_mode(
        &mut row,
        &ToolWaitMode::ExactAll {
            targets: targets.clone(),
        },
    );
    add_wait_outcome(
        &mut row,
        &settlement(
            false,
            ToolWaitOutcome::CompletionsDelivered {
                sources: sources.clone(),
            },
        ),
        &HashMap::new(),
    )
    .expect("plural outcome");
    assert_eq!(row["mode"], json!("exact_all"));
    assert_eq!(
        row["target_calls"],
        serde_json::to_value(targets).expect("targets serialize")
    );
    assert_eq!(row["outcome"], json!("completions_delivered"));
    assert_eq!(
        row["sources"],
        serde_json::to_value(sources).expect("sources serialize")
    );
}

/// Matched outer-turn boundaries expose only durable identifiers, status,
/// decision presence, and qualified timing.
#[test]
fn outer_turn_projection_has_exact_content_free_keys() {
    let agent_id = AgentId::parse("agent-turn").expect("agent id");
    let outer_turn_id = AgentOuterTurnId::parse("ot-prompt-turn-1").expect("turn id");
    let prompt_id = AgentPromptId::parse("prompt-turn").expect("prompt id");
    let facts = vec![
        fact(
            1,
            0,
            FactKind::OuterStarted {
                outer_turn_id: outer_turn_id.clone(),
                agent_prompt_id: prompt_id,
            },
        ),
        fact(
            2,
            1,
            FactKind::OuterFinished {
                outer_turn_id,
                automatic_compaction_decision_present: true,
            },
        ),
    ];
    let rows = outer_turn_rows(&agent_id, Some(UnixMicros::new(10)), &facts).expect("turn rows");
    let row = rows[0].value.as_object().expect("turn row");
    assert_eq!(row["status"], "settled");
    assert_eq!(
        row.keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "record_type",
            "agent_id",
            "journal_seq",
            "started_at_us",
            "outer_turn_id",
            "agent_prompt_id",
            "status",
            "terminal_journal_seq",
            "terminal_at_us",
            "recorded_at_wall_elapsed_us",
            "automatic_compaction_decision_present",
        ])
    );
}

/// Open dispatch and outer-turn boundaries remain explicit incomplete rows
/// without inventing a terminal, cause, or timing endpoint.
#[test]
fn incomplete_tool_and_outer_turn_rows_are_exact() {
    let agent_id = AgentId::parse("agent-incomplete").expect("agent id");
    let tool_call = call(1);
    let outer_turn_id = AgentOuterTurnId::parse("ot-incomplete").expect("turn id");
    let facts = vec![
        fact(
            1,
            0,
            FactKind::Declaration(vec![(tool_call, ToolCallId::from("incomplete-call"))]),
        ),
        fact(
            2,
            1,
            FactKind::Dispatch(tau_proto::AgentToolDispatchObserved { call: tool_call }),
        ),
        fact(
            3,
            2,
            FactKind::OuterStarted {
                outer_turn_id: outer_turn_id.clone(),
                agent_prompt_id: AgentPromptId::parse("prompt-incomplete").expect("prompt id"),
            },
        ),
    ];
    let by_id = observation_index(&agent_id, &facts).expect("observation index");
    let tool = tool_rows(&agent_id, Some(UnixMicros::new(10)), &facts, &by_id)
        .expect("tool rows")
        .remove(0)
        .value;
    assert_eq!(tool["status"], "incomplete");
    assert!(tool.get("cause").is_none());
    assert!(tool.get("terminal_journal_seq").is_none());
    assert_eq!(
        tool.as_object()
            .expect("tool object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "record_type",
            "agent_id",
            "call",
            "journal_seq",
            "dispatch_at_us",
            "status",
        ])
    );

    let outer = outer_turn_rows(&agent_id, Some(UnixMicros::new(10)), &facts)
        .expect("outer rows")
        .remove(0)
        .value;
    assert_eq!(outer["status"], "incomplete");
    assert!(outer.get("terminal_journal_seq").is_none());
    assert!(outer.get("automatic_compaction_decision_present").is_none());
}

/// Successful standalone transactions keep multiple attempts exactly once and
/// expose no provider endpoint, transport, or payload metadata.
#[test]
fn standalone_success_preserves_multi_attempt_accounting_without_backend() {
    let agent_id = AgentId::parse("agent-compact").expect("agent id");
    let transaction_id = CompactionTransactionId::parse("tx-success").expect("transaction id");
    let prompt_id = AgentPromptId::parse("prompt-compact").expect("prompt id");
    let attempt = |logical_attempt| AttemptData {
        prompt_id: prompt_id.clone(),
        logical_attempt: ProviderAttempt::new(logical_attempt).expect("attempt"),
        transaction_id: transaction_id.clone(),
        model: "provider/private-model".parse().expect("model"),
        usage: AttemptUsage::Unknown,
        cost: None,
        output: tau_proto::StandaloneExecutionOutput::Accepted,
    };
    let facts = vec![
        fact(
            1,
            0,
            FactKind::StandaloneStarted {
                transaction_id: transaction_id.clone(),
                compact_prompt_id: prompt_id.clone(),
                trigger: "manual",
            },
        ),
        fact(2, 1, FactKind::StandaloneAccounted(attempt(1))),
        fact(3, 2, FactKind::StandaloneAccounted(attempt(2))),
        fact(4, 3, FactKind::StandaloneSucceeded(transaction_id.clone())),
    ];
    let row = standalone_rows(&agent_id, Some(UnixMicros::new(10)), &facts)
        .expect("standalone rows")
        .remove(0)
        .value;
    assert_eq!(row["status"], "succeeded");
    assert_eq!(row["attempt_count"], 2);
    assert_eq!(row["attempts"][0]["logical_attempt"], 1);
    assert_eq!(row["attempts"][1]["logical_attempt"], 2);
    assert!(!row.to_string().contains("base_url"));
    assert!(!row.to_string().contains("transport"));
}

/// Distillation drops model attribution, cache observations, cache ceilings,
/// and cumulative per-model stats before the performance fact is retained.
#[test]
fn standalone_usage_distillation_discards_excluded_metadata() {
    let private_model: tau_proto::ModelId = "provider/private-model".parse().expect("model");
    let mut usage = tau_proto::ProviderTokenUsage {
        model: Some(private_model.clone()),
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 20,
        prompt_cache_read_ceiling_tokens: Some(999),
        cache: Some(Box::new(tau_proto::ProviderCacheUsage {
            read_tokens: Some(123),
            write_tokens: Some(456),
            ..Default::default()
        })),
        response_received_tokens: 7,
        ..Default::default()
    };
    usage.stats.start_request(&private_model);
    usage.stats.add_sent(&private_model, 50_000, 40_000);
    usage.stats.add_received(&private_model, 30_000);

    let distilled = AttemptUsage::from(StandaloneExecutionUsage::Known(usage));
    assert!(matches!(
        distilled,
        AttemptUsage::Known {
            prompt_sent_tokens: 10,
            prompt_cached_tokens: 10,
            response_received_tokens: 7,
        }
    ));
}
