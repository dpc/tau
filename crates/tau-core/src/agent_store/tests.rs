use std::cell::Cell;
use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::sync as path_std_sync;
use std::time::Instant;

use super::*;

/// Builds a response with repeated declarations for exact first-occurrence
/// tests.
fn history_response(agent_id: &AgentId, prompt: &str) -> Event {
    let call = tau_proto::ToolCallItem {
        call_id: tau_proto::ToolCallId::new("repeated-call"),
        name: tau_proto::ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: None,
        responses_envelope: None,
    };
    let mut other = call.clone();
    other.call_id = tau_proto::ToolCallId::new("other-call");
    Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
        agent_prompt_id: prompt.parse().expect("prompt id"),
        agent_id: agent_id.clone(),
        output_items: vec![
            tau_proto::ContextItem::ToolCall(other),
            tau_proto::ContextItem::ToolCall(call.clone()),
            tau_proto::ContextItem::ToolCall(call),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        final_status_disposition: tau_proto::FinalStatusDisposition::Accepted,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        provider_attempt: Default::default(),
        automatic_compaction_decision: None,
        originator: Default::default(),
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

/// Gives projection-only test facts stable sequence and declaration identities.
fn history_record(seq: u64, event: Event) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        fold_semantics: AgentJournalFoldSemantics::for_new_event(&event),
        event,
        parent: AgentEventParent::InheritHead,
        recorded_at: UnixMicros::new(seq + 1),
    }
}

/// A finite worker snapshot must merge with the current accepted suffix, not
/// the earlier request cut, without changing live facts or folded authority.
#[test]
fn managed_history_cache_prefetch_merges_current_pinned_suffix() {
    let temp = tempfile::tempdir().expect("temporary root");
    let agent_id = AgentId::parse("history-cache").expect("agent id");
    let owner = Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(temp.path(), owner.clone()).expect("store");
    store.reserve_new_agent(agent_id.as_str()).expect("reserve");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        crate::DurabilityBarrierOutcome::Durable
    );
    let prefix = store
        .agent_history_prefix(&agent_id)
        .expect("capture")
        .expect("readable");
    assert_eq!(prefix.next_seq().get(), 1);
    let first = store.agent_events(agent_id.as_str()).expect("resident");
    store
        .evict_agent_history(&agent_id, Some(1))
        .expect("evict written prefix");
    assert!(store.agent_history_is_evicted(&agent_id));
    assert!(matches!(
        store.agent_events(agent_id.as_str()),
        Err(AgentStoreError::HistoryNotResident { .. })
    ));
    assert_eq!(
        store.agent_history_pin_usage(&agent_id, 1).expect("pin"),
        (0, 0)
    );
    let prefetched = std::thread::spawn(move || prefix.prefetch())
        .join()
        .expect("reader")
        .expect("valid prefix");
    // Accept records after the reader has already produced its result. The
    // result must never replace or omit these current accepted facts.
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "after read"),
        )
        .expect("append while cold");
    store
        .append_agent_message_fact_at(
            agent_id.as_str(),
            None,
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::parse("cache-test").expect("publisher"),
                MessageAgentTarget::new(agent_id.as_str()),
                tau_proto::MessageFactId::new("late-message"),
                tau_proto::MessageParty {
                    stable_id: "sender".into(),
                    display_name: None,
                    sender_auth: None,
                    sender_trust: None,
                },
                None,
                "accepted after reader completion",
            )),
            UnixMicros::new(12),
        )
        .expect("raw append while cold");
    let projection = &store.managed_projections[&agent_id];
    let suffix = projection.history.records.clone();
    let accepted_bytes = projection.history.encoded_event_bytes;
    let nodes = projection.tree.nodes().len();
    assert_eq!(
        store.agent_history_pin_usage(&agent_id, 1).expect("pin"),
        (managed_agent_encoded_event_bytes(&suffix), 2),
    );
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        crate::DurabilityBarrierOutcome::Durable
    );
    // The write frontier overtook the captured cut; the request pin still
    // prevents ordinary eviction from dropping the merge suffix.
    store
        .evict_agent_history(&agent_id, Some(1))
        .expect("preserve request cut");
    assert_eq!(store.managed_projections[&agent_id].history.records, suffix);
    store
        .install_agent_history_prefix(prefetched)
        .expect("atomic cache handoff");
    let expected: Vec<_> = first.into_iter().chain(suffix).collect();
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("complete"),
        expected
    );
    assert!(!store.agent_history_is_evicted(&agent_id));
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(3));
    let projection = &store.managed_projections[&agent_id];
    assert_eq!(projection.history.encoded_event_bytes, accepted_bytes);
    assert_eq!(projection.tree.nodes().len(), nodes);
    assert_eq!(projection.tree.display_name(), Some("after read"));
    store
        .evict_agent_history(&agent_id, None)
        .expect("release pin");
    let history = &store.managed_projections[&agent_id].history;
    assert_eq!(history.first_resident_seq, 3);
    assert_eq!(history.records.capacity(), 0);
    assert_eq!(history.record_bytes.capacity(), 0);
    assert_eq!(history.encoded_event_bytes, accepted_bytes);
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(3));
}

/// A stale completion and a missing protected suffix must fail before changing
/// cache residency; neither can silently install an incomplete history.
#[test]
fn managed_history_cache_rejects_stale_completion_and_missing_suffix() {
    let temp = tempfile::tempdir().expect("temporary root");
    let agent_id = AgentId::parse("history-cache-authority").expect("agent id");
    let owner = Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(temp.path(), owner.clone()).expect("store");
    store.reserve_new_agent(agent_id.as_str()).expect("reserve");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        crate::DurabilityBarrierOutcome::Durable
    );
    let prefix = store
        .agent_history_prefix(&agent_id)
        .expect("capture")
        .expect("readable");
    let old = prefix.prefetch().expect("read on test thread");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "later"),
        )
        .expect("append");
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        crate::DurabilityBarrierOutcome::Durable
    );
    // Deliberately violate the caller's pin contract to exercise the handoff
    // invariant check rather than producing a successful partial history.
    store.evict_agent_history(&agent_id, None).expect("evict");
    assert!(matches!(
        store.install_agent_history_prefix(old),
        Err(AgentStoreError::InvalidEvent { .. })
    ));
    assert!(store.agent_history_is_evicted(&agent_id));
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(2));
    assert_eq!(
        store.agent(agent_id.as_str()).expect("tree").display_name(),
        Some("later")
    );
    let stale = store
        .agent_history_prefix(&agent_id)
        .expect("capture")
        .expect("readable")
        .prefetch()
        .expect("read on test thread");
    store
        .release_managed_agents(Duration::from_secs(2))
        .expect("release");
    store
        .prepare_existing_agent(agent_id.as_str())
        .expect("new lease");
    let recovered = store.agent_events(agent_id.as_str()).expect("recovered");
    assert!(matches!(
        store.install_agent_history_prefix(stale),
        Err(AgentStoreError::Persistence(
            PersistenceAdmissionError::StaleLease
        ))
    ));
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("unchanged"),
        recovered
    );
}

