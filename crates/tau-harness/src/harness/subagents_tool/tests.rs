use super::*;

/// Peer I/O admission rejects excess work before spawning another worker and
/// releases every process-wide slot when the admitted jobs finish.
#[test]
fn peer_io_admission_is_non_queued_and_bounded() {
    let outbound = (0..MAX_OUTBOUND_PEER_IO_JOBS)
        .map(|_| PeerIoPermit::outbound().expect("outbound slot"))
        .collect::<Vec<_>>();
    assert!(PeerIoPermit::outbound().is_none());
    drop(outbound);
    assert!(PeerIoPermit::outbound().is_some());

    let connection = tau_proto::ConnectionId::from("bounded-peer");
    let inbound = (0..MAX_INBOUND_PEER_AUTH_JOBS_PER_CONNECTION)
        .map(|_| PeerIoPermit::inbound(connection.clone()).expect("inbound slot"))
        .collect::<Vec<_>>();
    assert!(PeerIoPermit::inbound(connection).is_none());
    drop(inbound);
}

/// Isolated runtime lookup leases reject the seventeenth stalled operation and
/// restore capacity after every retained worker exits.
#[test]
fn runtime_lookup_admission_is_non_queued_and_recovers() {
    let permits = (0..MAX_OUTBOUND_PEER_IO_JOBS)
        .map(|_| RuntimeLookupPermit::try_acquire().expect("runtime lookup slot"))
        .collect::<Vec<_>>();
    assert!(RuntimeLookupPermit::try_acquire().is_none());
    drop(permits);
    assert!(RuntimeLookupPermit::try_acquire().is_some());
}

fn conv(id: &str) -> AgentId {
    crate::parse_agent_id(id)
}

fn wait_args_empty() -> CborValue {
    CborValue::Map(Vec::new())
}

fn wait_args_exact(call_id: &str) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Text(call_id.to_owned()),
    )])
}

fn wait_args_input(minutes: i64) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("timeout_minutes".to_owned()),
        CborValue::Integer(minutes.into()),
    )])
}

fn wait_tool_name() -> ToolName {
    ToolName::new(WAIT_TOOL_NAME)
}

fn slow_tool_name() -> ToolName {
    ToolName::new("slow")
}

