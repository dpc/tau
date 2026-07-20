use super::*;

/// Append one framed CBOR record to a semantic-store test journal.
fn append_persisted_record<T: serde::Serialize>(path: &Path, record: &T) {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).expect("encode persisted record");
    std::fs::create_dir_all(path.parent().expect("journal parent")).expect("create journal parent");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open journal");
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write record length");
    file.write_all(&encoded).expect("write record");
}

fn append_agent_creation(store: &mut tau_core::AgentStore, agent_id: &str) {
    store
        .append_agent_event(
            agent_id,
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: crate::parse_agent_id(agent_id),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed agent creation");
}

/// Collect message-category deliveries with the requested replay marker.
fn message_deliveries(sink: &Arc<Mutex<Vec<RoutedFrame>>>, replay: bool) -> Vec<Event> {
    sink.lock()
        .expect("delivery sink")
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery)
                if delivery.replay == replay
                    && delivery.event.name().category() == &tau_proto::EventCategory::Message =>
            {
                Some((*delivery.event).clone())
            }
            _ => None,
        })
        .collect()
}

fn message_delivery_sources(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    replay: bool,
) -> Vec<Option<tau_proto::ConnectionId>> {
    sink.lock()
        .expect("delivery sink")
        .iter()
        .filter(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if delivery.replay == replay
                        && delivery.event.name().category()
                            == &tau_proto::EventCategory::Message
            )
        })
        .map(|routed| routed.source_id.clone())
        .collect()
}

/// Construct one stamped fallback message fact for persistence/replay tests.
fn replay_message_fact() -> Event {
    Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new("unknown-agent"),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: Some("Sender".to_owned()),
            sender_auth: None,
        },
        None,
        "hello",
    ))
}

/// Durable fallback facts publish live only after append and replay with exact
/// payload/provenance; duplicate emits remain two independent records.
#[test]
fn fallback_message_fact_live_and_restart_replay_are_exact() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut report_payload = replay_message_fact();
    report_payload.stamp_message_publisher(tau_proto::MessagePublisherId::new("forged"));
    let Event::MessageDelivered(report_payload) = report_payload else {
        unreachable!("fixture is delivered fact");
    };
    let emitted_report = Event::MessageDeliveredReported(report_payload);
    let fact = replay_message_fact();
    {
        let mut h = quiet_provider_harness(&state_dir).expect("start");
        connect_ready_message_publisher(&mut h, "bridge-connection", "configured-bridge");
        let live_sink = connect_test_client(&mut h, "live-ui", tau_proto::ClientKind::Ui);
        h.handle_client_event(
            "live-ui",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::MESSAGE_DELIVERED,
                )],
            })),
        )
        .expect("subscribe live");

        h.handle_extension_event(
            "bridge-connection",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit::with_transient(
                emitted_report.clone(),
                true,
            ))),
        )
        .expect("first extension emit");
        h.handle_extension_event(
            "bridge-connection",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit::with_transient(
                emitted_report,
                false,
            ))),
        )
        .expect("duplicate extension emit");

        assert_eq!(
            message_deliveries(&live_sink, false),
            vec![fact.clone(), fact.clone()]
        );
        assert_eq!(
            message_delivery_sources(&live_sink, false),
            vec![
                Some(HARNESS_CONNECTION_ID.into()),
                Some(HARNESS_CONNECTION_ID.into()),
            ]
        );
        let records = h.store.session_events("s1").expect("fallback records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event, fact);
        assert_eq!(records[1].event, fact);
        h.shutdown().expect("shutdown");
    }

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state_dir, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let replay_sink = connect_test_client(&mut resumed, "late-ui", tau_proto::ClientKind::Ui);
    resumed
        .handle_client_event(
            "late-ui",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::MESSAGE_DELIVERED,
                )],
                live_selectors: Vec::new(),
            })),
        )
        .expect("subscribe replay");
    assert_eq!(
        message_deliveries(&replay_sink, true),
        vec![fact.clone(), fact]
    );
    assert_eq!(
        message_delivery_sources(&replay_sink, true),
        vec![
            Some(HARNESS_CONNECTION_ID.into()),
            Some(HARNESS_CONNECTION_ID.into()),
        ]
    );
    resumed.shutdown().expect("shutdown");
}

/// Ephemeral fallback facts remain available for same-daemon ordinary replay
/// without creating a session event file.
#[test]
fn ephemeral_fallback_message_fact_replays_in_same_daemon() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness_ephemeral(&state_dir).expect("start ephemeral");
    let fact = replay_message_fact();
    h.commit_message_fact(Some("bridge-connection"), fact.clone());

    let replay_sink = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "late-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED,
            )],
            live_selectors: Vec::new(),
        })),
    )
    .expect("subscribe replay");

    assert_eq!(message_deliveries(&replay_sink, true), vec![fact]);
    assert!(
        !state_dir
            .join("sessions")
            .join("s1")
            .join("events.cbor")
            .exists(),
        "ephemeral fallback must not create session files"
    );
}

/// A durable session retains ephemeral-agent membership and history only in
/// process memory, while a late same-daemon subscriber receives the same roster
/// and replay-marked agent facts as an already-connected subscriber.
#[test]
fn durable_session_late_replay_merges_ephemeral_agent_overlay() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        session_id: "s1".into(),
        role: "engineer".to_owned(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: None,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        parent_agent: None,
        ephemeral: true,
    })
    .expect("create ephemeral agent through harness lifecycle");
    let agent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral agent id");
    let cid = h
        .agent_routes
        .get(agent_id.as_str())
        .cloned()
        .expect("ephemeral runtime route");
    let prompt = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id.clone(),
        text: "ephemeral history".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    });
    h.publish_for_agent(&cid, prompt.clone());
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("ephemeral-message"),
        tau_proto::MessageParty {
            stable_id: "sender".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "ephemeral body",
    ));
    h.commit_message_fact(None, fact.clone());

    let sink = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "late-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
            ],
            live_selectors: Vec::new(),
        })),
    )
    .expect("subscribe late");

    let frames = sink.lock().expect("sink");
    assert!(frames.iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::SessionAgentLoaded(loaded))
                if loaded.agent_id == agent_id && loaded.ephemeral
        )
    }));
    assert!(frames.iter().any(|routed| {
        peel_delivery(&routed.frame)
            .is_some_and(|delivery| delivery.is_replay() && delivery.event() == &prompt)
    }));
    assert!(frames.iter().any(|routed| {
        peel_delivery(&routed.frame)
            .is_some_and(|delivery| delivery.is_replay() && delivery.event() == &fact)
    }));
    drop(frames);

    let durable_session_events = h.store.session_events("s1").expect("durable events");
    assert!(durable_session_events.iter().all(|record| {
        !matches!(
            &record.event,
            Event::SessionAgentLoaded(loaded) if loaded.agent_id == agent_id
        )
    }));
    assert!(
        !state_dir.join("agents").join(agent_id.as_str()).exists(),
        "ephemeral agent must not create durable files"
    );

    h.remove_agent(&cid);
    let after_unload = connect_test_client(&mut h, "after-unload", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "after-unload",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
            ],
            live_selectors: Vec::new(),
        })),
    )
    .expect("subscribe after unload");
    assert!(
        after_unload
            .lock()
            .expect("after-unload sink")
            .iter()
            .all(|routed| {
                peel_inner_event(&routed.frame).is_none_or(|event| match event {
                    Event::SessionAgentLoaded(loaded) => loaded.agent_id != agent_id,
                    Event::AgentPromptSubmitted(submitted) => submitted.agent_id != agent_id,
                    Event::MessageDelivered(delivered) => {
                        delivered.agent_id.as_str() != agent_id.as_str()
                    }
                    _ => true,
                })
            }),
        "matched process-local unload must remove roster and history traversal"
    );
}

