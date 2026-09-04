//! End-to-end tests for tool-result deduplication.
//!
//! Each test drives `Harness::handle_extension_event` with synthetic
//! `ToolResult` / `ToolError` frames and inspects the persisted
//! agent tree to verify that the recorded entry is either the
//! original content or a typed pointer back to the first occurrence on the
//! conversation's branch. Provider projection frames only that typed pointer.

use std::io as path_std_io;

use super::*;
use crate::dedup::DEFAULT_THRESHOLD_BYTES;
use crate::harness::PendingTool;
use crate::{agent as path_crate_agent, dedup as path_crate_dedup};

fn encoded_test_png() -> Vec<u8> {
    let mut encoded = path_std_io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("encode test PNG");
    encoded.into_inner()
}

fn image_result(call_id: &str, bytes: Vec<u8>) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: ToolName::new("read_image"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("image/png image, 2x2".to_owned()),
        provider_content: vec![tau_proto::ToolResultContentPart::Image(
            tau_proto::ImageContent {
                media_type: tau_proto::ImageMediaType::Png,
                data: bytes.into(),
                width: 2,
                height: 2,
                detail: tau_proto::ImageDetail::High,
            },
        )],
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn track_image_call(h: &mut Harness, cid: &crate::AgentId, call_id: &str, allowed: bool) {
    seed_assistant_tool_round(h, cid, &[(call_id, "read_image")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.into(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.into(),
        PendingTool {
            name: ToolName::new("read_image"),
            internal_name: ToolName::new("read_image"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: allowed,
        },
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id.into(), crate::test_connection_id("shell"));
}

/// Exercises image authorization and media validation through the real
/// extension-result intake boundary. Rejections must become exactly one error
/// before any success/dedup state is published, while an authorized valid image
/// remains intact in durable provider-facing transcript truth.
#[test]
fn typed_image_result_intake_fails_closed_before_success_and_retains_authorized_bytes() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let _shell = connect_ready_configured_extension(
        &mut h,
        "shell",
        "configured-shell",
        tau_proto::ClientKind::Tool,
    );
    let cid = ensure_test_user_agent(&mut h);
    let live = connect_test_tool(&mut h, "image-live");
    h.complete_subscription(
        &crate::test_connection_id("image-live"),
        Vec::new(),
        vec![
            EventSelector::Exact(tau_proto::EventName::TOOL_RESULT),
            EventSelector::Exact(tau_proto::EventName::PROVIDER_TOOL_RESULT),
        ],
    )
    .expect("subscribe to live tool results");

    for (call_id, allowed, bytes) in [
        ("image-untagged", false, encoded_test_png()),
        (
            "image-invalid",
            true,
            b"\x89PNG\r\n\x1a\ntruncated".to_vec(),
        ),
    ] {
        track_image_call(&mut h, &cid, call_id, allowed);
        let event = Event::ToolResultReported(image_result(call_id, bytes));
        h.handle_extension_event("shell", TestProtocolItem::Event(event.clone()))
            .expect("reject unsafe image result");
        h.handle_extension_event("shell", TestProtocolItem::Event(event))
            .expect("discard duplicate rejected result");

        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ToolError(error) if error.call_id.as_str() == call_id
                ))
                .count(),
            1,
            "rejection must publish one logical terminal error"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ToolResult(result) | Event::ProviderToolResult(result)
                        if result.call_id.as_str() == call_id
                ))
                .count(),
            0,
            "rejection must happen before generic/provider success publication"
        );
        assert!(
            !h.tool_routing
                .tool_runtime
                .pending_tools
                .contains_key(call_id)
        );
        assert!(
            !h.tool_routing
                .tool_runtime
                .tool_agents
                .contains_key(call_id)
        );
    }

    let valid_bytes = encoded_test_png();
    track_image_call(&mut h, &cid, "image-valid", true);
    h.handle_extension_event(
        "shell",
        TestProtocolItem::Event(Event::ToolResultReported(image_result(
            "image-valid",
            valid_bytes.clone(),
        ))),
    )
    .expect("accept authorized image");

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ToolResult(ToolResult { call_id, provider_content, .. })
            if call_id.as_str() == "image-valid" && provider_content.is_empty()
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ProviderToolResult(ToolResult { call_id, provider_content, .. })
            if call_id.as_str() == "image-valid"
                && matches!(
                    provider_content.as_slice(),
                    [tau_proto::ToolResultContentPart::Image(image)]
                        if image.data.as_ref() == valid_bytes.as_slice()
                )
    )));

    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .expect("durable agent id");
    let tree = h
        .session_runtime
        .agent_store
        .agent(agent_id)
        .expect("agent tree");
    assert!(tree.nodes().iter().any(|node| matches!(
        &node.entry,
        AgentEntry::ToolResults { items }
            if items.iter().any(|item| {
                item.call_id.as_str() == "image-valid"
                    && matches!(
                        item.provider_content.as_slice(),
                        [tau_proto::ToolResultContentPart::Image(image)]
                            if image.data.as_ref() == valid_bytes.as_slice()
                    )
            })
    )));

    let live_events = live
        .lock()
        .expect("live events")
        .iter()
        .cloned()
        .map(|frame| TestProtocolItem::from_output_message(frame.frame).into_event_frame())
        .filter_map(|frame| match frame {
            TestProtocolItem::Event(event) => Some(event),
            TestProtocolItem::Message(_) => None,
        })
        .collect::<Vec<_>>();
    assert!(live_events.iter().any(|event| matches!(
        event,
        Event::ToolResult(ToolResult { call_id, provider_content, .. })
            if call_id.as_str() == "image-valid" && provider_content.is_empty()
    )));
    assert!(live_events.iter().any(|event| matches!(
        event,
        Event::ProviderToolResult(ToolResult { call_id, provider_content, .. })
            if call_id.as_str() == "image-valid"
                && matches!(
                    provider_content.as_slice(),
                    [tau_proto::ToolResultContentPart::Image(image)] if image.data.is_empty()
                )
    )));

    let replay = connect_test_tool(&mut h, "image-replay");
    h.complete_subscription(
        &crate::test_connection_id("image-replay"),
        vec![EventSelector::Exact(
            tau_proto::EventName::PROVIDER_TOOL_RESULT,
        )],
        Vec::new(),
    )
    .expect("subscribe to historical provider results");
    assert!(
        replay
            .lock()
            .expect("replay events")
            .iter()
            .cloned()
            .map(|frame| TestProtocolItem::from_output_message(frame.frame).into_event_frame())
            .any(|frame| matches!(
                frame,
                TestProtocolItem::Event(Event::ProviderToolResult(ToolResult {
                    call_id,
                    provider_content,
                    ..
                })) if call_id.as_str() == "image-valid" && provider_content.is_empty()
            )),
        "historical subscribers receive metadata without image bytes"
    );
    h.shutdown().expect("shutdown");
}