/// Exact facts survive disposal of replay records and retain the first tool
/// declaration, including its item position, instead of recovery's last winner.
#[test]
fn managed_history_facts_do_not_depend_on_resident_records() {
    let agent_id = AgentId::parse("history-facts").expect("agent id");
    let records = vec![
        history_record(0, started_event(&agent_id)),
        history_record(1, history_response(&agent_id, "prompt-12")),
        history_record(2, history_response(&agent_id, "prompt-3")),
    ];
    let mut history = AgentHistory::from_records(records.clone());
    let call_id = tau_proto::ToolCallId::new("repeated-call");
    let expected_ref = tau_proto::ToolCallRef {
        declaration: records[1].observation_id,
        item_index: 1,
    };
    assert_eq!(history.tool_declarations[&call_id], expected_ref);
    assert_eq!(
        AgentHistory::tool_declaration_in(&records, &call_id),
        Some(expected_ref.clone())
    );
    let encoded = history.encoded_event_bytes;
    history.evict_before(history.accepted_count);
    let appended = history_record(3, history_response(&agent_id, "prompt-15"));
    let measured = managed_agent_encoded_event_bytes(std::slice::from_ref(&appended));
    history.push(appended, measured);
    assert_eq!(history.first_record.as_ref(), records.first());
    assert_eq!(history.accepted_count, 4);
    assert_eq!(history.next_prompt_index, 16);
    assert_eq!(history.tool_declarations[&call_id], expected_ref);
    assert_eq!(history.encoded_event_bytes, encoded + measured);
    assert_eq!(history.records.len(), 1);

    let temp = tempfile::tempdir().expect("temporary root");
    let mut store = AgentStore::read_only(temp.path());
    store.managed_projections.insert(
        agent_id.clone(),
        ManagedAgentProjection {
            tree: AgentTree::from_events(agent_id.clone(), &[]),
            summary: AgentSummary::default(),
            history,
        },
    );
    assert!(store.agent_has_committed_identity(&agent_id));
    assert_eq!(
        store.agent_started_role(agent_id.as_str()).expect("role"),
        Some("engineer".into())
    );
    assert_eq!(
        store
            .loaded_agent_creation(&agent_id)
            .expect("creation")
            .0
            .role,
        "engineer"
    );
    assert!(matches!(
        store.agent_creation_facts(&agent_id, facts_budget(MAX_RECORD_BYTES, u64::MAX)),
        Ok(AgentCreationFacts::Available { role, .. }) if role == "engineer"
    ));
    assert_eq!(
        store.loaded_agent_creation_record(&agent_id),
        records.first()
    );
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(4));
    assert_eq!(store.loaded_next_prompt_index(&agent_id), Some(16));
    assert_eq!(
        store.loaded_tool_declaration_ref(&agent_id, &call_id),
        Some(expected_ref)
    );
}

/// Every historical prompt-id family participates in collision avoidance, while
/// malformed numeric suffixes are ignored and the maximum u64 saturates.
#[test]
fn managed_history_prompt_cursor_covers_all_id_families() {
    let agent_id = AgentId::parse("history-cursor").expect("agent id");
    let prompt = |id: &str| {
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            agent_prompt_id: id.parse().expect("prompt id"),
            agent_id: agent_id.clone(),
            session_id: "session".parse().expect("session id"),
            model: "test/model".into(),
            model_params: None,
            outer_turn_id: None,
            operation: tau_proto::PromptOperation::Inference,
            originator: Default::default(),
            ctx_id: None,
        })
    };
    let dispatch = Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
        agent_id: agent_id.clone(),
        transaction_id: None,
        agent_prompt_id: "dispatch-30".parse().expect("prompt id"),
        through: tau_proto::AgentHead::Root,
        model: "test/model".into(),
        operation: tau_proto::PromptOperation::Inference,
        activation_cut: tau_proto::AgentHead::Root,
        output_length_continuation: None,
    });
    let compact =
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: tau_proto::CompactionTransactionId::parse("transaction-40")
                .expect("transaction"),
            compact_prompt_id: "compact-900".parse().expect("prompt id"),
            cut: tau_proto::AgentHead::Root,
            resume_through: None,
            model: "test/model".into(),
            operation: tau_proto::PromptOperation::Inference,
            originator: Default::default(),
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        });
    let mut history = AgentHistory::default();
    for (index, event) in [
        prompt("prompt-10"),
        history_response(&agent_id, "response-20"),
        dispatch,
        compact,
    ]
    .into_iter()
    .enumerate()
    {
        history.push(history_record(index as u64, event), 0);
        assert_eq!(history.next_prompt_index, (index as u64 + 1) * 10 + 1);
        assert_eq!(
            AgentHistory::next_prompt_index_in(&history.records),
            history.next_prompt_index
        );
    }
    history.push(history_record(4, prompt("prompt-not_numeric")), 0);
    assert_eq!(history.next_prompt_index, 41);
    history.push(history_record(5, prompt("prompt-18446744073709551615")), 0);
    assert_eq!(history.next_prompt_index, u64::MAX);
    history.push(history_record(6, prompt("prompt-2")), 0);
    assert_eq!(history.next_prompt_index, u64::MAX);
}

/// An invalid append cannot leak declaration or cursor facts, and successful
/// raw message appends advance the same accepted count used by structured
/// appends.
#[test]
fn managed_history_facts_follow_only_committed_replacements() {
    let temp = tempfile::tempdir().expect("temporary root");
    let agent_id = AgentId::parse("history-admission").expect("agent id");
    let call_id = tau_proto::ToolCallId::new("repeated-call");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let creation = store.loaded_agent_creation_record(&agent_id).cloned();
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(999)),
            history_response(&agent_id, "rejected-90"),
            UnixMicros::new(10),
        )
        .expect_err("invalid parent");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert_eq!(
        store.loaded_agent_creation_record(&agent_id),
        creation.as_ref()
    );
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(1));
    assert_eq!(store.loaded_next_prompt_index(&agent_id), Some(0));
    assert_eq!(store.loaded_tool_declaration_ref(&agent_id, &call_id), None);
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            injected_message_event(&agent_id, "accepted".into()),
        )
        .expect("accepted append");
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(2));
    store
        .append_agent_message_fact_at(
            agent_id.as_str(),
            None,
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::parse("history-test").expect("publisher"),
                MessageAgentTarget::new(agent_id.as_str()),
                tau_proto::MessageFactId::new("message"),
                tau_proto::MessageParty {
                    stable_id: "sender".into(),
                    display_name: None,
                    sender_auth: None,
                    sender_trust: None,
                },
                None,
                "accepted raw fact",
            )),
            UnixMicros::new(12),
        )
        .expect("raw message append");
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(3));
    let records = store.agent_events(agent_id.as_str()).expect("records");
    let replay = AgentHistory::from_records(records);
    assert_eq!(
        store.loaded_agent_record_count(&agent_id),
        Some(replay.accepted_count)
    );
    assert_eq!(
        store.loaded_next_prompt_index(&agent_id),
        Some(replay.next_prompt_index)
    );
    drop(store);
    let owner = Arc::new(
        SemanticPersistenceOwner::new(crate::PersistenceCapacity {
            max_frames: 1,
            ..Default::default()
        })
        .expect("bounded owner"),
    );
    let mut store = AgentStore::open_managed(temp.path(), owner).expect("managed store");
    store
        .prepare_existing_agent(agent_id.as_str())
        .expect("prepare");
    let held = store.persistence_leases[&agent_id]
        .try_reserve_frame()
        .expect("hold sole permit");
    let error = store
        .append_agent_event(
            agent_id.as_str(),
            None,
            history_response(&agent_id, "capacity-rejected-99"),
        )
        .expect_err("no frame capacity");
    assert!(matches!(
        error,
        AgentStoreError::Persistence(PersistenceAdmissionError::Full)
    ));
    assert_eq!(store.loaded_agent_record_count(&agent_id), Some(3));
    assert_eq!(store.loaded_next_prompt_index(&agent_id), Some(0));
    assert_eq!(store.loaded_tool_declaration_ref(&agent_id, &call_id), None);
    drop(held);
}

fn managed_charge_projection(event_count: usize) -> ManagedAgentProjection {
    let agent_id = AgentId::parse("charge-benchmark-agent").expect("agent id");
    let events: Vec<_> = (0..event_count)
        .map(|index| PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([index as u8; 16]),
            seq: PersistedAgentEventSeq::new(index as u64),
            source: None,
            event: Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: format!("record-{index}"),
                inference_activation: false,
                message_class: Default::default(),
            }),
            parent: AgentEventParent::InheritHead,
            fold_semantics: AgentJournalFoldSemantics::CommitOrder,
            recorded_at: UnixMicros::new(index as u64),
        })
        .collect();
    let tree = AgentTree::try_from_events(agent_id, &events).expect("valid benchmark records");
    let mut summary = AgentSummary::default();
    for event in &events {
        summary.apply(event);
    }
    ManagedAgentProjection::from_replay(tree, summary, events)
}

