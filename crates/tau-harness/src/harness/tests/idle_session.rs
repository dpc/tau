//! End-to-end runtime-loop checks of the opt-in inactivity policy.

use super::lifecycle::connect_socket_ui;
use super::*;
use crate::event::{HarnessCommand, ShutdownCause};

/// Work cannot expire an idle window, and completion starts a fresh full
/// window.
#[test]
fn work_completion_starts_new_window() {
    let mut clock = IdleSession::new(Some(Duration::from_secs(10)));
    let start = clock.deadline().expect("configured deadline") - Duration::from_secs(10);
    clock.checkpoint(start + Duration::from_secs(11), true, false);
    assert_eq!(clock.deadline(), None);
    clock.checkpoint(start + Duration::from_secs(20), false, false);
    assert_eq!(clock.deadline(), Some(start + Duration::from_secs(30)));
}

/// Passive checkpoints cannot prolong the configured inactivity window.
#[test]
fn passive_poll_does_not_reset_window() {
    let mut clock = IdleSession::new(Some(Duration::from_secs(10)));
    let start = clock.deadline().expect("configured deadline") - Duration::from_secs(10);
    clock.checkpoint(start + Duration::from_secs(9), false, false);
    assert_eq!(clock.deadline(), Some(start + Duration::from_secs(10)));
    clock.checkpoint(start + Duration::from_secs(9), false, true);
    assert_eq!(clock.deadline(), Some(start + Duration::from_secs(19)));
}

/// An expired idle session exits the serve loop and uses canonical shutdown.
#[test]
fn expired_idle_session_exits_without_a_client() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    harness
        .config
        .accepted_harness_settings
        .session_idle_shutdown = Some("1s".parse().expect("positive duration"));
    harness.runtime_io.idle_session = IdleSession::new(Some(Duration::from_secs(1)));
    harness.runtime_io.idle_session.checkpoint(
        Instant::now() - Duration::from_secs(2),
        false,
        true,
    );

    harness.run_event_loop(None).expect("idle loop exits");
    harness.shutdown().expect("canonical shutdown");
    assert!(harness.session_runtime.shutdown_published);
}

/// A queued authenticated prompt at an expired cutoff is admitted before the
/// idle shutdown decision and remains durable.
#[test]
fn queued_prompt_wins_at_idle_deadline() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let (ui, _client) = connect_socket_ui(&mut harness);
    harness
        .config
        .accepted_harness_settings
        .session_idle_shutdown = Some("1s".parse().expect("positive duration"));
    harness.runtime_io.idle_session = IdleSession::new(Some(Duration::from_secs(1)));
    harness.runtime_io.idle_session.checkpoint(
        Instant::now() - Duration::from_secs(2),
        false,
        true,
    );
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::from_connection_for_test(
            ui,
            HarnessInputMessage::emit(Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                literal: true,
                session_id: harness.session_runtime.current_session_id.clone(),
                text: "queued at cutoff".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                agent_id: agent_id.clone(),
                ctx_id: Some("idle-cutoff-prompt".to_owned()),
            })),
        ))
        .expect("queue authenticated prompt");
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::Command(HarnessCommand::Shutdown(
            ShutdownCause::ExternalSignal,
        )))
        .expect("end loop after prompt");

    harness.run_event_loop(None).expect("signal exits loop");
    assert!(
        harness
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent transcript")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::AgentPromptSubmitted(prompt) if prompt.text == "queued at cutoff"
            )),
        "queued prompt must be accepted rather than lost at the cutoff"
    );
    assert_eq!(
        harness.ui_runtime.shutdown_cause,
        Some(ShutdownCause::ExternalSignal)
    );
    harness.shutdown().expect("shutdown");
}

