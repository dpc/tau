use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Construct one progress payload with easily asserted content.
fn progress_payload(call_id: &str, message: &str) -> tau_proto::ToolProgress {
    tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("owned_tool"),
        message: Some(message.to_owned()),
        progress: None,
        display: None,
    }
}

/// Wrap one progress payload as a peer-authored report.
fn progress_report(call_id: &str, message: &str) -> Event {
    Event::ToolProgressReported(progress_payload(call_id, message))
}

/// Seed the minimum routed-call ownership needed by progress validation.
fn seed_routed_call(harness: &mut Harness, call_id: &str, source: &str) {
    harness.tool_routing.tool_runtime.tool_agents.insert(
        call_id.into(),
        tau_proto::AgentId::parse("agent").expect("valid agent id"),
    );
    harness
        .tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id.into(), crate::test_connection_id(source));
}

/// Collect committed progress reports and facts for one call in sequence order.
fn committed_progress(
    harness: &Harness,
    call_id: &str,
) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let matches_call = match &entry.event {
            Event::ToolProgressReported(progress) | Event::ToolProgress(progress) => {
                progress.call_id.as_str() == call_id
            }
            _ => false,
        };
        if matches_call {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// Register an exact interceptor for both the mutable report and protected
/// fact.
fn intercept_progress_family(harness: &mut Harness) {
    connect_test_tool(harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![
                    EventSelector::Exact(tau_proto::EventName::TOOL_PROGRESS_REPORTED),
                    EventSelector::Exact(tau_proto::EventName::TOOL_PROGRESS),
                ],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register progress interceptor");
}

/// A same-name interception replacement must commit as the peer observation
/// before downstream validation publishes the harness-sourced canonical fact.
#[test]
fn replaced_progress_report_is_validated_only_after_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    seed_routed_call(&mut harness, "call-replaced", "tool-owner");
    intercept_progress_family(&mut harness);

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-replaced", "original")),
        )
        .expect("park report");
    assert!(committed_progress(&harness, "call-replaced").is_empty());

    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(progress_report(
                    "call-replaced",
                    "replacement",
                )))),
            })),
        )
        .expect("commit replacement report");
    assert!(matches!(
        harness
            .runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ToolProgress(_))
    ));
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit canonical fact");

    assert!(matches!(
        committed_progress(&harness, "call-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolProgressReported(report)),
            (Some(canonical_source), Event::ToolProgress(canonical)),
        ] if report_source == "tool-owner"
            && canonical_source == HARNESS_CONNECTION_ID
            && report.message.as_deref() == Some("replacement")
            && canonical == report
    ));
}

/// Dropping the peer report must leave no committed observation or canonical
/// fact and must not alter the in-flight route.
#[test]
fn dropped_progress_report_has_no_downstream_effect() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Core,
    );
    seed_routed_call(&mut harness, "call-dropped", "tool-owner");
    intercept_progress_family(&mut harness);

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-dropped", "drop me")),
        )
        .expect("park report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop report");

    assert!(committed_progress(&harness, "call-dropped").is_empty());
    assert_eq!(
        harness
            .tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("call-dropped")
            .map(tau_proto::ConnectionId::as_str),
        Some("tool-owner")
    );
}

/// A known call without an exact active provider route must not rely on the
/// legacy permissive source fallback to derive canonical progress.
#[test]
fn progress_report_without_exact_route_commits_without_canonical_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    harness.tool_routing.tool_runtime.tool_agents.insert(
        "call-unrouted".into(),
        tau_proto::AgentId::parse("agent").expect("valid agent id"),
    );

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-unrouted", "unrouted")),
        )
        .expect("commit unrouted report");

    assert!(matches!(
        committed_progress(&harness, "call-unrouted").as_slice(),
        [(Some(source), Event::ToolProgressReported(_))] if source == "tool-owner"
    ));
}

