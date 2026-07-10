use std::path::PathBuf;

use tau_proto::{
    AgentDisplayNameSet, AgentHead, AgentHeadMoved, AgentId, AgentPromptId, AgentPromptSubmitted,
    CborValue, ContextItem, Event, EventSelector, HarnessNotice, HarnessOutputMessage, NoticeLevel,
    PromptMessageClass, PromptOriginator, ProviderResponseFinished, ProviderStopReason,
    SessionAgentLoaded, SessionAgentUnloaded, SessionId, ToolBackgroundError, ToolBackgroundResult,
    ToolCallId, ToolCallItem, ToolName, ToolRequest, ToolResult, ToolResultKind, ToolStarted,
    ToolType,
};

use crate::{
    AgentEntry, AgentEventParent, AgentStore, AgentStoreError, EventBus, NodeId,
    PersistedAgentEvent, PersistedAgentEventSeq, PersistedSessionEvent, PersistedSessionEventSeq,
    SessionStore, SessionStoreError, list_session_metas, memory_connection,
};

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
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open record stream");
    use std::io::Write;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .expect("write record length");
    file.write_all(&encoded).expect("write record body");
}

/// Buffered live delivery records the publish-time live selector match so a
/// later subscription update cannot drop or add already-committed events.
#[test]
fn event_bus_buffers_only_publish_time_live_matches() {
    let mut bus = EventBus::new();
    let (connection, inbox) = memory_connection("ext", tau_proto::ClientKind::Tool);
    let id = bus.connect(connection);
    bus.set_subscriptions(
        id.as_str(),
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
    )
    .expect("subscribe");
    bus.begin_catch_up(id.as_str()).expect("begin catch-up");

    let notice = Event::HarnessNotice(HarnessNotice {
        kind: "test".to_owned(),
        message: "buffer me".to_owned(),
        level: NoticeLevel::Info,
        always_show: false,
    });
    bus.publish(HarnessOutputMessage::deliver_live(
        tau_proto::UnixMicros::new(1),
        notice.clone(),
    ));
    bus.set_subscriptions(
        id.as_str(),
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
    )
    .expect("resubscribe while blocked");
    bus.finish_catch_up(id.as_str()).expect("finish catch-up");

    let frames = inbox.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame.delivered_event(), Some(&notice));
}

/// Removing historical selectors during catch-up releases the live stream so
/// peers cannot remain blocked forever after canceling their replay phase.
#[test]
fn event_bus_releases_catch_up_when_historical_selectors_are_cleared() {
    let mut bus = EventBus::new();
    let (connection, inbox) = memory_connection("ext", tau_proto::ClientKind::Tool);
    let id = bus.connect(connection);
    let live_selector = EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE);
    bus.set_subscriptions(
        id.as_str(),
        vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
        vec![live_selector.clone()],
    )
    .expect("subscribe with catch-up");
    bus.begin_catch_up(id.as_str()).expect("begin catch-up");

    let notice = Event::HarnessNotice(HarnessNotice {
        kind: "test".to_owned(),
        message: "release me".to_owned(),
        level: NoticeLevel::Info,
        always_show: false,
    });
    bus.publish(HarnessOutputMessage::deliver_live(
        tau_proto::UnixMicros::new(2),
        notice.clone(),
    ));
    assert!(inbox.snapshot().is_empty());

    bus.set_subscriptions(id.as_str(), Vec::new(), vec![live_selector])
        .expect("clear historical selectors");
    let frames = inbox.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame.delivered_event(), Some(&notice));
}

fn append_partial_record_header(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().expect("record parent")).expect("create parent");
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open record stream");
    file.write_all(&[1, 2, 3])
        .expect("write partial record length");
}

fn agent_prompt(agent_id: &str, text: &str) -> Event {
    Event::AgentPromptSubmitted(AgentPromptSubmitted {
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        text: text.to_owned(),
        message_class: PromptMessageClass::User,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    })
}

fn session_loaded(session_id: &str, agent_id: &str, ephemeral: bool) -> Event {
    Event::SessionAgentLoaded(SessionAgentLoaded {
        session_id: SessionId::from(session_id),
        agent_id: AgentId::parse(agent_id).expect("agent id"),
        ephemeral,
    })
}

