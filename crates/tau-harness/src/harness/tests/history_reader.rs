//! Cold-cache handoff and pin-aware lifecycle eviction cuts.

use std::collections::{HashMap, HashSet};

use super::lifecycle::connect_handshaking_tool;
use super::*;
use crate::agent::Agent;
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

/// Keeps one real user runtime available for queued-dispatch oracles while its
/// written replay prefix is cold.
fn cold_user_agent(h: &mut Harness) -> tau_proto::AgentId {
    let cid = ensure_test_user_agent(h);
    let agent = durable_agent_id_for_conversation(h, &cid);
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
    agent
}

fn selectors() -> Vec<EventSelector> {
    vec![EventSelector::Exact(
        tau_proto::EventName::AGENT_METADATA_SET,
    )]
}

/// Uses the real Subscribe and Ready handlers, avoiding the legacy test
/// adapter's implicit historical selector replacement.
fn subscribe_late_extension(h: &mut Harness, connection: &tau_proto::ConnectionId) {
    h.handle_extension_message(
        connection,
        HarnessInputMessage::Subscribe(Subscribe {
            historical_selectors: selectors(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_READY),
            ],
        }),
    )
    .expect("subscribe");
}

/// A cold restart keeps staged declarations and global prompt dispatch behind
/// successful subscription handoff, even after Ready and a declaration
/// callback.
#[test]
fn cold_history_late_ready_waits_for_successful_handoff() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_handshaking_tool(&mut h, "late-shell");
    let connection = crate::test_connection_id("late-shell");
    let cut = ReadCut::install(&mut h);
    subscribe_late_extension(&mut h, &connection);
    cut.wait();
    h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
        .expect("ready receipt");
    h.handle_extension_event_inner(
        &connection,
        Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
    )
    .expect("declaration after Ready");
    h.maybe_finish_extension_activation(Some(&connection))
        .expect("declaration callback");
    assert!(h.extensions.ready_received.contains(&connection));
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Handshaking
    );
    assert!(!h.extensions_all_ready());
    h.ensure_extension_startup_deadlines(Instant::now());
    assert!(!h.extensions.startup_deadlines.contains_key(&connection));
    let providers =
        h.agent_context_provider_ids(agent.clone(), "probe-init".parse().expect("init"));
    assert!(!providers.contains(&connection));
    assert!(matches!(
        h.submit_user_prompt(
            "s1".parse().expect("session"),
            "queued behind cold Ready".to_owned()
        )
        .expect("queue"),
        PromptSubmission::Queued
    ));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    // Unrelated committed work continues while the reader is physically held.
    h.publish_event(None, metadata(&agent, "progress during Ready hold"));
    cut.finish(&mut h);
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Ready
    );
    assert!(!h.extensions.ready_received.contains(&connection));
    assert!(
        h.agent_context_provider_ids(agent, "probe-init".parse().expect("init"))
            .contains(&connection)
    );
    let session = h.session_runtime.current_session_id.clone();
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(session, &role);
    let new_agent = durable_agent_id_for_conversation(&h, &cid);
    assert!(
        h.prompt_coordination.context_discovery.pending_agents[&new_agent]
            .waiting_on
            .contains(&connection)
    );
    let frames = sink.lock().expect("sink");
    let boundary = frames.iter().position(|frame| matches!(peel_inner_event(&frame.frame), Some(Event::SessionReplayComplete(done)) if done.error.is_none())).expect("handoff");
    let ready = frames
        .iter()
        .position(|frame| {
            matches!(
                peel_inner_event(&frame.frame),
                Some(Event::ExtensionReady(_))
            )
        })
        .expect("online readiness");
    assert!(boundary < ready);
    let load = frames
        .iter()
        .position(|frame| {
            matches!(
                peel_inner_event(&frame.frame),
                Some(Event::SessionAgentLoaded(_))
            )
        })
        .expect("new agent sees provider subscription");
    assert!(boundary < load);
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// A successful read before Ready must not manufacture readiness; duplicate
/// Subscribe rejection must leave the original continuation eligible to finish.
#[test]
fn cold_history_late_subscription_success_before_ready_and_duplicate() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    connect_handshaking_tool(&mut h, "late-tool");
    let connection = crate::test_connection_id("late-tool");
    let cut = ReadCut::install(&mut h);
    subscribe_late_extension(&mut h, &connection);
    cut.wait();
    subscribe_late_extension(&mut h, &connection);
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Handshaking
    );
    cut.finish(&mut h);
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Handshaking
    );
    h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
        .expect("Ready after handoff");
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Ready
    );
}