fn background_placeholder(call_id: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text("still running".to_owned()),
        kind: ToolResultKind::BackgroundPlaceholder,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_result(call_id: &str, text: &str) -> ToolBackgroundResult {
    ToolBackgroundResult {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn background_error(
    call_id: &str,
    message: &str,
    details: Option<CborValue>,
) -> ToolBackgroundError {
    ToolBackgroundError {
        call_id: call_id.into(),
        tool_name: slow_tool_name(),
        tool_type: ToolType::Function,
        message: message.to_owned(),
        details,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn start_wait_any(tracker: &mut WaitTracker, owner: &AgentId, call_id: &str) -> WaitStart {
    tracker.handle_wait_invoke(owner, call_id.into(), wait_tool_name(), &wait_args_empty())
}

fn start_wait_exact(
    tracker: &mut WaitTracker,
    owner: &AgentId,
    wait_call_id: &str,
    target_call_id: &str,
) -> WaitStart {
    tracker.handle_wait_invoke(
        owner,
        wait_call_id.into(),
        wait_tool_name(),
        &wait_args_exact(target_call_id),
    )
}

fn start_wait_input(tracker: &mut WaitTracker, owner: &AgentId, call_id: &str) -> WaitStart {
    tracker.handle_wait_invoke(owner, call_id.into(), wait_tool_name(), &wait_args_input(1))
}

fn start_reply(start: WaitStart) -> WaitReply {
    start.reply.expect("wait reply")
}

fn reply_result(reply: WaitReply) -> CborValue {
    match reply.kind {
        WaitReplyKind::Result { result, .. } => result,
        other => panic!("expected result reply, got {other:?}"),
    }
}

fn reply_result_with_display(reply: WaitReply) -> (CborValue, Option<ToolUseState>) {
    match reply.kind {
        WaitReplyKind::Result { result, display } => (result, display),
        other => panic!("expected result reply, got {other:?}"),
    }
}

fn reply_error(reply: WaitReply) -> (String, Option<CborValue>) {
    match reply.kind {
        WaitReplyKind::Error {
            message, details, ..
        } => (message, details),
        other => panic!("expected error reply, got {other:?}"),
    }
}

fn reply_error_with_display(reply: WaitReply) -> (String, Option<CborValue>, Option<ToolUseState>) {
    match reply.kind {
        WaitReplyKind::Error {
            message,
            details,
            display,
        } => (message, details, display),
        other => panic!("expected error reply, got {other:?}"),
    }
}

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
    })
}

#[test]
fn message_tool_schema_requires_recipient_and_message() {
    let spec = message_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    assert_eq!(
        parameters["required"],
        serde_json::json!(["recipient_id", "message"])
    );
}

#[test]
fn message_args_require_non_empty_recipient_and_message() {
    let ok = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text("user".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    let parsed = parse_message_args(&ok).expect("valid message args");
    assert_eq!(parsed.recipient_id, "user");
    assert_eq!(parsed.message, "hello");

    let missing = CborValue::Map(vec![(
        CborValue::Text("recipient_id".to_owned()),
        CborValue::Text("user".to_owned()),
    )]);
    assert_eq!(
        parse_message_args(&missing),
        Err("`message` is required".to_owned())
    );

    let empty = CborValue::Map(vec![
        (
            CborValue::Text("recipient_id".to_owned()),
            CborValue::Text(" ".to_owned()),
        ),
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("hello".to_owned()),
        ),
    ]);
    assert_eq!(
        parse_message_args(&empty),
        Err("`recipient_id` must not be empty".to_owned())
    );
}

/// The no-arg form is intentional, so the agent-visible schema must not
/// force `tool_call_id` even though the exact-id form remains available.
#[test]
fn wait_tool_schema_does_not_require_tool_call_id() {
    let spec = wait_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    let required = parameters
        .get("required")
        .and_then(serde_json::Value::as_array);

    assert!(required.is_none_or(|items| {
        items
            .iter()
            .all(|item| item.as_str() != Some("tool_call_id"))
    }));
}

/// `wait` returns the original background result to the model, but its UI
/// descriptor must describe the `wait` call itself. The source tool name is
/// useful context; source args/payload/stats are not, because rendering those
/// under the `wait` name made the transcript claim e.g. `wait cargo test`.
#[test]
fn wait_result_display_uses_wait_descriptor_with_source_tool_name() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let source_display = ToolUseState {
        args: "cargo test".to_owned(),
        stats: tau_proto::ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::Error,
        status_text: "1".to_owned(),
        payload: Some(tau_proto::ToolUsePayload::Text {
            text: "cargo test".to_owned(),
        }),
        ..Default::default()
    };
    assert!(
        tracker
            .record_background_result(
                ToolBackgroundResult {
                    call_id: "bg-shell".into(),
                    tool_name: ToolName::new("shell"),
                    tool_type: ToolType::Function,
                    result: CborValue::Text("done".to_owned()),
                    display: Some(source_display),
                    originator: tau_proto::PromptOriginator::User,
                },
                owner.clone()
            )
            .is_empty()
    );

    let (result, display) = reply_result_with_display(start_reply(start_wait_any(
        &mut tracker,
        &owner,
        "wait-shell",
    )));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-shell")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));

    let display = display.expect("wait display");
    assert_eq!(display.args, "shell");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "1");
    assert!(display.stats.is_empty());
    assert!(display.payload.is_none());
}

/// `wait({})` is now the shorthand for waiting on any background
/// completion scoped to the current conversation.
#[test]
fn wait_args_omitted_tool_call_id_parse_as_any_background() {
    assert_eq!(
        parse_wait_args(&wait_args_empty()),
        Ok(WaitTarget::AnyBackground)
    );
    let unrelated = CborValue::Map(vec![(
        CborValue::Text("unused".to_owned()),
        CborValue::Text("ignored".to_owned()),
    )]);
    assert_eq!(parse_wait_args(&unrelated), Ok(WaitTarget::AnyBackground));
}