/// Valid incoming facts fold and wake exactly once after commit, while sent and
/// universally invalid facts never activate inference.
#[test]
fn live_message_fact_projection_activates_only_valid_incoming_facts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.handle_ui_create_agent(tau_proto::UiCreateAgent {
        session_id: "s1".into(),
        role: "engineer".to_owned(),
        model_override: None,
        metadata: Vec::new(),
        initial_prompt: None,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
        parent_agent: None,
        ephemeral: true,
    })
    .expect("create target");
    let agent_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("target agent");
    assert!(h.agent_routes.contains_key(agent_id.as_str()));
    let delivered = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("bridge"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "u1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));
    let prompts_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count();
    h.commit_message_fact(None, delivered);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before + 1,
        "valid incoming fact should request one inference activation"
    );
    assert!(matches!(
        h.agent_store
            .agent(agent_id.as_str())
            .expect("projected tree")
            .nodes()
            .last()
            .map(|node| &node.entry),
        Some(tau_core::AgentEntry::MessageFact { .. })
    ));
    let projected_prompt = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id => Some(prompt),
            _ => None,
        })
        .expect("message fact activation prompt");
    assert!(
        projected_prompt
            .system_prompt
            .contains("<tau_message> elements are committed canonical external-message facts.")
    );
    assert!(projected_prompt.context.blocks.iter().any(|block| {
        matches!(
            block,
            tau_proto::ContextBlock::UserInput(input)
                if input.items.iter().any(|item| matches!(
                    item,
                    tau_proto::ContextItem::Message(message)
                        if message.role == tau_proto::ContextRole::User
                            && matches!(
                                message.content.first(),
                                Some(tau_proto::ContentPart::Text { text })
                                    if text.contains("<tau_message event=\"created\"")
                            )
                ))
        )
    }));

    h.commit_message_fact(
        None,
        Event::MessageSent(tau_proto::MessageSent::new(
            tau_proto::MessagePublisherId::new("bridge"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("m2"),
            None,
            None,
            "reply",
        )),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before + 1,
        "sent fact must not add an activation"
    );

    h.commit_message_fact(
        None,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::new("bridge"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("m3"),
            tau_proto::MessageParty {
                stable_id: String::new(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "invalid party",
        )),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before + 1,
        "unprojectable fact must not add an activation"
    );

    let cid = h
        .agent_routes
        .get(agent_id.as_str())
        .cloned()
        .expect("target conversation");
    h.agents.get_mut(&cid).expect("target runtime").terminating = true;
    h.commit_message_fact(
        None,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::new("bridge"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("m4"),
            tau_proto::MessageParty {
                stable_id: "u1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "arrived while terminating",
        )),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before + 1,
        "terminating target must retain the projection without waking"
    );
    assert!(matches!(
        h.agent_store
            .agent(agent_id.as_str())
            .expect("terminating target tree")
            .nodes()
            .last()
            .map(|node| &node.entry),
        Some(tau_core::AgentEntry::MessageFact { .. })
    ));
}

/// A live incoming fact committed during an open tool round stays pending until
/// terminal placement, then produces exactly one prompt after the tool result.
#[test]
fn live_message_fact_waits_for_tool_result_placement_before_single_wake() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    h.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agents[&cid]
        .agent_id
        .as_deref()
        .map(crate::parse_agent_id)
        .expect("durable agent id");
    seed_assistant_tool_round(&mut h, &cid, &[("call-1", "shell")]);
    let prompts_before = event_log_events(&h)
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count();

    h.commit_message_fact(
        None,
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::new("bridge"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("m1"),
            tau_proto::MessageParty {
                stable_id: "u1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "after tool",
        )),
    );

    assert!(
        h.agent_store
            .agent(agent_id.as_str())
            .expect("agent tree")
            .nodes()
            .iter()
            .all(|node| !matches!(node.entry, tau_core::AgentEntry::MessageFact { .. })),
        "fact projection must remain pending while tool adjacency is open"
    );
    assert!(matches!(
        h.agents[&cid].pending_message_wakes.front(),
        Some(crate::agent::PendingMessageWake {
            source: crate::agent::PendingMessageWakeSource::MessageFact { .. },
            node_id: None,
        })
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before
    );

    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
    );
    h.maybe_complete_agent_turn_for(&cid, "call-1");

    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before + 1,
        "terminal placement must release exactly one fact activation"
    );
    let prompt = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id => Some(prompt),
            _ => None,
        })
        .expect("fact activation prompt");
    let items = prompt.context.flatten();
    let tool_index = items
        .iter()
        .rposition(|item| matches!(item, ContextItem::ToolResult(_)))
        .expect("terminal tool result");
    let fact_index = items
        .iter()
        .rposition(|item| {
            matches!(
                item,
                ContextItem::Message(message)
                    if matches!(
                        message.content.first(),
                        Some(ContentPart::Text { text })
                            if text.contains("<tau_message event=\"created\"")
                    )
            )
        })
        .expect("projected fact");
    assert!(fact_index > tool_index);
    h.shutdown().expect("shutdown");
}

/// A legacy sidecar-only ghost reserves its id but cannot redirect a message
/// fact away from the session journal without validated agent identity.
#[test]
fn metadata_only_offline_agent_message_fact_uses_session_journal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let ghost_dir = h.agent_store.agents_dir().join("offline-agent");
    std::fs::create_dir_all(&ghost_dir).expect("create legacy ghost dir");
    std::fs::write(
        ghost_dir.join("meta.json"),
        br#"{"created_at":1,"last_touched":1,"last_user_interaction_time":1}"#,
    )
    .expect("seed legacy metadata-only ghost");
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new("offline-agent"),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));

    h.commit_message_fact(Some("bridge-connection"), fact.clone());

    assert!(!ghost_dir.join("events.cbor").exists());
    assert!(
        h.store
            .session_events("s1")
            .expect("session records")
            .iter()
            .any(|record| record.event == fact)
    );
    assert!(
        event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::AgentPromptCreated(prompt) if prompt.agent_id.as_str() == "offline-agent"
        )),
        "unloaded target must not wake"
    );
}

/// Cold replay reconstructs an offline agent's fact projection without
/// synthesizing a model activation.
#[test]
fn agent_message_fact_replay_projects_without_wake() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("offline-agent").expect("agent id");
    let live_projection = {
        let mut h = quiet_provider_harness(&state_dir).expect("start");
        append_agent_creation(&mut h.agent_store, agent_id.as_str());
        h.store
            .append_session_event(
                "s1",
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: "s1".into(),
                    agent_id: agent_id.clone(),
                    ephemeral: false,
                }),
            )
            .expect("seed membership");
        h.commit_message_fact(
            None,
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::new("bridge"),
                tau_proto::MessageAgentTarget::new(agent_id.as_str()),
                tau_proto::MessageFactId::new("m1"),
                tau_proto::MessageParty {
                    stable_id: "u1".to_owned(),
                    display_name: Some("Alice".to_owned()),
                    sender_auth: Some(tau_proto::MessageSenderAuth::VerifiedAllowlisted),
                },
                Some(tau_proto::MessageConversation {
                    stable_id: "c1".to_owned(),
                    display_name: None,
                    alias: Some("general".to_owned()),
                }),
                "persisted message",
            )),
        );
        let projection = h
            .agent_store
            .agent(agent_id.as_str())
            .expect("live transcript")
            .nodes()
            .last()
            .expect("live fact node")
            .entry
            .clone();
        h.shutdown().expect("shutdown");
        projection
    };

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state_dir, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    resumed
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("load replayed agent journal");
    let replayed_projection = resumed
        .agent_store
        .agent(agent_id.as_str())
        .expect("replayed transcript")
        .nodes()
        .last()
        .expect("replayed fact node")
        .entry
        .clone();
    assert_eq!(replayed_projection, live_projection);
    let tau_core::AgentEntry::MessageFact { item, .. } = &replayed_projection else {
        panic!("replayed message fact");
    };
    let tau_proto::ContentPart::Text { text } = &item.content[0];
    assert_eq!(
        text,
        "<tau_message event=\"created\" publisher=\"bridge\" message_ref=\"m1\" sender_ref=\"u1\" sender_display=\"Alice\" sender_auth=\"verified_allowlisted\" conversation=\"general\" content_trust=\"external\">persisted message</tau_message>"
    );
    assert!(
        event_log_events(&resumed).iter().all(|event| !matches!(
            event,
            Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id
        )),
        "journal replay must not synthesize a model activation"
    );
    resumed.shutdown().expect("shutdown");
}

