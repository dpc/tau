//! Cold self-compaction terminal repair must retain the successful branch
//! owner.

use std::fs::File;

use super::*;

/// The desired cold repair must not deliver the terminal on a sibling branch.
#[test]
fn cold_self_terminal_waits_for_owning_branch() {
    reproduce("root");
}

/// Both cold publications must retain their exact owners across Full and
/// offbranch capacity wakes, without rerunning an approved terminal
/// interceptor.
#[test]
fn cold_self_terminal_and_checkpoint_retry_on_owning_branch() {
    reproduce("root-rejected");
}

/// Missing-background, correct-branch and already-delivered prefixes resume
/// once, while an already committed checkpoint stays dispatch-uncertain.
#[test]
fn cold_self_terminal_safe_prefix_controls() {
    for mode in ["background", "correct", "delivered", "checkpoint"] {
        reproduce(mode);
    }
}

/// Drive the real self-tool handler, retain the same intercepted terminal with
/// one injected semantic Full, and restore only its accepted journal prefix.
fn reproduce(mode: &str) {
    let offbranch = mode.starts_with("root");
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
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let call_id = ToolCallId::from(format!("audit-self-{mode}"));
    let compact = start_seeded_self_compaction(&mut h, &cid, call_id.clone());
    append_byte_fit_text(
        &mut h,
        &cid,
        "retained late self-compaction suffix".to_owned(),
    );
    connect_test_tool(&mut h, "audit-terminal");
    h.handle_extension_event(
        "audit-terminal",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept terminal");
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("closed tool context"),
    )
    .expect("success");
    assert!(matches!(
        &h.runtime_io.publication.pending_intercept.as_ref().expect("held terminal").event,
        Event::AgentPromptSteered(steered) if steered.self_compaction_terminal.is_some()
    ));
    let boundary = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("boundary");
    let transaction = event_log_events(&h)
        .iter()
        .find_map(|event| match event {
            Event::AgentCompacted(compacted) => Some(compacted.transaction_id.clone()),
            _ => None,
        })
        .expect("successful transaction");
    if offbranch {
        h.handle_ui_navigate_tree(
            crate::harness::harness_connection_id(),
            tau_proto::UiNavigateTree {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id.clone()),
                target: tau_proto::UiTreeNavigationTarget::Root,
            },
        )
        .expect("queue legal root navigation");
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            Some(boundary)
        );
    }
    if !matches!(mode, "delivered" | "checkpoint") {
        reject_next_semantic_admission(&h);
    } else if mode == "delivered" {
        // Once the terminal commits, park its checkpoint to obtain a delivered,
        // not dispatch-uncertain, accepted prefix.
        connect_test_tool(&mut h, "audit-checkpoint");
        h.handle_extension_event(
            "audit-checkpoint",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept checkpoint");
    }
    h.handle_extension_event(
        "audit-terminal",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("same terminal passes interception");
    let mut cut = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("accepted prefix")
        .to_vec();
    if mode == "background" {
        // The success committed, but its background-tool result has not.
        let success = cut
            .iter()
            .position(|record| matches!(record.event, Event::AgentCompacted(_)))
            .expect("success in accepted prefix");
        cut.truncate(success + 1);
    }
    let terminals = |records: &[tau_core::PersistedAgentEvent]| {
        records.iter().filter(|record|
        matches!(&record.event, Event::AgentPromptSteered(s) if s.self_compaction_terminal.is_some())).count()
    };
    let checkpoints = |records: &[tau_core::PersistedAgentEvent]| {
        records
            .iter()
            .filter(|record| {
                matches!(&record.event, Event::AgentInferenceDispatchStarted(s)
            if s.transaction_id.as_ref() == Some(&transaction))
            })
            .count()
    };
    assert_eq!(
        terminals(&cut),
        usize::from(matches!(mode, "delivered" | "checkpoint"))
    );
    assert_eq!(checkpoints(&cut), usize::from(mode == "checkpoint"));
    assert_eq!(
        cut.iter()
            .filter(|record| matches!(&record.event, Event::AgentCompacted(_)))
            .count(),
        1
    );
    assert_eq!(cut.iter().filter(|record|
        matches!(&record.event, Event::ToolBackgroundResult(r) if r.call_id == call_id)).count(), usize::from(mode != "background"));
    if offbranch {
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            None
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&cid)
        );
    }
    h.shutdown().expect("close fixture");
    let mut journal = File::create(
        state
            .join("agents")
            .join(agent_id.as_str())
            .join("events.cbor"),
    )
    .expect("rewrite exact accepted prefix, excluding shutdown cleanup");
    for record in &cut {
        let mut encoded = Vec::new();
        ciborium::into_writer(record, &mut encoded).expect("encode");
        journal
            .write_all(&(encoded.len() as u64).to_le_bytes())
            .expect("length");
        journal.write_all(&encoded).expect("record");
    }
    journal.sync_all().expect("sync");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("ordinary constructor and discovery");
    let cid = resumed.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    let records = resumed
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("cold records")
        .to_vec();
    if offbranch {
        assert_eq!(terminals(&records), 0);
        assert_eq!(checkpoints(&records), 0);
        resumed.retry_pending_agent_publications();
        resumed.handle_publication_capacity_ready();
        resumed
            .resume_restored_compaction_checkpoints(RestoredCheckpointAuthority::DiscoveryComplete);
        let waiting = resumed
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("waiting");
        assert_eq!(terminals(&waiting), 0);
        assert_eq!(checkpoints(&waiting), 0);
        assert_eq!(
            resumed.agent_runtime.agent_registry.agents[&cid]
                .identity
                .head,
            None
        );
        resumed
            .dispatch_prompt_for_agent(
                &cid,
                PendingPrompt::user("input on the unrelated branch".to_owned()),
            )
            .expect("fresh offbranch input");
        resumed.try_advance_queue();
        resumed.drain_publish_idle_dispatches();
        assert!(
            !event_log_events(&resumed).iter().any(|event| matches!(
                event, Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            )),
            "fresh input must not escape the retained standalone owner"
        );
        if mode == "root-rejected" {
            intercept_recovery_event(
                &mut resumed,
                "cold-terminal",
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            );
            intercept_recovery_event(
                &mut resumed,
                "cold-checkpoint",
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            );
        }
        resumed
            .handle_ui_navigate_tree(
                crate::harness::harness_connection_id(),
                tau_proto::UiNavigateTree {
                    session_id: test_session_id("s1"),
                    target_agent_id: Some(agent_id.clone()),
                    target: tau_proto::UiTreeNavigationTarget::Node(boundary),
                },
            )
            .expect("return to owning boundary");
        if mode == "root-rejected" {
            reject_cold_recovery_publications(&mut resumed, &cid, &agent_id, boundary);
        }
        resumed.retry_pending_agent_publications();
        resumed.handle_publication_capacity_ready();
        resumed
            .resume_restored_compaction_checkpoints(RestoredCheckpointAuthority::DiscoveryComplete);
        let after = resumed
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("after");
        assert_eq!(checkpoints(&after), 1);
        assert_eq!(terminals(&after), 1);
    }
    resumed.retry_pending_agent_publications();
    resumed.handle_publication_capacity_ready();
    resumed.resume_restored_compaction_checkpoints(RestoredCheckpointAuthority::DiscoveryComplete);
    let records = resumed
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("final records");
    assert_eq!(terminals(&records), 1);
    assert_eq!(checkpoints(&records), 1);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event, Event::ToolBackgroundResult(result) if result.call_id == call_id
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event, Event::AgentPromptStarted(prompt)
                    if prompt.operation == tau_proto::PromptOperation::Inference
            ))
            .count(),
        1
    );
    let terminal = records.iter().find(|record|
        matches!(&record.event, Event::AgentPromptSteered(s) if s.self_compaction_terminal.is_some()))
        .expect("one terminal");
    assert_eq!(
        terminal.parent,
        tau_core::AgentEventParent::from_head(tau_proto::AgentHead::Node(boundary))
    );
    let terminal_node = resumed
        .session_runtime
        .agent_store
        .agent(agent_id.as_str())
        .expect("tree")
        .node_for_durable_event_seq(terminal.seq)
        .expect("terminal node");
    assert!(records.iter().any(|record| matches!(&record.event,
        Event::AgentInferenceDispatchStarted(checkpoint)
        if checkpoint.transaction_id.as_ref() == Some(&transaction)
            && checkpoint.through == tau_proto::AgentHead::Node(terminal_node))));
    assert!(
        !resumed
            .prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    let prompts = event_log_events(&resumed)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::Inference =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), usize::from(mode != "checkpoint"));
    if mode != "checkpoint" {
        validate_closed_tool_timeline(&prompts[0].context).expect("closed resumed tool context");
        let context = serde_json::to_string(&prompts[0].context).expect("context");
        assert!(context.contains("strict fake replacement"));
        assert_eq!(
            context
                .matches("retained late self-compaction suffix")
                .count(),
            1
        );
        assert_eq!(context.matches("Self compact terminal:").count(), 1);
        assert!(!context.contains("input on the unrelated branch"));
    } else {
        assert!(matches!(
            resumed.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        ));
    }
}