/// A bare wait with no candidates points callers at the explicit input mode
/// rather than leaving an indefinite wait as an accidental interpretation.
#[test]
fn no_arg_wait_without_candidates_has_actionable_input_guidance() {
    let mut tracker = WaitTracker::default();
    let reply = start_reply(start_wait_any(&mut tracker, &conv("main"), "wait-empty"));
    assert_eq!(reply_error(reply).0, NO_BACKGROUND_WAIT_CANDIDATES);
}

/// Invalid explicit ids still fail early so a typo does not silently turn
/// into a broad no-arg wait.
#[test]
fn wait_args_reject_non_string_and_empty_tool_call_id() {
    let non_string = CborValue::Map(vec![(
        CborValue::Text("tool_call_id".to_owned()),
        CborValue::Bool(true),
    )]);
    assert_eq!(
        parse_wait_args(&non_string),
        Err("`tool_call_id` must be a string".to_owned())
    );
    assert_eq!(
        parse_wait_args(&wait_args_exact("   ")),
        Err("`tool_call_id` must not be empty".to_owned())
    );
}

/// Input waits accept positive whole minutes, clamp before conversion, reject
/// ambiguous or malformed forms, and explicitly diagnose the removed boolean.
#[test]
fn wait_args_parse_explicit_input_mode_and_reject_ambiguous_forms() {
    for (minutes, effective) in [(1, 1), (60, 60), (61, 60), (i64::MAX, 60)] {
        assert_eq!(
            parse_wait_args(&wait_args_input(minutes)),
            Ok(WaitTarget::AnyInput(Duration::from_secs(effective * 60)))
        );
    }
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![(
            CborValue::Text("timeout_minutes".to_owned()),
            CborValue::Integer(u64::MAX.into()),
        )])),
        Ok(WaitTarget::AnyInput(Duration::from_secs(60 * 60)))
    );
    for minutes in [0, -1] {
        assert_eq!(
            parse_wait_args(&wait_args_input(minutes)),
            Err("`timeout_minutes` must be at least 1".to_owned())
        );
    }
    for value in [
        CborValue::Float(1.0),
        CborValue::Float(1.5),
        CborValue::Text("1".to_owned()),
        CborValue::Bool(true),
        CborValue::Null,
    ] {
        assert_eq!(
            parse_wait_args(&CborValue::Map(vec![(
                CborValue::Text("timeout_minutes".to_owned()),
                value,
            )])),
            Err("`timeout_minutes` must be an integer".to_owned())
        );
    }
    for entries in [
        vec![
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("call".to_owned()),
            ),
        ],
        vec![
            (
                CborValue::Text("tool_call_id".to_owned()),
                CborValue::Text("call".to_owned()),
            ),
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
        ],
    ] {
        assert_eq!(
            parse_wait_args(&CborValue::Map(entries)),
            Err("`tool_call_id` and `timeout_minutes` are mutually exclusive".to_owned())
        );
    }
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(1.into()),
            ),
            (
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(2.into()),
            ),
        ])),
        Err("`timeout_minutes` must not be repeated".to_owned())
    );
    assert_eq!(
        parse_wait_args(&CborValue::Map(vec![(
            CborValue::Text("any_input".to_owned()),
            CborValue::Bool(true),
        )])),
        Err(
            "`any_input` is no longer supported; use `timeout_minutes` with a positive integer"
                .to_owned()
        )
    );
}

/// Activating input completes only the addressed agent's input waiter, leaves
/// another agent's waiter untouched, and never copies input content into the
/// bounded input-wait result.
///
/// See `DESIGN-tau-harness-activating-input-wait`.
#[test]
fn activating_input_wakes_only_target_owned_waits() {
    let owner = conv("owner");
    let other = conv("other");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-owner")
            .reply
            .is_none()
    );
    assert!(
        start_wait_input(&mut tracker, &other, "input-other")
            .reply
            .is_none()
    );

    let replies = tracker.activate_waits_for(&owner);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-owner");
    assert_eq!(
        reply_result(replies.into_iter().next().expect("input reply")),
        CborValue::Map(vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )])
    );

    let other_replies = tracker.activate_waits_for(&other);
    assert_eq!(other_replies.len(), 1);
    assert_eq!(other_replies[0].wait_call_id.as_str(), "input-other");
}

