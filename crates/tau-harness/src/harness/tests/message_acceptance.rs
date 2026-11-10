//! Directed acceptance, concurrency, replay and asynchronous persistence
//! oracles.

use tau_proto::{MessageAcceptanceOutcome as Outcome, MessageAcceptanceRequest};

use super::*;

/// Connect one authenticated publisher and retain its directed reply sink.
fn bridge(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let frames =
        connect_ready_configured_extension(h, "accept-bridge", "gccp", tau_proto::ClientKind::Tool);
    h.extensions
        .entries
        .get_mut("accept-bridge")
        .expect("bridge")
        .peer_capabilities = [tau_proto::PeerCapability::MessageBridge]
        .into_iter()
        .collect();
    frames
}

/// Build a stable semantic message; only the attempt correlation changes.
fn request(h: &Harness, target: &AgentId, attempt: &str) -> MessageAcceptanceRequest {
    MessageAcceptanceRequest {
        request_id: attempt.to_owned(),
        session_id: h.session_runtime.current_session_id.clone(),
        report: tau_proto::MessageDelivered::new(
            tau_proto::RawMessagePublisherId::new("untrusted-claim"),
            tau_proto::MessageAgentTarget::new(target.as_str()),
            tau_proto::MessageFactId::new("stable-message"),
            tau_proto::MessageParty {
                stable_id: "peer-address".to_owned(),
                display_name: None,
                sender_auth: None,
                sender_trust: Some(tau_proto::MessageSenderTrust::Untrusted),
            },
            None,
            "immutable incoming body",
        ),
    }
}

/// Exercise the configured-extension protocol ingress, not the private handler.
fn submit(h: &mut Harness, request: MessageAcceptanceRequest) {
    h.handle_extension_message(
        &crate::test_connection_id("accept-bridge"),
        HarnessInputMessage::MessageAcceptanceRequest(Box::new(request)),
    )
    .expect("directed acceptance ingress");
}

/// Extract one exact private attempt outcome without treating events as ACKs.
fn outcome(frames: &Arc<Mutex<Vec<RoutedFrame>>>, attempt: &str) -> Option<Outcome> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match &frame.frame {
            HarnessOutputMessage::MessageAcceptanceResult(result)
                if result.request_id == attempt =>
            {
                Some(result.outcome.clone())
            }
            _ => None,
        })
}

/// Count only canonical inputs, independently of wake dispatch timing.
fn canonical_count(h: &Harness) -> usize {
    event_log_events(h)
        .iter()
        .filter(|event| matches!(event, Event::MessageDelivered(_)))
        .count()
}

/// Lost results, cache eviction and changed content must never produce another
/// canonical input or activation; ordinary emits retain their old semantics.
#[test]
fn message_acceptance_deduplicates_retained_input_and_preserves_ordinary_reports() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    let first = request(&h, &target, "first");
    submit(&mut h, first.clone());
    assert_eq!(
        outcome(&frames, "first"),
        Some(Outcome::Accepted { duplicate: false })
    );
    let wakes = h.agent_runtime.agent_registry.agents[&target]
        .dispatch
        .pending_message_wakes
        .len();
    assert_eq!(canonical_count(&h), 1);
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::MessageDeliveredReported(_)))
    );
    frames.lock().expect("lost response").clear();
    h.session_runtime
        .agent_store
        .evict_agent_history(&target, None)
        .expect("evict history cache");
    let mut retry = first.clone();
    retry.request_id = "retry".to_owned();
    submit(&mut h, retry.clone());
    assert_eq!(
        outcome(&frames, "retry"),
        Some(Outcome::Accepted { duplicate: true })
    );
    assert_eq!(canonical_count(&h), 1);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&target]
            .dispatch
            .pending_message_wakes
            .len(),
        wakes
    );
    retry.request_id = "conflict".to_owned();
    retry.report.text.push_str(" changed");
    submit(&mut h, retry);
    assert!(matches!(
        outcome(&frames, "conflict"),
        Some(Outcome::Rejected { .. })
    ));
    assert_eq!(canonical_count(&h), 1);

    // A forged marker on ordinary reports cannot opt into dedup.
    let mut report = first.report;
    report.publisher_extension_id = tau_proto::RawMessagePublisherId::new("gccp");
    report.idempotent_acceptance = true;
    for _ in 0..2 {
        h.handle_extension_event_inner(
            &crate::test_connection_id("accept-bridge"),
            Event::MessageDeliveredReported(report.clone()),
        )
        .expect("ordinary report");
    }
    assert_eq!(canonical_count(&h), 3);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(target.as_str())
        .expect("history");
    let marked: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::MessageDelivered(message) if message.idempotent_acceptance => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].publisher_extension_id.as_str(), "gccp");
}