/// Expected loss of read authority fails the exact startup instead of leaving
/// a received Ready stuck forever or activating without its subscriptions.
#[test]
fn cold_history_late_ready_authority_failure_disconnects_without_fatal_error() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    connect_handshaking_tool(&mut h, "late-tool");
    let connection = crate::test_connection_id("late-tool");
    let cut = ReadCut::install(&mut h);
    subscribe_late_extension(&mut h, &connection);
    cut.wait();
    h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
        .expect("Ready receipt");
    h.session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(2))
        .expect("revoke");
    cut.finish(&mut h);
    assert_eq!(
        h.extensions.entries[&connection].state,
        path_crate_extension::ExtensionState::Disconnected
    );
    assert!(!h.extensions.ready_received.contains(&connection));
    assert!(!h.session_runtime.history.has_pending());
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Old read completion cannot release a replacement's Ready or bring back the
/// disconnected connection's staged capabilities.
#[test]
fn cold_history_late_ready_disconnect_cannot_activate_replacement() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    cold_agent(&mut h);
    connect_handshaking_tool(&mut h, "old-tool");
    let old = crate::test_connection_id("old-tool");
    let cut = ReadCut::install(&mut h);
    subscribe_late_extension(&mut h, &old);
    cut.wait();
    h.handle_extension_message(&old, HarnessInputMessage::Ready(Default::default()))
        .expect("Ready");
    h.handle_disconnect(&old);
    connect_handshaking_tool(&mut h, "replacement-tool");
    let replacement = crate::test_connection_id("replacement-tool");
    h.extensions
        .entries
        .get_mut(&replacement)
        .expect("replacement")
        .name = crate::test_extension_name("old-tool");
    cut.finish(&mut h);
    assert_eq!(
        h.extensions.entries[&replacement].state,
        path_crate_extension::ExtensionState::Handshaking
    );
    assert!(!h.extensions.ready_received.contains(&old));
    assert!(!h.extensions.ready_received.contains(&replacement));
}

/// Fatal corruption and broken suffix authority cannot release the Ready
/// barrier through extension-disconnect cleanup before session termination.
#[test]
fn cold_history_late_ready_integrity_failure_never_dispatches_queued_work() {
    for broken_suffix in [false, true] {
        let td = TempDir::new().expect("root");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        h.config.selected_model = Some("test/model".into());
        let agent = cold_user_agent(&mut h);
        connect_handshaking_tool(&mut h, "late-tool");
        let connection = crate::test_connection_id("late-tool");
        let cut = ReadCut::install(&mut h);
        subscribe_late_extension(&mut h, &connection);
        cut.wait();
        h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
            .expect("Ready");
        assert!(matches!(
            h.submit_user_prompt(
                "s1".parse().expect("session"),
                "must not dispatch on fatal read".to_owned()
            )
            .expect("queue"),
            PromptSubmission::Queued
        ));
        if broken_suffix {
            h.publish_event(None, metadata(&agent, "protected suffix"));
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
                .expect("violate pin");
            h.check_history_pin_budget();
        } else {
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
                .expect("corrupt fixture");
        }
        cut.finish(&mut h);
        assert!(h.runtime_io.publication.pending_error.is_some());
        assert_eq!(
            h.extensions.entries[&connection].state,
            path_crate_extension::ExtensionState::Handshaking
        );
        assert!(h.extensions.ready_received.contains(&connection));
        assert!(!event_log_events(&h).iter().any(|event| matches!(
            event,
            Event::AgentPromptCreated(_)
                | Event::ProviderPromptSubmitted(_)
                | Event::ToolStarted(_)
        )));
    }
}

