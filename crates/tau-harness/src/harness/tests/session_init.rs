//! Production-wiring tests for bounded session-context provider initialization.

use std::collections::HashSet;

use super::lifecycle::connect_handshaking_tool;
use super::*;
use crate::session_init_deadline::SessionInitDeadline;

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
            decoded_at: Instant::now(),
        })
        .expect("queue session-init extension event");
}

/// Ensures the production waiter reports absolute expiry with the dedicated
/// session-init error.
#[test]
fn waiter_classifies_absolute_expiry() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    while h.runtime_io.rx.try_recv().is_ok() {}
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([crate::test_connection_id("missing-provider")]),
    };
    let deadline = SessionInitDeadline::for_test(Instant::now());

    let error = h
        .wait_for_session_init_with_deadline(deadline)
        .expect_err("expired provider wait must fail");
    assert!(matches!(error, HarnessError::SessionInitTimeout));

    h.session_runtime.turn_state = TurnState::Idle;
    h.shutdown().expect("shutdown");
}

/// Ensures unrelated queued traffic cannot make a live provider fail merely
/// because its exact readiness arrives after the removed two-second idle cut.
#[test]
fn waiter_keeps_live_provider_owned_until_readiness_after_old_idle_cut() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let provider = "session-ready-after-contention";
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
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: h.session_runtime.current_session_id.clone(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: HashSet::from([provider.clone()]),
    };
    queue_extension_event(
        &h,
        &provider,
        Event::ExtensionEvent(
            tau_proto::CustomEvent::try_new(
                "test.session_init_contention"
                    .parse()
                    .expect("custom event name"),
                Some(h.session_runtime.current_session_id.clone()),
                CborValue::Null,
            )
            .expect("custom event"),
        ),
    );
    queue_extension_event(
        &h,
        &provider,
        Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
            session_id: h.session_runtime.current_session_id.clone(),
        }),
    );
    let wait_started_before_old_idle_cut = Instant::now() - Duration::from_secs(3);

    h.wait_for_session_init_with_deadline(SessionInitDeadline::new(
        wait_started_before_old_idle_cut,
    ))
    .expect("exact provider readiness must remain authoritative");
    assert!(matches!(h.session_runtime.turn_state, TurnState::Idle));

    h.shutdown().expect("shutdown");
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
    let deadline = SessionInitDeadline::for_test(Instant::now());

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
