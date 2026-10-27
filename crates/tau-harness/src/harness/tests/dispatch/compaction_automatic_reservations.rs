//! Cold cursor reconstruction includes terminal-owned automatic reservations.

use super::*;

/// Exercise an accepted off-branch prefix with real provider usage. Rejection
/// leaves either the outer finish or protected start owed while navigation is
/// admitted; recovery must close that decision without reusing its identity.
fn automatic_reservation_sequence(cut: &str, canceled: bool, ordinary_first: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    // A canceled prompt must inherit an actual canonical usage observation,
    // not hand-written runtime counters.
    if canceled {
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("usage evidence".to_owned()))
            .expect("dispatch");
        let prompt = read_nth_prompt_created(&h, 0);
        let mut response =
            provider_text_response(&prompt.agent_prompt_id, agent_id.clone(), "prior answer");
        response.usage = Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 250,
            response_received_tokens: 1,
            ..Default::default()
        });
        h.handle_provider_response_finished(response)
            .expect("prior response");
    }
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role")
        .compactions
        .insert(
            "reservation".to_owned(),
            tau_config::settings::CompactionPolicy {
                threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(100),
                enable: true,
                when: tau_config::settings::ContextPolicyWhen {
                    at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                    statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
                },
            },
        );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("decision".to_owned()))
        .expect("dispatch");
    let prompt = read_nth_prompt_created(&h, usize::from(canceled));
    // The canonical terminal is admitted, then the finish (or protected start)
    // encounters Full. Navigation below is another legal admitted publication.
    if cut != "started" {
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .reject_admissions_after_for_test(if cut == "protected" { 2 } else { 1 }, 1);
    }
    if canceled {
        h.finalize_canceled_in_flight_prompt(&cid);
    } else {
        let mut response =
            provider_text_response(&prompt.agent_prompt_id, agent_id.clone(), "done");
        response.usage = Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 250,
            response_received_tokens: 1,
            ..Default::default()
        });
        h.handle_provider_response_finished(response)
            .expect("terminal");
    }
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("records");
    let decision = records
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                response.automatic_compaction_decision.clone()
            }
            Event::AgentPromptTerminated(terminated) => {
                terminated.automatic_compaction_decision.clone()
            }
            _ => None,
        })
        .expect("genuine terminal decision");
    let number = decision
        .transaction_id
        .to_string()
        .strip_prefix("ct-")
        .expect("ct")
        .parse::<u64>()
        .expect("number");
    if cut == "started" {
        assert_eq!(records.iter().filter(|record| matches!(&record.event,
            Event::AgentStandaloneCompactionStarted(started) if started.transaction_id == decision.transaction_id)).count(), 1);
        h.shutdown().expect("shutdown");
        wait_for_session_unlock(&state, "s1");
        rewrite_agent_records(&state, &agent_id, &records);
        drop(h);
        let mut restored =
            quiet_standalone_provider_harness_for_with_start_reason_storage_mode_and_hook(
                "s1",
                &state,
                tau_proto::SessionStartReason::Resume,
                crate::HarnessStorageMode::Durable,
                None,
            )
            .expect("resume");
        assert!(restored.next_prompt_index_from_log(agent_id.as_str()) > number);
        assert!(
            !event_log_events(&restored).iter().any(|event| matches!(
                event,
                Event::AgentPromptCreated(_) | Event::AgentStandaloneCompactionStarted(_)
            )),
            "uncertain started work must never be resent"
        );
        restored.shutdown().expect("shutdown");
        return;
    }
    let original_head = agent_tree_for_conversation(&h, &cid)
        .head()
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    assert!(
        !records
            .iter()
            .any(|record| matches!(&record.event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.handle_ui_navigate_tree(
        crate::harness::harness_connection_id(),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::Root,
        },
    )
    .expect("navigate off branch");
    if cut != "protected" {
        h.retry_outer_turn_finish(&cid);
    }
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("prefix")
        .to_vec();
    let closures = records.iter().filter(|record| matches!(&record.event,
        Event::AgentStandaloneCompactionFailed(failed) if failed.transaction_id == decision.transaction_id
            && failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch)).count();
    assert_eq!(closures, usize::from(cut != "protected"));
    if cut != "live" {
        h.shutdown().expect("shutdown");
        wait_for_session_unlock(&state, "s1");
        rewrite_agent_records(&state, &agent_id, &records);
        h.session_runtime.agent_store =
            tau_core::AgentStore::open(state.join("agents")).expect("fallback");
        assert_eq!(
            h.session_runtime
                .agent_store
                .loaded_next_prompt_index(&agent_id),
            None
        );
        assert_eq!(h.next_prompt_index_from_log(agent_id.as_str()), number + 1);
        drop(h);
        h = quiet_standalone_provider_harness_for_with_start_reason_storage_mode_and_hook(
            "s1",
            &state,
            tau_proto::SessionStartReason::Resume,
            crate::HarnessStorageMode::Durable,
            None,
        )
        .expect("resume");
    }
    let cid = h
        .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        .expect("loaded agent");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: original_head,
        }),
    );
    if ordinary_first {
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("masking inference".to_owned()))
            .expect("ordinary");
        let next = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::AgentPromptCreated(prompt) => Some(prompt),
                _ => None,
            })
            .next_back()
            .expect("ordinary prompt");
        h.handle_provider_response_finished(provider_text_response(
            &next.agent_prompt_id,
            agent_id.clone(),
            "small answer",
        ))
        .expect("answer");
    }
    let before = event_log_events(&h).len();
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    h.retry_pending_agent_publications();
    h.retry_pending_agent_publications();
    let events = event_log_events(&h);
    let starts = events[before..]
        .iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1, "exactly one admitted manual start");
    assert_eq!(
        starts[0].transaction_id.to_string(),
        format!("ct-{}", number + 1 + u64::from(ordinary_first))
    );
    // Closing a decision must not relax the reserved-ID validator. The same
    // otherwise valid manual claim with the closed ID remains forbidden.
    let mut reused = starts[0].clone();
    reused.transaction_id = decision.transaction_id.clone();
    let error = agent_tree_for_conversation(&h, &cid)
        .validate_event(&Event::AgentStandaloneCompactionStarted(reused))
        .expect_err("closed decision remains reserved");
    assert!(
        error
            .to_string()
            .contains("reserved by an automatic decision"),
        "{error}"
    );
    assert!(
        !h.prompt_coordination
            .compaction_runtime
            .rejected_ui_starts
            .contains_key(&cid)
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("final records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(&record.event, Event::AgentManualCompactionRequested(_)))
            .count(),
        1
    );
    assert_eq!(records.iter().filter(|record| matches!(&record.event,
        Event::AgentStandaloneCompactionStarted(started) if started.transaction_id == starts[0].transaction_id)).count(), 1);
    assert_eq!(records.iter().filter(|record| matches!(&record.event,
        Event::AgentPromptStarted(started) if started.agent_prompt_id == starts[0].compact_prompt_id)).count(), 1);
    assert_eq!(records.iter().filter(|record| matches!(&record.event,
        Event::AgentStandaloneCompactionFailed(failed) if failed.transaction_id == decision.transaction_id
            && failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch)).count(), 1);
    assert!(!records.iter().any(|record| matches!(&record.event,
        Event::AgentStandaloneCompactionStarted(started) if started.transaction_id == decision.transaction_id)));
    h.shutdown().expect("shutdown");
}

/// Closed normal and canceled decisions remain reserved across normal
/// discovery.
#[test]
fn automatic_reservation_closed_cold_manual_start_is_fresh() {
    automatic_reservation_sequence("closed", false, false);
    automatic_reservation_sequence("closed", true, false);
}

/// Off-branch recovery may close a rejected protected start before allocating a
/// compact prompt; the terminal reservation must already advance its cursor.
#[test]
fn automatic_reservation_protected_start_cold_manual_start_is_fresh() {
    automatic_reservation_sequence("protected", false, false);
}

/// Live preparation already consumes the ID; ordinary inference before manual
/// work is a masking control, not a prescription for recovering a stuck
/// request.
#[test]
fn automatic_reservation_live_and_ordinary_first_controls() {
    automatic_reservation_sequence("live", false, false);
    automatic_reservation_sequence("closed", false, true);
}

/// A committed eager start already contributes its identity, but restoration
/// must not replay its uncertain provider work.
#[test]
fn automatic_reservation_started_restart_control() {
    automatic_reservation_sequence("started", false, false);
}