/// Both an activation Err and an internally recorded fatal error with Ok must
/// stop the production callback tail after activation has removed the barrier.
/// Inject only those result-boundary outcomes; queueing and dispatch stay real.
#[test]
fn cold_history_late_ready_activation_error_never_dispatches_queued_work() {
    for returned_error in [false, true] {
        let td = TempDir::new().expect("root");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        h.config.selected_model = Some("test/model".into());
        ensure_test_user_agent(&mut h);
        connect_handshaking_tool(&mut h, "late-tool");
        let connection = crate::test_connection_id("late-tool");
        assert!(matches!(
            h.submit_user_prompt(
                "s1".parse().expect("session"),
                "must stay queued after activation error".to_owned()
            )
            .expect("queue"),
            PromptSubmission::Queued
        ));
        h.set_extension_state(&connection, path_crate_extension::ExtensionState::Ready);
        assert!(
            h.extensions_all_ready(),
            "activation already removed the barrier"
        );
        let error = HarnessError::Participant("deferred activation failed".to_owned());
        let outcome = if returned_error {
            Err(error)
        } else {
            h.runtime_io.publication.pending_error = Some(error);
            Ok(())
        };
        h.finish_late_subscription_activation(outcome);
        assert!(h.runtime_io.publication.pending_error.is_some());
        assert!(!event_log_events(&h).iter().any(|event| matches!(
            event,
            Event::AgentPromptCreated(_)
                | Event::ProviderPromptSubmitted(_)
                | Event::ToolStarted(_)
        )));
    }
}

/// Roster and session-generation cancellation terminalize startup rather than
/// allowing an empty pending map to act as proof of successful subscription.
#[test]
fn cold_history_late_ready_canceled_binding_cannot_activate() {
    for session_changed in [false, true] {
        let td = TempDir::new().expect("root");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        cold_agent(&mut h);
        connect_handshaking_tool(&mut h, "late-tool");
        let connection = crate::test_connection_id("late-tool");
        let cut = ReadCut::install(&mut h);
        subscribe_late_extension(&mut h, &connection);
        cut.wait();
        h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
            .expect("Ready");
        if session_changed {
            h.session_runtime.current_session_generation = h
                .session_runtime
                .current_session_generation
                .saturating_next();
        } else {
            let new_agent = prepared_roster_agent(&mut h, "new-cold-agent");
            h.session_runtime
                .agent_store
                .evict_agent_history(&new_agent, None)
                .expect("new cold roster");
            h.pin_history_roster_agent(&new_agent);
        }
        cut.finish(&mut h);
        h.maybe_finish_extension_activation(Some(&connection))
            .expect("no later callback bypass");
        assert_eq!(
            h.extensions.entries[&connection].state,
            path_crate_extension::ExtensionState::Disconnected
        );
        assert!(!h.extensions.ready_received.contains(&connection));
        assert!(h.runtime_io.publication.pending_error.is_none());
    }
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
        sinks.push(if index == 0 {
            connect_handshaking_tool(&mut h, &name)
        } else {
            connect_test_client(&mut h, &name, tau_proto::ClientKind::Ui)
        });
        h.complete_subscription(&crate::test_connection_id(&name), selectors(), selectors())
            .expect("defer");
        if index == 0 {
            h.handle_extension_message(
                &crate::test_connection_id(&name),
                HarnessInputMessage::Ready(Default::default()),
            )
            .expect("Ready waiting on subscription");
        }
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
    assert_eq!(
        h.extensions.entries[&crate::test_connection_id("history-0")].state,
        path_crate_extension::ExtensionState::Disconnected,
        "budget rejection must terminalize the late startup"
    );
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
        sinks.push(if index == 8 {
            connect_handshaking_tool(&mut h, &name)
        } else {
            connect_test_client(&mut h, &name, tau_proto::ClientKind::Ui)
        });
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
    assert_eq!(
        h.extensions.entries[&crate::test_connection_id("history-8")].state,
        path_crate_extension::ExtensionState::Disconnected,
        "immediate rejection must fail startup even before Ready arrives"
    );
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
        h.config.selected_model = Some("test/model".into());
        cold_user_agent(&mut h);
        connect_handshaking_tool(&mut h, "history-ui");
        let connection = crate::test_connection_id("history-ui");
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
        h.handle_extension_message(&connection, HarnessInputMessage::Ready(Default::default()))
            .expect("Ready");
        assert!(matches!(
            h.submit_user_prompt(
                "s1".parse().expect("session"),
                "queued behind typed failure".to_owned()
            )
            .expect("queue"),
            PromptSubmission::Queued
        ));
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
        assert_eq!(
            h.extensions.entries[&connection].state,
            if fatal {
                path_crate_extension::ExtensionState::Handshaking
            } else {
                path_crate_extension::ExtensionState::Disconnected
            }
        );
        if fatal {
            assert!(!event_log_events(&h).iter().any(|event| matches!(
                event,
                Event::AgentPromptCreated(_)
                    | Event::ProviderPromptSubmitted(_)
                    | Event::ToolStarted(_)
            )));
        }
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
    assert_eq!(h.history_request_pin(&added), Some(0));
    h.finish_history_startup();
    h.evict_eligible_history();
    assert!(
        !h.session_runtime
            .agent_store
            .agent_history_is_evicted(&added),
        "the held read's new-roster cut protects the entire written cache"
    );
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
    assert_eq!(h.history_request_pin(&added), None);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&added)
    );
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

