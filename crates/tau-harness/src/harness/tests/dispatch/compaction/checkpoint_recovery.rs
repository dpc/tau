//! End-to-end harness ownership checks for successful standalone checkpoints.

use std::fs::File;

use super::*;
use crate::harness::RuntimeEventWait;

/// Runs automatic compaction through a real terminal handler, retaining a late
/// suffix, then exercises the checkpoint's live or cold recovery owner.
fn check_checkpoint_recovery(recovery: &str) {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let cid = ensure_test_user_agent(&mut h);
    establish_exact_provider_usage(&mut h, &cid, 900);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("owed activation".to_owned()))
        .expect("start automatic compaction");
    let compact = read_nth_prompt_created(&h, 1);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let started = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .expect("automatic start");
    assert!(
        started
            .resume_through
            .is_some_and(|head| head != tau_proto::AgentHead::Root)
    );
    append_byte_fit_text(&mut h, &cid, "retained late suffix".to_owned());
    let persistence = h
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("owner")
        .clone();
    if recovery == "physical" {
        let blocker = h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        );
        assert_eq!(
            persistence.wait_for_latest_durability_for_test(Duration::from_secs(5)),
            tau_core::DurabilityBarrierOutcome::Durable
        );
        super::standalone_accounting::drain_actual_persistence_commands(&mut h);
        persistence.pause_frame_writes_for_test(true);
        append_byte_fit_text(&mut h, &blocker, "b".repeat(1024 * 1024));
    }
    connect_test_tool(&mut h, "checkpoint-cut");
    h.handle_extension_event(
        "checkpoint-cut",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept checkpoint");
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("balanced compact response"),
    )
    .expect("sole successful compaction terminal");
    let checkpoint = match &h
        .runtime_io
        .publication
        .pending_intercept
        .as_ref()
        .expect("checkpoint intercepted")
        .event
    {
        Event::AgentInferenceDispatchStarted(checkpoint) => checkpoint.clone(),
        other => panic!("wrong intercepted event: {other:?}"),
    };
    assert_eq!(
        checkpoint.transaction_id.as_ref(),
        Some(&started.transaction_id)
    );
    assert_eq!(checkpoint.model, started.model);
    assert_eq!(checkpoint.activation_cut, started.cut);
    if recovery == "physical" {
        // The accepted blocker and terminal frames remain charged. Reject only
        // the checkpoint; retiring those frames must supply the recovery edge.
        persistence.limit_available_bytes_for_test(0);
    } else if recovery != "unrejected" {
        reject_next_semantic_admission(&h);
    }
    h.handle_extension_event(
        "checkpoint-cut",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release checkpoint into admission");
    h.handle_disconnect(&crate::test_connection_id("checkpoint-cut"));
    let checkpoints = |h: &Harness| {
        h.session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("canonical records")
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentInferenceDispatchStarted(owner)
                    if owner.transaction_id.as_ref() == Some(&started.transaction_id) =>
                {
                    Some(owner.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(checkpoints(&h).len(), usize::from(recovery == "unrejected"));
    let mut cid = cid;
    if recovery == "cold" {
        h.publish_for_agent(
            &cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: tau_proto::AgentHead::Root,
            }),
        );
        let cut = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("complete crash prefix")
            .to_vec();
        h.shutdown()
            .expect("close fixture before restoring crash prefix");
        // Restore only the complete accepted prefix, not shutdown's cleanup.
        let journal_path = state_dir
            .join("agents")
            .join(agent_id.as_str())
            .join("events.cbor");
        let mut journal = File::create(journal_path).expect("rewrite crash cut");
        for record in &cut {
            let mut encoded = Vec::new();
            ciborium::into_writer(record, &mut encoded).expect("encode record");
            journal
                .write_all(&(encoded.len() as u64).to_le_bytes())
                .expect("length");
            journal.write_all(&encoded).expect("record");
        }
        journal.sync_all().expect("sync crash cut");
        h = quiet_provider_harness_with_start_reason(
            &state_dir,
            tau_proto::SessionStartReason::Resume,
        )
        .expect("cold restore and discovery");
        cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
        assert!(checkpoints(&h).is_empty(), "off-branch discovery must wait");
        h.retry_pending_agent_publications();
        h.handle_publication_capacity_ready();
        assert!(checkpoints(&h).is_empty(), "off-branch wakes must wait");
        assert!(matches!(
            &h.agent_runtime.agent_registry.agents[&cid].dispatch.activation_dispatch,
            crate::agent::ActivationDispatchState::AwaitingCheckpoint { through, .. }
                if through == &checkpoint.through
        ));
        h.publish_for_agent(
            &cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: checkpoint.through,
            }),
        );
    } else if recovery == "physical" {
        assert!(super::standalone_accounting::drain_actual_persistence_commands(&mut h) > 0);
        assert!(
            checkpoints(&h).is_empty(),
            "Full notification cannot invent capacity"
        );
        h.retry_pending_agent_publications();
        super::standalone_accounting::drain_actual_persistence_commands(&mut h);
        assert!(
            checkpoints(&h).is_empty(),
            "unchanged pressure retains the owner"
        );
        persistence.pause_frame_writes_for_test(false);
        assert_eq!(
            persistence.wait_for_latest_durability_for_test(Duration::from_secs(5)),
            tau_core::DurabilityBarrierOutcome::Durable
        );
        assert!(super::standalone_accounting::drain_actual_persistence_commands(&mut h) > 0);
    } else if recovery == "progress" {
        h.retry_pending_agent_publications();
    } else {
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .signal_capacity_ready_for_test();
        let RuntimeEventWait::Event(event) = h.next_runtime_event() else {
            panic!("capacity wake must reach runtime");
        };
        assert!(matches!(
            event,
            HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress)
        ));
        h.handle_runtime_event(event, &mut 0)
            .expect("capacity wake");
    }
    let committed = checkpoints(&h);
    assert_eq!(committed.len(), 1, "recovery={recovery}");
    let owner = &committed[0];
    assert_eq!(owner.through, checkpoint.through);
    assert_eq!(owner.model, checkpoint.model);
    assert_eq!(owner.activation_cut, checkpoint.activation_cut);
    if recovery != "cold" {
        assert_eq!(owner.agent_prompt_id, checkpoint.agent_prompt_id);
    }
    for _ in 0..2 {
        h.retry_pending_agent_publications();
        h.handle_publication_capacity_ready();
    }
    assert_eq!(
        checkpoints(&h),
        committed,
        "retry must not duplicate dispatch"
    );
    let events = event_log_events(&h);
    let prompts = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.agent_prompt_id == owner.agent_prompt_id =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    let context = serde_json::to_string(&prompts[0].context).expect("context");
    assert_eq!(context.matches("strict fake replacement").count(), 1);
    assert_eq!(context.matches("retained late suffix").count(), 1);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("records");
    assert_eq!(records.iter().filter(|record| matches!(
        &record.event, Event::AgentCompacted(c) if c.transaction_id == started.transaction_id
    )).count(), 1);
    assert_eq!(records.iter().filter(|record| matches!(
        &record.event, Event::AgentStandaloneCompactionStarted(s) if s.transaction_id == started.transaction_id
    )).count(), 1);
    let cold = tau_core::AgentTree::try_from_events(agent_id.clone(), &records).expect("replay");
    assert!(matches!(
        cold.standalone_compaction_recovery(),
        Some(tau_core::StandaloneCompactionRecovery::DispatchUncertain(checkpoint))
            if checkpoint == *owner
    ));
    h.shutdown().expect("shutdown");
}

/// Capacity recovery alone must release a rejected checkpoint exactly once,
/// preserving its model and replacement-plus-late-suffix context.
#[test]
fn standalone_checkpoint_capacity_wake_resumes_exact_owner() {
    check_checkpoint_recovery("capacity");
}

/// Real charged worker frames force Full. Only their retirement supplies the
/// capacity edge, and unchanged pressure must not create a self-waking loop.
#[test]
fn standalone_checkpoint_worker_capacity_release_resumes_exact_owner() {
    check_checkpoint_recovery("physical");
}

/// Ordinary progress is another external retry opportunity; it must not need
/// navigation or a second compaction terminal.
#[test]
fn standalone_checkpoint_progress_resumes_exact_owner() {
    check_checkpoint_recovery("progress");
}

/// Extra capacity/progress notifications cannot redispatch a committed owner.
#[test]
fn standalone_checkpoint_unrejected_control_is_not_duplicated() {
    check_checkpoint_recovery("unrejected");
}

/// A complete crash prefix selected at Root must wait through cold discovery,
/// then resume exactly once when its successful boundary is reselected.
#[test]
fn standalone_checkpoint_cold_off_branch_resumes_at_boundary() {
    check_checkpoint_recovery("cold");
}
