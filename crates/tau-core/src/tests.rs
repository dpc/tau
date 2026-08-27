use std::fs as path_std_fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fs2::FileExt;
use tau_proto::{
    AgentDisplayNameSet, AgentHead, AgentHeadMoved, AgentId, AgentPromptId, AgentPromptSubmitted,
    CborValue, ContextItem, Event, EventSelector, HarnessNotice, HarnessOutputMessage,
    MessageAgentTarget, MessageDeleted, MessageDelivered, MessageEdited, MessageFactId,
    MessageFactRef, MessageParty, MessageReactionAdded, MessageReactionRemoved, MessageSent,
    NoticeLevel, PromptMessageClass, PromptOriginator, ProviderResponseFinished,
    ProviderStopReason, SessionAgentLoaded, SessionAgentUnloaded, SessionId, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolCallItem, ToolName, ToolRequest, ToolResult,
    ToolResultKind, ToolStarted, ToolType,
};

use crate::memory::MemorySink;
use crate::{
    AgentEntry, AgentEventParent, AgentStore, AgentStoreError, Connection, ConnectionOrigin,
    EventBus, MemoryInbox, NodeId, PendingConnectionMetadata, PersistedAgentEvent,
    PersistedAgentEventSeq, PersistedSessionEvent, PersistedSessionEventSeq, SessionMembership,
    SessionStore, SessionStoreError, list_session_metas, memory_connection,
};

/// Creates an in-memory test connection with an explicit transport origin.
fn test_connection(origin: ConnectionOrigin) -> (Connection, MemoryInbox) {
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        PendingConnectionMetadata {
            id: Some(test_connection_id("test-connection")),
            name: test_extension_name("test"),
            kind: tau_proto::ClientKind::Ui,
            origin,
        },
        Box::new(MemorySink::new(inbox.clone())),
    );
    (connection, inbox)
}

fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "tau-core-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    path
}

fn append_raw_cbor<T: serde::Serialize>(path: &std::path::Path, record: &T) {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).expect("encode test record");
    std::fs::create_dir_all(path.parent().expect("record parent")).expect("create parent");
    let mut file = path_std_fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open record stream");
    use std::io::Write;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write record length");
    file.write_all(&encoded).expect("write record body");
}

/// Construct one representative canonical message fact.
fn delivered_message_fact(agent_id: &str, message_id: &str) -> Event {
    Event::MessageDelivered(MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new(agent_id),
        MessageFactId::new(message_id),
        MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: Some("Sender".to_owned()),
            sender_auth: None,
        },
        None,
        "hello",
    ))
}

/// Construct all six message fact variants, including unresolved references.
fn all_message_facts(agent_id: &str) -> Vec<Event> {
    let publisher = tau_proto::MessagePublisherId::parse("bridge-main")
        .expect("canonical publisher id must satisfy the identifier grammar");
    let agent = MessageAgentTarget::new(agent_id);
    let target = MessageFactRef {
        publisher_extension_id: tau_proto::RawMessagePublisherId::new("other-bridge"),
        message_id: MessageFactId::new("unresolved"),
    };
    vec![
        delivered_message_fact(agent_id, "m1"),
        Event::MessageEdited(MessageEdited::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            None,
            None,
            "edited",
        )),
        Event::MessageDeleted(MessageDeleted::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            None,
            None,
        )),
        Event::MessageReactionAdded(MessageReactionAdded::new(
            publisher.clone(),
            agent.clone(),
            target.clone(),
            None,
            None,
            "👍",
        )),
        Event::MessageReactionRemoved(MessageReactionRemoved::new(
            publisher.clone(),
            agent.clone(),
            target,
            None,
            None,
            "👍",
        )),
        Event::MessageSent(MessageSent::new(
            publisher,
            agent,
            MessageFactId::new("m2"),
            None,
            None,
            "sent",
        )),
    ]
}

/// Socket clients may subscribe to recognized families without any durable
/// state or externally supplied validation mechanism.
#[test]
fn event_bus_accepts_known_socket_subscription() {
    let mut bus = EventBus::new();
    let (connection, _) = test_connection(ConnectionOrigin::Socket);
    let id = bus.connect(connection);
    let selector = EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE);

    bus.set_subscriptions(&id, Vec::new(), vec![selector.clone()])
        .expect("known socket family should be accepted");

    assert_eq!(bus.live_subscriptions(&id), Some([selector].as_slice()));
}

/// Unknown families remain unrestricted for non-socket connections because the
/// retained admissibility rule applies only at the socket boundary.
#[test]
fn event_bus_accepts_unknown_non_socket_subscription() {
    let mut bus = EventBus::new();
    let (connection, _) = test_connection(ConnectionOrigin::InMemory);
    let id = bus.connect(connection);
    let selector = EventSelector::Prefix("unknown.".to_owned());

    bus.set_subscriptions(&id, Vec::new(), vec![selector.clone()])
        .expect("non-socket subscription should remain unrestricted");

    assert_eq!(bus.live_subscriptions(&id), Some([selector].as_slice()));
}

/// The bus validates the de-duplicated historical/live union before replacing
/// either selector set, preventing a partial commit when one side is invalid.
#[test]
fn event_bus_rejects_invalid_socket_union_before_commit() {
    let mut bus = EventBus::new();
    let (connection, _) = test_connection(ConnectionOrigin::Socket);
    let id = bus.connect(connection);
    let existing = EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE);
    bus.set_subscriptions(&id, vec![existing.clone()], vec![existing.clone()])
        .expect("initial subscription");

    let error = bus
        .set_subscriptions(
            &id,
            vec![EventSelector::Prefix("unknown.".to_owned())],
            vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
        )
        .expect_err("unknown historical family should reject the combined replacement");

    assert!(matches!(
        error,
        crate::RouteError::SubscriptionDenied { .. }
    ));
    assert_eq!(
        bus.historical_subscriptions(&id),
        Some([existing.clone()].as_slice())
    );
    assert_eq!(bus.live_subscriptions(&id), Some([existing].as_slice()));
}

/// Buffered live delivery records the publish-time live selector match so a
/// later subscription update cannot drop or add already-committed events.
#[test]
fn event_bus_buffers_only_publish_time_live_matches() {
    let mut bus = EventBus::new();
    let (connection, inbox) =
        memory_connection(test_extension_name("ext"), tau_proto::ClientKind::Tool);
    let id = bus.connect(connection);
    bus.set_subscriptions(
        &id,
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
    )
    .expect("subscribe");
    bus.begin_catch_up(&id).expect("begin catch-up");

    let notice = Event::HarnessNotice(HarnessNotice {
        kind: "test".to_owned(),
        message: "buffer me".to_owned(),
        level: NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    });
    bus.publish(HarnessOutputMessage::deliver_live(
        tau_proto::UnixMicros::new(1),
        notice.clone(),
    ));
    bus.set_subscriptions(
        &id,
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
    )
    .expect("resubscribe while blocked");
    bus.finish_catch_up(&id).expect("finish catch-up");

    let frames = inbox.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame.delivered_event(), Some(&notice));
}

/// Removing historical selectors during catch-up releases the live stream so
/// peers cannot remain blocked forever after canceling their replay phase.
#[test]
fn event_bus_releases_catch_up_when_historical_selectors_are_cleared() {
    let mut bus = EventBus::new();
    let (connection, inbox) =
        memory_connection(test_extension_name("ext"), tau_proto::ClientKind::Tool);
    let id = bus.connect(connection);
    let live_selector = EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE);
    bus.set_subscriptions(
        &id,
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![live_selector.clone()],
    )
    .expect("subscribe with catch-up");
    bus.begin_catch_up(&id).expect("begin catch-up");

    let notice = Event::HarnessNotice(HarnessNotice {
        kind: "test".to_owned(),
        message: "release me".to_owned(),
        level: NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    });
    bus.publish(HarnessOutputMessage::deliver_live(
        tau_proto::UnixMicros::new(2),
        notice.clone(),
    ));
    assert!(inbox.snapshot().is_empty());

    bus.set_subscriptions(&id, Vec::new(), vec![live_selector])
        .expect("clear historical selectors");
    let frames = inbox.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame.delivered_event(), Some(&notice));
}

fn append_partial_record_header(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().expect("record parent")).expect("create parent");
    use std::io::Write;
    let mut file = path_std_fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open record stream");
    file.write_all(&[1, 2, 3])
        .expect("write partial record length");
}

fn agent_prompt(agent_id: &str, text: &str) -> Event {
    Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: PromptMessageClass::User,
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    })
}

fn session_loaded(session_id: &str, agent_id: &str, ephemeral: bool) -> Event {
    Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: SessionId::parse(session_id).expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        ephemeral,
    })
}