/// There can be only one pending input wait per runtime agent, preventing a
/// second tool call from replacing or aliasing the first waiter.
#[test]
fn duplicate_input_wait_is_rejected_without_replacing_first() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-first")
            .reply
            .is_none()
    );
    let duplicate = start_reply(start_wait_input(&mut tracker, &owner, "input-second"));
    assert_eq!(
        reply_error(duplicate).0,
        "existing input wait for this agent already in progress"
    );
    let replies = tracker.activate_waits_for(&owner);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-first");
}

/// Cancellation removes runtime waiter state, so input accepted later cannot
/// produce a second terminal result for the canceled wait call.
#[test]
fn cancelled_input_wait_cannot_be_woken_later() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-cancel")
            .reply
            .is_none()
    );
    let cancelled =
        tracker.record_tool_cancelled(&HashSet::from([ToolCallId::from("input-cancel")]));
    assert!(cancelled.replies.is_empty());
    assert!(tracker.activate_waits_for(&owner).is_empty());
    assert!(
        tracker
            .expire_input_waits(Instant::now() + Duration::from_secs(120))
            .is_empty()
    );
}

/// Unloading an endpoint drops its runtime-only waiter so a later endpoint
/// reusing the same runtime id cannot inherit or complete stale tool state.
#[test]
fn discarded_input_wait_is_not_inherited_by_reused_agent_id() {
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        start_wait_input(&mut tracker, &owner, "input-old")
            .reply
            .is_none()
    );
    tracker.discard_input_wait_for(&owner);
    assert!(tracker.activate_waits_for(&owner).is_empty());
    assert!(
        tracker
            .expire_input_waits(Instant::now() + Duration::from_secs(120))
            .is_empty()
    );
    assert!(
        start_wait_input(&mut tracker, &owner, "input-new")
            .reply
            .is_none()
    );
    let replies = tracker.activate_waits_for(&owner);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].wait_call_id.as_str(), "input-new");
}

/// Monotonic input deadlines expose the earliest waiter, expire all due
/// waiters exactly once, isolate later targets, and use warning UI metadata.
#[test]
fn input_wait_deadlines_are_ordered_and_expire_exactly_once() {
    let now = Instant::now();
    let first = conv("first");
    let first_peer = conv("first-peer");
    let second = conv("second");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .handle_wait_invoke_at(
                &first,
                "wait-first".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
            )
            .reply
            .is_none()
    );
    assert!(
        tracker
            .handle_wait_invoke_at(
                &first_peer,
                "wait-first-peer".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
            )
            .reply
            .is_none()
    );
    assert!(
        tracker
            .handle_wait_invoke_at(
                &second,
                "wait-second".into(),
                wait_tool_name(),
                &wait_args_input(2),
                now,
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(60))
    );
    assert!(
        tracker
            .expire_input_waits(now + Duration::from_secs(59))
            .is_empty()
    );

    let replies = tracker.expire_input_waits(now + Duration::from_secs(60));
    assert_eq!(replies.len(), 2);
    let reply = replies
        .into_iter()
        .find(|reply| reply.wait_call_id.as_str() == "wait-first")
        .expect("first due timeout");
    let (result, display) = reply_result_with_display(reply);
    assert_eq!(
        result,
        CborValue::Map(vec![(
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(true),
        )])
    );
    let display = display.expect("timeout display");
    assert_eq!(display.status, ToolUseStatus::Warning);
    assert_eq!(display.status_text, "timeout");
    assert!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .is_empty()
    );
    assert_eq!(
        tracker.next_input_wait_deadline(),
        Some(now + Duration::from_secs(120))
    );
    assert_eq!(tracker.activate_waits_for(&second).len(), 1);
    assert_eq!(tracker.next_input_wait_deadline(), None);
}

/// Event-loop serialization makes timeout-before-input terminal exactly once.
#[test]
fn expired_input_wait_cannot_be_activated_later() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "wait".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .len(),
        1
    );
    assert!(tracker.activate_waits_for(&owner).is_empty());
}