fn provider_tool_call(agent_id: &str, call_id: &str) -> Event {
    Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: AgentPromptId::from("prompt-1"),
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
        originator: PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

fn background_placeholder(call_id: &str) -> Event {
    Event::ProviderToolResult(ToolResult {
        call_id: ToolCallId::from(call_id),
        tool_name: ToolName::new("example_tool"),
        tool_type: ToolType::Function,
        result: CborValue::Null,
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

#[test]
fn agent_store_rejects_empty_display_name() {
    // Display names are user-visible labels. Blank durable updates must not
    // suppress the id fallback in UIs or extensions.
    let agents_dir = temp_dir("empty-display-name");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    let error = store
        .append_agent_event(
            "agent-1",
            None,
            Event::AgentDisplayNameSet(AgentDisplayNameSet {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                display_name: "   ".to_owned(),
            }),
        )
        .expect_err("blank display names are invalid");

    assert!(matches!(error, AgentStoreError::InvalidEvent { .. }));
    let _ = std::fs::remove_dir_all(agents_dir);
}

#[test]
fn agent_meta_initializes_and_explicitly_bumps_last_user_interaction() {
    // User-interaction time is metadata state, not derived from replayable
    // transcript events. Background agent events must not refresh it when old
    // agents are loaded or replayed; accepted UI prompts call the explicit bump.
    let agents_dir = temp_dir("last-user-interaction");
    let mut store = AgentStore::open(&agents_dir).expect("open agent store");

    store
        .record_agent_meta("agent-1")
        .expect("record initial metadata");
    let meta = store
        .agent_meta("agent-1")
        .expect("read initial agent meta")
        .expect("agent meta exists");
    assert_ne!(meta.created_at, 0);
    assert_eq!(meta.last_user_interaction_time, meta.created_at);

    let meta_path = agents_dir.join("agent-1").join("meta.json");
    std::fs::write(
        &meta_path,
        r#"{
  "created_at": 1,
  "last_touched": 1,
  "last_user_interaction_time": 1
}"#,
    )
    .expect("seed deterministic metadata");

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
    assert_eq!(meta.last_user_interaction_time, 1);

    store
        .record_agent_user_interaction("agent-1")
        .expect("record explicit user interaction");
    let meta = store
        .agent_meta("agent-1")
        .expect("read meta after user interaction")
        .expect("agent meta exists");
    assert!(meta.last_user_interaction_time > 1);

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
                seq: PersistedAgentEventSeq::new(seq as u64),
                source: None,
                event,
                parent: AgentEventParent::InheritHead,
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
                seq: PersistedAgentEventSeq::new(seq),
                source: None,
                event,
                parent,
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
            seq: PersistedAgentEventSeq::new(1),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::InheritHead,
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
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::Under(NodeId::new(99)),
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

#[test]
fn session_store_persists_only_membership_facts() {
    let sessions_dir = temp_dir("sessions");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let loaded = Event::SessionAgentLoaded(SessionAgentLoaded {
        session_id: SessionId::from("session-1"),
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
                session_id: SessionId::from("session-1"),
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

/// Restore logs fail closed on corrupt sequence state rather than silently
/// restarting at sequence zero and appending over suspect history.
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
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");

    let error = store
        .append_session_restore_event_at(
            "session-1",
            None,
            bad.event.clone(),
            tau_proto::UnixMicros::new(2),
        )
        .expect_err("invalid restore sequence must fail");
    assert!(matches!(error, SessionStoreError::InvalidSequence { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Restore-log appends read existing records first, so a torn restore log is
/// reported instead of being extended with a misleading fresh sequence.
#[test]
fn session_restore_append_rejects_truncated_existing_log() {
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

    let error = store
        .append_session_restore_event_at("session-1", None, event, tau_proto::UnixMicros::new(2))
        .expect_err("truncated restore log must fail");
    assert!(matches!(error, SessionStoreError::Read { .. }));

    let _ = std::fs::remove_dir_all(sessions_dir);
}

/// Restore-log appends validate the semantic contents of existing records, not
/// only their CBOR framing and sequence numbers, so a membership log cannot be
/// accidentally extended as if it were a restore stream.
#[test]
fn session_restore_append_rejects_wrong_existing_event_kind() {
    let sessions_dir = temp_dir("bad-session-restore-kind");
    let path = sessions_dir.join("session-1").join("restore-events.cbor");
    let wrong = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(0),
        source: None,
        event: Event::SessionAgentLoaded(SessionAgentLoaded {
            session_id: SessionId::from("session-1"),
            agent_id: AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        }),
        recorded_at: tau_proto::UnixMicros::new(1),
    };
    append_raw_cbor(&path, &wrong);
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
        .expect_err("wrong restore event kind must fail");
    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));

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
        session_id: SessionId::from("session-1"),
        agent_id: AgentId::parse("agent-ephemeral").expect("agent id"),
        ephemeral: true,
    });

    store
        .append_session_event_at_with_persistence(
            "session-1",
            None,
            event,
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
                session_id: SessionId::from("session-1"),
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
            session_loaded("../escaped", "agent-1", false),
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

#[test]
fn session_store_accepts_cli_minted_path_safe_prefixes() {
    // CLI session ids are minted from raw cwd basenames plus a suffix, so the
    // store grammar must allow path-safe spaces, dots, and Unicode characters.
    let sessions_dir = temp_dir("sessions-cli-shaped");
    let mut store = SessionStore::open(&sessions_dir).expect("open session store");
    let session_id = "my project.café-abc123";

    store
        .append_session_event(
            session_id,
            None,
            session_loaded(session_id, "agent-1", false),
        )
        .expect("append cli-shaped session id");

    let reopened = SessionStore::open(&sessions_dir).expect("reopen session store");
    assert!(reopened.session(session_id).is_some());

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
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: agent_prompt("agent-1", "hello"),
            parent: AgentEventParent::InheritHead,
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
        session_id: SessionId::from("session-1"),
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