/// A live-only subscription must not scan cold agent journals merely to emit
/// its completion markers; its first live event is immediately eligible.
#[test]
fn cold_history_live_only_subscription_never_requires_prefetch() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = cold_agent(&mut h);
    let sink = connect_test_client(&mut h, "live-ui", tau_proto::ClientKind::Ui);
    h.complete_subscription(
        &crate::test_connection_id("live-ui"),
        Vec::new(),
        selectors(),
    )
    .expect("live-only handoff");
    assert!(!h.session_runtime.history.has_pending());
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(sink.lock().expect("sink").iter().any(|frame| matches!(
        peel_inner_event(&frame.frame),
        Some(Event::SessionReplayComplete(complete)) if complete.error.is_none()
    )));
    h.publish_event(None, metadata(&agent, "live"));
    assert_eq!(metadata_deliveries(&sink), [("live".to_owned(), false)]);
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Recreate only the runtime identity after a real unload; the store must keep
/// its existing tree and writer lease rather than opening a second owner.
fn reload_runtime(h: &mut Harness, agent: &tau_proto::AgentId) {
    let mut runtime = Agent::new(
        agent.clone(),
        999,
        h.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    runtime.identity.agent_id = Some(agent.clone());
    runtime.identity.role = Some("engineer".to_owned());
    h.agent_runtime
        .agent_registry
        .agents
        .insert(agent.clone(), runtime);
    h.ensure_loaded_agent_for_agent(agent, agent);
}

/// A retained lease may reload only after its cold history is ready. Neither an
/// empty provider wait set nor an explicit finalize call may publish context or
/// membership while the physical read is held.
#[test]
fn cold_history_reload_prefetches_before_membership_and_discovery() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    h.remove_agent_expected(&cid);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable,
    );
    h.evict_eligible_history();
    let lease = h
        .session_runtime
        .agent_store
        .agent_history_prefix(&agent)
        .expect("prefix")
        .expect("written prefix");
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    let sink = connect_test_client(&mut h, "load-ui", tau_proto::ClientKind::Ui);
    let live = vec![
        EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
        EventSelector::Exact(tau_proto::EventName::HARNESS_AGENT_CONTEXT_INITIALIZED),
    ];
    h.complete_subscription(&crate::test_connection_id("load-ui"), Vec::new(), live)
        .expect("live watcher");
    sink.lock().expect("sink").clear();
    let cut = ReadCut::install(&mut h);
    reload_runtime(&mut h, &agent);
    cut.wait();
    assert!(h.history_load_is_pending(&agent));
    h.finalize_agent_discovery(&agent)
        .expect("early finalize stays gated");
    h.observe_semantic_persistence_progress();
    assert!(
        h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    assert!(sink.lock().expect("sink").is_empty());
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    cut.finish(&mut h);
    lease
        .validate_authority()
        .expect("unload/reload retains writer lease");
    assert!(!h.history_load_is_pending(&agent));
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    let frames = sink.lock().expect("sink");
    for event_name in [
        tau_proto::EventName::SESSION_AGENT_LOADED,
        tau_proto::EventName::HARNESS_AGENT_CONTEXT_INITIALIZED,
    ] {
        assert_eq!(
            frames
                .iter()
                .filter_map(|frame| peel_inner_event(&frame.frame))
                .filter(|event| event.name() == event_name)
                .count(),
            1
        );
    }
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Written-progress sweeps and per-agent retirement cannot release startup's
/// independent protection before the real session-initialization tail.
#[test]
fn cold_history_startup_pin_survives_progress_and_agent_retirement() {
    let td = TempDir::new().expect("root");
    let agent = {
        let mut h = quiet_provider_harness(td.path()).expect("initial harness");
        let cid = ensure_test_user_agent(&mut h);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        h.shutdown().expect("clean initial journal");
        agent
    };
    wait_for_session_unlock(td.path(), "s1");
    let protected = agent.clone();
    let mut h = quiet_provider_harness_for_with_start_reason_storage_mode_and_hook(
        "s1",
        td.path(),
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::Durable,
        Some(Box::new(move |h| {
            assert!(
                !h.session_runtime
                    .agent_store
                    .agent_history_is_evicted(&protected)
            );
            h.observe_semantic_persistence_progress();
            h.retire_history_load(&protected);
            h.begin_history_preparation(&protected);
            h.finish_history_preparation(&protected);
            assert!(
                !h.session_runtime
                    .agent_store
                    .agent_history_is_evicted(&protected),
                "only startup still protects the full accepted prefix"
            );
        })),
    )
    .expect("resumed harness");
    h.evict_eligible_history();
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent),
        "the successful initialization tail releases startup without another write"
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Nested preparation, load reaction and discovery protect independently, and
/// stale initialization completions cannot release a newer attempt's history.
#[test]
fn cold_history_lifecycle_pins_release_only_their_exact_owner() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let agent = prepared_roster_agent(&mut h, "pinned-history");
    let load = tau_proto::SessionAgentLoaded {
        session_id: "s1".parse().expect("session"),
        agent_id: agent.clone(),
        agent_initialization_id: "current-init".parse().expect("initialization"),
        ephemeral: false,
    };
    let mut context = tau_proto::AgentInitializationContextSet {
        discovery_revision: 0,
        discovery_refreshes: Vec::new(),
        discovery_diagnostics: Vec::new(),
        session_id: load.session_id.clone(),
        agent_id: agent.clone(),
        agent_initialization_id: "stale-init".parse().expect("initialization"),
        agents_message: None,
        effective_skills: Vec::new(),
        agents_files: Vec::new(),
    };
    h.begin_history_load(&load);
    h.begin_history_preparation(&agent);
    h.begin_history_preparation(&agent);
    h.finish_history_discovery(&context);
    h.finish_history_preparation(&agent);
    assert!(
        !h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    h.enter_history_load_reaction(&load);
    h.finish_history_load_reaction(&load);
    h.finish_history_preparation(&agent);
    assert!(
        !h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent),
        "discovery retains protection after other owners finish"
    );
    context.agent_initialization_id = load.agent_initialization_id;
    h.finish_history_discovery(&context);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
}

