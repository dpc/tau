use super::*;

fn boundary(session: &str, error: Option<&str>, replay: bool) -> HarnessOutputMessage {
    let mut delivery = tau_proto::EventDelivery::direct(Event::SessionReplayComplete(
        tau_proto::SessionReplayComplete {
            session_id: session.parse().expect("session"),
            error: error.map(str::to_owned),
        },
    ));
    delivery.replay = replay;
    HarnessOutputMessage::Deliver(delivery)
}

/// Only the exact, non-replayed session boundary admits work; a new attachment
/// has independent admission even when the old connection finished.
#[test]
fn exact_live_boundary_admits_only_its_attachment() {
    let mut handoff = SubscriptionHandoff::new(Some("s1".parse().expect("session")));
    assert!(handoff.submission_blocked().is_some());
    handoff.observe(&boundary("s1", None, true));
    assert!(handoff.submission_blocked().is_some());
    handoff.observe(&boundary("s1", None, false));
    assert!(handoff.submission_blocked().is_none());
    let replacement = SubscriptionHandoff::new(Some("s1".parse().expect("session")));
    assert!(replacement.submission_blocked().is_some());
}

/// A failed per-agent replay must not be masked by the following session
/// marker.
#[test]
fn agent_failure_is_terminal_before_session_boundary() {
    let mut handoff = SubscriptionHandoff::new(None);
    handoff.observe(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
        tau_proto::AgentReplayComplete {
            agent_id: "agent-1".parse().expect("agent"),
            session_id: Some("s1".parse().expect("session")),
            error: Some("history reader busy".to_owned()),
        },
    )));
    handoff.observe(&boundary("s1", None, false));
    assert_eq!(
        handoff.failure(),
        Some("Agent replay failed: history reader busy")
    );
    assert!(handoff.submission_blocked().is_some());
}

/// Session rejection, mismatched identity and disconnect are explicit failures,
/// never readiness or an implicit request to retry user work.
#[test]
fn terminal_handoff_failures_remain_closed() {
    for message in [
        boundary("other", None, false),
        boundary("s1", Some("suffix budget exhausted"), false),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("closed during replay".to_owned()),
        }),
    ] {
        let mut handoff = SubscriptionHandoff::new(Some("s1".parse().expect("session")));
        handoff.observe(&message);
        let error = handoff.failure().expect("explicit failure").to_owned();
        handoff.observe(&boundary("s1", None, false));
        assert_eq!(handoff.submission_blocked(), Some(error.as_str()));
    }
}