fn provider_tool_call(agent_id: &str, call_id: &str) -> Event {
    Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: AgentPromptId::parse("prompt-1")
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: ToolCallId::from(call_id),
            name: ToolName::new("example_tool"),
            tool_type: ToolType::Function,
            arguments: CborValue::Null,
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

fn background_placeholder(call_id: &str) -> Event {
    Event::ProviderToolResult(ToolResult {
        presentation: Default::default(),
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("example_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: ToolResultKind::BackgroundPlaceholder,
        display: None,
        originator: PromptOriginator::User,
    })
}

fn background_result(call_id: &str) -> Event {
    Event::ToolBackgroundResult(ToolBackgroundResult {
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("example_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Null,
        display: None,
        originator: PromptOriginator::User,
    })
}

fn background_error(call_id: &str) -> Event {
    Event::ToolBackgroundError(ToolBackgroundError {
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("example_tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    })
}

fn manual_compaction_request(agent_id: &str, request_id: &str) -> Event {
    Event::AgentManualCompactionRequested(tau_proto::AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse(request_id).expect("request id"),
        caller_agent_id: tau_proto::AgentId::parse("caller").expect("caller id"),
        target_agent_id: tau_proto::AgentId::parse(agent_id).expect("target id"),
        initiating_agent_prompt_id: "ap-origin"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        initiating_tool_call_id: "call-origin".into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
        visible_tool_name: ToolName::new("agent_compact"),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: 0,
        model: "provider/model".into(),
        resume_inference: false,
    })
}

/// Durable replay must preserve compact-fact ordinary-inference generation
/// authority, while stale manual compaction admission leaves no journal trace.
#[test]
fn manual_compaction_generation_replays_and_guards_durable_admission() {
    let agents_dir = temp_dir("manual-compaction-generation-replay");
    let checkpoint = |id: &str| {
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: AgentId::parse("target").expect("agent id"),
            transaction_id: None,
            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            through: tau_proto::AgentHead::Root,
            model: Some("provider/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
            output_length_continuation: None,
        })
    };
    let prompt = |id: &str, operation| {
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            agent_prompt_id: id
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: AgentId::parse("target").expect("agent id"),
            session_id: "session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            model: "provider/model".into(),
            operation,
            originator: PromptOriginator::User,
            ctx_id: None,
        })
    };
    let counters = |store: &AgentStore| {
        let tree = store.agent("target").expect("target tree");
        tree.ordinary_inference_generation()
    };

    let mut store = AgentStore::open(&agents_dir).expect("open store");
    let compaction_transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-generation").expect("transaction id");
    for event in [
        checkpoint("ap-first"),
        prompt("ap-first", tau_proto::PromptOperation::Inference),
        Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
            automatic_compaction_decision: None,
            agent_id: AgentId::parse("target").expect("agent id"),
            agent_prompt_id: "ap-first"
                .parse()
                .expect("known-safe AgentPromptId must be valid"),
            reason: tau_proto::AgentPromptTerminationReason::Canceled,
            originator: PromptOriginator::User,
        }),
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: AgentId::parse("target").expect("agent id"),
            transaction_id: compaction_transaction_id.clone(),
            compact_prompt_id: "ap-compact"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            cut: tau_proto::AgentHead::Root,
            resume_through: None,
            model: "provider/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::AutomaticThreshold,
        }),
        prompt(
            "ap-compact",
            tau_proto::PromptOperation::StandaloneCompaction,
        ),
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: AgentId::parse("target").expect("agent id"),
            transaction_id: compaction_transaction_id,
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
            context_retreat: None,
        }),
        checkpoint("ap-second"),
        prompt("ap-second", tau_proto::PromptOperation::Inference),
    ] {
        store
            .append_agent_event("target", None, event)
            .expect("append prompt");
    }
    assert_eq!(counters(&store), 2);

    drop(store);
    let mut reopened = AgentStore::open(&agents_dir).expect("reopen store");
    assert_eq!(counters(&reopened), 2);

    let sequence_before = reopened
        .agent("target")
        .expect("target tree")
        .next_event_seq();
    let event_count_before = reopened
        .agent_events("target")
        .expect("target events")
        .len();
    let mut request = manual_compaction_request("target", "cr-generation");
    let Event::AgentManualCompactionRequested(requested) = &mut request else {
        unreachable!("helper constructs a manual compaction request");
    };
    requested.target_generation = 1;
    assert!(matches!(
        reopened.append_agent_event("target", None, request.clone()),
        Err(AgentStoreError::InvalidEvent { .. })
    ));
    assert_eq!(
        reopened
            .agent("target")
            .expect("target tree")
            .next_event_seq(),
        sequence_before
    );
    assert_eq!(
        reopened
            .agent_events("target")
            .expect("target events")
            .len(),
        event_count_before
    );
    assert_eq!(counters(&reopened), 2);

    let Event::AgentManualCompactionRequested(requested) = &mut request else {
        unreachable!("helper constructs a manual compaction request");
    };
    requested.target_generation = 2;
    reopened
        .append_agent_event("target", None, request)
        .expect("append current-generation request");
    drop(reopened);

    let reopened = AgentStore::open(&agents_dir).expect("reopen accepted request");
    assert_eq!(counters(&reopened), 2);
    assert!(matches!(
        reopened
            .agent("target")
            .expect("target tree")
            .manual_compaction_recoveries()
            .as_slice(),
        [crate::ManualCompactionRecovery::Waiting(request)]
            if request.request_id.to_string() == "cr-generation"
    ));
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Manual request facts must survive a durable cold reopen and remain
/// queryable as accepted-but-unstarted recovery state.
#[test]
fn manual_compaction_request_replays_after_durable_reopen() {
    let agents_dir = temp_dir("manual-compaction-durable");
    {
        let mut store = AgentStore::open(&agents_dir).expect("open store");
        store
            .append_agent_event(
                "target",
                None,
                manual_compaction_request("target", "cr-durable"),
            )
            .expect("append request");
    }
    let mut reopened = AgentStore::open(&agents_dir).expect("reopen store");
    let tree = reopened
        .load_agent("target")
        .expect("load target")
        .expect("target exists");
    assert!(matches!(
        tree.manual_compaction_recoveries().as_slice(),
        [crate::ManualCompactionRecovery::Waiting(request)]
            if request.request_id.to_string() == "cr-durable"
    ));
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Ephemeral agents fold the same semantic request state while creating no
/// durable per-agent directory.
#[test]
fn manual_compaction_request_stays_memory_only_for_ephemeral_agent() {
    let agents_dir = temp_dir("manual-compaction-ephemeral");
    let mut store = AgentStore::open(&agents_dir).expect("open store");
    store
        .mark_agent_ephemeral("target-ephemeral")
        .expect("mark ephemeral");
    store
        .append_agent_event(
            "target-ephemeral",
            None,
            manual_compaction_request("target-ephemeral", "cr-ephemeral"),
        )
        .expect("append request");
    assert!(matches!(
        store
            .agent("target-ephemeral")
            .expect("memory tree")
            .manual_compaction_recoveries()
            .as_slice(),
        [crate::ManualCompactionRecovery::Waiting(_)]
    ));
    assert!(!agents_dir.join("target-ephemeral").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Whitespace-only display names leave durable and cached agent state
/// unchanged; a later valid retry reuses the sequence and survives cold replay.
#[test]
fn agent_store_rejects_empty_display_name() {
    let agents_dir = temp_dir("empty-display-name");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed valid agent");
    let journal_path = agents_dir.join(agent_id.as_str()).join("events.cbor");
    let checkpoint_path = agents_dir.join(agent_id.as_str()).join("meta.json");
    let journal_before = std::fs::read(&journal_path).expect("baseline journal");
    let checkpoint_before = std::fs::read(&checkpoint_path).expect("baseline checkpoint");
    let events_before = store
        .agent_events(agent_id.as_str())
        .expect("baseline events");
    let tree_before = store
        .agent(agent_id.as_str())
        .expect("baseline tree")
        .clone();

    let error = store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "   ".to_owned(),
            }),
        )
        .expect_err("blank display names are invalid");

    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert_eq!(
        std::fs::read(&journal_path).expect("journal remains"),
        journal_before
    );
    assert_eq!(
        std::fs::read(&checkpoint_path).expect("checkpoint remains"),
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
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "Research".to_owned(),
            }),
        )
        .expect("valid display name appends");
    assert_eq!(retry.seq, PersistedAgentEventSeq::new(1));
    drop(store);

    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    reopened
        .lock_and_recover_agent(agent_id.as_str())
        .expect("reopen agent");
    assert_eq!(
        reopened
            .agent(agent_id.as_str())
            .expect("replayed agent")
            .display_name(),
        Some("Research")
    );
    assert_eq!(
        reopened
            .agent_events(agent_id.as_str())
            .expect("replayed events")
            .into_iter()
            .map(|record| record.seq)
            .collect::<Vec<_>>(),
        vec![
            PersistedAgentEventSeq::new(0),
            PersistedAgentEventSeq::new(1)
        ]
    );
    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_meta_initializes_and_explicitly_bumps_last_user_interaction() {
    // Accepted visible interactions must be durable content-free facts so the
    // checkpoint can reconstruct them after sidecar loss.
    let agents_dir = temp_dir("last-user-interaction");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("commit creation");
    let meta = store
        .agent_meta("agent-1")
        .expect("read initial agent meta")
        .expect("agent meta exists");
    assert_ne!(meta.created_at, 0);
    assert_eq!(meta.last_user_interaction_time, 0);

    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                display_name: "Research".to_owned(),
            }),
        )
        .expect("append display-name event");
    let meta = store
        .agent_meta("agent-1")
        .expect("read meta after background event")
        .expect("agent meta exists");
    assert_eq!(meta.last_user_interaction_time, 0);

    store
        .record_agent_user_interaction("agent-1")
        .expect("record explicit user interaction");
    let meta = store
        .agent_meta("agent-1")
        .expect("read meta after user interaction")
        .expect("agent meta exists");
    assert!(meta.last_user_interaction_time > 0);

    let meta_path = agents_dir.join("agent-1").join("meta.json");
    drop(store);
    std::fs::write(
        &meta_path,
        br#"{
  "created_at": 1,
  "last_touched": 2,
  "last_user_interaction_time": 3,
  "latest_user_prompt_preview": "private legacy prompt"
}"#,
    )
    .expect("replace checkpoint with preview-bearing v1");
    let _reopened = AgentStore::open(&agents_dir).expect("strict replay succeeds");
    let migrated_json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&meta_path).expect("strict load republishes checkpoint"),
    )
    .expect("decode migrated checkpoint");
    assert_eq!(migrated_json["schema_version"], 2);
    assert!(migrated_json.get("latest_user_prompt_preview").is_none());
    let entries = crate::list_agent_entries(&agents_dir).expect("list repaired agent");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, crate::AgentListStatus::Fresh);
    assert!(
        entries[0]
            .summary
            .as_ref()
            .and_then(|summary| summary.last_user_interaction_at_micros)
            .is_some()
    );

    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A valid checkpoint must make fresh listing independent of journal payload
