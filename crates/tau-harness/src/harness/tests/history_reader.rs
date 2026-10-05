//! Cold-cache handoff cuts; production eviction is deliberately not enabled
//! yet.

use super::*;
use crate::event::HarnessCommand;
use crate::history_reader::HistoryReader;

/// The reader stays outside the runtime until explicitly released.
struct ReadCut {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    completion: mpsc::Receiver<HarnessEvent>,
}

impl ReadCut {
    fn install(h: &mut Harness) -> Self {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (tx, completion) = mpsc::channel();
        h.session_runtime.history.set_test_reader(
            HistoryReader::start_with_reader(tx, move |prefix| {
                entered_tx.send(()).expect("entered");
                release_rx.recv().expect("release");
                prefix.prefetch()
            })
            .expect("reader"),
        );
        Self {
            entered,
            release,
            completion,
        }
    }

    fn wait(&self) {
        self.entered
            .recv_timeout(Duration::from_secs(2))
            .expect("active read");
    }

    fn finish(self, h: &mut Harness) {
        self.release.send(()).expect("release");
        let HarnessEvent::Command(HarnessCommand::HistoryReadCompleted(completed)) = self
            .completion
            .recv_timeout(Duration::from_secs(2))
            .expect("completion")
        else {
            panic!("unexpected completion");
        };
        h.complete_history_read(*completed);
    }
}

fn metadata(agent: &tau_proto::AgentId, value: &str) -> Event {
    Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent.clone(),
        key: "history-test".into(),
        value: CborValue::Text(value.to_owned()),
        inheritable: false,
        mutation_id: None,
    })
}

/// Prepare authoritative membership without a runtime route: replay must use
/// the canonical roster, not the routing registry.
fn prepared_roster_agent(h: &mut Harness, name: &str) -> tau_proto::AgentId {
    let agent = crate::parse_agent_id(name);
    h.session_runtime
        .agent_store
        .reserve_new_agent(agent.as_str())
        .expect("reserve");
    h.session_runtime
        .agent_store
        .append_agent_event(
            agent.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::default()),
                agent_id: agent.clone(),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("creation");
    h.session_runtime
        .agent_store
        .append_agent_event(agent.as_str(), None, metadata(&agent, "before"))
        .expect("metadata");
    h.session_runtime
        .store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".parse().expect("session id"),
                agent_id: agent.clone(),
                agent_initialization_id: "history-init".parse().expect("init id"),
                ephemeral: false,
            }),
        )
        .expect("membership");
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable,
    );
    agent
}

fn cold_agent(h: &mut Harness) -> tau_proto::AgentId {
    let agent = prepared_roster_agent(h, "cold-history");
    h.session_runtime
        .agent_store
        .evict_agent_history(&agent, None)
        .expect("evict");
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    agent
}

fn selectors() -> Vec<EventSelector> {
    vec![EventSelector::Exact(
        tau_proto::EventName::AGENT_METADATA_SET,
    )]
}

fn metadata_deliveries(sink: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<(String, bool)> {
    sink.lock()
        .expect("sink")
        .iter()
        .filter_map(|routed| {
            let Event::AgentMetadataSet(set) = peel_inner_event(&routed.frame)? else {
                return None;
            };
            let HarnessOutputMessage::Deliver(delivery) = &routed.frame else {
                return None;
            };
            let CborValue::Text(value) = &set.value else {
                return None;
            };
            Some((value.clone(), delivery.replay))
        })
        .collect()
}

/// No new eligibility or catch-up begins before read completion; the handoff
/// merges updates accepted after the captured written cut. Existing replay
/// emits the current metadata snapshot before historical metadata facts.
#[test]
fn cold_history_subscribe_merges_current_suffix_before_live_handoff() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    let connection = crate::test_connection_id("history-ui");
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    h.publish_event(None, metadata(&agent, "during"));
    assert!(metadata_deliveries(&sink).is_empty());
    cut.finish(&mut h);
    assert_eq!(
        metadata_deliveries(&sink),
        [
            ("during".to_owned(), true),
            ("before".to_owned(), true),
            ("during".to_owned(), true)
        ]
    );
    h.publish_event(None, metadata(&agent, "after"));
    assert_eq!(
        metadata_deliveries(&sink).last(),
        Some(&("after".to_owned(), false))
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Replacing selectors leaves the old live subscription active during I/O.
#[test]
fn cold_history_replacement_keeps_old_live_subscription() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let connection = crate::test_connection_id("history-ui");
    h.complete_subscription(&connection, Vec::new(), selectors())
        .expect("old live");
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), Vec::new())
        .expect("replacement");
    cut.wait();
    h.publish_event(None, metadata(&agent, "during"));
    assert_eq!(metadata_deliveries(&sink), [("during".to_owned(), false)]);
    cut.finish(&mut h);
    h.publish_event(None, metadata(&agent, "after"));
    assert_eq!(
        metadata_deliveries(&sink),
        [
            ("during".to_owned(), false),
            ("during".to_owned(), true),
            ("before".to_owned(), true),
            ("during".to_owned(), true),
        ]
    );
}