/// The replay-derived aggregate produces the same saturating admission charge
/// as an explicit record scan, including arithmetic overflow.
#[test]
fn managed_projection_cached_charge_matches_full_measurement() {
    for event_count in [0, 1, 17, 257] {
        let projection = managed_charge_projection(event_count);
        let new_record_bytes = usize::MAX / 8;
        let explicit = managed_agent_encoded_event_bytes(&projection.history.records)
            .saturating_add(new_record_bytes)
            .saturating_mul(5)
            .saturating_add(std::mem::size_of::<ManagedAgentProjection>());
        assert_eq!(
            managed_agent_projection_charge(&projection, new_record_bytes),
            explicit,
            "event_count={event_count}"
        );
    }
}

/// Manual asymptotic benchmark reports cached-charge latency across histories
/// from empty to large; it deliberately compares scaling instead of enforcing
/// a flaky wall-clock threshold.
#[test]
#[ignore = "manual managed-charge asymptotic benchmark"]
fn benchmark_managed_projection_cached_charge_scaling() {
    const SAMPLES: usize = 1_000_000;
    for event_count in [0, 16, 1_024, 65_536] {
        let projection = managed_charge_projection(event_count);
        let started = Instant::now();
        let mut checksum = 0;
        for sample in 0..SAMPLES {
            checksum ^= std::hint::black_box(managed_agent_projection_charge(
                std::hint::black_box(&projection),
                sample,
            ));
        }
        eprintln!(
            "managed_charge events={event_count} samples={SAMPLES} elapsed={:?} checksum={checksum}",
            started.elapsed()
        );
    }
}

/// Normal-build inspection state rejects lock/repair mutation without
/// artifacts.
#[test]
fn read_only_agent_store_rejects_recovery_without_mutation() {
    let temp = tempfile::tempdir().expect("temporary root");
    let root = temp.path().join("agents");
    let mut store = AgentStore::read_only(&root);
    let error = store
        .lock_and_recover_agent("missing-agent")
        .expect_err("read-only recovery rejects");
    assert!(error.to_string().contains("unavailable"));
    assert!(!root.exists());
}

fn facts_budget(max_record_bytes: u64, remaining_bytes: u64) -> AgentCreationFactsBudget {
    AgentCreationFactsBudget {
        max_record_bytes,
        remaining_bytes,
    }
}

/// An invalid parent wins over a record-only V1 owner overlap, and each
/// rejected append leaves the journal and cached tree untouched while replay
/// still rejects the corrupted overlap.
#[test]
fn overlapping_v1_owner_append_rejects_before_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("v1-overlap").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("durable store");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                inference_activation: true,
                message_class: Default::default(),
            }),
        )
        .expect("seed head");
    let checkpoint = |prompt: &str| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_id.clone(),
            transaction_id: None,
            agent_prompt_id: tau_proto::AgentPromptId::parse(prompt).expect("prompt id"),
            through: tau_proto::AgentHead::Node(NodeId::new(0)),
            model: "provider/model".into(),
            operation: tau_proto::PromptOperation::Inference,
            activation_cut: tau_proto::AgentHead::Root,
            output_length_continuation: None,
        })
    };
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-first"),
            UnixMicros::new(1),
        )
        .expect("first owner");
    let before = store.agent_events(agent_id.as_str()).expect("events");
    let before_tree = store.agent(agent_id.as_str()).expect("tree").clone();
    let journal = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_bytes = fs::read(&journal).expect("journal bytes");
    let parent_error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(99)),
            checkpoint("ap-invalid-parent"),
            UnixMicros::new(2),
        )
        .expect_err("unknown parent must win over owner overlap");
    assert!(
        parent_error
            .to_string()
            .contains("parent referenced unknown node_id"),
        "event validation must retain precedence over record-only validation"
    );
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    assert_eq!(store.agent(agent_id.as_str()).expect("tree"), &before_tree);
    assert_eq!(fs::read(&journal).expect("journal bytes"), before_bytes);
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::Under(NodeId::new(0)),
            checkpoint("ap-second"),
            UnixMicros::new(2),
        )
        .expect_err("overlap must reject");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    assert_eq!(store.agent(agent_id.as_str()).expect("tree"), &before_tree);
    assert_eq!(fs::read(&journal).expect("journal bytes"), before_bytes);
    drop(store);
    let mut reopened = AgentStore::open_fixture(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("clean reopen");
    assert_eq!(
        reopened.agent_events(agent_id.as_str()).expect("events"),
        before
    );
    let mut overlapping_replay = before.clone();
    overlapping_replay.push(PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([9; 16]),
        seq: PersistedAgentEventSeq::new(2),
        source: None,
        event: checkpoint("ap-replay-overlap"),
        parent: AgentEventParent::Under(NodeId::new(0)),
        fold_semantics: AgentJournalFoldSemantics::InferenceDeferredInputV1,
        recorded_at: UnixMicros::new(3),
    });
    assert!(
        AgentTree::try_from_events(agent_id, &overlapping_replay).is_err(),
        "cold replay must reject overlapping marked owners"
    );
}

/// A store-wide memory-only policy keeps unexpected agent activity replayable
/// without consulting or creating its supplied durable root.
#[test]
fn memory_only_store_never_touches_durable_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("state/agents");
    let seeded_agent_dir = agents_dir.join("seeded-agent");
    fs::create_dir_all(&seeded_agent_dir).expect("seed durable agent root");
    let seeded_journal = seeded_agent_dir.join("events.cbor");
    fs::write(&seeded_journal, b"seeded durable bytes").expect("seed durable journal");
    let mut store = AgentStore::open_memory_only(&agents_dir);
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    assert!(!store.agent_id_is_reserved(agent_id.as_str()));
    assert!(!store.agent_id_is_reserved("seeded-agent"));
    assert!(
        store
            .lock_and_recover_agent("seeded-agent")
            .expect("memory-only recovery is process-local")
            .is_none()
    );

    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends in memory");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "preview"),
        )
        .expect("projection appends in memory");
    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("memory replay")
            .len(),
        2
    );
    assert_eq!(
        store.agent(&agent_id).and_then(AgentTree::display_name),
        Some("preview")
    );
    assert_eq!(
        fs::read(&seeded_journal).expect("seed remains readable"),
        b"seeded durable bytes"
    );
    assert!(!seeded_agent_dir.join("lock").exists());
    assert!(!agents_dir.join(agent_id.as_str()).exists());
}

/// Managed role lookup clones only the creation role rather than the retained
/// owned-payload suffix.
#[test]
fn managed_agent_started_role_ignores_retained_history_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("managed-role").expect("agent id");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation appends");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            injected_message_event(&agent_id, "x".repeat(1024 * 1024)),
        )
        .expect("owned payload appends");

    assert_eq!(
        store
            .agent_started_role(agent_id.as_str())
            .expect("role lookup"),
        Some("engineer".to_owned())
    );
}