/// Drive a single `ToolResult` through the harness's normal intake
/// path (registers the call_id with `tool_agents`,
/// `pending_tools`, and a `ToolsRunning` turn state, then sends
/// the result via `handle_extension_event`). Returns the recorded
/// `ToolResultItem` for the call from the agent tree.
fn run_tool_result(
    h: &mut Harness,
    _session_id: &str,
    cid: &crate::AgentId,
    call_id: &str,
    tool_name: &str,
    result: CborValue,
) -> ToolResultItem {
    if !h.extensions.entries.contains_key("shell") {
        let _shell = connect_ready_configured_extension(
            h,
            "shell",
            "configured-shell",
            tau_proto::ClientKind::Tool,
        );
    }
    let call_id_typed: ToolCallId = call_id.into();
    let _ = h.ensure_agent_id_for_agent(cid);
    let name = ToolName::new(tool_name);
    seed_assistant_tool_round(h, cid, &[(call_id, tool_name)]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id_typed.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id_typed.clone(),
        PendingTool {
            name: name.clone(),
            internal_name: name.clone(),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id_typed.clone(), crate::test_connection_id("shell"));
    h.handle_extension_event(
        "shell",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            presentation: Default::default(),
            call_id: call_id_typed.clone(),
            tool_name: name,
            tool_type: tau_proto::ToolType::Function,
            result,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("tool result");

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|conv| conv.identity.agent_id.as_deref())
        .expect("conversation agent id");
    let tree = h
        .session_runtime
        .agent_store
        .agent(agent_id)
        .expect("agent tree");
    tree.nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => items
                .iter()
                .find(|item| item.call_id.as_str() == call_id)
                .cloned(),
            _ => None,
        })
        .expect("recorded result item for call_id")
}