/// When timeout wins before a background completion, that later completion
/// remains available to the ordinary bare background collector.
#[test]
fn input_timeout_does_not_consume_later_background_completion() {
    let now = Instant::now();
    let owner = conv("owner");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .handle_wait_invoke_at(
                &owner,
                "input-wait".into(),
                wait_tool_name(),
                &wait_args_input(1),
                now,
            )
            .reply
            .is_none()
    );
    assert_eq!(
        tracker
            .expire_input_waits(now + Duration::from_secs(60))
            .len(),
        1
    );
    assert!(
        tracker
            .record_background_result(background_result("later-bg", "done"), owner.clone())
            .is_empty()
    );
    let collected = start_wait_any(&mut tracker, &owner, "collect-later");
    assert_eq!(
        cbor_map_text(
            &reply_result(start_reply(collected)),
            ORIGINAL_TOOL_CALL_ID_HEADER,
        ),
        Some("later-bg")
    );
}

/// Completed any-waits must use deterministic finish order, not HashMap
/// iteration order. The call that finishes first is consumed first even if
/// its id sorts after a later completion.
#[test]
fn no_arg_wait_consumes_oldest_completed_background_result_for_owner() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-b", "first finished"), owner.clone())
            .is_empty()
    );
    assert!(
        tracker
            .record_background_result(background_result("bg-a", "second finished"), owner.clone())
            .is_empty()
    );

    let first = start_wait_any(&mut tracker, &owner, "wait-first");
    assert_eq!(
        first.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-b")
    );
    let first_result = reply_result(start_reply(first));
    assert_eq!(
        cbor_map_text(&first_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-b")
    );
    assert_eq!(
        cbor_map_text(&first_result, "output"),
        Some("first finished")
    );

    let second = start_wait_any(&mut tracker, &owner, "wait-second");
    assert_eq!(
        second.suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-a")
    );
    let second_result = reply_result(start_reply(second));
    assert_eq!(
        cbor_map_text(&second_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-a")
    );
    assert_eq!(
        cbor_map_text(&second_result, "output"),
        Some("second finished")
    );
}

/// If a same-conversation background call is still running, `wait({})`
/// must block and resolve when the first matching completion arrives.
#[test]
fn no_arg_wait_blocks_on_running_background_call_and_resolves() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );

    let start = start_wait_any(&mut tracker, &owner, "wait-any");
    assert!(start.reply.is_none());
    assert!(start.suppress_call_id.is_none());

    let replies = tracker.record_background_result(background_result("bg-run", "done"), owner);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].suppress_call_id.as_ref().map(|id| id.as_str()),
        Some("bg-run")
    );
    let result = reply_result(replies.into_iter().next().expect("reply"));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-run")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));
}

/// If a no-arg wait is active while its only background call is canceled, the
/// cancellation is the completion it was waiting for. It must reply immediately
/// instead of parking the waiter behind the newly stored cancellation result.
#[test]
fn no_arg_wait_resolves_when_running_background_call_is_cancelled() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel-any".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-cancel-any"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-cancel-any")
            .reply
            .is_none()
    );

    let cancelled =
        tracker.record_tool_cancelled(&HashSet::from([ToolCallId::from("bg-cancel-any")]));
    assert_eq!(
        cancelled
            .suppress_call_ids
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec!["bg-cancel-any"]
    );
    assert_eq!(cancelled.replies.len(), 1);
    let (message, details, display) =
        reply_error_with_display(cancelled.replies.into_iter().next().expect("wait reply"));
    assert_eq!(message, "Tool call canceled");
    let details = details.expect("original call id details");
    assert_eq!(
        cbor_map_text(&details, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-cancel-any")
    );
    let display = display.expect("wait cancellation display");
    assert_eq!(display.args, "slow");
    assert_eq!(display.status, ToolUseStatus::Error);
}

/// Background errors are completions too. A no-arg wait must return the
/// error and include the original background id in provider-visible
/// details.
#[test]
fn no_arg_wait_returns_background_error_with_original_id_details() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    let details = CborValue::Map(vec![(
        CborValue::Text("hint".to_owned()),
        CborValue::Text("bad input".to_owned()),
    )]);
    assert!(
        tracker
            .record_background_error(
                background_error("bg-fail", "boom", Some(details)),
                owner.clone()
            )
            .is_empty()
    );

    let reply = start_reply(start_wait_any(&mut tracker, &owner, "wait-error"));
    let (message, details) = reply_error(reply);
    assert_eq!(message, "boom");
    let details = details.expect("details");
    assert_eq!(
        cbor_map_text(&details, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-fail")
    );
    assert_eq!(cbor_map_text(&details, "hint"), Some("bad input"));
}

