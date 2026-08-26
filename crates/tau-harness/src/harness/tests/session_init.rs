//! Production-wiring tests for bounded session-context provider initialization.

use std::collections::HashSet;

use super::lifecycle::connect_handshaking_tool;
use super::*;
use crate::session_init_deadline::SessionInitDeadline;

/// Builds a validated session id for session-initialization tests.
fn session_id(value: impl Into<String>) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("test session id")
}

/// Queues one extension event through the central production event channel.
fn queue_extension_event(h: &Harness, connection_id: &tau_proto::ConnectionId, event: Event) {
    let message = HarnessInputMessage::emit(event);
    let frame_bytes = tau_proto::ProtocolMessageBytes::new(
        tau_proto::encode_message_to_vec(&message)
            .expect("encode queued session-init event")
            .len() as u64,
    )
    .expect("encoded event frame is nonempty");
    h.runtime_io
        .tx
        .send(HarnessEvent::FromConnection {
            connection_id: connection_id.clone(),
            message: Box::new(message),
            frame_bytes,
        })
        .expect("queue session-init extension event");
}

/// Ensures direct committed discovery and readiness renew only for accepted
/// current-session contributions from providers still in the wait set.
#[test]
fn direct_discovery_and_readiness_renew_only_for_outstanding_provider() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider = "session-progress-direct";
    let remaining = "session-progress-remaining";
    let _provider_sink = connect_handshaking_tool(&mut h, provider);
    let _remaining_sink = connect_handshaking_tool(&mut h, remaining);
    for connection in [provider, remaining] {
        h.handle_extension_message(
            &crate::test_connection_id(connection),
            TestMessage::Ready(tau_proto::Ready { message: None }),
        )
        .expect("ready provider");
        h.handle_extension_event(
            connection,
            TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
                tau_proto::ExtensionSessionContextProviderRegister {},
            )),
        )
        .expect("register provider");
    }
    let provider = crate::test_connection_id(provider);
    let remaining = crate::test_connection_id(remaining);
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([provider.clone(), remaining.clone()]),
    };
    let initial = h.session_runtime.session_init_progress_generation;

    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(
                "test.session_init_generic"
                    .parse()
                    .expect("custom event name"),
                Some(h.session_runtime.current_session_id.clone()),
                CborValue::Null,
            )
            .expect("custom event"),
        )),
    )
    .expect("generic event");
    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: session_id("wrong-session"),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        )),
    )
    .expect("wrong-session discovery");
    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady {
                session_id: session_id("wrong-session"),
            },
        )),
    )
    .expect("wrong-session readiness");
    assert_eq!(h.session_runtime.session_init_progress_generation, initial);

    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: h.session_runtime.current_session_id.clone(),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        )),
    )
    .expect("direct discovery");
    let after_discovery = h.session_runtime.session_init_progress_generation;
    assert_ne!(after_discovery, initial);

    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady {
                session_id: h.session_runtime.current_session_id.clone(),
            },
        )),
    )
    .expect("accepted readiness");
    let after_readiness = h.session_runtime.session_init_progress_generation;
    assert_ne!(after_readiness, after_discovery);
    assert!(matches!(
        &h.session_runtime.turn_state,
        TurnState::InitializingSession { waiting_on, .. }
            if *waiting_on == HashSet::from([remaining])
    ));

    for event in [
        Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
            session_id: h.session_runtime.current_session_id.clone(),
        }),
        Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: h.session_runtime.current_session_id.clone(),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        ),
    ] {
        h.handle_extension_event(provider.as_str(), TestProtocolItem::Event(event))
            .expect("duplicate or non-waiter event");
    }
    assert_eq!(
        h.session_runtime.session_init_progress_generation,
        after_readiness
    );

    h.session_runtime.turn_state = TurnState::Idle;
    h.shutdown().expect("shutdown");
}