/// A committed offline agent identity selects its agent journal without a live
/// route; session membership alone is not identity authority.
#[test]
fn member_agent_message_fact_uses_agent_journal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let agent_id = tau_proto::AgentId::parse("member-agent").expect("agent id");
    h.store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("seed membership");
    append_agent_creation(&mut h.agent_store, agent_id.as_str());
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));

    h.commit_message_fact(Some("bridge-connection"), fact.clone());

    assert_eq!(
        h.agent_store
            .agent_events(agent_id.as_str())
            .expect("member agent records")[1]
            .event,
        fact
    );
}

/// A live conversation route alone selects an agent journal when membership
/// and pre-existing store state are absent.
#[test]
fn live_route_only_message_fact_uses_agent_journal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid).clone();
    h.store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
            }),
        )
        .expect("remove membership");
    h.agent_store =
        AgentStore::open(td.path().join("isolated-agent-store")).expect("empty agent store");
    assert!(!h.agent_store.agent_exists(agent_id.as_str()));
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));

    h.commit_message_fact(Some("bridge-connection"), fact.clone());

    assert_eq!(
        h.agent_store
            .agent_events(agent_id.as_str())
            .expect("live-route agent records")[0]
            .event,
        fact
    );
}

/// A selected-journal append failure produces no committed runtime fact and no
/// subscriber delivery.
#[test]
fn fallback_message_fact_storage_failure_prevents_delivery() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    let live_sink = connect_test_client(&mut h, "live-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "live-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED,
            )],
        })),
    )
    .expect("subscribe live");
    let event_path = state_dir.join("sessions").join("s1").join("events.cbor");
    if event_path.exists() {
        std::fs::remove_file(&event_path).expect("remove empty session stream");
    }
    std::fs::create_dir_all(&event_path).expect("block event stream with directory");

    h.commit_message_fact(Some("bridge-connection"), replay_message_fact());

    assert!(message_deliveries(&live_sink, false).is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| event.name() != tau_proto::EventName::MESSAGE_DELIVERED)
    );
}

/// A known-agent journal append failure likewise prevents runtime commit and
/// delivery instead of falling back to the session journal.
#[test]
fn known_agent_message_fact_storage_failure_prevents_delivery() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    h.agent_store
        .append_agent_event(
            "offline-agent",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: tau_proto::AgentId::parse("offline-agent").expect("agent id"),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("create agent identity");
    let live_sink = connect_test_client(&mut h, "live-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "live-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED,
            )],
        })),
    )
    .expect("subscribe live");
    let event_path = state_dir
        .join("agents")
        .join("offline-agent")
        .join("events.cbor");
    std::fs::remove_file(&event_path).expect("remove agent stream");
    std::fs::create_dir_all(&event_path).expect("block agent stream with directory");
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new("offline-agent"),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));

    h.commit_message_fact(Some("bridge-connection"), fact);

    assert!(message_deliveries(&live_sink, false).is_empty());
    assert!(
        h.store
            .session_events("s1")
            .expect("session records")
            .iter()
            .all(|record| record.event.name().category() != &tau_proto::EventCategory::Message),
        "known-agent failure must not reroute to session fallback"
    );
}

/// A semantically invalid later session record prevents replay of every earlier
/// fallback fact from that journal.
#[test]
fn invalid_later_session_record_prevents_partial_message_replay() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    h.store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("cache loaded membership");
    h.agent_store
        .record_agent_meta(agent_id.as_str())
        .expect("reserve agent");
    h.commit_message_fact(
        Some("bridge-connection"),
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::new("configured-bridge"),
            tau_proto::MessageAgentTarget::new(agent_id.as_str()),
            tau_proto::MessageFactId::new("agent-message"),
            tau_proto::MessageParty {
                stable_id: "sender-1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            "agent history",
        )),
    );
    h.commit_message_fact(Some("bridge-connection"), replay_message_fact());
    append_persisted_record(
        &state_dir.join("sessions").join("s1").join("events.cbor"),
        &tau_core::PersistedSessionEvent {
            seq: tau_core::PersistedSessionEventSeq::new(2),
            source: None,
            event: Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "wrong-session".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                ephemeral: false,
            }),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );
    let sink = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);

    h.handle_client_event(
        "late-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            ],
            live_selectors: Vec::new(),
        })),
    )
    .expect("subscribe replay");

    assert!(message_deliveries(&sink, true).is_empty());
    assert!(
        sink.lock()
            .expect("replay sink")
            .iter()
            .all(|frame| !matches!(
                peel_inner_event(&frame.frame),
                Some(Event::SessionAgentLoaded(_))
            )),
        "invalid session journal must not expose cached roster or traverse agents"
    );
}

/// A structurally invalid later agent record prevents replay of every earlier
/// message fact from that agent journal.
#[test]
fn invalid_later_agent_record_prevents_partial_message_replay() {
    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("start");
    append_agent_creation(&mut h.agent_store, "agent-1");
    h.store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("load agent in session");
    h.agent_store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                key: "cached-key".into(),
                value: CborValue::Text("cached-value".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
        )
        .expect("cache agent metadata");
    let fact = Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("configured-bridge"),
        tau_proto::MessageAgentTarget::new("agent-1"),
        tau_proto::MessageFactId::new("m1"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ));
    h.commit_message_fact(Some("bridge-connection"), fact);
    append_persisted_record(
        &state_dir.join("agents").join("agent-1").join("events.cbor"),
        &tau_core::PersistedAgentEvent {
            seq: tau_core::PersistedAgentEventSeq::new(3),
            source: None,
            event: Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::new("configured-bridge"),
                tau_proto::MessageAgentTarget::new("agent-2"),
                tau_proto::MessageFactId::new("m2"),
                tau_proto::MessageParty {
                    stable_id: "sender-1".to_owned(),
                    display_name: None,
                    sender_auth: None,
                },
                None,
                "bad owner",
            )),
            parent: tau_core::AgentEventParent::InheritHead,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );
    let sink = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);

    h.handle_client_event(
        "late-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::AGENT_STATS_UPDATED),
            ],
            live_selectors: Vec::new(),
        })),
    )
    .expect("subscribe replay");

    assert!(message_deliveries(&sink, true).is_empty());
    assert!(
        sink.lock()
            .expect("replay sink")
            .iter()
            .all(|frame| !matches!(
                peel_inner_event(&frame.frame),
                Some(Event::AgentMetadataSet(_))
            )),
        "invalid agent journal must not expose cached metadata"
    );
}