/// Cold preparation keeps one authoritative tree, preserves accepted history
/// across appends, and leaves no stale loaded tree after release. Eager
/// fixtures exercise transfer of an inspection tree as well as ordinary managed
/// recovery.
#[test]
fn restored_managed_agent_retains_only_authoritative_tree() {
    for eager_fixture in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        let agent_id = AgentId::parse("single-restored-tree").expect("agent id");
        let accepted = {
            let mut writer = AgentStore::open_fixture(temp.path()).expect("writer");
            for event in [
                started_event(&agent_id),
                injected_message_event(&agent_id, "retained history".repeat(1024)),
                display_name_event(&agent_id, "before recovery"),
            ] {
                writer
                    .append_agent_event(agent_id.as_str(), None, event)
                    .expect("seed history");
            }
            writer
                .agent_events(agent_id.as_str())
                .expect("seed records")
        };
        let mut store = if eager_fixture {
            AgentStore::open_fixture(temp.path()).expect("eager fixture")
        } else {
            let owner = path_std_sync::Arc::new(
                SemanticPersistenceOwner::new(Default::default()).expect("owner"),
            );
            AgentStore::open_managed(temp.path(), owner).expect("managed store")
        };
        store
            .prepare_existing_agent(agent_id.as_str())
            .expect("recover agent");
        store
            .prepare_existing_agent(agent_id.as_str())
            .expect("preparation is idempotent");
        assert!(!store.agents.contains_key(&agent_id));
        assert_eq!(store.managed_projections.len(), 1);
        assert_eq!(
            store
                .agent_events(agent_id.as_str())
                .expect("recovered records"),
            accepted
        );
        let tree = &store.managed_projections[&agent_id].tree;
        assert!(std::ptr::eq(store.agent(agent_id.as_str()).unwrap(), tree));
        assert!(std::ptr::eq(store.agents()[0], tree));
        assert_eq!(tree.display_name(), Some("before recovery"));
        assert_eq!(
            store.agent_started_role(agent_id.as_str()).expect("role"),
            Some("engineer".to_owned())
        );

        store
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(&agent_id, "after recovery"),
            )
            .expect("append to recovered projection");
        assert!(!store.agents.contains_key(&agent_id));
        assert_eq!(
            store.agent(agent_id.as_str()).unwrap().display_name(),
            Some("after recovery")
        );
        assert!(matches!(
            store
                .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
                .expect("roster facts"),
            AgentCreationFacts::Available {
                display_name: Some(display_name),
                ..
            } if display_name == "after recovery"
        ));
        let updated = store
            .agent_events(agent_id.as_str())
            .expect("updated records");
        assert_eq!(&updated[..accepted.len()], accepted.as_slice());
        assert_eq!(updated.len(), accepted.len() + 1);
        store
            .release_managed_agents(Duration::from_secs(2))
            .expect("release drains accepted records");
        assert!(store.agent(agent_id.as_str()).is_none());
        assert!(store.agents().is_empty());
        assert!(store.managed_projections.is_empty());
        assert_eq!(
            store.agent_events(agent_id.as_str()).expect("disk replay"),
            updated
        );
        assert_eq!(
            store
                .load_agent(agent_id.as_str())
                .expect("explicit reload")
                .unwrap()
                .display_name(),
            Some("after recovery")
        );
        assert!(!store.agents.contains_key(&agent_id));
        assert_eq!(
            store
                .agent_events(agent_id.as_str())
                .expect("reloaded records"),
            updated
        );
        let reader = AgentStore::open(temp.path()).expect("read-only inspection");
        assert_eq!(
            reader.agent(agent_id.as_str()).unwrap().display_name(),
            Some("after recovery")
        );
        assert_eq!(
            reader
                .agent_events(agent_id.as_str())
                .expect("cold records"),
            updated
        );
    }
}

/// Newly reserved agents and cold-restored agents expose no loaded tree after
/// their managed generation is released; durable records remain replayable.
#[test]
fn fresh_managed_agent_release_leaves_no_loaded_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let owner =
        path_std_sync::Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(temp.path(), owner).expect("managed store");
    let agent_id = AgentId::parse("fresh-managed-tree").expect("agent id");
    store.reserve_new_agent(agent_id.as_str()).expect("reserve");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("create agent");
    let accepted = store
        .agent_events(agent_id.as_str())
        .expect("accepted records");
    store
        .release_managed_agents(Duration::from_secs(2))
        .expect("release");
    assert!(store.agent(agent_id.as_str()).is_none());
    assert!(store.agents().is_empty());
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("disk replay"),
        accepted
    );
    assert!(
        store
            .load_agent(agent_id.as_str())
            .expect("reload fresh agent")
            .is_some()
    );
    assert!(!store.agents.contains_key(&agent_id));
}

/// Maintenance removal of a recovered projection cannot expose its older
/// preparation tree; snapshot and explicit replay retain the same records.
#[test]
fn restored_managed_snapshot_leaves_no_stale_loaded_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("snapshot-restored-tree").expect("agent id");
    let accepted = {
        let mut writer = AgentStore::open_fixture(temp.path()).expect("writer");
        writer
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("create agent");
        writer
            .agent_events(agent_id.as_str())
            .expect("seed records")
    };
    let owner =
        path_std_sync::Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(temp.path(), owner).expect("managed store");
    store
        .lock_and_recover_agent(agent_id.as_str())
        .expect("recover");
    let snapshot = store
        .capture_managed_snapshot([agent_id.clone()], Duration::from_secs(2))
        .expect("maintenance snapshot");
    assert!(!store.agents.contains_key(&agent_id));
    assert!(store.agent(agent_id.as_str()).is_none());
    assert!(store.agents().is_empty());
    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("snapshot records")
            .collect::<Result<Vec<_>, _>>()
            .expect("read snapshot"),
        accepted
    );
    assert_eq!(
        store.agent_events(agent_id.as_str()).expect("disk replay"),
        accepted
    );
    drop(snapshot);
    assert!(
        store
            .load_agent(agent_id.as_str())
            .expect("reload after maintenance")
            .is_some()
    );
    let leases = store.managed_persistence_leases();
    store
        .persistence_owner
        .as_ref()
        .unwrap()
        .release(&leases, Duration::from_secs(2))
        .expect("group release");
    store.finish_managed_release();
    assert!(store.agent(agent_id.as_str()).is_none());
    assert!(store.agents().is_empty());
    assert!(!store.agents.contains_key(&agent_id));
}

/// A present managed projection without a creation event returns no role and
/// must not fall through to the durable journal.
#[test]
fn managed_agent_started_role_missing_start_does_not_fall_back() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("managed-missing-start").expect("agent id");
    {
        let mut writer = AgentStore::open_fixture(temp.path()).expect("writer opens");
        writer
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation appends");
    }
    let owner = path_std_sync::Arc::new(
        crate::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store = AgentStore::open_managed(temp.path(), owner).expect("managed store");
    store
        .prepare_existing_agent(agent_id.as_str())
        .expect("existing agent prepares");
    store.managed_projections.insert(
        agent_id.clone(),
        ManagedAgentProjection::empty(agent_id.clone()),
    );

    assert_eq!(
        store
            .agent_started_role(agent_id.as_str())
            .expect("role lookup"),
        None
    );
}

/// Memory-only and non-managed durable role lookup retain the full
/// `agent_events` fallback behavior.
#[test]
fn agent_started_role_preserves_existing_fallbacks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let durable_id = AgentId::parse("durable-role").expect("agent id");
    let no_start_id = AgentId::parse("durable-no-start").expect("agent id");
    {
        let mut writer = AgentStore::open_fixture(temp.path()).expect("writer opens");
        writer
            .append_agent_event(durable_id.as_str(), None, started_event(&durable_id))
            .expect("creation appends");
        writer
            .append_agent_event(
                no_start_id.as_str(),
                None,
                injected_message_event(&no_start_id, "no creation record".to_owned()),
            )
            .expect("non-creation event appends");
    }
    let durable = AgentStore::open(temp.path()).expect("read-only store opens");
    assert_eq!(
        durable
            .agent_started_role(durable_id.as_str())
            .expect("durable role lookup"),
        Some("engineer".to_owned())
    );
    assert_eq!(
        durable
            .agent_started_role(no_start_id.as_str())
            .expect("no-start durable lookup"),
        None
    );

    let ephemeral_id = AgentId::parse("ephemeral-role").expect("agent id");
    let mut ephemeral = AgentStore::open_memory_only(temp.path().join("ephemeral"));
    ephemeral
        .append_agent_event(ephemeral_id.as_str(), None, started_event(&ephemeral_id))
        .expect("memory-only creation appends");
    assert_eq!(
        ephemeral
            .agent_started_role(ephemeral_id.as_str())
            .expect("memory-only role lookup"),
        Some("engineer".to_owned())
    );

    assert!(matches!(
        durable.agent_started_role("../invalid"),
        Err(AgentStoreError::InvalidAgentId { .. })
    ));
}