/// An authenticated non-tool UI action renews the idle window even when it
/// does not start an in-flight agent turn.
#[test]
fn accepted_ui_action_renews_idle_window() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let (ui, _client) = connect_socket_ui(&mut harness);
    harness
        .config
        .accepted_harness_settings
        .session_idle_shutdown = Some("1s".parse().expect("positive duration"));
    harness.runtime_io.idle_session = IdleSession::new(Some(Duration::from_secs(1)));
    harness.runtime_io.idle_session.checkpoint(
        Instant::now() - Duration::from_secs(2),
        false,
        true,
    );
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::from_connection_for_test(
            ui,
            HarnessInputMessage::emit(Event::UiRoleSelect(tau_proto::UiRoleSelect {
                role: harness.config.selected_role.clone(),
            })),
        ))
        .expect("queue UI action");
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::Command(HarnessCommand::Shutdown(
            ShutdownCause::ExternalSignal,
        )))
        .expect("end loop after action");

    let before_run = Instant::now();
    harness.run_event_loop(None).expect("signal exits loop");
    assert!(
        harness
            .runtime_io
            .idle_session
            .deadline()
            .expect("idle deadline")
            >= before_run + Duration::from_secs(1),
        "accepted UI action starts a fresh idle window"
    );
    harness.shutdown().expect("shutdown");
}

/// A passive status poll may be served but cannot renew an expired idle
/// window, even when an attached UI sent the request.
#[test]
fn passive_poll_does_not_renew_runtime_idle_window() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let (ui, mut client) = connect_socket_ui(&mut harness);
    harness
        .config
        .accepted_harness_settings
        .session_idle_shutdown = Some("1s".parse().expect("positive duration"));
    harness.runtime_io.idle_session = IdleSession::new(Some(Duration::from_secs(1)));
    harness.runtime_io.idle_session.checkpoint(
        Instant::now() - Duration::from_secs(2),
        false,
        true,
    );
    let original_deadline = harness.runtime_io.idle_session.deadline();
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::from_connection_for_test(
            ui,
            HarnessInputMessage::GetCurrentSession(tau_proto::GetCurrentSession {
                request_id: "idle-poll".to_owned(),
            }),
        ))
        .expect("queue passive poll");

    harness
        .run_event_loop(None)
        .expect("idle shutdown after poll");
    let served_poll = (0..16).any(|_| {
        matches!(
            client.read_message().expect("read UI response"),
            Some(HarnessOutputMessage::CurrentSessionResult(result))
                if result.request_id == "idle-poll"
        )
    });
    assert!(
        served_poll,
        "passive poll must be handled before idle shutdown"
    );
    assert_eq!(harness.ui_runtime.shutdown_cause, None);
    assert_eq!(
        harness.runtime_io.idle_session.deadline(),
        original_deadline,
        "passive request must not prolong the window"
    );
    harness.shutdown().expect("shutdown");
}

/// Even after the wall time elapses, accepted in-flight work prevents idle
/// teardown until the tool completes or times out.
#[test]
fn accepted_agent_work_defers_idle_shutdown() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    harness
        .config
        .accepted_harness_settings
        .session_idle_shutdown = Some("1s".parse().expect("positive duration"));
    harness.runtime_io.idle_session = IdleSession::new(Some(Duration::from_secs(1)));
    harness.runtime_io.idle_session.checkpoint(
        Instant::now() - Duration::from_secs(2),
        false,
        true,
    );
    let agent = harness.create_durable_user_agent(
        harness.session_runtime.current_session_id.clone(),
        &harness.config.selected_role.clone(),
    );
    harness
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&agent)
        .expect("loaded agent")
        .execution
        .tools_in_flight = 1;
    harness
        .runtime_io
        .tx
        .send(HarnessEvent::Command(HarnessCommand::Shutdown(
            ShutdownCause::ExternalSignal,
        )))
        .expect("queue signal while agent is working");

    harness.run_event_loop(None).expect("signal exits loop");
    assert_eq!(harness.runtime_io.idle_session.deadline(), None);
    assert_eq!(
        harness.ui_runtime.shutdown_cause,
        Some(ShutdownCause::ExternalSignal)
    );
    harness.shutdown().expect("shutdown");
}