/// A queued duplicate rechecks at commit, not before interception; immutable
/// canonical facts also retain their existing must-pass/drop protection.
#[test]
fn message_acceptance_serializes_overlapping_intercepted_attempts() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    connect_test_tool(&mut h, "accept-interceptor");
    h.handle_extension_event(
        "accept-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let first = request(&h, &target, "first");
    let mut second = first.clone();
    second.request_id = "second".to_owned();
    submit(&mut h, first);
    submit(&mut h, second);
    assert_eq!(outcome(&frames, "first"), None);
    assert_eq!(canonical_count(&h), 0);
    for _ in 0..2 {
        h.handle_extension_event(
            "accept-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("must-pass canonical fact");
    }
    assert_eq!(
        outcome(&frames, "first"),
        Some(Outcome::Accepted { duplicate: false })
    );
    assert_eq!(
        outcome(&frames, "second"),
        Some(Outcome::Accepted { duplicate: true })
    );
    assert_eq!(canonical_count(&h), 1);
}

/// Queue rejection is a definite refusal; paused filesystem writes never delay
/// a later successful ordinary acceptance or its duplicate response.
#[test]
fn message_acceptance_respects_nonblocking_persistence_admission() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    let mut input = request(&h, &target, "refused");
    reject_next_semantic_admission(&h);
    submit(&mut h, input.clone());
    assert!(matches!(
        outcome(&frames, "refused"),
        Some(Outcome::Rejected { .. })
    ));
    assert_eq!(canonical_count(&h), 0);
    let owner = h
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("owner")
        .clone();
    owner.pause_frame_writes_for_test(true);
    input.request_id = "paused".to_owned();
    submit(&mut h, input.clone());
    let accepted = outcome(&frames, "paused");
    input.request_id = "paused-retry".to_owned();
    submit(&mut h, input);
    let duplicate = outcome(&frames, "paused-retry");
    owner.pause_frame_writes_for_test(false);
    assert_eq!(accepted, Some(Outcome::Accepted { duplicate: false }));
    assert_eq!(duplicate, Some(Outcome::Accepted { duplicate: true }));
    assert_eq!(canonical_count(&h), 1);
}

/// Invalid input, unavailable targets and missing bridge authority cannot
/// become fallback journal facts or create recipient agents.
#[test]
fn message_acceptance_rejects_unprojectable_unloaded_and_unauthorized_input() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    let mut input = request(&h, &target, "empty");
    input.report.text.clear();
    submit(&mut h, input);
    assert!(matches!(
        outcome(&frames, "empty"),
        Some(Outcome::Rejected { .. })
    ));
    let missing: AgentId = "missing".parse().expect("agent");
    let input = request(&h, &missing, "missing");
    submit(&mut h, input);
    assert!(matches!(
        outcome(&frames, "missing"),
        Some(Outcome::Rejected { .. })
    ));
    let input = request(&h, &target, "unauthorized");
    h.extensions
        .entries
        .get_mut("accept-bridge")
        .expect("bridge")
        .peer_capabilities
        .clear();
    submit(&mut h, input);
    assert!(matches!(
        outcome(&frames, "unauthorized"),
        Some(Outcome::Rejected { .. })
    ));
    assert_eq!(canonical_count(&h), 0);
    assert!(!h.agent_runtime.agent_registry.agents.contains_key(&missing));
}

/// Interception cannot preserve authority across a session generation change.
#[test]
fn message_acceptance_rechecks_session_generation_after_interception() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    connect_test_tool(&mut h, "accept-interceptor");
    h.handle_extension_event(
        "accept-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let input = request(&h, &target, "stale");
    submit(&mut h, input);
    h.session_runtime.current_session_generation = h
        .session_runtime
        .current_session_generation
        .saturating_next();
    h.handle_extension_event(
        "accept-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release");
    assert!(matches!(
        outcome(&frames, "stale"),
        Some(Outcome::Rejected { .. })
    ));
    assert_eq!(canonical_count(&h), 0);
}

/// A reopened journal answers a same-ID retry without republishing input; an
/// ephemeral recipient instead keeps the same contract for its live lifetime.
#[test]
fn message_acceptance_restart_rebuilds_dedup_and_ephemeral_targets_are_supported() {
    let tmp = TempDir::new().expect("tempdir");
    let saved;
    {
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        let target = ensure_test_user_agent(&mut h);
        let frames = bridge(&mut h);
        saved = request(&h, &target, "saved");
        submit(&mut h, saved.clone());
        assert_eq!(
            outcome(&frames, "saved"),
            Some(Outcome::Accepted { duplicate: false })
        );
        h.shutdown().expect("flush normal shutdown");
    }
    {
        let mut h = quiet_provider_harness_with_start_reason(
            tmp.path(),
            tau_proto::SessionStartReason::Resume,
        )
        .expect("reopen");
        let frames = bridge(&mut h);
        let mut retry = saved;
        retry.request_id = "restored".to_owned();
        submit(&mut h, retry);
        assert_eq!(
            outcome(&frames, "restored"),
            Some(Outcome::Accepted { duplicate: true })
        );
        assert_eq!(canonical_count(&h), 0);
    }
    let ephemeral_dir = TempDir::new().expect("ephemeral");
    let mut h = quiet_provider_harness_ephemeral(ephemeral_dir.path()).expect("ephemeral harness");
    let target = ensure_test_user_agent(&mut h);
    let frames = bridge(&mut h);
    let input = request(&h, &target, "ephemeral");
    submit(&mut h, input.clone());
    assert_eq!(
        outcome(&frames, "ephemeral"),
        Some(Outcome::Accepted { duplicate: false })
    );
    let mut retry = input;
    retry.request_id = "ephemeral-retry".to_owned();
    submit(&mut h, retry);
    assert_eq!(
        outcome(&frames, "ephemeral-retry"),
        Some(Outcome::Accepted { duplicate: true })
    );
}