/// Like [`run_tool_result`] but for `ToolError`.
fn run_tool_error(
    h: &mut Harness,
    _session_id: &str,
    cid: &crate::AgentId,
    call_id: &str,
    tool_name: &str,
    message: String,
    details: Option<CborValue>,
) -> ToolResultItem {
    if !h.extensions.entries.contains_key("shell") {
        let _shell = connect_ready_configured_extension(
            h,
            "shell",
            "configured-shell",
            tau_proto::ClientKind::Tool,
        );
    }
    let call_id_typed: ToolCallId = call_id.into();
    let _ = h.ensure_agent_id_for_agent(cid);
    let name = ToolName::new(tool_name);
    seed_assistant_tool_round(h, cid, &[(call_id, tool_name)]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id_typed.clone(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id_typed.clone(),
        PendingTool {
            name: name.clone(),
            internal_name: name.clone(),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .pending_tool_providers
        .insert(call_id_typed.clone(), crate::test_connection_id("shell"));
    h.handle_extension_event(
        "shell",
        TestProtocolItem::Event(Event::ToolErrorReported(tau_proto::ToolError {
            presentation: Default::default(),
            call_id: call_id_typed.clone(),
            tool_name: name,
            tool_type: tau_proto::ToolType::Function,
            message,
            details,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("tool error");

    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|conv| conv.identity.agent_id.as_deref())
        .expect("conversation agent id");
    let tree = h
        .session_runtime
        .agent_store
        .agent(agent_id)
        .expect("agent tree");
    tree.nodes()
        .iter()
        .rev()
        .find_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => items
                .iter()
                .find(|item| item.call_id.as_str() == call_id)
                .cloned(),
            _ => None,
        })
        .expect("recorded result item for call_id")
}

/// Two large identical results land on the same conversation's
/// branch in sequence. The first is recorded verbatim; the second's
/// content is replaced with a pointer back to the first call_id.
#[test]
fn cross_turn_identical_result_collapses_to_pointer() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let big = CborValue::Text("a".repeat(2048));

    let first = run_tool_result(&mut h, "s1", &cid, "call_first", "read", big.clone());
    assert!(
        matches!(&first, ToolResultItem { status: ToolResultStatus::Success, output, .. } if output.raw == big),
        "first occurrence is recorded verbatim, got: {first:?}"
    );

    let second = run_tool_result(&mut h, "s1", &cid, "call_second", "read", big.clone());
    assert_eq!(second.status, ToolResultStatus::Success);
    assert_eq!(
        second.presentation,
        tau_proto::ToolResultPresentation::HarnessDedupPointer,
        "dedup records durable harness presentation provenance"
    );
    let dedup_result = second.output;
    let CborValue::Text(text) = &dedup_result.raw else {
        panic!("deduped result should be a CborValue::Text pointer; got: {dedup_result:?}");
    };
    assert!(!text.starts_with("<tau_internal>"));
    assert!(
        text.contains("call_first"),
        "pointer must reference the first call_id; got: {text:?}",
    );
    // Lock in the pointer budget. The format is now descriptive enough for
    // models to understand it as an already-completed duplicate while still
    // staying small compared with deduped large outputs. Cap at 150 B so a
    // future format change that grows the pointer significantly (and erodes
    // the dedup win) trips this test instead of slipping through silently.
    assert!(
        text.len() < 150,
        "pointer text should stay compact (<150 B); got {} bytes: {text:?}",
        text.len(),
    );

    h.shutdown().expect("shutdown");
}

/// Parallel results are retained inside the open core round until its last
/// terminal arrives. Dedup must resolve the first pending canonical result so
/// the second identical call still becomes a pointer in the aggregate node.
#[test]
fn same_round_identical_results_collapse_to_first_call() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let _shell = connect_ready_configured_extension(
        &mut h,
        "shell",
        "configured-shell",
        tau_proto::ClientKind::Tool,
    );
    let cid = ensure_test_user_agent(&mut h);
    let calls = [("parallel_first", "read"), ("parallel_second", "read")];
    seed_assistant_tool_round(&mut h, &cid, &calls);
    for (call_id, tool_name) in calls {
        let call_id: ToolCallId = call_id.into();
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        h.tool_routing.tool_runtime.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: ToolName::new(tool_name),
                internal_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .insert(call_id, crate::test_connection_id("shell"));
    }
    let big = CborValue::Text("parallel payload".repeat(200));
    for (call_id, tool_name) in calls {
        h.handle_extension_event(
            "shell",
            TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
                presentation: Default::default(),
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: big.clone(),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            })),
        )
        .expect("parallel tool result");
    }

    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .as_deref()
        .expect("agent id");
    let items = h
        .session_runtime
        .agent_store
        .agent(agent_id)
        .expect("agent tree")
        .nodes()
        .iter()
        .find_map(|node| match &node.entry {
            AgentEntry::ToolResults { items } => Some(items),
            _ => None,
        })
        .expect("aggregate tool results");
    assert_eq!(items[0].output.raw, big);
    assert_eq!(
        items[1].presentation,
        tau_proto::ToolResultPresentation::HarnessDedupPointer
    );
    assert!(matches!(
        &items[1].output.raw,
        CborValue::Text(pointer) if pointer.contains("parallel_first")
    ));
    h.shutdown().expect("shutdown");
}