/// Unloading an already-written discovery-protected cache must evict at the
/// final teardown edge, even if no further agent append or progress wake
/// occurs.
#[test]
fn cold_history_idle_unload_releases_final_discovery_protection() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    h.begin_history_preparation(&agent);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    let prefix = h
        .session_runtime
        .agent_store
        .agent_history_prefix(&agent)
        .expect("prefix")
        .expect("written prefix");
    let read = prefix.prefetch().expect("read");
    h.session_runtime
        .agent_store
        .install_agent_history_prefix(read)
        .expect("install");
    h.prompt_coordination
        .context_discovery
        .pending_agents
        .insert(
            agent.clone(),
            PendingAgentDiscovery {
                revision: 0,
                publishing_revision: None,
                retained_install: None,
                superseded_refreshes: Vec::new(),
                validated_skills: HashMap::new(),
                workdir_sources: HashMap::new(),
                initialization_id: "idle-unload".parse().expect("initialization"),
                skill_candidates: HashMap::new(),
                skills: HashMap::new(),
                agents_files: Vec::new(),
                waiting_on: HashSet::new(),
            },
        );
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    h.finish_history_preparation(&agent);
    assert!(
        !h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    h.remove_agent_expected(&cid);
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Live notice consumption must retain exact deduplication after eviction,
/// including byte-exact background text rather than a prefix match.
#[test]
fn cold_history_live_notice_dedup_survives_eviction() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let session = h.session_runtime.current_session_id.clone();
    h.begin_history_preparation(&agent);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    let prefix = h
        .session_runtime
        .agent_store
        .agent_history_prefix(&agent)
        .expect("prefix")
        .expect("written prefix");
    let read = prefix.prefetch().expect("read");
    h.session_runtime
        .agent_store
        .install_agent_history_prefix(read)
        .expect("install");
    let restore = crate::harness::restore_notice_prompt_for_elapsed(None);
    let background = "exact persisted background notice";
    for text in [&restore, background] {
        h.session_runtime
            .agent_store
            .append_agent_event(
                agent.as_str(),
                None,
                Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                    agent_id: agent.clone(),
                    text: text.to_owned(),
                    inference_activation: false,
                    message_class: tau_proto::PromptMessageClass::Internal,
                }),
            )
            .expect("persist notice");
    }
    for cold in [false, true] {
        if cold {
            assert_eq!(
                h.session_runtime
                    .persistence_owner
                    .as_ref()
                    .expect("owner")
                    .wait_for_latest_durability_for_test(Duration::from_secs(2)),
                tau_core::DurabilityBarrierOutcome::Durable
            );
            h.finish_history_preparation(&agent);
            assert!(
                h.session_runtime
                    .agent_store
                    .agent_history_is_evicted(&agent)
            );
        }
        h.prompt_coordination
            .pending_notices
            .restore_sessions
            .insert(session.clone(), None);
        h.prompt_coordination
            .pending_notices
            .restore_background_notices
            .insert(
                (session.clone(), agent.clone()),
                vec![background.to_owned()],
            );
        assert!(
            h.take_pending_restore_prompts_for_user_prompt(&cid)
                .is_empty()
        );
        assert!(
            !h.agent_internal_prompt_already_persisted(
                &agent,
                "exact persisted background notice "
            )
        );
    }
}