/// Managed role projection preserves recovery's first encountered creation even
/// when it is not sequence zero, without broadening committed routing identity.
#[test]
fn managed_history_role_preserves_nonzero_creation_after_recovery() {
    let temp = tempfile::tempdir().expect("temporary root");
    let agent_id = AgentId::parse("historical-late-creation").expect("agent id");
    let directory = temp.path().join(agent_id.as_str());
    fs::create_dir_all(&directory).expect("agent directory");
    File::create(directory.join("lock")).expect("existing agent lock");
    let mut journal = File::create(directory.join("events.cbor")).expect("journal");
    let mut later = started_event(&agent_id);
    if let Event::AgentStarted(started) = &mut later {
        started.role = "later-role".into();
    }
    for record in [
        history_record(0, display_name_event(&agent_id, "before creation")),
        history_record(1, started_event(&agent_id)),
        history_record(2, later),
    ] {
        let mut encoded = Vec::new();
        ciborium::into_writer(&record, &mut encoded).expect("encode history");
        journal
            .write_all(&(encoded.len() as u64).to_le_bytes())
            .expect("frame length");
        journal.write_all(&encoded).expect("frame");
    }
    drop(journal);
    let observer = AgentStore::open_lazy(temp.path()).expect("observer");
    let expected = observer
        .agent_started_role(agent_id.as_str())
        .expect("observed role");
    assert_eq!(expected, Some("engineer".into()));
    let owner = Arc::new(SemanticPersistenceOwner::new(Default::default()).expect("owner"));
    let mut store = AgentStore::open_managed(temp.path(), owner).expect("managed store");
    store
        .prepare_existing_agent(agent_id.as_str())
        .expect("recover historical stream");
    assert_eq!(
        store
            .agent_started_role(agent_id.as_str())
            .expect("managed role"),
        expected
    );
    store
        .managed_projections
        .get_mut(&agent_id)
        .expect("projection")
        .history
        .evict_before(3);
    assert_eq!(
        store
            .agent_started_role(agent_id.as_str())
            .expect("role without records"),
        expected
    );
    assert!(!store.agent_has_committed_identity(&agent_id));
    assert!(store.loaded_agent_creation_record(&agent_id).is_none());
}

/// Non-managed role lookup validates the complete durable history before
/// exposing an otherwise valid creation role.
#[test]
fn agent_started_role_preserves_non_managed_validation_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("invalid-durable-role").expect("agent id");
    let journal_path;
    {
        let mut writer = AgentStore::open_fixture(temp.path()).expect("writer opens");
        writer
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation appends");
        journal_path = writer.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let duplicate_start = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([9; 16]),
        seq: PersistedAgentEventSeq::new(2),
        source: None,
        event: started_event(&agent_id),
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::new(43),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&duplicate_start, &mut encoded).expect("duplicate start encodes");
    let mut journal = OpenOptions::new()
        .append(true)
        .open(journal_path)
        .expect("journal opens");
    journal
        .write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("frame length appends");
    journal.write_all(&encoded).expect("frame appends");

    let store = AgentStore::open_lazy(temp.path()).expect("lazy store opens");
    let events_error = store
        .agent_events(agent_id.as_str())
        .expect_err("full event load rejects the invalid sequence");
    let role_error = store
        .agent_started_role(agent_id.as_str())
        .expect_err("role lookup preserves the full-load failure");
    assert_eq!(role_error.to_string(), events_error.to_string());
    assert_eq!(
        std::mem::discriminant(&role_error),
        std::mem::discriminant(&events_error)
    );
}

/// Permanent retired-ID tombstones reserve the namespace in discovery and at
/// the durable reservation boundary.
#[test]
fn retired_agent_id_cannot_be_reserved_again() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_id = AgentId::parse("retired-agent").expect("agent id");
    let tombstone = super::retired_agent_tombstone(&agents_dir, &agent_id);
    fs::create_dir_all(tombstone.parent().expect("retired root")).expect("retired root");
    fs::write(&tombstone, []).expect("tombstone");
    let owner = path_std_sync::Arc::new(
        crate::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store = AgentStore::open_managed(&agents_dir, owner).expect("managed store");

    assert!(store.agent_id_is_reserved(agent_id.as_str()));
    assert!(matches!(
        store.reserve_new_agent(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
    assert!(!agents_dir.join(agent_id.as_str()).exists());
}

/// A dangling tombstone symlink still reserves the stable ID for both durable
/// and ephemeral creation paths.
#[cfg(unix)]
#[test]
fn dangling_retired_tombstone_symlink_reserves_agent_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_id = AgentId::parse("dangling-retired").expect("agent id");
    let tombstone = super::retired_agent_tombstone(&agents_dir, &agent_id);
    fs::create_dir_all(tombstone.parent().expect("retired root")).expect("retired root");
    unix_fs::symlink(temp.path().join("missing-target"), &tombstone).expect("dangling tombstone");
    let owner = path_std_sync::Arc::new(
        crate::SemanticPersistenceOwner::new(Default::default()).expect("persistence owner"),
    );
    let mut store = AgentStore::open_managed(&agents_dir, owner).expect("managed store");

    assert!(store.agent_id_is_reserved(agent_id.as_str()));
    assert!(matches!(
        store.reserve_new_agent(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
    assert!(matches!(
        store.mark_agent_ephemeral(agent_id.as_str()),
        Err(AgentStoreError::PersistenceConflict { .. })
    ));
}

/// Returns the CBOR payload size of the representative oversized agent record.
fn encoded_record_length(text_len: usize, seq: u64) -> usize {
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let event = injected_message_event(&agent_id, "x".repeat(text_len));
    let fold_semantics = AgentJournalFoldSemantics::for_new_event(&event);
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([7; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        source: None,
        event,
        parent: AgentEventParent::InheritHead,
        fold_semantics,
        recorded_at: UnixMicros::new(42),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&record, &mut encoded).expect("test record encodes");
    encoded.len()
}

/// Builds a message injection whose text can exercise the encoded record bound.
fn injected_message_event(agent_id: &AgentId, text: String) -> Event {
    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        agent_id: agent_id.clone(),
        text,
        inference_activation: false,
        message_class: Default::default(),
    })
}

/// Produces a message body whose agent record has the requested encoded size.
fn body_for_encoded_length(encoded_length: usize, seq: u64) -> String {
    const PROBE_BODY_BYTES: usize = 1024 * 1024;
    let overhead = encoded_record_length(PROBE_BODY_BYTES, seq) - PROBE_BODY_BYTES;
    let body = "x".repeat(encoded_length - overhead);
    assert_eq!(
        encoded_record_length(body.len(), seq),
        encoded_length,
        "large-record CBOR overhead should remain stable"
    );
    body
}

/// An oversized durable append leaves the journal, cached fold, sequence, and
/// checkpoint unchanged, then permits a retry at the rejected sequence.
#[test]
fn oversized_agent_append_is_atomic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    let checkpoint_path = store.agent_dir(agent_id.as_str()).join("meta.json");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let checkpoint_before = fs::read(&checkpoint_path).expect("baseline checkpoint");
    let events_before = store
        .agent_events(agent_id.as_str())
        .expect("baseline events");
    let tree_before = store
        .agent(agent_id.as_str())
        .expect("baseline tree")
        .clone();
    let oversized_body = body_for_encoded_length((MAX_RECORD_BYTES + 1) as usize, 1);

    let error = store
        .append_agent_event_at_with_observation_id(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            injected_message_event(&agent_id, oversized_body),
            UnixMicros::new(42),
            tau_proto::ObservationId::from_bytes([7; 16]),
        )
        .expect_err("oversized record must fail");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
    assert_eq!(
        fs::read(&journal_path).expect("journal remains"),
        journal_before
    );
    assert_eq!(
        fs::read(&checkpoint_path).expect("checkpoint remains"),
        checkpoint_before
    );
    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("events remain"),
        events_before
    );
    let tree = store.agent(agent_id.as_str()).expect("tree remains");
    assert_eq!(tree, &tree_before);
    assert_eq!(tree.head(), tree_before.head());
    assert_eq!(tree.next_event_seq(), tree_before.next_event_seq());

    let retry = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            display_name_event(&agent_id, "retry"),
            UnixMicros::new(43),
        )
        .expect("later bounded record appends");
    assert_eq!(retry.seq, PersistedAgentEventSeq::new(1));
    drop(store);

    let mut reopened = AgentStore::open_fixture(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("replay succeeds");
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events"),
        vec![
            events_before[0].clone(),
            PersistedAgentEvent {
                observation_id: retry.observation_id,
                seq: retry.seq,
                source: None,
                event: display_name_event(&agent_id, "retry"),
                parent: AgentEventParent::InheritHead,
                fold_semantics: AgentJournalFoldSemantics::CommitOrder,
                recorded_at: UnixMicros::new(43),
            },
        ]
    );
}

/// A later semantic append and fold complete while prior journal sync remains

/// A first durable append creates and locks its missing branch, while later
/// appends reuse that retained ownership and preserve the journal sequence.
#[test]
fn durable_repeated_append_reuses_first_append_branch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let agent_dir = temp.path().join(agent_id.as_str());
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");

    assert!(!agent_dir.exists(), "first append must create the branch");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("first append creates and locks branch");
    assert!(
        agent_dir.join("lock").is_file(),
        "first append creates lock"
    );
    assert!(
        agent_dir.join("events.cbor").is_file(),
        "first append creates journal"
    );

    let second = store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "warm"),
        )
        .expect("warm append reuses retained branch lock");
    assert_eq!(second.seq, PersistedAgentEventSeq::new(1));

    drop(store);
    let mut reopened = AgentStore::open_fixture(temp.path()).expect("reopen store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("replay journal");
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events")
            .len(),
        2
    );
}