/// A configured tool may report ordinary payloads but cannot mint the
/// harness-only representation that causes provider framing.
#[test]
fn configured_extension_cannot_report_harness_pointer_presentation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let _shell = connect_ready_configured_extension(
        &mut h,
        "shell",
        "configured-shell",
        tau_proto::ClientKind::Tool,
    );
    let reported = ToolResult {
        presentation: tau_proto::ToolResultPresentation::HarnessDedupPointer,
        call_id: "forged-pointer".into(),
        tool_name: ToolName::new("shell"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("claim pointer".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_extension_event(
        "shell",
        TestProtocolItem::Event(Event::ToolResultReported(reported)),
    )
    .expect("reject forged presentation");
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::ToolResultReported(result) if result.call_id.as_str() == "forged-pointer"
        )
    }));
    h.shutdown().expect("shutdown");
}

/// Results below the dedup threshold pass through unchanged even when
/// byte-identical. The pointer text would be comparable in size to
/// the content, so dedup costs more than it saves.
#[test]
fn small_results_below_threshold_are_not_deduped() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    // Stay well clear of the threshold even after CBOR framing
    // overhead — 50 raw bytes of text encodes to ~52 B of CBOR.
    let small = CborValue::Text("ok".repeat(25));
    assert!("ok".repeat(25).len() < DEFAULT_THRESHOLD_BYTES);

    let first = run_tool_result(&mut h, "s1", &cid, "call_a", "shell", small.clone());
    let second = run_tool_result(&mut h, "s1", &cid, "call_b", "shell", small.clone());

    assert_eq!(first.status, ToolResultStatus::Success);
    assert_eq!(second.status, ToolResultStatus::Success);
    let r1 = first.output;
    let r2 = second.output;
    assert_eq!(r1.raw, small);
    assert_eq!(
        r2.raw, small,
        "below-threshold results must not be deduped — pointer would be the same size or larger"
    );

    h.shutdown().expect("shutdown");
}