/// Logical disconnect discards a valid physical result without hydrating the
/// cache or making the replacement subscription effective.
#[test]
fn cold_history_disconnect_cancels_handoff() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let connection = crate::test_connection_id("history-ui");
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    h.cancel_connection_history(&connection);
    cut.finish(&mut h);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(metadata_deliveries(&sink).is_empty());
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// A revoked captured generation is an expected cancellation, not corrupt
/// history and not a reason to terminate the session.
#[test]
fn cold_history_revoked_authority_is_nonfatal() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(
        &crate::test_connection_id("history-ui"),
        selectors(),
        selectors(),
    )
    .expect("defer");
    cut.wait();
    h.session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(2))
        .expect("revoke");
    cut.finish(&mut h);
    assert!(metadata_deliveries(&sink).is_empty());
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Actual corruption of the captured inode remains fatal even when the client
/// cancels while the physical read is in progress.
#[test]
fn cold_history_corruption_is_fatal_after_logical_cancellation() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let connection = crate::test_connection_id("history-ui");
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    h.cancel_connection_history(&connection);
    path_std_fs::OpenOptions::new()
        .write(true)
        .open(
            td.path()
                .join("agents")
                .join(agent.as_str())
                .join("events.cbor"),
        )
        .expect("captured inode")
        .set_len(0)
        .expect("truncate fixture");
    cut.finish(&mut h);
    assert!(metadata_deliveries(&sink).is_empty());
    assert!(matches!(
        h.runtime_io.publication.pending_error,
        Some(HarnessError::AgentStore(_))
    ));
}