/// A later writable lifetime re-covers both an existing store root and its

/// The roster enrichment path reads immutable creation fields and the latest
/// in-memory display projection without scanning transcript history.
#[test]
fn creation_facts_read_valid_first_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: Some(AgentId::parse("parent").expect("parent id")),
                role: "engineer".to_owned(),
                display_name: Some("Initial".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "Latest".to_owned(),
            }),
            UnixMicros::new(43),
        )
        .expect("name appends");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("within budget");

    assert!(matches!(
        facts,
        AgentCreationFacts::Available {
            started_at: Some(started_at),
            role,
            display_name: Some(display_name),
            ..
        } if started_at == UnixMicros::new(42)
            && role == "engineer"
            && display_name == "Latest"
    ));
}

/// Cold roster enrichment accepts a journal-bound checkpoint but suppresses a

/// Missing journals remain categorical rows rather than becoming I/O errors.
#[test]
fn creation_facts_report_missing_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("missing").expect("agent id");

    let facts = store
        .agent_creation_facts(&agent_id, facts_budget(256 * 1024, 4 * 1024 * 1024))
        .expect("missing does not consume budget");

    assert_eq!(facts, AgentCreationFacts::Missing);
    assert_eq!(facts.bytes_read(), 0);
}

/// Aggregate roster enrichment fails before allocating a first record that does
/// not fit the caller's remaining budget.
#[test]
fn creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    let result = store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1));

    assert_eq!(result, Err(AgentCreationFactsBudgetExceeded));
}

/// In-memory ephemeral creation projections consume the same aggregate budget
/// even though no journal read is needed.
#[test]
fn ephemeral_creation_facts_enforce_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let agent_id = AgentId::parse("ephemeral").expect("agent id");
    store
        .mark_agent_ephemeral(agent_id.as_str())
        .expect("mark ephemeral");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: true,
            }),
            UnixMicros::new(42),
        )
        .expect("creation appends");

    assert_eq!(
        store.agent_creation_facts(&agent_id, facts_budget(256 * 1024, 1)),
        Err(AgentCreationFactsBudgetExceeded)
    );
}

/// Truncated durable records charge their advertised bounded length so repeated
/// malformed members cannot bypass the aggregate roster budget.
#[test]
fn truncated_creation_records_consume_aggregate_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = AgentStore::open_fixture(temp.path()).expect("store opens");
    let record_length = 256 * 1024_u64;
    let mut remaining = 4 * 1024 * 1024_u64;
    for index in 0..17 {
        let agent_id = AgentId::parse(format!("truncated-{index}")).expect("agent id");
        let dir = store.agent_dir(agent_id.as_str());
        fs::create_dir_all(&dir).expect("agent dir");
        let mut bytes = record_length.to_le_bytes().to_vec();
        bytes.resize(8 + record_length as usize - 1, 0);
        fs::write(dir.join("events.cbor"), bytes).expect("truncated journal");

        let facts = store.agent_creation_facts(&agent_id, facts_budget(record_length, remaining));
        if index < 16 {
            let facts = facts.expect("record fits remaining budget");
            assert_eq!(
                facts,
                AgentCreationFacts::Unreadable {
                    bytes_read: record_length
                }
            );
            remaining = remaining.saturating_sub(facts.bytes_read());
        } else {
            assert_eq!(facts, Err(AgentCreationFactsBudgetExceeded));
        }
    }
}

/// A durable agent append failure rolls back the frame, leaves its checkpoint

/// An uncertain agent-journal rollback poisons only that live stream and later

/// A poisoned agent journal keeps its early write rejection ahead of later

/// Read-only replay rejects a partial payload at EOF, while the next locked

/// A complete framed record with a malformed durable controlled identifier is

/// A complete journal frame with a malformed watched-agent status fails strict
/// replay and reports the fixed validation diagnostic without retaining a
/// permissive historical representation.
#[test]
fn strict_replay_rejects_framed_record_with_malformed_watch_work_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let journal_path;
    {
        let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        journal_path = store.agent_dir(agent_id.as_str()).join("events.cbor");
    }
    let valid = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1_u8; 16]),
        seq: PersistedAgentEventSeq::new(1),
        source: None,
        event: Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("message-1").expect("message id"),
            sender_id: AgentId::parse("watched-agent").expect("sender id"),
            sender_session_id: None,
            recipient_id: agent_id.clone(),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
                subscription_id: "watch-1".to_owned(),
                status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
                phase: tau_proto::AgentWorkStatusPhase::Working,
                title: Some("inspect replay".to_owned()),
                initial: false,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            sender_notice: None,
            recipient_notice: None,
            message: String::new(),
        }),
        parent: AgentEventParent::InheritHead,
        fold_semantics: crate::AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::new(42),
    };
    let mut malformed = serde_json::to_value(valid).expect("serialize record value");
    *malformed
        .pointer_mut("/event/payload/watch_work_status/title")
        .expect("watch-status title field") = serde_json::Value::Null;
    let mut encoded = Vec::new();
    ciborium::into_writer(&malformed, &mut encoded).expect("encode malformed framed record");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write frame length");
    file.write_all(&encoded).expect("write complete frame");
    drop(file);

    let error =
        AgentStore::open(temp.path()).expect_err("malformed watch status must fail strict replay");

    assert!(matches!(error, AgentStoreError::Decode { .. }));
    let diagnostic = error.to_string();
    assert!(
        diagnostic.contains("invalid watch work status: work-status title must be absent"),
        "replay diagnostic must identify the rejected watch-status shape: {diagnostic}"
    );
    assert!(
        diagnostic.len() < 512,
        "replay diagnostic must remain bounded: {diagnostic}"
    );
}