/// When an exact waiter and an any-waiter can both see the same completion,
/// the exact waiter gets the result and the any-waiter does not consume a
/// duplicate copy.
#[test]
fn explicit_waiter_wins_over_any_waiter_for_same_completion() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any")
            .reply
            .is_none()
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-exact", "bg-run")
            .reply
            .is_none()
    );

    let replies = tracker.record_background_result(background_result("bg-run", "done"), owner);
    assert!(replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-exact"
            && matches!(reply.kind, WaitReplyKind::Result { .. })
    }));
    assert!(replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-any"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, .. }
                    if message == NO_BACKGROUND_WAIT_CANDIDATES
            )
    }));
}

/// Cancellation follows the same arbitration as a normal completion: an exact
/// waiter for the canceled call consumes the cancellation result, and a no-arg
/// waiter must not receive a duplicate copy with original-call details.
#[test]
fn explicit_waiter_wins_over_any_waiter_for_same_cancelled_completion() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel-race".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-cancel-race"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-any-cancel")
            .reply
            .is_none()
    );
    assert!(
        start_wait_exact(&mut tracker, &owner, "wait-exact-cancel", "bg-cancel-race")
            .reply
            .is_none()
    );

    let cancelled =
        tracker.record_tool_cancelled(&HashSet::from([ToolCallId::from("bg-cancel-race")]));
    assert_eq!(cancelled.replies.len(), 2);
    assert!(cancelled.replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-exact-cancel"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, details, .. }
                    if message == "Tool call `bg-cancel-race` was cancelled"
                        && details.is_none()
            )
    }));
    assert!(cancelled.replies.iter().any(|reply| {
        reply.wait_call_id.as_str() == "wait-any-cancel"
            && matches!(
                &reply.kind,
                WaitReplyKind::Error { message, details, .. }
                    if message == "background tool call in this conversation was cancelled"
                        && details.is_none()
            )
    }));
    assert!(cancelled.suppress_call_ids.is_empty());
}

/// Parallel duplicate no-arg waits in one conversation would be ambiguous:
/// only one waiter may consume the next completion.
#[test]
fn duplicate_no_arg_waits_in_same_conversation_error() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-run".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-run"), owner.clone())
            .is_empty()
    );
    assert!(
        start_wait_any(&mut tracker, &owner, "wait-one")
            .reply
            .is_none()
    );

    let (message, details) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &owner,
        "wait-two",
    )));
    assert!(message.contains("existing wait for a background tool call"));
    assert!(details.is_none());
}

/// The no-arg form is scoped to its caller's conversation. A completion in
/// a different conversation must not be stolen by this wait.
#[test]
fn no_arg_wait_ignores_background_completions_from_other_conversations() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("side-bg", "side done"), side.clone())
            .is_empty()
    );

    let (message, _) = reply_error(start_reply(start_wait_any(
        &mut tracker,
        &main,
        "wait-main",
    )));
    assert!(message.contains("no background tool calls"));

    let side_result = reply_result(start_reply(start_wait_any(
        &mut tracker,
        &side,
        "wait-side",
    )));
    assert_eq!(
        cbor_map_text(&side_result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("side-bg")
    );
}

/// Exact-id waits are scoped to the calling conversation just like no-arg
/// waits. A caller that guesses or observes another conversation's completed
/// background id must not consume it or learn that it exists.
#[test]
fn exact_wait_does_not_consume_completed_background_result_from_other_conversation() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("side-bg", "side done"), side.clone())
            .is_empty()
    );

    let (message, details) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &main,
        "wait-main",
        "side-bg",
    )));
    assert_eq!(message, "unknown tool call: `side-bg`");
    assert!(details.is_none());

    assert_eq!(
        reply_result(start_reply(start_wait_exact(
            &mut tracker,
            &side,
            "wait-side",
            "side-bg",
        ))),
        CborValue::Text("side done".to_owned())
    );
}

