//! Durable successor reservations must survive cold numeric cursor
//! reconstruction.

use super::*;

/// Exercise real failure shaping, optionally restore a complete failure/start
/// prefix, then require the next transition to pass canonical admission.
fn reserved_successor_sequence(length: bool, crash_cut: Option<&str>, succeed: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    // Use the supported role prerequisite barrier so cold restore runs with a
    // ready provider. Early restoration without that barrier is a separate
    // route-availability concern, not the allocator transition under test.
    std::fs::create_dir_all(state.join("config")).expect("config directory");
    std::fs::write(
        state.join("config/harness.yaml"),
        "agents:\n  default_role: fixture\n  role_groups:\n    fixture:\n      roles:\n        fixture:\n          required_tools: [wait]\n",
    )
    .expect("role prerequisites");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_backend_capacity_compaction(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for text in ["old-A", "old-B", "old-C", "old-D"] {
        append_capacity_history(&mut h, &cid, text);
    }
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
        .expect("inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("reactive compaction");
    let first = read_nth_prompt_created(&h, 1);
    append_capacity_history(&mut h, &cid, "retained-suffix");
    let failure = |prompt: &tau_proto::AgentPromptCreated| {
        if length {
            local_summary_length_response(prompt, "draft-")
        } else {
            context_overflow_response(prompt)
        }
    };
    h.handle_provider_response_finished(failure(&first))
        .expect("first failure");
    let successor = read_nth_prompt_created(&h, 2);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("records")
        .to_vec();
    let (failure_index, reserved) = records
        .iter()
        .enumerate()
        .find_map(|(index, record)| match &record.event {
            Event::AgentStandaloneCompactionFailed(failed) => Some((index, failed.clone())),
            _ => None,
        })
        .expect("durable reservation");
    let (transaction_id, prompt_id) = if length {
        let plan = reserved
            .output_length_continuation
            .as_ref()
            .expect("length plan");
        (&plan.transaction_id, &plan.compact_prompt_id)
    } else {
        let plan = reserved.context_retreat.as_ref().expect("retreat plan");
        assert_ne!(plan.cut, reserved.cut);
        (&plan.transaction_id, &plan.compact_prompt_id)
    };
    let number = transaction_id
        .to_string()
        .strip_prefix("ct-")
        .expect("normal numeric transaction")
        .parse::<u64>()
        .expect("numeric suffix");
    assert_eq!(prompt_id.as_str(), format!("ap-{agent_id}-{number}"));
    assert_eq!(&successor.agent_prompt_id, prompt_id);
    let mut next_prompt = successor;
    if let Some(cut) = crash_cut {
        let end =
            if cut == "failure" {
                failure_index
            } else {
                records
                .iter()
                .position(|record| matches!(&record.event,
                    Event::AgentPromptStarted(started) if &started.agent_prompt_id == prompt_id
                ))
                .expect("successor materialization")
            };
        h.shutdown().expect("shutdown");
        wait_for_session_unlock(&state, "s1");
        rewrite_agent_records(&state, &agent_id, &records[..=end]);

        // The unprepared-journal fallback must agree with the managed history
        // index used by normal cold restore.
        h.session_runtime.agent_store =
            tau_core::AgentStore::open(state.join("agents")).expect("read-only store");
        assert_eq!(
            h.session_runtime
                .agent_store
                .loaded_next_prompt_index(&agent_id),
            None
        );
        assert_eq!(h.next_prompt_index_from_log(agent_id.as_str()), number + 1);
        drop(h);
        h = quiet_standalone_provider_harness_for_with_start_reason_and_storage_mode(
            "s1",
            &state,
            tau_proto::SessionStartReason::Resume,
            crate::HarnessStorageMode::Durable,
        )
        .expect("resume");
        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
                    Event::AgentStandaloneCompactionStarted(started)
                        if &started.transaction_id == transaction_id
                ))
                .count(),
            usize::from(cut == "failure"),
        );
        if cut == "start" {
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::AgentPromptCreated(_))),
                "ambiguous started work must not be replayed"
            );
            h.shutdown().expect("shutdown");
            return;
        }
        next_prompt = read_nth_prompt_created(&h, 0);
        assert_eq!(&next_prompt.agent_prompt_id, prompt_id);
        assert_eq!(
            next_prompt.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
    }
    let before = event_log_events(&h).len();
    if succeed {
        // The local-summary provider returns the assembled narrative, including
        // the provisional fragment carried in its continuation request.
        assert_eq!(next_prompt.local_summary_continuation.len(), 1);
        h.handle_provider_response_finished(provider_text_response(
            &next_prompt.agent_prompt_id,
            agent_id.clone(),
            "draft-finished-summary",
        ))
        .expect("successor success");
    } else {
        h.handle_provider_response_finished(failure(&next_prompt))
            .expect("successor failure");
    }
    let events = event_log_events(&h);
    let new_events = &events[before..];
    let created = new_events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        created.len(),
        1,
        "next transition must materialize exactly once"
    );
    assert_eq!(
        created[0].agent_prompt_id.as_str(),
        format!("ap-{agent_id}-{}", number + 1)
    );
    let admitted = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("canonical records");
    assert_eq!(
        admitted
            .iter()
            .filter(|record| matches!(&record.event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == created[0].agent_prompt_id
            ))
            .count(),
        1,
        "the request must pass durable unique-prompt admission"
    );
    if succeed {
        assert_eq!(created[0].operation, tau_proto::PromptOperation::Inference);
        assert_eq!(
            new_events
                .iter()
                .filter(|event| matches!(event, Event::AgentCompacted(_)))
                .count(),
            1
        );
        let context = serde_json::to_string(&created[0].context).expect("context");
        assert_eq!(context.matches("draft-finished-summary").count(), 1);
        assert_eq!(context.matches("retained-suffix").count(), 1);
        assert!(!context.contains("old-A"));
    } else {
        let next_failure = new_events
            .iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionFailed(failed) => Some(failed),
                _ => None,
            })
            .expect("next admitted failure");
        assert_eq!(&next_failure.transaction_id, transaction_id);
        let next_id = if length {
            &next_failure
                .output_length_continuation
                .as_ref()
                .expect("next length plan")
                .transaction_id
        } else {
            let plan = next_failure
                .context_retreat
                .as_ref()
                .expect("second reducible retreat");
            assert_ne!(plan.cut, next_failure.cut);
            &plan.transaction_id
        };
        assert_eq!(next_id.to_string(), format!("ct-{}", number + 1));
        assert_eq!(new_events.iter().filter(|event| matches!(event,
            Event::AgentStandaloneCompactionStarted(started) if &started.transaction_id == next_id
        )).count(), 1);
    }
    h.shutdown().expect("shutdown");
}