/// Ensures activation applies a current staged snapshot, while a snapshot
/// captured under an old session generation cannot renew provider waiting.
#[test]
fn staged_discovery_renews_only_after_current_generation_activation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let accepted = "session-progress-staged";
    let _accepted_sink = connect_handshaking_tool(&mut h, accepted);
    h.handle_extension_event(
        accepted,
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("stage provider registration");
    h.handle_extension_event(
        accepted,
        TestProtocolItem::Event(Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: h.session_runtime.current_session_id.clone(),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        )),
    )
    .expect("stage discovery");
    let accepted = crate::test_connection_id(accepted);
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([accepted.clone()]),
    };
    let before_activation = h.session_runtime.session_init_progress_generation;
    h.handle_extension_message(
        &accepted,
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("activate staged provider");
    assert_ne!(
        h.session_runtime.session_init_progress_generation,
        before_activation
    );

    let stale = "session-progress-stale";
    let _stale_sink = connect_handshaking_tool(&mut h, stale);
    h.handle_extension_event(
        stale,
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("stage stale provider registration");
    h.handle_extension_event(
        stale,
        TestProtocolItem::Event(Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: h.session_runtime.current_session_id.clone(),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        )),
    )
    .expect("stage stale discovery");
    let stale = crate::test_connection_id(stale);
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([stale.clone()]),
    };
    let before_stale_activation = h.session_runtime.session_init_progress_generation;
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_add(1);
    h.handle_extension_message(
        &stale,
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("activate provider with stale staged admission");
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_sub(1);
    assert_eq!(
        h.session_runtime.session_init_progress_generation, before_stale_activation,
        "stale-generation staged discovery must not renew session init"
    );

    h.session_runtime.turn_state = TurnState::Idle;
    h.shutdown().expect("shutdown");
}

/// Ensures the production waiter reports both idle and absolute receive expiry
/// with the dedicated session-init error.
#[test]
fn waiter_classifies_idle_and_absolute_expiry() {
    for absolute_expires in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        while h.runtime_io.rx.try_recv().is_ok() {}
        h.session_runtime.turn_state = TurnState::InitializingSession {
            session_id: h.session_runtime.current_session_id.clone(),
            reason: tau_proto::SessionStartReason::Initial,
            waiting_on: HashSet::from([crate::test_connection_id("missing-provider")]),
        };
        let now = Instant::now();
        let (idle, absolute) = if absolute_expires {
            (now + Duration::from_secs(1), now)
        } else {
            (now, now + Duration::from_secs(1))
        };
        let deadline = SessionInitDeadline::for_test(
            idle,
            absolute,
            h.session_runtime.session_init_progress_generation,
        );

        let error = h
            .wait_for_session_init_with_deadline(deadline)
            .expect_err("expired provider wait must fail");
        assert!(matches!(error, HarnessError::SessionInitTimeout));

        h.session_runtime.turn_state = TurnState::Idle;
        h.shutdown().expect("shutdown");
    }
}

/// Ensures queued final readiness wins at the exact deadline and synchronous
/// production finalization cannot retroactively become a provider timeout.
#[test]
fn waiter_prioritizes_final_readiness_and_runs_finalization() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider = "session-final-ready";
    let _sink = connect_handshaking_tool(&mut h, provider);
    h.handle_extension_message(
        &crate::test_connection_id(provider),
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("ready provider");
    h.handle_extension_event(
        provider,
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("register provider");
    while h.runtime_io.rx.try_recv().is_ok() {}
    let provider = crate::test_connection_id(provider);
    h.prompt_coordination
        .context_discovery
        .initialized_sessions
        .remove(&h.session_runtime.current_session_id);
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([provider.clone()]),
    };
    queue_extension_event(
        &h,
        &provider,
        Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
            session_id: h.session_runtime.current_session_id.clone(),
        }),
    );
    let now = Instant::now();
    let deadline =
        SessionInitDeadline::for_test(now, now, h.session_runtime.session_init_progress_generation);

    h.wait_for_session_init_with_deadline(deadline)
        .expect("final readiness and finalization must win");
    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));
    assert!(
        h.prompt_coordination
            .context_discovery
            .initialized_sessions
            .contains(&h.session_runtime.current_session_id),
        "complete_session_init must record the finalized session"
    );

    h.shutdown().expect("shutdown");
}