/// Unloading a pending cold reload releases logical protection immediately;
/// the canceled physical result cannot resurrect membership or discovery.
#[test]
fn cold_history_unload_cancels_pending_load_without_resurrection() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    h.remove_agent_expected(&cid);
    assert_eq!(
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .wait_for_latest_durability_for_test(Duration::from_secs(2)),
        tau_core::DurabilityBarrierOutcome::Durable
    );
    h.evict_eligible_history();
    let cut = ReadCut::install(&mut h);
    reload_runtime(&mut h, &agent);
    cut.wait();
    assert!(h.history_load_is_pending(&agent));
    h.remove_agent_expected(&agent);
    assert!(!h.history_load_is_pending(&agent));
    assert!(!h.session_runtime.history.has_pending());
    assert!(h.history_request_pin(&agent).is_none());
    h.publish_event(None, metadata(&agent, "ordinary append after cancellation"));
    cut.finish(&mut h);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&agent));
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .initialized_agent_context
            .contains_key(&agent)
    );
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}

/// Reader overload fails a cold initialization through ordinary teardown,
/// rather than dispatching without history or retaining immortal lifecycle
/// pins.
#[test]
fn cold_history_busy_load_releases_initialization_without_dispatch() {
    let td = TempDir::new().expect("root");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    h.remove_agent_expected(&cid);
    cold_agent(&mut h);
    h.evict_eligible_history();
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    let cut = ReadCut::install(&mut h);
    for index in 0..8 {
        let name = format!("occupied-reader-{index}");
        connect_test_client(&mut h, &name, tau_proto::ClientKind::Ui);
        h.complete_subscription(&crate::test_connection_id(&name), selectors(), selectors())
            .expect("occupy permit");
    }
    cut.wait();
    reload_runtime(&mut h, &agent);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&agent));
    assert!(!h.history_load_is_pending(&agent));
    assert!(h.history_request_pin(&agent).is_none());
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .initialized_agent_context
            .contains_key(&agent)
    );
    for index in 0..8 {
        h.cancel_connection_history(&crate::test_connection_id(&format!(
            "occupied-reader-{index}"
        )));
    }
    cut.finish(&mut h);
    assert!(
        h.session_runtime
            .agent_store
            .agent_history_is_evicted(&agent)
    );
    assert!(h.runtime_io.publication.pending_error.is_none());
}