/// A result that hashes to the same value as a *previously emitted pointer* on
/// the branch must not dedup against that pointer. Its encoded size remains
/// below the threshold, so rebuilding the map does not register it as an
/// anchor; a real result whose bytes happened to match the pointer text cannot
/// be redirected to the pointer's (wrong) call_id.
#[test]
fn pointer_entries_are_not_themselves_dedup_anchors() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let big = CborValue::Text("z".repeat(2048));
    let _ = run_tool_result(&mut h, "s1", &cid, "call_orig", "read", big.clone());
    let _ = run_tool_result(&mut h, "s1", &cid, "call_dup", "read", big.clone());

    // Force a rebuild on the next intake by clearing the cached dedup map. The
    // next result rebuilds from [Request_orig, Result_orig (real), Request_dup,
    // Result_dup (pointer)], where the pointer remains below the threshold.
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("default conv")
        .execution
        .result_dedup = path_crate_dedup::ResultDedupMap::new();

    let third = run_tool_result(&mut h, "s1", &cid, "call_third", "read", big.clone());
    assert_eq!(third.status, ToolResultStatus::Success);
    let result = third.output;
    let CborValue::Text(text) = &result.raw else {
        panic!("third occurrence should still dedup; got: {result:?}");
    };
    assert!(
        text.contains("call_orig"),
        "third occurrence must point at call_orig (the only real entry on the branch), \
         not at the pointer-bearing call_dup; got: {text:?}",
    );

    h.shutdown().expect("shutdown");
}

/// Errors with the same message and the same details collapse into a
/// pointer; errors that share a message but differ in details stay
/// distinct (distinct details are usually what the model needs to
/// react to).
#[test]
fn identical_errors_collapse_but_distinct_details_stay() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    // Above the threshold: a large "compile failed" message with identical
    // details.
    let long_msg = "compile failed: ".to_owned() + &"E0277 ".repeat(200);

    let first = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e1",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block X".to_owned())),
    );
    let ToolResultStatus::Error { message: m1 } = &first.status else {
        unreachable!()
    };
    assert_eq!(*m1, long_msg, "first error recorded verbatim");

    let second = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e2",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block X".to_owned())),
    );
    let ToolResultStatus::Error { message: m2 } = &second.status else {
        unreachable!()
    };
    assert!(
        !m2.starts_with("<tau_internal>"),
        "deduped error stays raw until provider projection; got message: {m2:?}",
    );
    assert_eq!(
        second.presentation,
        tau_proto::ToolResultPresentation::HarnessDedupPointer,
        "deduped error records durable harness presentation provenance",
    );
    assert!(
        second.output.raw == CborValue::Null,
        "deduped error should drop the details payload"
    );

    let third = run_tool_error(
        &mut h,
        "s1",
        &cid,
        "call_e3",
        "shell",
        long_msg.clone(),
        Some(CborValue::Text("stderr block Y — different".to_owned())),
    );
    let ToolResultStatus::Error { message: m3 } = &third.status else {
        unreachable!()
    };
    assert_eq!(
        *m3, long_msg,
        "different details means the model needs the full content; must NOT dedup",
    );

    h.shutdown().expect("shutdown");
}