/// Ensures every late subscriber receives a byte-free durable-result
/// projection, with UI clients additionally receiving the generic `tool.result`
/// event shape.
#[test]
fn ui_replay_projects_provider_image_result_without_bytes() {
    let result = ToolResult {
        call_id: "call-image".into(),
        tool_name: ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("image metadata".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: b"\x89PNG\r\n\x1a\nfixture".to_vec().into(),
                width: 1,
                height: 1,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };

    let ui = crate::harness::replay::project_agent_replay_event(
        Event::ProviderToolResult(result.clone()),
        true,
    );
    assert!(matches!(
        ui,
        Event::ToolResult(ToolResult {
            provider_content,
            ..
        }) if provider_content.is_empty()
    ));
    assert!(matches!(
        crate::harness::replay::project_agent_replay_event(
            Event::ProviderToolResult(result),
            false,
        ),
        Event::ProviderToolResult(ToolResult {
            provider_content,
            ..
        }) if provider_content.is_empty()
    ));
}

fn assistant_output(text: &str) -> Vec<tau_proto::ContextItem> {
    vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })]
}

fn provider_response_contains_text(finished: &ProviderResponseFinished, needle: &str) -> bool {
    finished.output_items.iter().any(|item| {
        matches!(
            item,
            tau_proto::ContextItem::Message(tau_proto::MessageItem { content, .. })
                if content.iter().any(|part| {
                    matches!(part, tau_proto::ContentPart::Text { text } if text.contains(needle))
                })
        )
    })
}

fn response_with_tool_calls(call_ids: &[&str]) -> ProviderResponseFinished {
    ProviderResponseFinished {
        agent_prompt_id: "sp-restored-tools".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: call_ids
            .iter()
            .map(|call_id| {
                ContextItem::ToolCall(ToolCallItem {
                    call_id: (*call_id).into(),
                    name: ToolName::new("read"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Null,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })
            })
            .collect(),
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn successful_tool_result(call_id: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(format!("result for {call_id}")),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn seed_restored_tool_round(state_dir: &Path, call_ids: &[&str], completed_call_ids: &[&str]) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed agent start");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                text: "before restart".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed user prompt");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::ProviderResponseFinished(response_with_tool_calls(call_ids)),
        )
        .expect("seed assistant tool calls");
    for call_id in completed_call_ids {
        agent_store
            .append_agent_event(
                "main",
                None,
                Event::ProviderToolResult(successful_tool_result(call_id)),
            )
            .expect("seed completed tool call");
    }
}

/// Session membership cannot manufacture a routable agent when the referenced
/// journal is missing, empty, or lacks the immutable creation fact.
#[test]
fn restore_rejects_membership_without_committed_agent_creation() {
    for journal_kind in ["missing", "empty", "creationless"] {
        let td = TempDir::new().expect("tempdir");
        let state_dir = td.path().join(journal_kind);
        let mut store =
            tau_core::SessionStore::open(state_dir.join("sessions")).expect("session store");
        store
            .append_session_event(
                "s1",
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: "s1".into(),
                    agent_id: crate::parse_agent_id("orphan"),
                    ephemeral: false,
                }),
            )
            .expect("seed membership");
        drop(store);
        let events_path = state_dir.join("agents/orphan/events.cbor");
        if journal_kind != "missing" {
            std::fs::create_dir_all(events_path.parent().expect("agent dir"))
                .expect("create agent dir");
            std::fs::write(&events_path, []).expect("create empty journal");
        }
        if journal_kind == "creationless" {
            append_persisted_record(
                &events_path,
                &tau_core::PersistedAgentEvent {
                    seq: tau_core::PersistedAgentEventSeq::new(0),
                    source: None,
                    event: Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                        inference_activation: false,
                        agent_id: crate::parse_agent_id("orphan"),
                        text: "orphan prompt".to_owned(),
                        message_class: tau_proto::PromptMessageClass::User,
                        originator: tau_proto::PromptOriginator::User,
                        submission_source: Default::default(),
                        display_name: None,
                        ctx_id: None,
                    }),
                    parent: tau_core::AgentEventParent::InheritHead,
                    recorded_at: tau_proto::UnixMicros::now(),
                },
            );
        }

        let before_len = std::fs::metadata(&events_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut h =
            echo_harness_with_start_reason("s1", &state_dir, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        assert!(
            !h.agent_routes.contains_key("orphan"),
            "{journal_kind} journal became routable"
        );
        assert!(!h.agents.contains_key(&crate::parse_agent_id("orphan")));
        h.commit_message_fact(
            Some("bridge"),
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::new("bridge"),
                tau_proto::MessageAgentTarget::new("orphan"),
                tau_proto::MessageFactId::new("must-fallback"),
                tau_proto::MessageParty {
                    stable_id: "sender".to_owned(),
                    display_name: None,
                    sender_auth: None,
                },
                None,
                "do not recreate an invalid agent journal",
            )),
        );
        let after_len = std::fs::metadata(&events_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        assert_eq!(
            after_len, before_len,
            "{journal_kind} journal was extended through stale membership"
        );
    }
}

fn seed_restored_tool_round_for_agent(
    state_dir: &Path,
    session_id: &str,
    agent_id: &str,
    call_ids: &[&str],
    completed_call_ids: &[&str],
) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            session_id,
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: session_id.into(),
                agent_id: crate::parse_agent_id(agent_id),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: crate::parse_agent_id(agent_id),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed agent start");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: crate::parse_agent_id(agent_id),
                text: format!("before restart for {agent_id}"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed user prompt");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: format!("sp-{agent_id}").into(),
                agent_id: crate::parse_agent_id(agent_id),
                ..response_with_tool_calls(call_ids)
            }),
        )
        .expect("seed assistant tool calls");
    for call_id in completed_call_ids {
        agent_store
            .append_agent_event(
                agent_id,
                None,
                Event::ProviderToolResult(successful_tool_result(call_id)),
            )
            .expect("seed completed tool call");
    }
}

fn provider_tool_errors(h: &Harness, call_id: &str) -> Vec<tau_proto::ToolError> {
    loaded_agent_events(h, "s1")
        .into_iter()
        .filter_map(|event| match event {
            Event::ProviderToolError(error) if error.call_id.as_str() == call_id => Some(error),
            _ => None,
        })
        .collect()
}

fn prompt_tool_result(prompt: &AgentPromptCreated, call_id: &str) -> Option<ToolResultItem> {
    prompt
        .context
        .flatten()
        .into_iter()
        .find_map(|item| match item {
            ContextItem::ToolResult(result) if result.call_id.as_str() == call_id => Some(result),
            _ => None,
        })
}

/// Regression: a cold resume used to leave the restored branch ending in an
/// assistant tool call with no matching tool result. The next provider prompt
/// then replayed an orphan tool call. Resume must close that foreground call
/// before the user can extend the branch.
#[test]
fn resume_repairs_unresolved_tool_call_before_next_prompt_context() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_restored_tool_round(&sp, &["interrupted-call"], &[]);

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");

    let errors = provider_tool_errors(&h, "interrupted-call");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("tau_internal: true"));
    assert!(errors[0].message.contains("Side effects may have occurred"));

    append_user_message_via_event(&mut h, "s1", "after restart");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    let repaired = prompt_tool_result(&prompt, "interrupted-call")
        .expect("synthetic tool result should be in provider context");
    assert!(matches!(repaired.status, ToolResultStatus::Error { .. }));

    h.shutdown().expect("shutdown");
}