/// Hold one semantic publication so a one-shot Full targets that exact phase.
fn intercept_recovery_event(h: &mut Harness, connection: &str, event: tau_proto::EventName) {
    connect_test_tool(h, connection);
    h.handle_extension_event(
        connection,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(event)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept cold publication");
}

/// Reject the restored terminal, retry it once on capacity, then reject its
/// checkpoint and prove that only branch reselection can admit that successor.
fn reject_cold_recovery_publications(
    h: &mut Harness,
    cid: &AgentId,
    agent_id: &AgentId,
    boundary: tau_core::NodeId,
) {
    assert!(matches!(
        &h.runtime_io.publication.pending_intercept.as_ref().expect("terminal").event,
        Event::AgentPromptSteered(terminal) if terminal.self_compaction_terminal.is_some()
    ));
    reject_next_semantic_admission(h);
    h.handle_extension_event(
        "cold-terminal",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject cold terminal admission");
    assert!(!h.session_runtime.agent_store.snapshot_agent_events_for_test(agent_id.as_str())
        .expect("records").iter().any(|record| matches!(
            &record.event, Event::AgentPromptSteered(terminal) if terminal.self_compaction_terminal.is_some()
        )));
    h.handle_publication_capacity_ready();
    let checkpoint = match &h
        .runtime_io
        .publication
        .pending_intercept
        .as_ref()
        .expect("checkpoint")
        .event
    {
        Event::AgentInferenceDispatchStarted(checkpoint) => checkpoint.clone(),
        other => panic!("approved terminal was intercepted again: {other:?}"),
    };
    // Queue navigation while the checkpoint is held, reproducing the same
    // legal admission ordering as the original terminal failure.
    h.handle_ui_navigate_tree(
        crate::harness::harness_connection_id(),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::Root,
        },
    )
    .expect("queue root navigation");
    reject_next_semantic_admission(h);
    h.handle_extension_event(
        "cold-checkpoint",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject checkpoint admission");
    h.handle_publication_capacity_ready();
    assert_eq!(
        h.agent_runtime.agent_registry.agents[cid].identity.head,
        None
    );
    assert!(
        !h.session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("records")
            .iter()
            .any(|record| matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
    );
    h.handle_disconnect(&crate::test_connection_id("cold-checkpoint"));
    h.handle_ui_navigate_tree(
        crate::harness::harness_connection_id(),
        tau_proto::UiNavigateTree {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            target: tau_proto::UiTreeNavigationTarget::Node(
                checkpoint.through.as_option().expect("terminal descendant"),
            ),
        },
    )
    .expect("reselect checkpoint branch");
    assert_ne!(checkpoint.through, tau_proto::AgentHead::Node(boundary));
    assert!(
        h.session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("records")
            .iter()
            .any(|record| matches!(
                &record.event, Event::AgentInferenceDispatchStarted(actual) if actual == &checkpoint
            ))
    );
}