/// On session resume / a new harness binding to an existing session
/// tree, the dedup map is rebuilt lazily from the branch the first
/// time a tool result intake needs it. A new identical result must
/// dedup against the pre-existing entry from before the restore.
#[test]
fn dedup_map_rebuilds_on_session_restore() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let big = CborValue::Text("q".repeat(2048));

    {
        let mut h = echo_harness(&sp).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        let _ = run_tool_result(&mut h, "s1", &cid, "call_pre_restore", "read", big.clone());
        h.shutdown().expect("shutdown");
        drop(h);
        wait_for_session_unlock(&sp, "s1");
    }

    // New harness pointing at the same state dir + session id —
    // simulates daemon restart / session resume. The default conv
    // starts with `result_dedup` empty and `head=Some(N)` from the
    // resumed tree; the first intake triggers a rebuild.
    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    let cid = ensure_test_user_agent(&mut h);
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("default conv")
            .identity
            .head
            .is_some(),
        "resumed default conversation must have a non-empty branch head",
    );

    let post = run_tool_result(&mut h, "s1", &cid, "call_post_restore", "read", big.clone());
    assert_eq!(post.status, ToolResultStatus::Success);
    let result = post.output;
    let CborValue::Text(text) = &result.raw else {
        panic!("post-restore identical result should dedup; got: {result:?}");
    };
    assert!(
        text.contains("call_pre_restore"),
        "post-restore dedup must point at the pre-restore call_id; got: {text:?}",
    );

    h.shutdown().expect("shutdown");
}

/// A conversation should only see its OWN branch's prior entries —
/// it must not dedup against content that exists in the tree but
/// only on a different conversation's branch. Modeled here by
/// pinning a side conversation with its own `head` and verifying its
/// dedup map starts empty (no entries from the default conv leak in).
#[test]
fn dedup_is_scoped_to_a_single_branch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let default_cid = ensure_test_user_agent(&mut h);
    let big = CborValue::Text("p".repeat(2048));

    // Land an entry on the default conversation's branch.
    let _ = run_tool_result(
        &mut h,
        "s1",
        &default_cid,
        "call_default",
        "read",
        big.clone(),
    );

    // Spawn a side conversation whose head is None (a fresh root —
    // not parented under the default conv's last node). Its dedup
    // map starts empty; an identical result on its branch must NOT
    // dedup against the default conv's call_default entry, because
    // the side conv's model has no visibility into the default
    // conv's history.
    let side_cid: crate::AgentId = crate::parse_agent_id("side-test");
    h.agent_runtime.agent_registry.agents.insert(
        side_cid.clone(),
        path_crate_agent::Agent::new(
            side_cid.clone(),
            1,
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("core-subagents"),
                query_id: "q-test".to_owned(),
            },
            None, // explicit-root: no inherited head
            None,
        ),
    );

    let side_outcome = run_tool_result(&mut h, "s1", &side_cid, "call_side", "read", big.clone());
    assert_eq!(side_outcome.status, ToolResultStatus::Success);
    let result = side_outcome.output;
    assert_eq!(
        result.raw, big,
        "side conversation's first identical result must NOT dedup against the default \
         conv's prior result — the model on the side conversation can't see that earlier \
         output in its assembled history",
    );

    h.shutdown().expect("shutdown");
}

/// A self-pointer is a defensive no-op: if the same call_id somehow
/// reaches the dedup intake twice (a tracking-map bug, not a model
/// behavior), the second pass must NOT replace the result with a
/// pointer to itself — that would be unrecoverable for the model.
#[test]
fn dedup_refuses_to_self_point() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let cid = ensure_test_user_agent(&mut h);
    let big = CborValue::Text("s".repeat(2048));

    let _first = run_tool_result(&mut h, "s1", &cid, "call_solo", "read", big.clone());

    // Manually run the dedup intake again on a result with the same
    // call_id and same content. Without the self-pointer guard this
    // would produce a dedup pointer to `call_solo` —
    // a pointer to itself.
    let mut replay = ToolResult {
        presentation: Default::default(),
        call_id: "call_solo".into(),
        tool_name: ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: big.clone(),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    };
    h.dedup_tool_result(&cid, &mut replay);
    assert_eq!(
        replay.result, big,
        "self-pointer guard must leave content untouched when the existing \
         dedup-map entry already points at the same call_id; got: {:?}",
        replay.result,
    );

    h.shutdown().expect("shutdown");
}