/// Builds a canonical uncertainty report without a retained task title.
fn unknown_work_status_event(agent_id: &AgentId) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("unknown-work-status").expect("message id"),
        sender_id: AgentId::parse("watched-agent").expect("sender id"),
        sender_session_id: None,
        recipient_id: agent_id.clone(),
        kind: tau_proto::AgentMessageKind::WatchWorkStatus,
        watch_provider_status: None,
        watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
            session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
            subscription_id: "watch-1".to_owned(),
            status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
            phase: tau_proto::AgentWorkStatusPhase::Unknown,
            title: None,
            initial: false,
        }),
        watch_long_wait: None,
        watch_lifecycle: None,
        sender_notice: None,
        recipient_notice: None,
        message: String::new(),
    })
}

/// Durable append accepts the canonical unknown-without-title work status.
#[test]
fn durable_append_accepts_unknown_watch_work_status_without_title() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let event = unknown_work_status_event(&agent_id);
    let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            started_event(&agent_id),
            UnixMicros::new(41),
        )
        .expect("baseline appends");

    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            event.clone(),
            UnixMicros::new(42),
        )
        .expect("unknown status without title appends");

    assert_eq!(
        store
            .agent_events(agent_id.as_str())
            .expect("durable events")
            .last()
            .expect("watch status event")
            .event,
        event
    );
}

/// Cold replay accepts and restores the canonical unknown-without-title status.
#[test]
fn cold_replay_restores_unknown_watch_work_status_without_title() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    let event = unknown_work_status_event(&agent_id);
    {
        let mut store = AgentStore::open_fixture(temp.path()).expect("store opens");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                started_event(&agent_id),
                UnixMicros::new(41),
            )
            .expect("baseline appends");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                AgentEventParent::InheritHead,
                event.clone(),
                UnixMicros::new(42),
            )
            .expect("unknown status without title appends");
    }

    let mut reopened = AgentStore::open_fixture(temp.path()).expect("cold store opens");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("cold replay accepts unknown status without title");
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events")
            .last()
            .expect("watch status event")
            .event,
        event
    );
}

/// Builds the immutable first event used by durable append tests.
fn started_event(agent_id: &AgentId) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id.clone(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

/// Builds one sequence-advancing event used by durable append tests.
fn display_name_event(agent_id: &AgentId, display_name: &str) -> Event {
    Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
        agent_id: agent_id.clone(),
        display_name: display_name.to_owned(),
    })
}

/// A read-only snapshot of a missing durable agent must fail without creating
/// the requested agent directory or lock file.
#[test]
fn journal_snapshot_does_not_create_missing_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-missing").expect("agent id");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect_err("missing journal must fail");

    assert!(matches!(error, AgentStoreError::JournalMissing { .. }));
    assert!(!temp.path().join(agent_id.as_str()).exists());
}

/// A live writer exposes only its exact checkpoint prefix and remains writable.
#[test]
fn journal_snapshot_uses_lock_held_checkpoint_without_disrupting_writer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");

    let snapshot = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect("active writer exposes committed checkpoint");
    let records = snapshot
        .records(&agent_id)
        .expect("selected journal")
        .collect::<Result<Vec<_>, _>>()
        .expect("valid checkpoint prefix");
    assert_eq!(records.len(), 1);

    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "still-live"),
        )
        .expect("later write survives trace");
}

/// Inactive strict capture derives EOF without trusting a stale checkpoint
/// witness.
#[test]
fn journal_snapshot_uses_inactive_eof_despite_mismatched_checkpoint_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-boundary").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.boundary_blake3_128 = "0".repeat(32);
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    drop(store);
    AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect("inactive snapshot ignores stale checkpoint");
}

/// A live checkpoint with a mismatched replay cursor exhausts bounded
/// observation retries as checkpoint corruption while leaving its writer
/// usable.
#[test]
fn journal_snapshot_rejects_lock_held_checkpoint_sequence_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-active-sequence").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let checkpoint_path = temp.path().join(agent_id.as_str()).join("meta.json");
    let mut checkpoint = read_checkpoint(&checkpoint_path).expect("checkpoint");
    checkpoint.journal.next_seq = checkpoint.journal.next_seq.next();
    fs::write(
        &checkpoint_path,
        serde_json::to_vec(&checkpoint).expect("encode checkpoint"),
    )
    .expect("rewrite checkpoint");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id.clone()])
        .expect_err("live mismatched checkpoint cursor");
    assert!(matches!(
        error,
        AgentStoreError::Read { path, source }
            if path == checkpoint_path && source.kind() == io::ErrorKind::InvalidData
    ));
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "still-live"),
        )
        .expect("writer survives failed trace");
}

/// Checkpoint production and validation share one fallible platform file
/// identity, so a writer can immediately read its own bound checkpoint.
#[test]
fn checkpoint_writer_reader_file_identity_round_trip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-identity-round-trip").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let agent_dir = temp.path().join(agent_id.as_str());
    let mut journal = File::open(agent_dir.join("events.cbor")).expect("journal");

    let checkpoint =
        read_journal_bound_checkpoint(&agent_dir.join("meta.json"), &agent_id, &mut journal)
            .expect("writer checkpoint binds to its journal");

    assert_eq!(checkpoint.agent_id, agent_id);
    assert_eq!(
        checkpoint.journal.covered_bytes,
        journal.metadata().expect("journal metadata").len()
    );
}

/// Live capture retries an inconsistent atomic checkpoint observation and
/// accepts the next exact journal-bound replacement within its fixed attempt
/// budget.
#[test]
fn journal_snapshot_retries_live_checkpoint_atomic_replacement_race() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-race").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    let generation_zero = prepared_snapshot_generation(temp.path(), &agent_id, 0);
    let generation_one = prepared_snapshot_generation(temp.path(), &agent_id, 1);
    install_snapshot_generation(&generation_zero, &agent_dir);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let checkpoint_path = agent_dir.join("meta.json");
    let expected = read_checkpoint(&generation_one.join("meta.json"))
        .expect("replacement checkpoint")
        .journal
        .covered_bytes;
    let attempts = Cell::new(0);
    let mut replacement = Some(generation_one);

    let covered =
        super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |attempt| {
            attempts.set(attempts.get() + 1);
            if attempt == 0 {
                install_snapshot_generation(
                    &replacement.take().expect("one replacement generation"),
                    &agent_dir,
                );
            }
        })
        .expect("second bounded observation succeeds");

    assert_eq!(attempts.get(), 2);
    assert_eq!(covered, expected);
    assert_eq!(
        read_checkpoint(&checkpoint_path)
            .expect("selected replacement checkpoint")
            .journal
            .covered_bytes,
        expected
    );
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_fixture(&target_root).expect("reopen replacement generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-capture"),
        )
        .expect("replacement generation remains appendable");
}

fn prepared_snapshot_generation(root: &Path, agent_id: &AgentId, suffix_events: usize) -> PathBuf {
    let generation_root = root.join(format!("generation-{suffix_events}"));
    let mut store = AgentStore::open_fixture(&generation_root).expect("generation store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
        .expect("generation creation");
    for index in 0..suffix_events {
        store
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(agent_id, &format!("generation-{suffix_events}-{index}")),
            )
            .expect("generation suffix");
    }
    drop(store);
    generation_root.join(agent_id.as_str())
}

fn prepared_collision_generation(root: &Path, agent_id: &AgentId, marker: &str) -> (PathBuf, u64) {
    let generation_root = root.join(format!("collision-{marker}"));
    let mut store = AgentStore::open_fixture(&generation_root).expect("generation store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(agent_id))
        .expect("generation creation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(agent_id, marker),
        )
        .expect("different equal-length prefix");
    let agent_dir = generation_root.join(agent_id.as_str());
    let final_record_offset = read_checkpoint(&agent_dir.join("meta.json"))
        .expect("middle checkpoint")
        .journal
        .covered_bytes;
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(agent_id, &"same-terminal-boundary-".repeat(8)),
        )
        .expect("identical long boundary suffix");
    drop(store);
    (agent_dir, final_record_offset)
}