/// A pre-Ready progress submission remains ordinary retained operational
/// traffic: its exact `persist=false` envelope is charged, deferred, and
/// processed only after activation.
#[test]
fn pre_ready_progress_report_preserves_retained_byte_accounting() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    harness
        .extensions
        .entries
        .get_mut("tool-owner")
        .expect("tool owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    seed_routed_call(&mut harness, "call-retained", "tool-owner");
    let report = progress_report("call-retained", "retained");
    let expected_bytes = Harness::encoded_emit_size(&report, false);

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(report),
                persist: false,
            })),
        )
        .expect("defer report before Ready");
    let stage = &harness.extensions.activation_staging["tool-owner"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(committed_progress(&harness, "call-retained").is_empty());

    harness
        .handle_extension_message(
            &crate::test_connection_id("tool-owner"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate and drain report");
    assert_eq!(
        harness.extensions.entries["tool-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(matches!(
        committed_progress(&harness, "call-retained").as_slice(),
        [
            (_, Event::ToolProgressReported(_)),
            (Some(source), Event::ToolProgress(_)),
        ] if source == HARNESS_CONNECTION_ID
    ));
}

/// A stale parked generation may commit its observation, but changing the live
/// logical configured-instance identity must prevent canonical publication.
#[test]
fn parked_stale_generation_cannot_publish_canonical_progress() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    seed_routed_call(&mut harness, "call-stale", "tool-owner");
    intercept_progress_family(&mut harness);

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-stale", "stale")),
        )
        .expect("park report");
    harness
        .extensions
        .entries
        .get_mut("tool-owner")
        .expect("live replacement")
        .instance_id = tau_proto::ExtensionInstanceId::new(43);
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit stale observation");

    assert!(matches!(
        committed_progress(&harness, "call-stale").as_slice(),
        [(Some(source), Event::ToolProgressReported(_))] if source == "tool-owner"
    ));
    assert_eq!(
        harness
            .tool_routing
            .tool_runtime
            .pending_tool_providers
            .get("call-stale")
            .map(tau_proto::ConnectionId::as_str),
        Some("tool-owner")
    );
}

/// Reports from a configured non-Tool/Core peer and peer-authored canonical
/// facts must fail authority admission before generic publication.
#[test]
fn progress_authority_rejects_wrong_kind_and_peer_canonical_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    seed_routed_call(&mut harness, "call-authority", "tool-owner");

    harness
        .handle_extension_event(
            "provider",
            TestProtocolItem::Event(progress_report("call-authority", "wrong kind")),
        )
        .expect("reject wrong kind");
    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(Event::ToolProgress(progress_payload(
                "call-authority",
                "forged canonical",
            ))),
        )
        .expect("reject peer canonical fact");

    assert!(committed_progress(&harness, "call-authority").is_empty());
}

/// Interceptors may not rewrite or drop a validated canonical progress fact,
/// even though the triggering report remains mutable.
#[test]
fn canonical_progress_is_immutable_and_must_pass() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    seed_routed_call(&mut harness, "call-protected", "tool-owner");
    intercept_progress_family(&mut harness);

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-protected", "original")),
        )
        .expect("park report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(Event::ToolProgress(
                    progress_payload("call-protected", "forged rewrite"),
                )))),
            })),
        )
        .expect("reject canonical rewrite");
    assert!(matches!(
        committed_progress(&harness, "call-protected").as_slice(),
        [
            (_, Event::ToolProgressReported(_)),
            (Some(source), Event::ToolProgress(progress)),
        ] if source == HARNESS_CONNECTION_ID
            && progress.message.as_deref() == Some("original")
    ));

    harness
        .handle_extension_event(
            "tool-owner",
            TestProtocolItem::Event(progress_report("call-protected", "second")),
        )
        .expect("park second report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit second report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("override canonical drop");
    assert!(matches!(
        committed_progress(&harness, "call-protected").as_slice(),
        [
            (_, Event::ToolProgressReported(_)),
            (_, Event::ToolProgress(_)),
            (_, Event::ToolProgressReported(_)),
            (_, Event::ToolProgress(progress)),
        ] if progress.message.as_deref() == Some("second")
    ));
}
