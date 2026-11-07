//! Recall crosses real canonical compaction, worker, and prompt ownership
//! paths.

use super::*;
use crate::artifact_worker::ArtifactWorker;
use crate::event::HarnessCommand;

/// Starts reactive compaction through its normal provider terminal path.
fn compact(h: &mut Harness) -> (AgentId, AgentPromptCreated) {
    h.prompt_coordination
        .prompt_runtime
        .recall_eager_test_delivery = false;
    dispatch::enable_remote_compaction_for_test_model(h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.submit_user_prompt(
        "s1".parse().expect("session"),
        "lost recall detail".to_owned(),
    )
    .expect("initial inference");
    let original = read_nth_prompt_created(h, 0);
    let cid = h
        .runtime_agent_id_for_target_agent(Some(original.agent_id.as_str()))
        .expect("runtime owner");
    h.handle_provider_response_finished(dispatch::context_overflow_response(&original))
        .expect("reactive overflow");
    let compact = read_nth_prompt_created(h, 1);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    h.handle_provider_response_finished(dispatch::standalone_compaction_success_response(
        &compact,
        "accepted summary",
    ))
    .expect("compaction terminal");
    (cid, compact)
}

/// Receives a physical archive result while retaining its charged permit.
fn completion(h: &mut Harness) -> Box<crate::compaction_recall::RecallCompleted> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "recall completion missing");
        if let Ok(HarnessEvent::Command(command)) =
            h.runtime_io.rx.recv_timeout(Duration::from_millis(20))
        {
            match command {
                HarnessCommand::RecallCompleted(done) => return done,
                other => h.handle_harness_command(other).expect("runtime command"),
            }
        }
    }
}

/// The next provider request waits for a full archive, contains one framed
/// locator, and keeps replacement bytes unchanged. Repeated callbacks cannot
/// send a second request or redo paid compaction.
#[test]
fn recall_precedes_resumed_inference_and_completion_is_exactly_once() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let (cid, compact_prompt) = compact(&mut h);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    assert!(h.send_prompt_to_agent_for(&cid).is_none());
    let done = completion(&mut h);
    let descriptor = done.result.clone().expect("archive");
    h.handle_harness_command(HarnessCommand::RecallCompleted(done))
        .expect("recall completion");
    let resumed = read_nth_prompt_created(&h, 2);
    let texts: Vec<_> = resumed
        .context
        .flatten()
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(message) => Some(
                message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text }
                        | ContentPart::SyntheticCompactionSummary { text }
                        | ContentPart::HarnessInternalText { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|text| text == "accepted summary"));
    let notice = texts
        .iter()
        .find(|text| text.contains(&descriptor.key.to_string()))
        .expect("recall notice");
    assert!(notice.starts_with("<tau_internal>"));
    assert!(notice.ends_with(tau_proto::TAU_INTERNAL_CLOSE));
    assert_eq!(notice.matches("<tau-artifact:").count(), 1);
    assert!(h.send_prompt_to_agent_for(&cid).is_none());
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        3
    );
    assert_eq!(event_log_events(&h).iter().filter(|event| matches!(event, Event::AgentCompacted(event) if event.compact_prompt_id == compact_prompt.agent_prompt_id)).count(), 1);
}

/// Cancellation/retirement after physical publication cannot revive the stale
/// checkpoint or consume a new runtime owner's callback.
#[test]
fn recall_late_completion_after_owner_retirement_cannot_dispatch() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let (cid, _) = compact(&mut h);
    let done = completion(&mut h);
    assert!(done.result.is_ok());
    h.cancel_agent_synchronized_publications(&cid);
    h.handle_harness_command(HarnessCommand::RecallCompleted(done))
        .expect("late completion");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
}

/// Ephemeral sessions never start a persistent archive worker, while an honest
/// disabled notice leaves the accepted summary and normal inference intact.
#[test]
fn recall_ephemeral_session_does_not_persist_transcript() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness_ephemeral(td.path()).expect("ephemeral harness");
    let (_, _) = compact(&mut h);
    let resumed = read_nth_prompt_created(&h, 2);
    assert!(resumed.context.flatten().iter().any(|item| matches!(item,
        ContextItem::Message(message) if message.content.iter().any(|part| matches!(part,
            ContentPart::Text { text } if text.contains("recall is disabled")
        ))
    )));
    assert!(h.runtime_io.artifacts.is_none());
    assert!(!td.path().join("artifacts").exists());
}

/// A crash-equivalent cut after successful compaction/CAS publication but
/// before callback delivery rebuilds the same archive from journals, even if
/// shared objects are missing, without another standalone provider request.
#[test]
fn recall_restart_rebuilds_missing_object_without_repeating_compaction() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let (_, compact_prompt) = compact(&mut h);
    let done = completion(&mut h);
    let original_key = done.result.as_ref().expect("initial archive").key.clone();
    h.shutdown().expect("shutdown");
    drop(done);
    drop(h);
    wait_for_session_unlock(td.path(), "s1");
    std::fs::remove_dir_all(td.path().join("artifacts")).expect("simulate missing objects");
    let restored =
        quiet_provider_harness_with_start_reason(td.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold restore");
    let resumed = read_nth_prompt_created(&restored, 0);
    assert_eq!(resumed.operation, tau_proto::PromptOperation::Inference);
    assert!(resumed.context.flatten().iter().any(|item| matches!(item,
        ContextItem::Message(message) if message.content.iter().any(|part| matches!(part,
            ContentPart::Text { text } if text.contains(&original_key.to_string())
        ))
    )));
    let records = restored
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(compact_prompt.agent_id.as_str())
        .expect("restored journal");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentCompacted(_)))
            .count(),
        1
    );
}