fn install_snapshot_generation(source: &Path, target: &Path) {
    let checkpoint = read_checkpoint(&source.join("meta.json")).expect("source checkpoint");
    fs::rename(source.join("events.cbor"), target.join("events.cbor"))
        .expect("atomically replace journal generation");
    crate::agent_checkpoint::write_checkpoint_atomic(&target.join("meta.json"), &checkpoint)
        .expect("atomically publish bound checkpoint");
}

/// Equal-length generations with equal replay cursors and boundary witnesses
/// still cannot cross the open/checkpoint observation boundary.
#[test]
fn journal_snapshot_rejects_collision_shaped_generation_crossing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-collision").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    let (generation_zero, zero_final_offset) =
        prepared_collision_generation(temp.path(), &agent_id, "generation-a");
    let (generation_one, one_final_offset) =
        prepared_collision_generation(temp.path(), &agent_id, "generation-b");
    assert_eq!(zero_final_offset, one_final_offset);
    let checkpoint_zero =
        read_checkpoint(&generation_zero.join("meta.json")).expect("old checkpoint");
    let mut checkpoint_one =
        read_checkpoint(&generation_one.join("meta.json")).expect("new checkpoint");
    let old_bytes = fs::read(generation_zero.join("events.cbor")).expect("old journal");
    let mut new_bytes = fs::read(generation_one.join("events.cbor")).expect("new journal");
    new_bytes[usize::try_from(one_final_offset).expect("bounded offset")..]
        .copy_from_slice(&old_bytes[usize::try_from(zero_final_offset).expect("bounded offset")..]);
    fs::write(generation_one.join("events.cbor"), &new_bytes).expect("copy identical final record");
    checkpoint_one.journal.boundary_blake3_128 =
        checkpoint_zero.journal.boundary_blake3_128.clone();
    crate::agent_checkpoint::write_checkpoint_atomic(
        &generation_one.join("meta.json"),
        &checkpoint_one,
    )
    .expect("publish collision-shaped checkpoint");
    assert_eq!(
        checkpoint_zero.journal.covered_bytes,
        checkpoint_one.journal.covered_bytes
    );
    assert_eq!(
        checkpoint_zero.journal.next_seq,
        checkpoint_one.journal.next_seq
    );
    assert_eq!(
        checkpoint_zero.journal.boundary_blake3_128,
        checkpoint_one.journal.boundary_blake3_128
    );
    assert_ne!(old_bytes, new_bytes);
    install_snapshot_generation(&generation_zero, &agent_dir);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let attempts = Cell::new(0);
    let mut replacement = Some(generation_one);

    let covered =
        super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |attempt| {
            attempts.set(attempts.get() + 1);
            if attempt == 0 {
                install_snapshot_generation(
                    &replacement.take().expect("one replacement generation"),
                    &agent_dir,
                );
            }
        })
        .expect("identity mismatch retries to the exact replacement");

    assert_eq!(attempts.get(), 2);
    assert_eq!(covered, checkpoint_one.journal.covered_bytes);
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_fixture(&target_root).expect("reopen replacement generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-collision-capture"),
        )
        .expect("replacement generation remains appendable");
}

/// Live capture attempts three real journal/checkpoint generation crossings
/// before returning a checkpoint-path error.
#[test]
fn journal_snapshot_live_checkpoint_retry_budget_is_bounded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-checkpoint-budget").expect("agent id");
    let target_root = temp.path().join("target");
    let agent_dir = target_root.join(agent_id.as_str());
    fs::create_dir_all(&agent_dir).expect("target agent dir");
    fs::File::create(agent_dir.join("lock")).expect("target lock");
    install_snapshot_generation(
        &prepared_snapshot_generation(temp.path(), &agent_id, 0),
        &agent_dir,
    );
    let mut replacements = (1..=3)
        .map(|suffix| prepared_snapshot_generation(temp.path(), &agent_id, suffix))
        .collect::<VecDeque<_>>();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(agent_dir.join("lock"))
        .expect("target lock");
    fs2::FileExt::lock_exclusive(&lock).expect("hold live writer lock");
    let checkpoint_path = agent_dir.join("meta.json");
    let attempts = Cell::new(0);

    let error = super::snapshot::capture_live_journal_for_test(&agent_dir, &agent_id, |_| {
        attempts.set(attempts.get() + 1);
        install_snapshot_generation(
            &replacements.pop_front().expect("replacement per attempt"),
            &agent_dir,
        );
    })
    .expect_err("retry budget exhausts");

    assert_eq!(attempts.get(), 3);
    assert!(matches!(
        error,
        AgentStoreError::Read { path, source }
            if path == checkpoint_path && source.kind() == io::ErrorKind::InvalidData
    ));
    fs2::FileExt::unlock(&lock).expect("release simulated writer");
    drop(lock);
    let mut store = AgentStore::open_fixture(&target_root).expect("reopen final generation");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            display_name_event(&agent_id, "post-exhaustion"),
        )
        .expect("final replacement remains appendable");
}

/// Inactive capture must acquire the shared lock before selecting EOF, so
/// an append and writer release immediately before acquisition are included.
#[test]
fn journal_snapshot_selects_inactive_eof_after_lock_acquisition() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-race").expect("agent id");
    let mut store = AgentStore::open_fixture(temp.path()).expect("store");
    store
        .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
        .expect("creation");
    let journal_path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let before_append = fs::metadata(&journal_path).expect("journal metadata").len();
    let mut store = Some(store);

    let snapshot = AgentJournalSnapshot::capture_for_test(temp.path(), [agent_id.clone()], |_| {
        store
            .as_mut()
            .expect("writer")
            .append_agent_event(
                agent_id.as_str(),
                None,
                display_name_event(&agent_id, "before-lock"),
            )
            .expect("append before snapshot lock");
        drop(store.take());
    })
    .expect("inactive snapshot after writer release");

    assert!(
        before_append
            < fs::metadata(&journal_path)
                .expect("updated journal metadata")
                .len()
    );
    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("snapshot records")
            .collect::<Result<Vec<_>, _>>()
            .expect("complete locked EOF")
            .len(),
        2
    );
}

/// Snapshot validation must reject a journal whose stored sequence no longer
/// matches its authoritative file position.
#[test]
fn journal_snapshot_rejects_non_monotonic_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-corrupt").expect("agent id");
    {
        let mut store = AgentStore::open_fixture(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    let mut bytes = fs::read(&path).expect("journal");
    let seq = bytes
        .windows(5)
        .position(|window| window == b"\x63seq\x00")
        .map(|offset| offset + 4)
        .expect("sequence encoding");
    bytes[seq] = 1;
    fs::write(&path, bytes).expect("corrupt sequence");

    let error = AgentJournalSnapshot::capture(temp.path(), [agent_id])
        .expect_err("corrupt journal must fail");

    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));
}

/// Snapshot validation must reject a crash-torn frame rather than exporting a
/// valid prefix as though it were the complete journal.
#[test]
fn journal_snapshot_rejects_torn_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agent_id = AgentId::parse("agent-torn").expect("agent id");
    {
        let mut store = AgentStore::open_fixture(temp.path()).expect("store");
        store
            .append_agent_event(agent_id.as_str(), None, started_event(&agent_id))
            .expect("creation");
    }
    let path = temp.path().join(agent_id.as_str()).join("events.cbor");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal")
        .write_all(&[1, 2, 3])
        .expect("torn header");

    let error =
        AgentJournalSnapshot::capture(temp.path(), [agent_id]).expect_err("torn journal must fail");

    assert!(matches!(error, AgentStoreError::Read { .. }));
}