/// Regression: a parallel tool round can be partly complete when the process
/// dies. Resume must preserve completed calls and synthesize errors only for
/// the missing foreground calls so the provider sees one balanced round.
#[test]
fn resume_repairs_only_missing_call_in_partial_parallel_round() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_restored_tool_round(&sp, &["done-call", "missing-call"], &["done-call"]);

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");

    assert!(provider_tool_errors(&h, "done-call").is_empty());
    assert_eq!(provider_tool_errors(&h, "missing-call").len(), 1);

    append_user_message_via_event(&mut h, "s1", "after restart");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    let completed = prompt_tool_result(&prompt, "done-call")
        .expect("completed tool result should remain in provider context");
    let repaired = prompt_tool_result(&prompt, "missing-call")
        .expect("missing tool result should be synthesized in provider context");
    assert!(matches!(completed.status, ToolResultStatus::Success));
    assert!(matches!(repaired.status, ToolResultStatus::Error { .. }));

    h.shutdown().expect("shutdown");
}

/// Regression: the resume repair writes durable events. A later cold resume
/// must see the already-closed tool round and avoid appending another synthetic
/// error for the same call.
#[test]
fn repeated_resume_does_not_duplicate_synthetic_tool_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_restored_tool_round(&sp, &["interrupted-once"], &[]);

    {
        let mut h =
            echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
                .expect("first resume");
        assert_eq!(provider_tool_errors(&h, "interrupted-once").len(), 1);
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    {
        let mut h =
            echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
                .expect("second resume");
        assert_eq!(provider_tool_errors(&h, "interrupted-once").len(), 1);
        h.shutdown().expect("shutdown");
    }
}

#[test]
fn resume_repairs_unresolved_tool_call_on_non_default_loaded_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_restored_tool_round_for_agent(&sp, "s1", "aaa_agent", &[], &[]);
    seed_restored_tool_round_for_agent(&sp, "s1", "zzz_agent", &["side-interrupted-call"], &[]);

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");

    let errors = provider_tool_errors(&h, "side-interrupted-call");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Side effects may have occurred"));

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_receives_replayed_agent_message_exact_selector() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.store
        .append_session_event(
            "s1",
            Some(HARNESS_CONNECTION_ID.into()),
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
    h.agent_store
        .append_agent_event(
            "agent-1",
            Some(HARNESS_CONNECTION_ID.into()),
            Event::AgentMessageSent(tau_proto::AgentMessageSent {
                message_id: "test-message".into(),
                sender_id: crate::parse_agent_id("agent-1"),
                recipient: tau_proto::AgentMessageRecipient::User,
                kind: tau_proto::AgentMessageKind::Message,
                message: "persisted hello".to_owned(),
            }),
        )
        .expect("seed agent message");

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_message = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !got_message {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        got_message = matches!(
            inner,
            TestProtocolItem::Event(Event::AgentMessageSent(message))
                if message.sender_id.as_str() == "agent-1"
                    && message.recipient == tau_proto::AgentMessageRecipient::User
                    && message.message == "persisted hello"
        );
    }

    assert!(got_message, "late UI should replay durable agent messages");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_receives_replayed_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.send_user_message("s1", "hello replay", None)
        .expect("send message");

    let events = loaded_agent_events(&h, "s1");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AgentPromptSubmitted(_))),
        "user prompt should be in a durable loaded-agent event log"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ProviderResponseFinished(_))),
        "final agent response should be in a durable loaded-agent event log"
    );
    assert!(
        events.iter().all(|event| !event.defaults_to_transient()),
        "transient events must not be persisted"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Prefix("session.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("provider.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_session_started = false;
    let mut got_agent_started = false;
    let mut got_prompt = false;
    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !(got_session_started && got_agent_started && got_prompt && got_response)
    {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        match inner {
            TestProtocolItem::Event(Event::SessionStarted(started))
                if started.session_id.as_str() == "s1" =>
            {
                got_session_started = true;
            }
            TestProtocolItem::Event(Event::AgentStarted(_)) => {
                got_agent_started = true;
            }
            TestProtocolItem::Event(Event::AgentPromptSubmitted(prompt))
                if prompt.text == "hello replay" =>
            {
                got_prompt = true;
            }
            TestProtocolItem::Event(Event::ProviderResponseFinished(finished))
                if finished.output_items.iter().any(|item| {
                    matches!(
                        item,
                        tau_proto::ContextItem::Message(tau_proto::MessageItem { content, .. })
                            if matches!(&content[0], tau_proto::ContentPart::Text { text }
                                if text.contains("hello replay"))
                    )
                }) =>
            {
                got_response = true;
            }
            _ => {}
        }
    }

    assert!(
        got_session_started,
        "late UI should replay current session start"
    );
    assert!(
        got_agent_started,
        "late UI should replay agent display metadata"
    );
    assert!(got_prompt, "late UI should replay prior user prompt");
    assert!(got_response, "late UI should replay prior agent response");

    h.shutdown().expect("shutdown");
}

/// Returns the delivery wrapper for frames carrying an event payload.
fn peel_delivery(message: &HarnessOutputMessage) -> Option<&tau_proto::EventDelivery> {
    match message {
        HarnessOutputMessage::Deliver(delivery) => Some(delivery),
        _ => None,
    }
}

/// Extension subscriptions share the UI late-join path: a late extension is
/// caught up with selector-matched durable facts, delivered as replay-marked
/// frames so side-effecting consumers can distinguish history from live
/// occurrences. This replaced the older live-only rule, whose protection
/// (e.g. std-notifications not replaying sounds) now lives in the replay
/// marker instead of withheld delivery.
#[test]
fn extension_subscribe_replays_durable_facts_as_replay_frames() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let past_text = "past extension replay guard";
    h.send_user_message("s1", past_text, None)
        .expect("send past message");

    let durable_events = loaded_agent_events(&h, "s1");
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ProviderResponseFinished(finished)
                if provider_response_contains_text(finished, past_text))
        }),
        "test setup: past provider response should be durable and eligible for replay",
    );

    let extension_events = connect_test_tool(&mut h, "late-extension");
    h.handle_extension_message(
        "late-extension",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
        }),
    )
    .expect("extension subscribe");

    {
        let events = extension_events.lock().expect("sink");
        let replay_index = events
            .iter()
            .position(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, past_text)
                        )
                })
            })
            .expect("replay response");
        let agent_boundary_index = events
            .iter()
            .position(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(delivery.event(), Event::AgentReplayComplete(_))
                })
            })
            .expect("agent replay boundary");
        let session_boundary_index = events
            .iter()
            .position(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(delivery.event(), Event::SessionReplayComplete(_))
                })
            })
            .expect("session replay boundary");
        assert!(
            replay_index < agent_boundary_index && agent_boundary_index < session_boundary_index,
            "historical replay must precede non-replay replay-complete boundaries"
        );
        assert!(
            events.iter().any(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, past_text)
                        )
                })
            }),
            "late extension should receive the past provider response as a replay frame",
        );
    }

    let live_text = "future live extension event";
    h.send_user_message("s1", live_text, None)
        .expect("send live message");

    {
        let events = extension_events.lock().expect("sink");
        let live_index = events
            .iter()
            .position(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, live_text)
                        )
                })
            })
            .expect("live response");
        let session_boundary_index = events
            .iter()
            .position(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(delivery.event(), Event::SessionReplayComplete(_))
                })
            })
            .expect("session replay boundary");
        assert!(
            session_boundary_index < live_index,
            "buffered/future live deliveries must follow session replay_complete"
        );
        assert!(
            events.iter().any(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, live_text)
                        )
                })
            }),
            "live provider responses must not be marked as replay",
        );
    }

    h.shutdown().expect("shutdown");
}