/// Exact-id waits must also reject another conversation's still-running
/// background call. Without this guard, a cross-agent exact wait could park
/// first and consume the later completion.
#[test]
fn exact_wait_does_not_attach_to_running_background_call_from_other_conversation() {
    let main = conv("main");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("side-bg".into(), slow_tool_name(), side.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("side-bg"), side.clone())
            .is_empty()
    );

    let (message, details) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &main,
        "wait-main",
        "side-bg",
    )));
    assert_eq!(message, "unknown tool call: `side-bg`");
    assert!(details.is_none());

    let start = start_wait_exact(&mut tracker, &side, "wait-side", "side-bg");
    assert!(start.reply.is_none());
    let replies = tracker.record_background_result(background_result("side-bg", "side done"), side);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        reply_result(replies.into_iter().next().expect("side wait reply")),
        CborValue::Text("side done".to_owned())
    );
}

/// Once a no-arg wait consumes a completion, a later exact wait for that
/// original id must report that the result was already handled.
#[test]
fn exact_wait_after_no_arg_consumes_reports_already_consumed() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-once", "done"), owner.clone())
            .is_empty()
    );
    let _ = start_reply(start_wait_any(&mut tracker, &owner, "wait-any"));

    let (message, _) = reply_error(start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-exact",
        "bg-once",
    )));
    assert!(message.contains("already consumed"));
}

/// Side-conversation teardown transfers background ownership to the parent;
/// the wait tracker must follow that transfer so the parent can consume a
/// completion with `wait({})` after the side conversation disappears.
#[test]
fn transferred_background_owner_can_be_consumed_by_parent_no_arg_wait() {
    let parent = conv("parent");
    let side = conv("side");
    let mut tracker = WaitTracker::default();
    assert!(
        tracker
            .record_background_result(background_result("bg-side", "done"), side.clone())
            .is_empty()
    );
    tracker.transfer_call_owner(&"bg-side".into(), &side, &parent);

    let result = reply_result(start_reply(start_wait_any(
        &mut tracker,
        &parent,
        "wait-parent",
    )));
    assert_eq!(
        cbor_map_text(&result, ORIGINAL_TOOL_CALL_ID_HEADER),
        Some("bg-side")
    );
    assert_eq!(cbor_map_text(&result, "output"), Some("done"));
}

/// A canceled background call remains waitable once. It should not be marked
/// consumed before the caller has a chance to retrieve the cancellation result.
#[test]
fn exact_wait_after_background_cancel_returns_cancel_error_once() {
    let owner = conv("main");
    let mut tracker = WaitTracker::default();
    tracker.record_tool_invoke("bg-cancel".into(), slow_tool_name(), owner.clone());
    assert!(
        tracker
            .record_tool_result(background_placeholder("bg-cancel"), owner.clone())
            .is_empty()
    );

    let cancelled = HashSet::from([ToolCallId::from("bg-cancel")]);
    let wait_cancel = tracker.record_tool_cancelled(&cancelled);
    assert!(wait_cancel.replies.is_empty());

    let reply = start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-cancel",
        "bg-cancel",
    ));
    let (message, details) = reply_error(reply);
    assert_eq!(message, "Tool call canceled");
    assert!(details.is_none());

    let reply = start_reply(start_wait_exact(
        &mut tracker,
        &owner,
        "wait-cancel-again",
        "bg-cancel",
    ));
    let (message, _) = reply_error(reply);
    assert_eq!(message, "result for tool call `bg-cancel` already consumed");
}

/// Ensures sent/received agent-message events can be correlated uniquely even
/// when one sender emits multiple messages with the same timestamp.
#[test]
fn generated_agent_message_ids_are_unique_for_same_sender_and_timestamp() {
    let mut seen = std::collections::HashSet::new();
    let sender_id = crate::parse_agent_id("agent-test");
    let timestamp = tau_proto::UnixMicros::new(42);

    for sequence in 1..=1000 {
        let id = build_agent_message_id(&sender_id, timestamp, sequence);
        assert!(seen.insert(id), "message id must be unique");
    }
}