/// Eight outstanding requests count their retained suffixes separately. Growth
/// past either limit cancels only history, and later normal appends still
/// commit.
fn suffix_budget_cancels_history(large_payload: bool) {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let cut = ReadCut::install(&mut h);
    let mut sinks = Vec::new();
    for index in 0..8 {
        let name = format!("history-{index}");
        sinks.push(connect_test_client(
            &mut h,
            &name,
            tau_proto::ClientKind::Ui,
        ));
        h.complete_subscription(&crate::test_connection_id(&name), selectors(), selectors())
            .expect("defer");
    }
    cut.wait();
    if large_payload {
        h.publish_event(None, metadata(&agent, &"x".repeat(9 * 1024 * 1024)));
    } else {
        for index in 0..513 {
            h.publish_event(None, metadata(&agent, &format!("record-{index}")));
        }
    }
    assert!(h.runtime_io.publication.pending_error.is_none());
    assert!(
        sinks
            .iter()
            .all(|sink| sink.lock().expect("sink").iter().any(|routed| {
                matches!(peel_inner_event(&routed.frame), Some(Event::HarnessNotice(notice))
            if notice.message.contains("history suffix budget exceeded"))
            })),
        "all logical requests rejected explicitly"
    );
    let before = h
        .session_runtime
        .agent_store
        .loaded_agent_record_count(&agent)
        .expect("count");
    h.publish_event(None, metadata(&agent, "after-overload"));
    assert_eq!(
        h.session_runtime
            .agent_store
            .loaded_agent_record_count(&agent),
        Some(before + 1)
    );
    cut.finish(&mut h);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(
        sinks
            .iter()
            .all(|sink| metadata_deliveries(sink).is_empty())
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

#[test]
fn cold_history_aggregate_record_budget_cancels_without_blocking_appends() {
    suffix_budget_cancels_history(false);
}

#[test]
fn cold_history_aggregate_byte_budget_cancels_without_blocking_appends() {
    suffix_budget_cancels_history(true);
}

/// Saturation rejects only the ninth independent request. A duplicate on the
/// first connection does not replace its already-owned continuation.
#[test]
fn cold_history_busy_and_duplicate_admission_preserve_first_request() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    let cut = ReadCut::install(&mut h);
    let mut sinks = Vec::new();
    for index in 0..9 {
        let name = format!("history-{index}");
        sinks.push(connect_test_client(
            &mut h,
            &name,
            tau_proto::ClientKind::Ui,
        ));
        h.complete_subscription(&crate::test_connection_id(&name), selectors(), selectors())
            .expect("defer");
    }
    cut.wait();
    h.complete_subscription(
        &crate::test_connection_id("history-0"),
        selectors(),
        Vec::new(),
    )
    .expect("duplicate");
    let notices = |sink: &Arc<Mutex<Vec<RoutedFrame>>>, needle: &str| {
        sink.lock().expect("sink").iter().any(|routed| {
            matches!(peel_inner_event(&routed.frame), Some(Event::HarnessNotice(notice))
                if notice.message.contains(needle))
        })
    };
    assert!(notices(&sinks[8], "history reader busy"));
    assert!(notices(&sinks[0], "history request already pending"));
    // Cancel queued requests so this deterministic reader only enters once.
    for index in 1..8 {
        h.cancel_connection_history(&crate::test_connection_id(&format!("history-{index}")));
    }
    cut.finish(&mut h);
    assert!(!metadata_deliveries(&sinks[0]).is_empty());
    assert!(metadata_deliveries(&sinks[8]).is_empty());
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// A violated protected cut is an integrity error, never disguised as ordinary
/// budget overload.
#[test]
fn cold_history_missing_protected_suffix_is_fatal() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(
        &crate::test_connection_id("history-ui"),
        selectors(),
        selectors(),
    )
    .expect("defer");
    cut.wait();
    h.publish_event(None, metadata(&agent, "protected"));
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    // Deliberately bypass the future lifecycle/pin-aware eviction owner.
    h.session_runtime
        .agent_store
        .evict_agent_history(&agent, None)
        .expect("violate pin");
    h.check_history_pin_budget();
    assert!(matches!(
        h.runtime_io.publication.pending_error,
        Some(HarnessError::AgentStore(_))
    ));
    cut.finish(&mut h);
}

/// Prompt-anchor navigation remains inert while cold history is read, then
/// revalidates and publishes its ordinary durable head move.
#[test]
fn cold_history_prompt_navigation_waits_for_handoff() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    append_user_message_via_event(&mut h, "s1", "first prompt");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let before = h.agent_runtime.agent_registry.agents[&cid].identity.head;
    assert!(before.is_some());
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    h.session_runtime
        .agent_store
        .evict_agent_history(&agent, None)
        .expect("evict");
    connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.handle_ui_navigate_tree(
        &crate::test_connection_id("history-ui"),
        tau_proto::UiNavigateTree {
            session_id: "s1".parse().expect("session"),
            target_agent_id: Some(agent),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(1),
        },
    )
    .expect("defer navigation");
    cut.wait();
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        before
    );
    cut.finish(&mut h);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        None
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Tree rendering keeps its response pending until complete history is present.
#[test]
fn cold_history_tree_request_waits_for_handoff() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    append_user_message_via_event(&mut h, "s1", "first prompt");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    h.session_runtime
        .agent_store
        .evict_agent_history(&agent, None)
        .expect("evict");
    let sink = connect_test_client_with_origin(
        &mut h,
        "history-ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let cut = ReadCut::install(&mut h);
    h.handle_ui_tree_request(
        &crate::test_connection_id("history-ui"),
        tau_proto::UiTreeRequest {
            session_id: "s1".parse().expect("session"),
            target_agent_id: Some(agent),
        },
    );
    cut.wait();
    assert!(sink.lock().expect("sink").is_empty());
    cut.finish(&mut h);
    assert!(sink.lock().expect("sink").iter().any(|routed| {
        matches!(peel_inner_event(&routed.frame), Some(Event::HarnessNotice(notice))
            if notice.message.contains("first prompt"))
    }));
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Pre-read authority/admission failures do not promote a writer failure to
/// history corruption. Unexpected lifecycle errors still fail closed.
#[test]
fn cold_history_classifies_authority_failures_without_masking_integrity_errors() {
    use tau_core::PersistenceAdmissionError as Error;
    let cases: [(fn() -> Error, bool); 8] = [
        (|| Error::StaleLease, false),
        (|| Error::Unavailable, false),
        (|| Error::Poisoned, false),
        (|| Error::CreationFailed, false),
        (|| Error::NotPrepared, false),
        (|| Error::Full, false),
        (|| Error::StreamNotFound, true),
        (
            || Error::Lifecycle("unexpected reader invariant".to_owned()),
            true,
        ),
    ];
    for (error, fatal) in cases {
        let td = TempDir::new().expect("root");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        cold_agent(&mut h);
        connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
        let (tx, rx) = mpsc::channel();
        h.session_runtime.history.set_test_reader(
            HistoryReader::start_with_reader(tx, move |_| {
                Err(tau_core::AgentStoreError::Persistence(error()))
            })
            .expect("reader"),
        );
        h.complete_subscription(
            &crate::test_connection_id("history-ui"),
            selectors(),
            selectors(),
        )
        .expect("defer");
        let HarnessEvent::Command(HarnessCommand::HistoryReadCompleted(completed)) =
            rx.recv_timeout(Duration::from_secs(2)).expect("completion")
        else {
            panic!("unexpected completion")
        };
        h.complete_history_read(*completed);
        assert_eq!(
            h.runtime_io.publication.pending_error.is_some(),
            fatal,
            "{:?}",
            error()
        );
    }
}

/// Duplicate UI operations use their own error protocol, without stealing or
/// completing the original subscription's continuation.
#[test]
fn cold_history_duplicate_ui_operations_preserve_original_subscription() {
    use crate::harness::history_runtime::HistoryOperation;
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let connection = crate::test_connection_id("history-ui");
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    for operation in [
        HistoryOperation::Tree(tau_proto::UiTreeRequest {
            session_id: "s1".parse().expect("session"),
            target_agent_id: Some(agent.clone()),
        }),
        HistoryOperation::Navigate(tau_proto::UiNavigateTree {
            session_id: "s1".parse().expect("session"),
            target_agent_id: Some(agent.clone()),
            target: tau_proto::UiTreeNavigationTarget::PromptAnchor(1),
        }),
    ] {
        assert!(h.defer_history_operation(&connection, vec![agent.clone()], operation));
    }
    {
        let frames = sink.lock().expect("sink");
        let notices = frames
            .iter()
            .filter_map(|frame| match peel_inner_event(&frame.frame) {
                Some(Event::HarnessNotice(notice)) => Some(notice),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|notice| notice.kind
            == tau_proto::notice_kind::UI_COMMAND_ERROR
            && notice.message.contains("history request already pending")));
        assert!(frames.iter().all(|frame| !matches!(
            peel_inner_event(&frame.frame),
            Some(Event::SessionReplayComplete(_))
        )));
    }
    cut.finish(&mut h);
    assert!(!metadata_deliveries(&sink).is_empty());
    assert_eq!(
        sink.lock()
            .expect("sink")
            .iter()
            .filter(|frame| matches!(
                peel_inner_event(&frame.frame),
                Some(Event::SessionReplayComplete(_))
            ))
            .count(),
        1
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// A resident agent entering the canonical roster during the read participates
/// in the same handoff without starting another read.
#[test]
fn cold_history_replay_includes_new_resident_roster_member() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    let connection = crate::test_connection_id("history-ui");
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    let added = prepared_roster_agent(&mut h, "new-history");
    // The helper accepts membership directly; invoke its owning accepted-load
    // reaction explicitly, just as production publication does.
    h.pin_history_roster_agent(&added);
    h.publish_event(None, metadata(&added, "new-roster"));
    assert!(metadata_deliveries(&sink).is_empty());
    cut.finish(&mut h);
    let deliveries = metadata_deliveries(&sink);
    assert_eq!(
        deliveries
            .iter()
            .filter(|(value, replay)| *replay && value == "new-roster")
            .count(),
        2,
        "new member contributes its current snapshot and historical fact"
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// A completed physical read from an old session binding cannot hydrate caches
/// or complete a request against the new binding.
#[test]
fn cold_history_old_session_generation_has_no_handoff_effect() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let connection = crate::test_connection_id("history-ui");
    let sink = connect_test_client(&mut h, "history-ui", tau_proto::ClientKind::Ui);
    let cut = ReadCut::install(&mut h);
    h.complete_subscription(&connection, selectors(), selectors())
        .expect("defer");
    cut.wait();
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_next();
    cut.finish(&mut h);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(sink.lock().expect("sink").is_empty());
    assert!(h.runtime_io.publication.pending_error.is_none());
}