/// Loading an existing agent into an already-live session replays that agent's
/// durable history to restore-aware subscribers before the agent boundary.
#[test]
fn live_agent_load_replays_existing_agent_history_to_subscribers() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("loaded-later").expect("agent id");
    let mut agent_store = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: agent_id.clone(),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed agent start");
    agent_store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text("/tmp/live-load-cwd".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
        )
        .expect("seed metadata");
    agent_store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: "history before load".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed prompt");

    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_test_tool(&mut h, "restore-ext");
    h.handle_extension_message(
        "restore-ext",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::AGENT_STATS_UPDATED),
            ],
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe");
    sink.lock().expect("sink").clear();

    let cid = crate::parse_agent_id(agent_id.as_str());
    let mut agent = crate::agent::Agent::new(
        cid.clone(),
        h.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    agent.agent_id = Some(agent_id.to_string());
    h.agents.insert(cid.clone(), agent);
    h.agent_routes.insert(agent_id.to_string(), cid.clone());
    h.session_loaded_agents.insert(agent_id.clone());
    h.agent_navigation_modes
        .insert(agent_id.clone(), tau_proto::AgentNavigationMode::Active);

    h.publish_event(
        None,
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }),
    );

    let events = sink.lock().expect("sink");
    let load_index = events
        .iter()
        .position(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                !delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::SessionAgentLoaded(loaded) if loaded.agent_id == agent_id
                    )
            })
        })
        .expect("live load");
    let replay_index = events
        .iter()
        .position(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::AgentPromptSubmitted(prompt)
                            if prompt.agent_id == agent_id && prompt.text == "history before load"
                    )
            })
        })
        .expect("loaded-agent replay");
    let metadata_index = events
        .iter()
        .position(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::AgentMetadataSet(metadata)
                            if metadata.agent_id == agent_id
                                && metadata.key.as_str() == "ext_core-shell_cwd"
                    )
            })
        })
        .expect("loaded-agent metadata replay");
    let boundary_index = events
        .iter()
        .position(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                !delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::AgentReplayComplete(done) if done.agent_id == agent_id
                    )
            })
        })
        .expect("agent boundary");
    let stats_index = events
        .iter()
        .position(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::AgentStatsUpdated(stats) if stats.agent_id == agent_id
                    )
            })
        })
        .expect("loaded-agent stats snapshot");
    assert!(
        metadata_index < replay_index && replay_index < stats_index && stats_index < boundary_index
    );
    assert!(
        load_index < boundary_index,
        "live load and its per-agent restore boundary must both be delivered"
    );
    h.shutdown().expect("shutdown");
}

/// A corrupt session restore log must produce both an operator-visible replay
/// error notice and an errored `session.replay_complete` boundary, preventing
/// restore consumers from treating partial state as successful catch-up.
#[test]
fn session_replay_complete_reports_restore_log_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let path = tau_config::settings::sessions_dir_of(&sp)
        .join("s1")
        .join("restore-events.cbor");
    std::fs::create_dir_all(path.parent().expect("restore parent")).expect("create parent");
    std::fs::write(&path, 8_u64.to_le_bytes()).expect("write truncated restore record");

    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_test_tool(&mut h, "restore-ext");
    h.handle_extension_message(
        "restore-ext",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            live_selectors: Vec::new(),
        }),
    )
    .expect("subscribe");

    let events = sink.lock().expect("sink");
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::HarnessNotice(notice)
                            if notice.kind == tau_proto::notice_kind::HARNESS_REPLAY_ERROR
                                && notice.message.contains("session restore events")
                    )
            })
        }),
        "restore log corruption should emit a replay error notice"
    );
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                !delivery.is_replay()
                    && matches!(
                    delivery.event(),
                    Event::SessionReplayComplete(done)
                        if done.error.as_deref().is_some_and(|error| error.contains("session restore events"))
                )
            })
        }),
        "session replay boundary should carry the restore error"
    );
    h.shutdown().expect("shutdown");
}

/// A corrupt loaded-agent transcript must produce an agent-specific failed
/// boundary and propagate that failure to the session replay boundary.
#[test]
fn replay_complete_boundaries_report_agent_log_errors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("corrupt-agent").expect("agent id");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
    drop(store);
    let path = sp
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    std::fs::create_dir_all(path.parent().expect("agent parent")).expect("create parent");
    std::fs::write(&path, 8_u64.to_le_bytes()).expect("write truncated agent record");

    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_test_tool(&mut h, "restore-ext");
    h.handle_extension_message(
        "restore-ext",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            live_selectors: Vec::new(),
        }),
    )
    .expect("subscribe");

    let events = sink.lock().expect("sink");
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::HarnessNotice(notice)
                            if notice.kind == tau_proto::notice_kind::HARNESS_REPLAY_ERROR
                                && notice.message.contains("corrupt-agent")
                    )
            })
        }),
        "agent log corruption should emit a replay error notice"
    );
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                !delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::AgentReplayComplete(done)
                            if done.agent_id == agent_id && done.error.is_some()
                    )
            })
        }),
        "agent replay boundary should carry the agent log error"
    );
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                !delivery.is_replay()
                    && matches!(
                    delivery.event(),
                    Event::SessionReplayComplete(done)
                        if done.error.as_deref().is_some_and(|error| error.contains("corrupt-agent"))
                )
            })
        }),
        "session replay boundary should include the agent log error"
    );
    h.shutdown().expect("shutdown");
}