/// size and a post-checkpoint append must repair only its validated suffix.
#[test]
fn agent_checkpoint_lists_fresh_and_repairs_a_suffix() {
    let agents_dir = temp_dir("agent-checkpoint-suffix");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                parent_agent: None,
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("append creation");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first event");
    let before = crate::list_agent_entries(&agents_dir).expect("fresh list");
    assert_eq!(before[0].status, crate::AgentListStatus::Fresh);

    let checkpoint_path = agents_dir.join("agent-1/meta.json");
    let old_checkpoint = std::fs::read(&checkpoint_path).expect("checkpoint bytes");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "second"))
        .expect("append second event");
    std::fs::write(&checkpoint_path, old_checkpoint).expect("restore stale checkpoint");
    drop(store);

    let repaired = crate::list_agent_entries(&agents_dir).expect("repair suffix");
    assert_eq!(repaired[0].status, crate::AgentListStatus::Fresh);
    let checkpoint: crate::AgentCheckpoint =
        serde_json::from_slice(&std::fs::read(&checkpoint_path).expect("repaired checkpoint"))
            .expect("decode checkpoint");
    assert_eq!(checkpoint.journal.next_seq, 3);
    assert_eq!(
        checkpoint.journal.covered_bytes,
        std::fs::metadata(agents_dir.join("agent-1/events.cbor"))
            .expect("journal metadata")
            .len()
    );
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Corrupt or missing summary JSON must never hide a journal-backed agent, and
/// a journal larger than the foreground budget must not be scanned implicitly.
#[test]
fn agent_checkpoint_listing_exposes_unrepairable_summary_state() {
    let agents_dir = temp_dir("agent-checkpoint-visible-corruption");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("meta.json"), b"{").expect("corrupt checkpoint");
    std::fs::write(agent_dir.join("events.cbor"), vec![0_u8; 300 * 1024])
        .expect("large invalid journal");

    let entries = crate::list_agent_entries(&agents_dir).expect("list artifacts");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].identity, crate::AgentListIdentity::JournalBacked);
    assert_eq!(entries[0].status, crate::AgentListStatus::CorruptSummary);
    assert!(entries[0].summary.is_none());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Foreground repair must reject a huge declared frame before allocating its
/// payload, even when the corrupt journal itself contains only the header. An
/// explicit deadline isolates frame validation from the elapsed-time budget.
#[test]
fn agent_checkpoint_repair_rejects_declared_frame_over_budget() {
    let agents_dir = temp_dir("agent-checkpoint-frame-budget");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("meta.json"), b"{").expect("corrupt checkpoint");
    std::fs::write(
        agent_dir.join("events.cbor"),
        (64_u64 * 1024 * 1024).to_le_bytes(),
    )
    .expect("oversized frame header");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("bounded list");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, crate::AgentListStatus::RepairFailed);
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Empty journal artifacts and validation-created empty tree caches reserve an
/// id but must not become semantic message-routing identities.
#[test]
fn agent_store_requires_committed_creation_for_routing_identity() {
    let agents_dir = temp_dir("agent-routing-identity");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("events.cbor"), []).expect("empty journal");
    let mut store = AgentStore::open_lazy(&agents_dir).expect("open store");
    assert!(store.agent_id_is_reserved("agent-1"));
    assert!(!store.agent_is_known_for_routing("agent-1"));

    store
        .validate_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Root,
            &agent_prompt("agent-1", "prospective"),
            tau_proto::UnixMicros::now(),
        )
        .expect("prospective validation");
    assert!(store.agent("agent-1").is_some(), "empty tree was cached");
    assert!(!store.agent_is_known_for_routing("agent-1"));

    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("commit creation");
    assert!(store.agent_is_known_for_routing("agent-1"));
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Empty and creationless journals remain visible artifacts but can never be
/// promoted to a fresh journal-backed identity.
#[test]
fn agent_checkpoint_rejects_creationless_identity() {
    for (name, records) in [
        ("empty", Vec::new()),
        (
            "creationless",
            vec![PersistedAgentEvent {
                observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
                seq: PersistedAgentEventSeq::new(0),
                source: None,
                event: agent_prompt("agent-1", "orphan"),
                parent: AgentEventParent::InheritHead,
                fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
                recorded_at: tau_proto::UnixMicros::now(),
            }],
        ),
    ] {
        let agents_dir = temp_dir(&format!("checkpoint-{name}"));
        let events_path = agents_dir.join("agent-1/events.cbor");
        std::fs::create_dir_all(events_path.parent().expect("agent dir")).expect("agent dir");
        std::fs::write(&events_path, []).expect("empty journal");
        for record in records {
            append_raw_cbor(&events_path, &record);
        }

        let entries = crate::list_agent_entries(&agents_dir).expect("list invalid identity");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].identity,
            crate::AgentListIdentity::UnverifiedArtifact
        );
        assert_ne!(entries[0].status, crate::AgentListStatus::Fresh);
        assert!(!agents_dir.join("agent-1/meta.json").exists());
        let _ = std::fs::remove_dir_all(agents_dir);
    }
}