/// A failure-only Length crash cut must consume the reserved numeric identity
/// before the next Length response reserves and starts another successor.
#[test]
fn numeric_length_reservation_restores_next_transition() {
    reserved_successor_sequence(true, Some("failure"), false);
}

/// Capacity retreat must restore its reservation before a second reducible
/// rejection; sufficient closed cuts distinguish collisions from exhaustion.
#[test]
fn numeric_retreat_reservation_restores_next_transition() {
    reserved_successor_sequence(false, Some("failure"), false);
}

/// Successful restored compaction must allocate a fresh ordinary prompt and
/// admit the exact replacement plus suffix rather than collide at
/// materialization.
#[test]
fn numeric_length_reservation_restores_successful_inference() {
    reserved_successor_sequence(true, Some("failure"), true);
}

/// Uninterrupted failure application already consumes the reservation;
/// restoring the cursor must not change the existing live Length/retreat
/// progression.
#[test]
fn numeric_reservations_live_transition_control() {
    reserved_successor_sequence(true, None, false);
    reserved_successor_sequence(false, None, false);
}

/// Once a reserved successor has started, cold recovery must count its identity
/// but must not replay its ambiguous paid work for either successor category.
#[test]
fn numeric_reservations_started_restart_control() {
    reserved_successor_sequence(true, Some("start"), false);
    reserved_successor_sequence(false, Some("start"), false);
}