/// A late extension subscribe announces the current session state — the same
/// `SessionStarted`/`SessionAgentLoaded` snapshot a late UI gets. This is what
/// lets a respawned extension rebuild per-agent state (e.g. agent context)
/// after a mid-session crash instead of rejoining with amnesia.
#[test]
fn extension_subscribe_announces_current_session_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Load an agent into the session before the extension subscribes.
    h.send_user_message("s1", "hello snapshot", None)
        .expect("send message");

    let extension_events = connect_test_tool(&mut h, "respawned-extension");
    h.handle_extension_message(
        "respawned-extension",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            ],
        }),
    )
    .expect("extension subscribe");

    let events = extension_events.lock().expect("sink");
    assert!(
        events.iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionStarted(started)) if started.session_id.as_str() == "s1"
            )
        }),
        "late extension should be told the current session",
    );
    assert!(
        events.iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionAgentLoaded(loaded)) if loaded.session_id.as_str() == "s1"
            )
        }),
        "late extension should be told about already-loaded agents",
    );
    drop(events);

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_and_recalled_prompt_lifecycle_is_not_durable() {
    // Queue/recall state is process-local scheduler/UI state. Persisting only
    // part of that lifecycle makes cold resume resurrect prompts that already
    // dispatched or were recalled, so both events must remain transient.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");

    h.publish_event(
        None,
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "edit me".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_event(
        None,
        Event::AgentPromptRecalled(AgentPromptRecalled {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "edit me".to_owned(),
        }),
    );

    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().all(|entry| !matches!(
            entry.event,
            Event::AgentPromptQueued(_) | Event::AgentPromptRecalled(_)
        )),
        "cold replay must not resurrect transient queue lifecycle events: {events:?}"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_final_but_not_stale_queued_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let spid: AgentPromptId = "sp-replay".into();
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    let session_id = h.agents[&cid].session_id.clone();
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_event(
        None,
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "queued for reconnect".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_event(
        None,
        Event::AgentPromptCreated(AgentPromptCreated {
            agent_id: crate::parse_agent_id(&agent_id),
            agent_prompt_id: spid.clone(),
            session_id: session_id.clone(),
            system_prompt: String::new(),
            context: tau_proto::PromptContext { blocks: Vec::new() }, // Vec::new(),
            tools: Vec::new(),
            tools_ref: None,
            model: "test/model".parse().expect("model id"),
            model_params: Default::default(),
            tool_choice: Default::default(),
            originator: Default::default(),
            compaction: None,

            share_user_cache_key: false,
            ctx_id: None,
            operation: tau_proto::PromptOperation::Inference,
        }),
    );
    h.publish_event(
        None,
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: None,
            originator: Default::default(),
        }),
    );
    h.publish_event(
        None,
        Event::AgentCompactionTriggered(tau_proto::AgentCompactionTriggered {
            agent_id: crate::parse_agent_id(&agent_id),
            originator: tau_proto::PromptOriginator::User,
            resume_inference: false,
        }),
    );
    h.publish_event(
        None,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: spid,
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: assistant_output("final"),
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: Default::default(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("provider.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut replayed = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(event) = inner {
            replayed.push(event.name());
        }
    }

    assert!(replayed.contains(&tau_proto::EventName::PROVIDER_RESPONSE_FINISHED));
    assert!(replayed.contains(&tau_proto::EventName::AGENT_COMPACTION_TRIGGERED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_PROMPT_QUEUED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_PROMPT_CREATED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_PROMPT_STARTED));
    assert!(!replayed.contains(&tau_proto::EventName::PROVIDER_RESPONSE_UPDATED));

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_only_current_active_queue() {
    // Queue lifecycle events are transient; durable replay must not resurrect
    // prompts that already dispatched. A late UI still needs the harness's
    // current in-memory queue so it can show prompts that are actually pending.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.agents
        .get_mut(&cid)
        .expect("default conversation")
        .pending_prompts
        .push_back(crate::agent::PendingPrompt::user("still queued".to_owned()));

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("agent.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut queued = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(Event::AgentPromptQueued(event)) = inner {
            queued.push(event);
        }
    }

    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "still queued");
    assert_eq!(queued[0].agent_id.as_str(), agent_id);

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_terminal_tool_events() {
    // Background completions and cancellation are terminal UI facts. A
    // late UI needs them to clear running tool blocks that were created
    // from earlier live progress before the UI joined.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");

    // Seed known tool calls in the agent transcript before recording terminal
    // UI facts for them. Background completions are separate facts that arrive
    // after the foreground round has already been closed with a placeholder.
    let seed_tool_call = |h: &mut Harness, spid: &str, call_id: &str, tool_name: &str| {
        h.publish_for_agent(
            &cid,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: spid.into(),
                agent_id: crate::parse_agent_id(&agent_id),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new(tool_name),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                originator: Default::default(),
                usage: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        );
    };

    seed_tool_call(
        &mut h,
        "sp-background-result",
        "background-result-call",
        "background_ok",
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            call_id: "background-result-call".into(),
            tool_name: ToolName::new("background_ok"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("running".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: Default::default(),

            display: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "background-result-call".into(),
            tool_name: ToolName::new("background_ok"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            originator: Default::default(),

            display: None,
        }),
    );

    seed_tool_call(
        &mut h,
        "sp-background-error",
        "background-error-call",
        "background_err",
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            call_id: "background-error-call".into(),
            tool_name: ToolName::new("background_err"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("running".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: Default::default(),

            display: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "background-error-call".into(),
            tool_name: ToolName::new("background_err"),
            tool_type: tau_proto::ToolType::Function,
            message: "failed after backgrounding".to_owned(),
            details: None,
            originator: Default::default(),

            display: None,
        }),
    );

    seed_tool_call(&mut h, "sp-cancelled", "cancelled-call", "cancel_me");
    h.publish_for_agent(
        &cid,
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "cancelled-call".into(),
            tool_name: ToolName::new("cancel_me"),
            tool_type: tau_proto::ToolType::Function,
        }),
    );

    let durable_events = loaded_agent_events(&h, "s1");
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolBackgroundResult(result)
                if result.call_id.as_str() == "background-result-call")
        }),
        "background result should be in a durable loaded-agent event log"
    );
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "background-error-call")
        }),
        "background error should be in a durable loaded-agent event log"
    );
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolCancelled(cancelled)
                if cancelled.call_id.as_str() == "cancelled-call")
        }),
        "cancellation should be in a durable loaded-agent event log"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("tool.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_background_result = false;
    let mut got_background_error = false;
    let mut got_cancelled = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !(got_background_result && got_background_error && got_cancelled)
    {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        let TestProtocolItem::Event(event) = inner else {
            continue;
        };
        match event {
            Event::ToolBackgroundResult(result)
                if result.call_id.as_str() == "background-result-call" =>
            {
                got_background_result = true;
            }
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "background-error-call" =>
            {
                got_background_error = true;
            }
            Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancelled-call" => {
                got_cancelled = true;
            }
            _ => {}
        }
    }

    assert!(
        got_background_result,
        "late UI should replay background tool result"
    );
    assert!(
        got_background_error,
        "late UI should replay background tool error"
    );
    assert!(got_cancelled, "late UI should replay tool cancellation");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_does_not_replay_runtime_extension_setup() {
    // Extension discovery/context-ready events are runtime setup facts. The
    // durable replay path now comes from session membership plus loaded-agent
    // transcripts, so these extension events should neither land in the
    // membership log nor be replayed from a transcript to late UI clients.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Inject synthetic discovery events as if ext-shell had reported
    // them during eager init. They should remain runtime-only events.
    h.publish_event(
        Some(&tools_conn),
        Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
            file_path: "/test/AGENTS.md".into(),
            content: "# test\n".to_owned(),
        }),
    );
    h.publish_event(
        Some(&tools_conn),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: default_session_id().into(),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        }),
    );

    // Hook up a fake UI client via a UnixStream pair.
    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");

    // Find the UI connection the bus assigned. `accept_client`
    // gives it name "socket-ui".
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    // Trigger subscribe + replay via the normal client-event path.
    h.handle_client_event(
        &ui_conn,
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("extension.".to_owned())],
        })),
    )
    .expect("subscribe");

    let session_events = h
        .store
        .session_events(h.current_session_id.as_str())
        .expect("events");
    assert!(
        session_events.iter().all(|e| !matches!(
            &e.event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )),
        "runtime extension setup must not be persisted in the session membership log"
    );

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut agents_md_count = 0;
    let mut context_ready_count = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        let TestProtocolItem::Event(inner) = inner else {
            continue;
        };
        match inner {
            Event::ExtAgentsMdAvailable(a)
                if a.file_path == std::path::Path::new("/test/AGENTS.md") =>
            {
                agents_md_count += 1;
            }
            Event::ExtensionContextReady(_) => {
                context_ready_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        agents_md_count, 0,
        "runtime agents_md setup should not replay to late UI clients"
    );
    assert_eq!(
        context_ready_count, 0,
        "runtime context-ready setup should not replay to late UI clients"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn resumed_harness_replays_persisted_session_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    {
        let mut h = echo_harness_for("s1", &sp).expect("start");
        h.selected_model = Some("test/model".into());

        h.submit_user_prompt("s1".into(), "remember potato".to_owned())
            .expect("submit first prompt");
        let spid = h
            .prompt_agents
            .keys()
            .next()
            .expect("first session prompt id")
            .clone();
        let cid = h
            .prompt_agents
            .get(&spid)
            .expect("first prompt conversation")
            .clone();
        let agent_id = h
            .agents
            .get(&cid)
            .and_then(|conv| conv.agent_id.as_ref())
            .expect("first prompt agent id")
            .clone();
        h.handle_provider_response_finished(ProviderResponseFinished {
            agent_prompt_id: spid,
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: assistant_output("remembered potato"),
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("persist agent response");

        h.shutdown().expect("shutdown");
        drop(h);
        wait_for_session_unlock(&sp, "s1");
    }

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    resumed.selected_model = Some("test/model".into());

    resumed
        .submit_user_prompt("s1".into(), "what was it?".to_owned())
        .expect("submit resumed prompt");
    let spid = resumed
        .prompt_agents
        .keys()
        .next()
        .expect("resumed session prompt id")
        .clone();
    let prompt = read_prompt_created(&resumed, &spid);
    let serialized = serde_json::to_string(&prompt.context.flatten()).expect("json");

    assert!(
        serialized.contains("remember potato"),
        "resumed prompt must replay persisted user message: {serialized}",
    );
    assert!(
        serialized.contains("remembered potato"),
        "resumed prompt must replay persisted agent response: {serialized}",
    );
    assert!(
        serialized.contains("what was it?"),
        "resumed prompt must include the new prompt: {serialized}",
    );

    resumed.shutdown().expect("shutdown");
}

#[test]
fn thinking_is_persisted_but_excluded_from_prompt_replay() {
    // Linear-prefix and prompt-cache hygiene depends on
    // `assemble_conversation` ignoring the persisted thinking
    // field. Otherwise the model would see its own reasoning
    // summary echoed back as plain assistant text.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "first");

    let spid1 = h.send_prompt_to_agent("s1");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: assistant_output("answer"),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("persist agent response");

    append_user_message_via_event(&mut h, "s1", "second");
    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);
    let serialized = serde_json::to_string(&prompt2.context.flatten()).expect("json");
    assert!(
        !serialized.contains("The user is asking"),
        "prompt replay must not echo reasoning summary back to the model",
    );

    h.shutdown().expect("shutdown");
}

/// Peers that subscribed before a resumed session finished initializing must
/// end up with the same view as a late subscriber: `SessionStarted(Resume)`
/// live, then the loaded-agent roster and replay-marked transcript facts at
/// init completion. Without the init-completion catch-up, the durable history
/// of a resumed session — which predates the process and is never published
/// live — would be visible only to peers that subscribed after init.
#[test]
fn resumed_session_init_catches_up_subscribers_that_joined_before_init() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let past_text = "remembered before resume";
    {
        let mut h = echo_harness_for("s1", &sp).expect("start");
        h.selected_model = Some("test/model".into());
        h.send_user_message("s1", past_text, None)
            .expect("seed past message");
        h.shutdown().expect("shutdown");
        drop(h);
        wait_for_session_unlock(&sp, "s1");
    }

    // Fresh harness bound to a different session; the extension subscribes
    // while no s1 state is in play, mirroring a startup extension that is
    // already subscribed when a resume initializes.
    let mut h = echo_harness_for("s2", &sp).expect("start");
    let extension_events = connect_test_tool(&mut h, "early-extension");
    h.handle_extension_message(
        "early-extension",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
                EventSelector::Exact(tau_proto::EventName::PROVIDER_RESPONSE_FINISHED),
            ],
        }),
    )
    .expect("extension subscribe");
    extension_events.lock().expect("sink").clear();

    h.switch_session("s1".into(), tau_proto::SessionStartReason::Resume)
        .expect("switch to resumed session");

    let events = extension_events.lock().expect("sink");
    let started_count = events
        .iter()
        .filter(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionStarted(started)) if started.session_id.as_str() == "s1"
            )
        })
        .count();
    assert_eq!(
        started_count, 1,
        "already-subscribed peer should see exactly one live SessionStarted, not a duplicate from catch-up",
    );
    assert!(
        events.iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionAgentLoaded(loaded)) if loaded.session_id.as_str() == "s1"
            )
        }),
        "already-subscribed peer should learn the resumed session's loaded agents at init completion",
    );
    assert!(
        events.iter().any(|routed| {
            peel_delivery(&routed.frame).is_some_and(|delivery| {
                delivery.is_replay()
                    && matches!(
                        delivery.event(),
                        Event::ProviderResponseFinished(finished)
                            if provider_response_contains_text(finished, past_text)
                    )
            })
        }),
        "already-subscribed peer should receive the resumed transcript as replay-marked frames",
    );
    drop(events);

    h.shutdown().expect("shutdown");
}