/// Recall uses the same finite worker admission as ordinary transfers. Busy
/// cannot stall inference indefinitely or turn an accepted summary into retry.
#[test]
fn recall_busy_preserves_summary_and_resumes_with_unavailable_notice() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut worker = ArtifactWorker::start(td.path(), h.runtime_io.tx.clone()).expect("worker");
    for index in 0..8 {
        worker
            .submit(
                "test".to_owned(),
                crate::test_connection_id("test"),
                tau_proto::ArtifactRequest {
                    request_id: format!("busy-{index}").parse().expect("request"),
                    expected_session_id: "s1".parse().expect("session"),
                    op: tau_proto::ArtifactOp::Available,
                },
            )
            .expect("occupy physical permit");
    }
    h.runtime_io.artifacts = Some(worker);
    compact(&mut h);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(5)),
        tau_core::DurabilityBarrierOutcome::Durable,
    );
    // Observe the writer frontier without consuming the eight queued artifact
    // completions: their physical permits must still make recall Busy.
    h.observe_semantic_persistence_progress();
    let resumed = read_nth_prompt_created(&h, 2);
    assert!(resumed.context.flatten().iter().any(|item| matches!(item,
        ContextItem::Message(message) if message.content.iter().any(|part| matches!(part,
            ContentPart::Text { text } if text.contains("recall is unavailable")
        ))
    )));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentCompacted(_)))
            .count(),
        1
    );
}

/// Owns a deterministic writer pause; a failed assertion must still unblock
/// teardown instead of stranding the persistence worker.
struct PausedWriter(Arc<tau_core::SemanticPersistenceOwner>);

impl Drop for PausedWriter {
    fn drop(&mut self) {
        self.0.pause_frame_writes_for_test(false);
    }
}

/// Pauses frame writes only after the source agent's physical generation
/// exists.
fn pause_writer(h: &mut Harness) -> PausedWriter {
    ensure_test_user_agent(h);
    let owner = h
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("persistent owner")
        .clone();
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(5)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    owner.pause_frame_writes_for_test(true);
    PausedWriter(owner)
}

/// An accepted but unwritten compaction parks metadata only: no artifact
/// worker or deletion guard is occupied. The real frontier wake admits exactly
/// one job, whose completion releases exactly one provider request.
#[test]
fn recall_frontier_wakeup_admits_only_after_accepted_prefix_is_written() {
    let td = TempDir::new().expect("state");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let pause = pause_writer(&mut h);
    let (cid, _) = compact(&mut h);
    assert!(
        h.runtime_io.artifacts.is_none(),
        "no physical permit before readable frontier"
    );
    h.observe_semantic_persistence_progress();
    assert!(
        h.runtime_io.artifacts.is_none(),
        "unrelated progress cannot fabricate frontier"
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    let owner = pause.0.clone();
    drop(pause);
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(5)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    let done = completion(&mut h);
    assert!(done.result.is_ok());
    h.handle_harness_command(HarnessCommand::RecallCompleted(done))
        .expect("completion");
    assert_eq!(
        read_nth_prompt_created(&h, 2).operation,
        tau_proto::PromptOperation::Inference
    );
    h.observe_semantic_persistence_progress();
    assert!(h.send_prompt_to_agent_for(&cid).is_none());
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        3
    );
}

/// Frontier and worker waits share one nonrenewable runtime deadline. Expiry
/// releases inference even before physical completion; late work cannot revive
/// or overwrite that outcome.
#[test]
fn recall_deadline_releases_frontier_and_worker_waits_without_retry() {
    for awaiting_frontier in [true, false] {
        let td = TempDir::new().expect("state");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let pause = awaiting_frontier.then(|| pause_writer(&mut h));
        compact(&mut h);
        let done = (!awaiting_frontier).then(|| completion(&mut h));
        let deadline = h
            .next_compaction_recall_deadline()
            .expect("pending recall deadline");
        h.process_runtime_deadlines_at(deadline);
        assert!(h.next_compaction_recall_deadline().is_none());
        let resumed = read_nth_prompt_created(&h, 2);
        assert!(resumed.context.flatten().iter().any(|item| matches!(item,
            ContextItem::Message(message) if message.content.iter().any(|part| matches!(part,
                ContentPart::Text { text } if text.contains("recall is unavailable")
            ))
        )));
        drop(pause);
        if let Some(done) = done {
            h.handle_harness_command(HarnessCommand::RecallCompleted(done))
                .expect("late completion");
        } else {
            h.observe_semantic_persistence_progress();
            assert!(h.runtime_io.artifacts.is_none());
        }
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            3
        );
    }
}