/// An expired foreground-repair budget must not promote an unvalidated journal
/// artifact to a routable identity merely because the listing task was delayed.
#[test]
fn agent_checkpoint_expired_rebuild_remains_unverified() {
    let agents_dir = temp_dir("agent-checkpoint-expired-rebuild");
    let events_path = agents_dir.join("agent-1/events.cbor");
    std::fs::create_dir_all(events_path.parent().expect("agent dir")).expect("agent dir");
    std::fs::write(&events_path, []).expect("empty journal");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() - Duration::from_secs(1),
    )
    .expect("list deadline-expired artifact");

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].identity,
        crate::AgentListIdentity::UnverifiedArtifact
    );
    assert_eq!(entries[0].status, crate::AgentListStatus::MissingSummary);
    assert!(!agents_dir.join("agent-1/meta.json").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A writer holding an unvalidated journal lock must leave a missing-summary
/// artifact unverified while exposing that listing could not repair it.
#[test]
fn agent_checkpoint_busy_missing_summary_remains_unverified() {
    let agents_dir = temp_dir("agent-checkpoint-busy-rebuild");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("events.cbor"), []).expect("empty journal");
    let lock = path_std_fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(agent_dir.join("lock"))
        .expect("open lock");
    lock.lock_exclusive().expect("hold writer lock");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("list busy artifact");

    FileExt::unlock(&lock).expect("release writer lock");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].identity,
        crate::AgentListIdentity::UnverifiedArtifact
    );
    assert_eq!(entries[0].status, crate::AgentListStatus::Busy);
    assert!(!agent_dir.join("meta.json").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A missing checkpoint over a journal exceeding the foreground repair budget
/// must remain unverified without reading or allocating the journal payload.
#[test]
fn agent_checkpoint_budget_deferred_missing_summary_remains_unverified() {
    let agents_dir = temp_dir("agent-checkpoint-budget-deferred-rebuild");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(agent_dir.join("events.cbor"), vec![0_u8; 300 * 1024])
        .expect("over-budget journal");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("list budget-deferred artifact");

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].identity,
        crate::AgentListIdentity::UnverifiedArtifact
    );
    assert_eq!(entries[0].status, crate::AgentListStatus::MissingSummary);
    assert!(!agent_dir.join("meta.json").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A filesystem error during initial checkpoint reconstruction must not create
/// journal-backed identity from a missing summary.
#[test]
fn agent_checkpoint_io_failed_missing_summary_remains_unverified() {
    let agents_dir = temp_dir("agent-checkpoint-io-failed-rebuild");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(agent_dir.join("events.cbor")).expect("journal directory");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("list I/O-failed artifact");

    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].identity,
        crate::AgentListIdentity::UnverifiedArtifact
    );
    assert_eq!(entries[0].status, crate::AgentListStatus::RepairFailed);
    assert!(!agent_dir.join("meta.json").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A complete matching creation record must promote a missing-summary journal
/// only after full reconstruction writes its checkpoint.
#[test]
fn agent_checkpoint_matching_creation_rebuilds_as_journal_backed() {
    let agents_dir = temp_dir("agent-checkpoint-matching-creation-rebuild");
    let agent_dir = agents_dir.join("agent-1");
    let events_path = agent_dir.join("events.cbor");
    append_raw_cbor(
        &events_path,
        &PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("rebuild matching creation");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].identity, crate::AgentListIdentity::JournalBacked);
    assert_eq!(entries[0].status, crate::AgentListStatus::Fresh);
    assert!(agent_dir.join("meta.json").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A legacy sidecar retains its existing journal-backed deferred classification
/// when the repair budget cannot scan its associated journal.
#[test]
fn agent_checkpoint_budget_deferred_legacy_summary_remains_journal_backed() {
    let agents_dir = temp_dir("agent-checkpoint-legacy-budget-deferred-rebuild");
    let agent_dir = agents_dir.join("agent-1");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("meta.json"),
        serde_json::to_vec(&crate::AgentMeta {
            created_at: 1,
            last_touched: 1,
            last_user_interaction_time: 0,
            display_name: None,
            latest_user_prompt_preview: None,
        })
        .expect("encode legacy summary"),
    )
    .expect("write legacy summary");
    std::fs::write(agent_dir.join("events.cbor"), vec![0_u8; 300 * 1024])
        .expect("over-budget journal");

    let entries = crate::agent_checkpoint::list_agent_entries_until_for_test(
        &agents_dir,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("list legacy artifact");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].identity, crate::AgentListIdentity::JournalBacked);
    assert_eq!(entries[0].status, crate::AgentListStatus::Legacy);
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// A corrupt checkpoint over a 65-record journal must stop at the foreground
/// record cap and leave the untrusted sidecar untouched.
#[test]
fn agent_checkpoint_full_rebuild_stops_before_record_65() {
    let agents_dir = temp_dir("agent-checkpoint-record-budget");
    let mut store = AgentStore::open(&agents_dir).expect("open store");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),

                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("creation");
    for index in 1..65 {
        store
            .append_agent_event(
                "agent-1",
                None,
                Event::AgentDisplayNameSet(AgentDisplayNameSet {
                    agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                    display_name: format!("name-{index}"),
                }),
            )
            .expect("append small record");
    }
    drop(store);
    let meta_path = agents_dir.join("agent-1/meta.json");
    std::fs::write(&meta_path, b"{").expect("corrupt summary");

    let entries = crate::list_agent_entries(&agents_dir).expect("bounded list");
    assert_eq!(entries[0].status, crate::AgentListStatus::CorruptSummary);
    assert_eq!(std::fs::read(&meta_path).expect("sidecar retained"), b"{");
    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_persists_transcript_under_agent_directory() {
    let agents_dir = temp_dir("agents");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let outcome = store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "hello"))
        .expect("append agent event");

    assert_eq!(outcome.seq.get(), 0);
    assert_eq!(outcome.folded_node_id.map(|id| id.get()), Some(0));
    assert!(agents_dir.join("agent-1").join("events.cbor").exists());

    let reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree");
    assert_eq!(tree.agent_id(), "agent-1");
    assert_eq!(tree.current_branch().len(), 1);
    assert!(matches!(
        tree.current_branch()[0],
        AgentEntry::UserInput { .. }
    ));

    let events = reopened.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, agent_prompt("agent-1", "hello"));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_duplicate_background_completion_before_persisting() {
    // A backgrounded tool call has a singular real completion after its
    // provider-visible placeholder. Retrying or racing a second background
    // result/error for the same call must fail before it is written to the
    // durable event log.
    let agents_dir = temp_dir("agents-duplicate-background-completion");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, provider_tool_call("agent-1", "call-1"))
        .expect("append assistant tool call");
    store
        .append_agent_event("agent-1", None, background_placeholder("call-1"))
        .expect("append background placeholder");
    store
        .append_agent_event("agent-1", None, background_result("call-1"))
        .expect("append first background completion");

    let duplicate_result = store
        .append_agent_event("agent-1", None, background_result("call-1"))
        .expect_err("duplicate background result must be rejected");
    assert!(matches!(
        duplicate_result,
        AgentStoreError::InvalidEvent { .. }
    ));
    let duplicate_error = store
        .append_agent_event("agent-1", None, background_error("call-1"))
        .expect_err("background error after result must be rejected");
    assert!(matches!(
        duplicate_error,
        AgentStoreError::InvalidEvent { .. }
    ));

    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 3);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_duplicate_background_error_before_persisting() {
    // A failed background completion is just as terminal as a successful one.
    // Once ToolBackgroundError is recorded, later result/error completions for
    // the same call id must be rejected before they can reach the event log.
    let agents_dir = temp_dir("agents-duplicate-background-error");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, provider_tool_call("agent-1", "call-1"))
        .expect("append assistant tool call");
    store
        .append_agent_event("agent-1", None, background_placeholder("call-1"))
        .expect("append background placeholder");
    store
        .append_agent_event("agent-1", None, background_error("call-1"))
        .expect("append first background error");

    let duplicate_result = store
        .append_agent_event("agent-1", None, background_result("call-1"))
        .expect_err("background result after error must be rejected");
    assert!(matches!(
        duplicate_result,
        AgentStoreError::InvalidEvent { .. }
    ));
    let duplicate_error = store
        .append_agent_event("agent-1", None, background_error("call-1"))
        .expect_err("second background error must be rejected");
    assert!(matches!(
        duplicate_error,
        AgentStoreError::InvalidEvent { .. }
    ));

    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 3);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_accepts_background_completion_for_explicit_parent_branch() {
    // Background completions are durable side events for the branch containing
    // the original tool call. Validation must use the explicit fold parent
    // instead of the mutable global head so another branch cannot make a valid
    // late completion look unknown.
    let agents_dir = temp_dir("agents-background-completion-explicit-parent");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, provider_tool_call("agent-1", "call-1"))
        .expect("append assistant tool call");
    let branch_a_head = store
        .append_agent_event("agent-1", None, background_placeholder("call-1"))
        .expect("append background placeholder")
        .folded_node_id
        .expect("placeholder closes the tool round");
    let branch_b_head = store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Root,
            agent_prompt("agent-1", "branch b"),
            tau_proto::UnixMicros::now(),
        )
        .expect("append unrelated branch")
        .folded_node_id
        .expect("prompt folds");

    let wrong_parent = store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Under(branch_b_head),
            background_error("call-1"),
            tau_proto::UnixMicros::now(),
        )
        .expect_err("unrelated explicit branch must not validate call id");
    assert!(matches!(wrong_parent, AgentStoreError::InvalidEvent { .. }));

    store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Under(branch_a_head),
            background_result("call-1"),
            tau_proto::UnixMicros::now(),
        )
        .expect("completion belongs to explicit branch A parent");

    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 4);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_duplicate_background_completion_on_replay() {
    // Durable replay validates the same singular background-completion
    // invariant as live appends. A tampered or old log containing two real
    // completions for one backgrounded call must fail closed instead of letting
    // the later completion overwrite the earlier one in folded state.
    let agents_dir = temp_dir("agents-duplicate-background-completion-replay");
    let events_path = agents_dir.join("agent-1").join("events.cbor");
    for (seq, event) in [
        provider_tool_call("agent-1", "call-1"),
        background_placeholder("call-1"),
        background_result("call-1"),
        background_error("call-1"),
    ]
    .into_iter()
    .enumerate()
    {
        append_raw_cbor(
            &events_path,
            &PersistedAgentEvent {
                observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
                seq: PersistedAgentEventSeq::new(seq as u64),
                source: None,
                event,
                parent: AgentEventParent::InheritHead,
                fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
                recorded_at: tau_proto::UnixMicros::now(),
            },
        );
    }

    let error = AgentStore::open(&agents_dir).expect_err("duplicate completion must fail load");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_replays_background_completion_for_explicit_parent_branch() {
    // Durable replay must accept a late background completion whose explicit
    // parent points to the branch containing the original call, even if a later
    // unrelated event moved the global head to another branch.
    let agents_dir = temp_dir("agents-background-completion-explicit-parent-replay");
    let events_path = agents_dir.join("agent-1").join("events.cbor");
    for (seq, event, parent) in [
        (
            0,
            provider_tool_call("agent-1", "call-1"),
            AgentEventParent::InheritHead,
        ),
        (
            1,
            background_placeholder("call-1"),
            AgentEventParent::InheritHead,
        ),
        (
            2,
            agent_prompt("agent-1", "branch b"),
            AgentEventParent::Root,
        ),
        (
            3,
            background_result("call-1"),
            AgentEventParent::Under(NodeId::new(1)),
        ),
    ] {
        append_raw_cbor(
            &events_path,
            &PersistedAgentEvent {
                observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
                seq: PersistedAgentEventSeq::new(seq),
                source: None,
                event,
                parent,
                fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
                recorded_at: tau_proto::UnixMicros::now(),
            },
        );
    }

    let store = AgentStore::open(&agents_dir).expect("explicit parent replay should succeed");
    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 4);

    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Ephemeral agents must have normal live transcript semantics without
/// reserving any durable agent directory, event log, metadata, or lock file.
#[test]
fn agent_store_ephemeral_transcript_folds_and_replays_without_files() {
    let agents_dir = temp_dir("agents-ephemeral");
    let mut store = AgentStore::open_lazy(&agents_dir).expect("open agent store");

    store
        .mark_agent_ephemeral("agent-ephemeral")
        .expect("mark ephemeral");
    assert!(store.agent_exists("agent-ephemeral"));
    let outcome = store
        .append_agent_event(
            "agent-ephemeral",
            None,
            agent_prompt("agent-ephemeral", "keep this live only"),
        )
        .expect("append ephemeral event");

    assert_eq!(outcome.seq.get(), 0);
    assert_eq!(outcome.folded_node_id.map(|id| id.get()), Some(0));
    assert!(
        !agents_dir.join("agent-ephemeral").exists(),
        "ephemeral agent must not create durable state"
    );
    let tree = store
        .agent("agent-ephemeral")
        .expect("ephemeral tree should be live");
    assert_eq!(tree.current_branch().len(), 1);
    let events = store
        .agent_events("agent-ephemeral")
        .expect("ephemeral replay events");
    assert_eq!(events.len(), 1);
    assert_eq!(
        store
            .agent_meta("agent-ephemeral")
            .expect("read ephemeral meta")
            .expect("ephemeral meta")
            .latest_user_prompt_preview
            .as_deref(),
        Some("keep this live only")
    );

    let reopened = AgentStore::open_lazy(&agents_dir).expect("reopen agent store");
    assert!(
        !reopened.agent_exists("agent-ephemeral"),
        "ephemeral agent must be forgotten on store reopen"
    );

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_non_sequential_persisted_sequence_on_load() {
    let agents_dir = temp_dir("agents-bad-seq");
    let events_path = agents_dir.join("agent-1").join("events.cbor");

    // Persisted sequence is deliberately redundant with file order. Loading must
    // reject a mismatch so a reordered or spliced event stream is caught before
    // it is folded into the agent tree.
    append_raw_cbor(
        &events_path,
        &PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(1),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = AgentStore::open(&agents_dir).expect_err("bad sequence must fail load");
    assert!(matches!(error, AgentStoreError::InvalidSequence { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_partial_persisted_record_header_on_load() {
    // A torn length header is log corruption, not a clean end-of-file. Loading
    // must fail instead of silently truncating the durable agent transcript.
    let agents_dir = temp_dir("agents-torn-header");
    let events_path = agents_dir.join("agent-1").join("events.cbor");
    append_partial_record_header(&events_path);

    let error = AgentStore::open(&agents_dir).expect_err("torn header must fail load");
    assert!(matches!(error, AgentStoreError::Read { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_validates_persisted_parent_references_on_load() {
    // Durable replay must validate the same parent constraints as appends. A
    // tampered log with a dangling parent must fail before building the tree.
    let agents_dir = temp_dir("agents-bad-parent");
    let events_path = agents_dir.join("agent-1").join("events.cbor");
    append_raw_cbor(
        &events_path,
        &PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::Under(NodeId::new(99)),
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = AgentStore::open(&agents_dir).expect_err("bad parent must fail load");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_replays_explicit_root_parent_after_reopen() {
    let agents_dir = temp_dir("agents-explicit-root-parent");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "second"))
        .expect("append second prompt");
    store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Root,
            agent_prompt("agent-1", "fresh branch"),
            tau_proto::UnixMicros::now(),
        )
        .expect("append fresh branch prompt");

    let reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree");
    let fresh_branch = tree.nodes().last().expect("fresh branch node");

    assert_eq!(fresh_branch.parent_id, None);
    assert_eq!(tree.head(), Some(fresh_branch.id));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_unknown_explicit_parent_before_persisting() {
    let agents_dir = temp_dir("agents-unknown-explicit-parent");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");

    let error = store
        .append_agent_event_at(
            "agent-1",
            None,
            AgentEventParent::Under(NodeId::new(999)),
            agent_prompt("agent-1", "dangling parent"),
            tau_proto::UnixMicros::now(),
        )
        .expect_err("agent store must reject unknown explicit parents");
    match error {
        AgentStoreError::InvalidEvent { source } => {
            assert!(source.to_string().contains("unknown node_id: 999"));
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let events = store.agent_events("agent-1").expect("agent events");
    assert_eq!(events.len(), 1);
    let tree = store.agent("agent-1").expect("agent tree");
    assert_eq!(tree.nodes().len(), 1);

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_restores_head_move_before_next_append() {
    let agents_dir = temp_dir("agents-head-move");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "second"))
        .expect("append second prompt");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentHeadMoved(AgentHeadMoved {
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                head: AgentHead::Node(NodeId::new(0)),
            }),
        )
        .expect("persist head move");
    drop(store);

    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree after reopen");
    assert_eq!(tree.head(), Some(NodeId::new(0)));

    reopened
        .append_agent_event(
            "agent-1",
            None,
            agent_prompt("agent-1", "branched after resume"),
        )
        .expect("append resumed branch prompt");

    let tree = reopened.agent("agent-1").expect("agent tree after append");
    let branched = tree.nodes().last().expect("branched node");
    assert_eq!(branched.parent_id, Some(NodeId::new(0)));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_restores_root_head_move_before_next_append() {
    // Rewinding to before the first prompt is represented by a durable
    // `AgentHead::Root` head move. Replaying the agent log must preserve that
    // root cursor so the next user prompt starts a new root branch after
    // restart instead of inheriting the previous leaf.
    let agents_dir = temp_dir("agents-root-head-move");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "first"))
        .expect("append first prompt");
    store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentHeadMoved(AgentHeadMoved {
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                head: AgentHead::Root,
            }),
        )
        .expect("persist root head move");
    drop(store);

    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let tree = reopened.agent("agent-1").expect("agent tree after reopen");
    assert_eq!(tree.head(), None);

    reopened
        .append_agent_event(
            "agent-1",
            None,
            agent_prompt("agent-1", "branched from root after resume"),
        )
        .expect("append resumed root branch prompt");

    let tree = reopened.agent("agent-1").expect("agent tree after append");
    let branched = tree.nodes().last().expect("branched node");
    assert_eq!(branched.parent_id, None);

    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Durable session membership facts retain their fold and replay behavior.
#[test]
fn session_store_persists_membership_facts() {
    let sessions_dir = temp_dir("sessions");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let loaded = Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: SessionId::parse("session-1").expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        ephemeral: false,
    });
    let outcome = store
        .append_session_event("session-1", None, loaded.clone())
        .expect("append loaded");

    assert_eq!(outcome.seq.get(), 0);
    assert_eq!(outcome.folded_node_id, None);
    assert!(sessions_dir.join("session-1").join("events.cbor").exists());
    assert!(
        store
            .session("session-1")
            .expect("session membership")
            .contains_agent(&AgentId::parse("agent-1").expect("agent id"))
    );

    store
        .append_session_event(
            "session-1",
            None,
            Event::SessionAgentUnloaded(SessionAgentUnloaded {
                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
            }),
        )
        .expect("append unloaded");

    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    let membership = reopened.session("session-1").expect("session membership");
    assert_eq!(membership.session_id(), "session-1");
    assert!(!membership.contains_agent(&AgentId::parse("agent-1").expect("agent id")));
    let events = reopened.session_events("session-1").expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, loaded);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Raw append persists first, then derives the same transcript projection live
/// and after durable replay.
#[test]
fn agent_store_raw_message_fact_append_projects_after_commit_and_replay() {
    let agents_dir = temp_dir("agent-raw-message");
    let fact = delivered_message_fact("agent-1", "m1");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let outcome = store
        .append_agent_message_fact_at("agent-1", None, fact.clone(), tau_proto::UnixMicros::now())
        .expect("append raw message fact");
    assert_eq!(outcome.seq.get(), 0);
    assert!(outcome.folded_node_id.is_some());
    assert_eq!(
        store.agent("agent-1").expect("loaded agent").nodes().len(),
        1
    );
    let live_projection = store.agent("agent-1").expect("loaded agent").nodes()[0]
        .entry
        .clone();
    assert_eq!(
        store.agent_events("agent-1").expect("agent events")[0].event,
        fact
    );

    drop(store);
    let mut reopened = AgentStore::open(&agents_dir).expect("reopen agent store");
    let prompt = reopened
        .append_agent_event("agent-1", None, agent_prompt("agent-1", "after fact"))
        .expect("append after raw fact");
    assert_eq!(prompt.seq.get(), 1);
    assert_eq!(
        reopened
            .agent("agent-1")
            .expect("replayed agent")
            .nodes()
            .len(),
        2
    );
    assert_eq!(
        reopened.agent("agent-1").expect("replayed agent").nodes()[0].entry,
        live_projection
    );

    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Raw agent append rejects non-message events and a fact whose claimed target
/// differs from the selected journal before creating any record.
#[test]
fn agent_store_raw_message_fact_append_enforces_category_and_owner() {
    let agents_dir = temp_dir("agent-raw-message-owner");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");
    let wrong_category = store
        .append_agent_message_fact_at(
            "agent-1",
            None,
            agent_prompt("agent-1", "not a fact"),
            tau_proto::UnixMicros::now(),
        )
        .expect_err("non-message raw append must fail");
    assert!(matches!(
        wrong_category,
        AgentStoreError::UnsupportedRawEvent { .. }
    ));
    let wrong_owner = store
        .append_agent_message_fact_at(
            "agent-1",
            None,
            delivered_message_fact("agent-2", "m1"),
            tau_proto::UnixMicros::now(),
        )
        .expect_err("mismatched owner must fail");
    assert!(matches!(
        wrong_owner,
        AgentStoreError::MessageFactTargetMismatch { .. }
    ));
    assert!(!agents_dir.join("agent-1").join("events.cbor").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Durable replay rejects a raw message record with a noncanonical fold parent
/// instead of silently ignoring its structural ownership metadata.
#[test]
fn agent_store_replay_rejects_noncanonical_raw_message_parent() {
    let agents_dir = temp_dir("agent-raw-message-parent");
    append_raw_cbor(
        &agents_dir.join("agent-1").join("events.cbor"),
        &PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: delivered_message_fact("agent-1", "m1"),
            parent: AgentEventParent::Under(NodeId::new(99)),
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = AgentStore::open(&agents_dir).expect_err("noncanonical raw parent must fail");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Ephemeral agent raw facts replay from memory without creating an agent
/// journal, metadata sidecar, or lock file.
#[test]
fn ephemeral_agent_raw_message_fact_replays_without_files() {
    let agents_dir = temp_dir("ephemeral-agent-raw-message");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");
    store
        .mark_agent_ephemeral("agent-1")
        .expect("mark ephemeral");
    let fact = delivered_message_fact("agent-1", "m1");
    store
        .append_agent_message_fact_at("agent-1", None, fact.clone(), tau_proto::UnixMicros::now())
        .expect("append ephemeral raw fact");

    assert_eq!(
        store.agent_events("agent-1").expect("memory replay")[0].event,
        fact
    );
    assert!(!agents_dir.join("agent-1").exists());
    let _ = std::fs::remove_dir_all(agents_dir);
}

/// Session journals retain unrouteable message facts without changing the
/// folded loaded-agent membership and keep one contiguous sequence.
#[test]
fn session_store_persists_fallback_message_facts_without_membership_fold() {
    let sessions_dir = temp_dir("session-fallback-message");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let agent_id = AgentId::parse("agent-1").expect("agent id");
    store
        .append_session_event(
            "session-1",
            None,
            Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("append membership");
    let fact = delivered_message_fact("invalid target", "m1");
    let outcome = store
        .append_session_event("session-1", None, fact.clone())
        .expect("append fallback fact");
    assert_eq!(outcome.seq.get(), 1);
    assert!(
        store
            .session("session-1")
            .expect("membership")
            .contains_agent(&agent_id)
    );

    drop(store);
    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    assert!(
        reopened
            .session("session-1")
            .expect("replayed membership")
            .contains_agent(&agent_id)
    );
    let events = reopened
        .session_events("session-1")
        .expect("session events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].seq.get(), 1);
    assert_eq!(events[1].event, fact);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Session fallback accepts every message variant and preserves unresolved
/// references without native-message validation, including after cold replay
/// of the exact ordered records and sequences.
#[test]
fn session_store_preserves_all_message_fact_variants_and_unresolved_refs() {
    let sessions_dir = temp_dir("session-all-message-facts");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let facts = all_message_facts("invalid target");
    for (index, fact) in facts.iter().cloned().enumerate() {
        let outcome = store
            .append_session_event("session-1", None, fact)
            .expect("append fallback fact");
        assert_eq!(outcome.seq.get(), index as u64);
    }
    let expected = facts
        .into_iter()
        .enumerate()
        .map(|(index, event)| (PersistedSessionEventSeq::new(index as u64), event))
        .collect::<Vec<_>>();
    let records = store
        .session_events("session-1")
        .expect("live fallback records");
    assert_eq!(
        records
            .into_iter()
            .map(|record| (record.seq, record.event))
            .collect::<Vec<_>>(),
        expected
    );
    drop(store);

    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    assert_eq!(
        reopened
            .session_events("session-1")
            .expect("replayed fallback records")
            .into_iter()
            .map(|record| (record.seq, record.event))
            .collect::<Vec<_>>(),
        expected
    );
    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Ephemeral sessions retain ordinary membership and fallback records for
/// same-daemon replay while creating no session files.
#[test]
fn ephemeral_session_retains_fallback_message_facts_in_memory() {
    let sessions_dir = temp_dir("ephemeral-session-fallback");
    let mut store = SessionStore::open_ephemeral(&sessions_dir).expect("open ephemeral store");
    store
        .append_session_event(
            "session-1",
            None,
            Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                ephemeral: true,
            }),
        )
        .expect("retain membership");
    let fact = delivered_message_fact("invalid target", "m1");
    let outcome = store
        .append_session_event("session-1", None, fact.clone())
        .expect("retain fallback");

    assert_eq!(outcome.seq.get(), 1);
    let events = store.session_events("session-1").expect("memory events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event, fact);
    assert!(!sessions_dir.exists());
}

/// Session restore facts keep execution correlation out of agent transcript
/// storage while remaining replayable from a session-scoped log.
#[test]
fn session_restore_log_persists_tool_execution_facts_separately() {
    let sessions_dir = temp_dir("session-restore");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let request = Event::ToolRequest(ToolRequest {
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::new("demo"),
        tool_type: ToolType::Function,
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    });
    let started = Event::ToolStarted(ToolStarted {
        call_id: ToolCallId::from("call-1"),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    });

    store
        .append_session_restore_event_at(
            "session-1",
            None,
            request.clone(),
            tau_proto::UnixMicros::new(10),
        )
        .expect("append restore request");
    store
        .append_session_restore_event_at(
            "session-1",
            None,
            started.clone(),
            tau_proto::UnixMicros::new(11),
        )
        .expect("append restore started");

    assert!(!sessions_dir.join("session-1").join("events.cbor").exists());
    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    let events = reopened
        .session_restore_events("session-1")
        .expect("restore events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, request);
    assert_eq!(events[1].event, started);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Ephemeral sessions keep restore facts in memory for same-daemon replay while
/// avoiding durable restore-event files.
#[test]
fn ephemeral_session_restore_log_replays_from_memory_only() {
    let sessions_dir = temp_dir("ephemeral-session-restore");
    let mut store = SessionStore::open_ephemeral(&sessions_dir).expect("open ephemeral store");
    let started = Event::ToolStarted(ToolStarted {
        call_id: ToolCallId::from("call-ephemeral"),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    });

    store
        .append_session_restore_event_at(
            "session-1",
            None,
            started.clone(),
            tau_proto::UnixMicros::new(12),
        )
        .expect("append memory restore fact");

    assert!(!sessions_dir.exists());
    let events = store
        .session_restore_events("session-1")
        .expect("memory restore events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, started);
}

/// Restore logs reject a complete invalid sequence without changing journal
/// bytes.
#[test]
fn session_restore_append_rejects_invalid_existing_sequence() {
    let sessions_dir = temp_dir("bad-session-restore-seq");
    let session_dir = sessions_dir.join("session-1");
    let path = session_dir.join("restore-events.cbor");
    let bad = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(7),
        source: None,
        event: Event::ToolStarted(ToolStarted {
            call_id: ToolCallId::from("call-bad"),
            tool_name: ToolName::new("demo"),
            arguments: CborValue::Null,
            agent_id: AgentId::parse("agent-1").expect("agent id"),
            originator: PromptOriginator::User,
        }),
        recorded_at: tau_proto::UnixMicros::new(1),
    };
    append_raw_cbor(&path, &bad);
    let bytes_before = std::fs::read(&path).expect("invalid restore journal");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let error = store
        .append_session_restore_event_at(
            "session-1",
            None,
            bad.event.clone(),
            tau_proto::UnixMicros::new(2),
        )
        .expect_err("invalid complete restore sequence fails closed");
    assert!(matches!(error, SessionStoreError::Read { .. }));
    assert_eq!(
        std::fs::read(&path).expect("unchanged journal"),
        bytes_before
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Restore-log append recovery removes a torn suffix before appending sequence
/// zero again.
#[test]
fn session_restore_append_recovers_truncated_existing_log() {
    let sessions_dir = temp_dir("bad-session-restore-truncated");
    let path = sessions_dir.join("session-1").join("restore-events.cbor");
    std::fs::create_dir_all(path.parent().expect("restore parent")).expect("create parent");
    std::fs::write(&path, 8_u64.to_le_bytes()).expect("write torn header");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let event = Event::ToolStarted(ToolStarted {
        call_id: ToolCallId::from("call-torn"),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    });

    store
        .append_session_restore_event_at("session-1", None, event, tau_proto::UnixMicros::new(2))
        .expect("truncated restore log recovers");
    let records = store
        .session_restore_events("session-1")
        .expect("recovered restore log reads");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].seq, PersistedSessionEventSeq::new(0));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Restore-log append rejects a complete semantically invalid record without
/// changing journal bytes.
#[test]
fn session_restore_append_rejects_wrong_existing_event_kind() {
    let sessions_dir = temp_dir("bad-session-restore-kind");
    let path = sessions_dir.join("session-1").join("restore-events.cbor");
    let wrong = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(0),
        source: None,
        event: Event::SessionAgentLoaded(SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: SessionId::parse("session-1").expect("known-safe SessionId must be valid"),
            agent_id: AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        }),
        recorded_at: tau_proto::UnixMicros::new(1),
    };
    append_raw_cbor(&path, &wrong);
    let bytes_before = std::fs::read(&path).expect("invalid restore journal");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let event = Event::ToolStarted(ToolStarted {
        call_id: ToolCallId::from("call-good"),
        tool_name: ToolName::new("demo"),
        arguments: CborValue::Null,
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::User,
    });

    let error = store
        .append_session_restore_event_at("session-1", None, event, tau_proto::UnixMicros::new(2))
        .expect_err("wrong complete restore event kind fails closed");
    assert!(matches!(error, SessionStoreError::Read { .. }));
    assert_eq!(
        std::fs::read(&path).expect("unchanged journal"),
        bytes_before
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Per-agent ephemerality uses memory-only session membership facts: the live
/// daemon must know the agent is loaded, but session resume must not learn that
/// agent id from disk.
#[test]
fn session_store_can_fold_one_membership_fact_without_persisting_it() {
    let sessions_dir = temp_dir("sessions-one-ephemeral");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let event = Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: SessionId::parse("session-1").expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse("agent-ephemeral").expect("agent id"),
        ephemeral: true,
    });

    store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            event.clone(),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("append memory-only membership");

    assert!(
        store
            .session("session-1")
            .expect("live membership")
            .contains_agent(&AgentId::parse("agent-ephemeral").expect("agent id"))
    );
    assert!(
        !sessions_dir.join("session-1").exists(),
        "memory-only membership must not create a session directory"
    );
    assert_eq!(
        store
            .ephemeral_membership_events("session-1")
            .expect("process-local membership")
            .into_iter()
            .map(|record| record.event)
            .collect::<Vec<_>>(),
        vec![event],
        "same-daemon replay must retain the ephemeral membership overlay"
    );
    let reopened = SessionStore::open_lazy(&sessions_dir).expect("reopen session store");
    assert!(
        reopened
            .session_events("session-1")
            .expect("events")
            .is_empty(),
        "memory-only membership must be absent from durable replay"
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_memory_only_fact_does_not_skip_later_durable_sequence() {
    // Memory-only membership facts are live state only. They must not consume a
    // durable sequence number, or a later durable append would make replay fail.
    let sessions_dir = temp_dir("sessions-ephemeral-then-durable");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            session_loaded("session-1", "agent-ephemeral", true),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("append memory-only membership");
    let durable = store
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-durable", false),
        )
        .expect("append durable membership");

    assert_eq!(durable.seq.get(), 0);
    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    let events = reopened.session_events("session-1").expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].seq.get(), 0);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_memory_only_fact_between_durable_facts_keeps_sequence_contiguous() {
    // Interleaving a memory-only membership fact between durable records must not
    // create an on-disk sequence gap that would break later resume.
    let sessions_dir = temp_dir("sessions-durable-ephemeral-durable");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let first = store
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-one", false),
        )
        .expect("append first durable membership");
    store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            session_loaded("session-1", "agent-ephemeral", true),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("append memory-only membership");
    let second = store
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-two", false),
        )
        .expect("append second durable membership");

    assert_eq!(first.seq.get(), 0);
    assert_eq!(second.seq.get(), 1);
    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    let events = reopened.session_events("session-1").expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq.get(), 0);
    assert_eq!(events[1].seq.get(), 1);

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Durable-session ephemeral membership uses an independent contiguous overlay,
/// enforces its restricted lifecycle, and never consumes durable sequences.
#[test]
fn session_store_ephemeral_membership_overlay_is_strict_and_independently_sequenced() {
    let sessions_dir = temp_dir("sessions-strict-ephemeral-overlay");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let first_durable = store
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-durable-one", false),
        )
        .expect("first durable membership");
    assert_eq!(first_durable.seq.get(), 0);

    let unmatched = store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-ephemeral").expect("agent id"),
            }),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect_err("unmatched process-local unload must fail");
    assert!(matches!(unmatched, SessionStoreError::InvalidEvent { .. }));
    for invalid in [
        session_loaded("session-1", "agent-not-ephemeral", false),
        delivered_message_fact("invalid target", "overlay-message"),
    ] {
        let error = store
            .append_session_event_at_with_persistence(
                "session-1",
                None,
                invalid,
                tau_proto::UnixMicros::now(),
                crate::SessionPersistenceMode::Ephemeral,
            )
            .expect_err("invalid process-local overlay event must fail");
        assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    }

    let load = store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            session_loaded("session-1", "agent-ephemeral", true),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("overlay load");
    let unload = store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-ephemeral").expect("agent id"),
            }),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("overlay unload");
    let reload = store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            session_loaded("session-1", "agent-ephemeral", true),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("overlay reload");
    assert_eq!(
        [load.seq.get(), unload.seq.get(), reload.seq.get()],
        [0, 1, 2]
    );

    let second_durable = store
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-durable-two", false),
        )
        .expect("second durable membership");
    assert_eq!(second_durable.seq.get(), 1);
    let overlay = store
        .ephemeral_membership_events("session-1")
        .expect("validated overlay");
    let durable = store.session_events("session-1").expect("durable records");
    assert_eq!(
        durable
            .iter()
            .map(|record| record.seq.get())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    let mut membership = SessionMembership::from_events(
        SessionId::parse("session-1").expect("known-safe SessionId must be valid"),
        &durable,
    );
    membership
        .apply_ephemeral_membership_overlay(&overlay)
        .expect("compose overlay");
    assert!(
        membership.contains_agent(&AgentId::parse("agent-ephemeral").expect("ephemeral agent id"))
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_rejects_non_sequential_persisted_sequence_on_load() {
    let sessions_dir = temp_dir("sessions-bad-seq");
    let events_path = sessions_dir.join("session-1").join("events.cbor");

    // Persisted sequence is deliberately redundant with file order. Loading must
    // reject a mismatch so a reordered or spliced membership stream is caught
    // before it is folded into the session view.
    append_raw_cbor(
        &events_path,
        &PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(1),
            source: None,
            event: Event::SessionAgentLoaded(SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: SessionId::parse("session-1")
                    .expect("known-safe SessionId must be valid"),
                agent_id: AgentId::parse("agent-1").expect("agent id"),
                ephemeral: false,
            }),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = SessionStore::open(&sessions_dir).expect_err("bad sequence must fail load");
    assert!(matches!(error, SessionStoreError::InvalidSequence { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// A lock-time reload rejects a complete invalid sequence without reusing an
/// unlocked cached cursor or changing journal bytes.
#[test]
fn session_store_rejects_invalid_lock_time_reload() {
    let sessions_dir = temp_dir("sessions-lock-reload-corrupt");
    let events_path = sessions_dir.join("session-1").join("events.cbor");
    let mut setup = SessionStore::open(&sessions_dir).expect("setup session store");
    setup
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-old", false),
        )
        .expect("baseline membership");
    drop(setup);
    let mut store = SessionStore::open(&sessions_dir).expect("preload unlocked membership");

    std::fs::remove_file(&events_path).expect("remove baseline journal");
    append_raw_cbor(
        &events_path,
        &PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(5),
            source: None,
            event: session_loaded("session-1", "agent-corrupt", false),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );
    let bytes_before = std::fs::read(&events_path).expect("invalid journal");
    store
        .lock_and_load_session("session-1")
        .expect_err("complete invalid sequence fails closed");
    assert_eq!(
        std::fs::read(&events_path).expect("unchanged journal"),
        bytes_before
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Lock-time recovery preserves and composes the existing process-local
/// ephemeral membership overlay.
#[test]
fn session_store_replay_retry_preserves_ephemeral_membership_overlay() {
    let sessions_dir = temp_dir("sessions-lock-reload-overlay");
    let events_path = sessions_dir.join("session-1").join("events.cbor");
    let mut setup = SessionStore::open(&sessions_dir).expect("setup session store");
    setup
        .append_session_event(
            "session-1",
            None,
            session_loaded("session-1", "agent-durable", false),
        )
        .expect("baseline membership");
    drop(setup);
    let mut store = SessionStore::open(&sessions_dir).expect("preload unlocked membership");
    store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            session_loaded("session-1", "agent-ephemeral", true),
            tau_proto::UnixMicros::now(),
            crate::SessionPersistenceMode::Ephemeral,
        )
        .expect("ephemeral membership");

    std::fs::remove_file(&events_path).expect("remove baseline journal");
    append_raw_cbor(
        &events_path,
        &PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(5),
            source: None,
            event: session_loaded("session-1", "agent-corrupt", false),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );
    let bytes_before = std::fs::read(&events_path).expect("invalid journal");
    store
        .lock_and_load_session("session-1")
        .expect_err("complete invalid sequence fails closed");
    assert_eq!(
        std::fs::read(&events_path).expect("unchanged journal"),
        bytes_before
    );

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_rejects_partial_persisted_record_header_on_load() {
    // A partial length header means the durable membership log was torn. Resume
    // must surface that corruption instead of dropping the incomplete tail.
    let sessions_dir = temp_dir("sessions-torn-header");
    let events_path = sessions_dir.join("session-1").join("events.cbor");
    append_partial_record_header(&events_path);

    let error = SessionStore::open(&sessions_dir).expect_err("torn header must fail load");
    assert!(matches!(error, SessionStoreError::Read { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn session_store_validates_persisted_membership_events_on_load() {
    // Durable replay must reject non-membership records instead of silently
    // ignoring them and advancing the sequence cursor over corrupted data.
    let sessions_dir = temp_dir("sessions-bad-event");
    let events_path = sessions_dir.join("session-1").join("events.cbor");
    append_raw_cbor(
        &events_path,
        &PersistedSessionEvent {
            seq: PersistedSessionEventSeq::new(0),
            source: None,
            event: agent_prompt("agent-1", "not membership"),
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = SessionStore::open(&sessions_dir).expect_err("bad event must fail load");
    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Session-store entry points must reject identifiers that could escape their
/// single-directory storage boundary.
#[test]
fn session_store_rejects_path_escaping_session_ids() {
    // Session ids are used as directory names. They must be a single safe path
    // component so raw protocol ids cannot escape the configured store root.
    let sessions_dir = temp_dir("sessions-path-safe");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let error = store
        .append_session_event(
            "../escaped",
            None,
            session_loaded("safe-session", "agent-1", false),
        )
        .expect_err("path escaping id must fail");
    assert!(matches!(error, SessionStoreError::InvalidSessionId { .. }));
    assert!(!sessions_dir.join("..").join("escaped").exists());

    let error = store
        .session_events("/tmp/escaped")
        .expect_err("absolute id must fail");
    assert!(matches!(error, SessionStoreError::InvalidSessionId { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Session storage and protocol decoding must enforce one identifier grammar
/// instead of admitting a broader path-safe subset.
#[test]
fn session_store_rejects_identifiers_outside_the_shared_grammar() {
    // The store must use the protocol type's grammar rather than accepting a
    // broader path-safe subset that journals cannot subsequently decode.
    let sessions_dir = temp_dir("sessions-cli-shaped");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    for session_id in ["my project-abc123", "my.project-abc123", "café-abc123"] {
        let error = store
            .append_session_event(
                session_id,
                None,
                session_loaded("safe-session", "agent-1", false),
            )
            .expect_err("out-of-grammar session id must fail");
        assert!(matches!(error, SessionStoreError::InvalidSessionId { .. }));
    }

    let _ = std::fs::remove_dir_all(sessions_dir);
}

#[test]
fn list_session_metas_skips_invalid_session_directories() {
    // Listing is best-effort discovery. Invalid directory names should not leak
    // path-unsafe ids to resume/cleanup callers or make valid sessions vanish.
    let sessions_dir = temp_dir("sessions-list-invalid");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    store
        .record_session_meta("valid-session")
        .expect("record valid meta");

    let invalid_name = "x".repeat(SESSION_ID_TEST_INVALID_LEN);
    let invalid_meta_dir = sessions_dir.join(invalid_name);
    std::fs::create_dir_all(&invalid_meta_dir).expect("create invalid session dir");
    std::fs::write(
        invalid_meta_dir.join("meta.json"),
        serde_json::to_vec(&crate::SessionMeta {
            created_at: 1,
            last_touched: 1,
        })
        .expect("encode meta"),
    )
    .expect("write invalid meta");

    let metas = list_session_metas(&sessions_dir).expect("list metas");
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].0.as_str(), "valid-session");

    let _ = std::fs::remove_dir_all(sessions_dir);
}

const SESSION_ID_TEST_INVALID_LEN: usize = 129;

#[test]
fn agent_store_rejects_path_escaping_agent_ids_for_read_paths() {
    // Read/probe helpers also join ids into store paths, so invalid AgentIds
    // must fail before a raw string can escape the configured agents root.
    let agents_dir = temp_dir("agents-path-safe");
    let store = AgentStore::open(&agents_dir).expect("open agent store");

    let error = store
        .agent_events("../escaped")
        .expect_err("path escaping id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));
    assert!(store.agent_meta("../escaped").is_err());
    assert!(crate::agent_is_locked(&agents_dir, "../escaped").is_err());

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_invalid_agent_ids_without_panicking() {
    // Invalid AgentIds must return typed store errors at all public write/load
    // boundaries instead of reaching internal parse panics or escaped paths.
    let agents_dir = temp_dir("agents-invalid-id");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let error = store
        .append_agent_event("../escaped", None, agent_prompt("agent-1", "hello"))
        .expect_err("invalid append id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));
    let error = store
        .mark_agent_ephemeral("../escaped")
        .expect_err("invalid ephemeral id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));
    let error = store
        .record_agent_meta("../escaped")
        .expect_err("invalid metadata id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));
    let error = store
        .record_agent_user_interaction("../escaped")
        .expect_err("invalid interaction id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_invalid_agent_directory_names_on_open() {
    // Corrupt or manually-created durable directories with unsafe names must be
    // surfaced as load errors, not parsed with `expect` during eager open.
    let agents_dir = temp_dir("agents-invalid-dir");
    let events_path = agents_dir.join("bad.agent").join("events.cbor");
    append_raw_cbor(
        &events_path,
        &PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0_u8; 16]),
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::InheritHead,
            fold_semantics: crate::AgentJournalFoldSemantics::Legacy,
            recorded_at: tau_proto::UnixMicros::now(),
        },
    );

    let error = AgentStore::open(&agents_dir).expect_err("invalid directory id must fail");
    assert!(matches!(error, AgentStoreError::InvalidAgentId { .. }));

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_store_rejects_non_agent_transcript_events() {
    let agents_dir = temp_dir("agent-rejects-non-transcript");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let session_event = Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        session_id: SessionId::parse("session-1").expect("known-safe SessionId must be valid"),
        agent_id: AgentId::parse("agent-1").expect("agent id"),
        ephemeral: false,
    });
    let error = store
        .append_agent_event("agent-1", None, session_event)
        .expect_err("agent store must reject session membership events");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));

    let mismatched = agent_prompt("agent-2", "not this agent");
    let error = store
        .append_agent_event("agent-1", None, mismatched)
        .expect_err("agent store must reject mismatched agent events");
    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    assert!(!agents_dir.join("agent-1").join("events.cbor").exists());

    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn session_store_rejects_transcript_events() {
    let sessions_dir = temp_dir("session-rejects-transcript");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let error = store
        .append_session_event("session-1", None, agent_prompt("agent-1", "not membership"))
        .expect_err("session store must reject transcript events");

    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    assert!(!sessions_dir.join("session-1").join("events.cbor").exists());

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}

/// Builds a validated connection identifier used by this test module.
fn test_connection_id(value: impl AsRef<str>) -> tau_proto::ConnectionId {
    tau_proto::ConnectionId::parse(value.as_ref())
        .expect("test connection id must satisfy the identifier grammar")
}