/// Resume repair appends its synthetic tool errors to the durable log as it
/// publishes them live. Init-completion catch-up therefore runs before
/// repair, so a peer subscribed before init sees each synthetic error exactly
/// once — live — and not again as a replay-marked frame.
#[test]
fn resumed_session_repair_errors_are_not_duplicated_for_pre_init_subscribers() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    seed_restored_tool_round(&sp, &["call-restored"], &[]);

    let mut h = echo_harness_for("s2", &sp).expect("start");
    let extension_events = connect_test_tool(&mut h, "early-extension");
    h.handle_extension_message(
        "early-extension",
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_ERROR)],
        }),
    )
    .expect("extension subscribe");
    extension_events.lock().expect("sink").clear();

    h.switch_session("s1".into(), tau_proto::SessionStartReason::Resume)
        .expect("switch to resumed session");

    let events = extension_events.lock().expect("sink");
    let deliveries: Vec<bool> = events
        .iter()
        .filter_map(|routed| {
            peel_delivery(&routed.frame).and_then(|delivery| match delivery.event() {
                Event::ToolError(error) if error.call_id.as_str() == "call-restored" => {
                    Some(delivery.is_replay())
                }
                _ => None,
            })
        })
        .collect();
    assert_eq!(
        deliveries,
        vec![false],
        "synthetic repair error must arrive exactly once, live (got live/replay flags: {deliveries:?})",
    );
    drop(events);

    h.shutdown().expect("shutdown");
}

#[test]
fn replay_emits_latest_agent_metadata_before_session_agent_loaded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    {
        let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
        let mut sessions = tau_core::SessionStore::open(&sessions_dir).expect("session store");
        sessions
            .append_session_event(
                "s1",
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: "s1".into(),
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                    ephemeral: false,
                }),
            )
            .expect("seed session membership");
        let mut agents = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
        agents
            .append_agent_event(
                "agent-replay-meta",
                None,
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                    key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                    value: CborValue::Text("/first".to_owned()),
                    mutation_id: None,
                    inheritable: true,
                }),
            )
            .expect("seed first metadata");
        agents
            .append_agent_event(
                "agent-replay-meta",
                None,
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                    key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                    value: CborValue::Text("/latest".to_owned()),
                    mutation_id: Some(
                        tau_proto::AgentMetadataMutationId::parse("durable-live-token")
                            .expect("mutation id"),
                    ),
                    inheritable: true,
                }),
            )
            .expect("seed latest metadata");
    }

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    let sink = connect_test_client(&mut h, "metadata-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "metadata-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            ],
        })),
    )
    .expect("subscribe");

    let replayed: Vec<Event> = sink
        .lock()
        .expect("sink")
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame).cloned())
        .collect();
    let metadata_index = replayed
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentMetadataSet(set)
                    if set.agent_id.as_str() == "agent-replay-meta"
                        && set.key.as_str() == "ext_core-shell_cwd"
                        && set.value == CborValue::Text("/latest".to_owned())
                        && set.mutation_id.is_none()
            )
        })
        .expect("latest metadata replayed");
    let loaded_index = replayed
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::SessionAgentLoaded(loaded)
                    if loaded.agent_id.as_str() == "agent-replay-meta"
            )
        })
        .expect("session loaded replayed");
    assert!(metadata_index < loaded_index);
    assert!(replayed.iter().all(|event| !matches!(
        event,
        Event::AgentMetadataSet(set) if set.value == CborValue::Text("/first".to_owned())
    )));

    h.shutdown().expect("shutdown");
}
