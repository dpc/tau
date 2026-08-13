//! Closed grammar and resource validation for fake-provider scenarios.

use std::collections as path_std_collections;

use tau_proto::ProviderFailureKind;

use super::*;

pub(super) fn validate_v1(scenario: &ScenarioV1) -> ClientResult<()> {
    if scenario.version != 0 {
        return Err(ClientError::handler("ScenarioV1 version must be 0"));
    }
    if scenario.turns.is_empty() || scenario.turns.len() > MAX_TURNS {
        return Err(ClientError::handler("scenario must contain 1..=8 turns"));
    }
    let total_text = serde_json::to_vec(scenario)
        .map_err(|error| ClientError::handler(error.to_string()))?
        .len();
    if MAX_SCENARIO_BYTES < total_text {
        return Err(ClientError::handler("scenario exceeds 16384 bytes"));
    }
    match scenario.turns.as_slice() {
        [
            ScenarioTurnV1::Text {
                user_text: _,
                deltas,
                response,
            },
        ] if !deltas.is_empty() && deltas.len() <= MAX_DELTAS && deltas.concat() == *response => {}
        [
            ScenarioTurnV1::ToolCall {
                user_text: _,
                tool_name,
                call_id,
            },
            ScenarioTurnV1::ToolResult {
                call_id: result_id,
                response: _,
            },
        ] if tool_name.as_str() == tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME
            && !call_id.is_empty()
            && call_id.len() <= 256
            && call_id == result_id => {}
        [
            ScenarioTurnV1::StatusPolicyToolCall {
                user_text: _,
                order: _,
                initial_status: call_status,
                terminal_phase: call_phase,
            },
            ScenarioTurnV1::StatusPolicyToolResult {
                initial_status: result_status,
                terminal_phase: result_phase,
            },
            ScenarioTurnV1::WorkingFollowupToolResult {
                initial_status: followup_status,
                terminal_phase: followup_phase,
            },
            ScenarioTurnV1::WorkingFinalStatusCall {
                terminal_phase: challenge_phase,
            },
            ScenarioTurnV1::TerminalStatusResult {
                terminal_phase,
                response: _,
            },
        ] if call_status == result_status
            && result_status == followup_status
            && call_phase == result_phase
            && result_phase == followup_phase
            && followup_phase == challenge_phase
            && challenge_phase == terminal_phase => {}
        [
            ScenarioTurnV1::Text {
                user_text: _,
                deltas: _,
                response: _,
            },
        ] => {
            return Err(ClientError::handler(
                "text scenario requires 1..=8 deltas concatenating to the final response",
            ));
        }
        _ => {
            return Err(ClientError::handler(
                "scenario grammar must be one text turn or one supported tool sequence",
            ));
        }
    }
    Ok(())
}

/// Validates the closed version-two lane grammar and its resource bounds.
pub(super) fn validate_v2(scenario: &ScenarioV2) -> ClientResult<()> {
    if scenario.version != 0 {
        return Err(ClientError::handler("ScenarioV2 version must be 0"));
    }
    if scenario.lanes.is_empty() || scenario.lanes.len() > 8 {
        return Err(ClientError::handler("scenario must contain 1..=8 lanes"));
    }
    if serde_json::to_vec(scenario)
        .map_err(|error| ClientError::handler(error.to_string()))?
        .len()
        > MAX_SCENARIO_BYTES
    {
        return Err(ClientError::handler("scenario exceeds 16384 bytes"));
    }
    let mut ids = path_std_collections::HashSet::new();
    let mut barriers: HashMap<&str, (usize, std::collections::HashSet<&str>)> = HashMap::new();
    let mut dummy_call_ids = path_std_collections::HashSet::new();
    let mut core_call_ids = path_std_collections::HashSet::new();
    let mut agent_start_call_ids = path_std_collections::HashSet::new();
    let mut agent_watch_call_ids = path_std_collections::HashSet::new();
    for lane in &scenario.lanes {
        if lane.ctx_id.is_empty() || lane.ctx_id.len() > 64 || !ids.insert(lane.ctx_id.as_str()) {
            return Err(ClientError::handler(
                "lane ctx_id must be unique and contain 1..=64 bytes",
            ));
        }
        if lane.actions.is_empty() || lane.actions.len() > MAX_TURNS {
            return Err(ClientError::handler("lane must contain 1..=8 actions"));
        }
        for (action_index, action) in lane.actions.iter().enumerate() {
            match action {
                ScenarioActionV2::DummyToolCall { call_id, .. }
                    if !dummy_call_ids.insert(call_id.as_str())
                        || 2 < dummy_call_ids.len()
                        || call_id.is_empty()
                        || call_id.len() > 256
                        || !matches!(
                                lane.actions.get(action_index + 1),
                                Some(ScenarioActionV2::DummyToolResult {
                        call_id: result_id,
                                    ..
                                }) | Some(ScenarioActionV2::DummyToolRepair {
                                    call_id: result_id,
                                    ..
                                }) if result_id == call_id
                            ) =>
                {
                    return Err(ClientError::handler(
                        "scenario requires exactly one unique bounded dummy call/result pair",
                    ));
                }
                ScenarioActionV2::DummyToolResult { call_id, .. }
                    if call_id.is_empty()
                        || call_id.len() > 256
                        || !matches!(
                            action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                            Some(ScenarioActionV2::DummyToolCall {
                                call_id: request_id,
                                ..
                            }) if request_id == call_id
                        ) =>
                {
                    return Err(ClientError::handler(
                        "dummy tool result must have a 1..=256 byte id and matching prior call",
                    ));
                }
                ScenarioActionV2::DummyToolRepair {
                    call_id,
                    diagnostic,
                    ..
                } if call_id.is_empty()
                    || call_id.len() > 256
                    || diagnostic.is_empty()
                    || diagnostic.len() > 512
                    || !matches!(
                        action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                        Some(ScenarioActionV2::DummyToolCall {
                            call_id: request_id,
                            ..
                        }) if request_id == call_id
                    ) =>
                {
                    return Err(ClientError::handler(
                        "dummy tool repair must be bounded and follow its matching call",
                    ));
                }
                ScenarioActionV2::MessageCall {
                    call_id, message, ..
                } if call_id.is_empty()
                    || call_id.len() > 256
                    || message.is_empty()
                    || 4 * 1024 < message.len()
                    || !matches!(
                        lane.actions.get(action_index + 1),
                        Some(ScenarioActionV2::MessageSenderResult {
                            call_id: result_id,
                            message: result_message,
                            ..
                        }) if result_id == call_id && result_message == message
                    ) =>
                {
                    return Err(ClientError::handler(
                        "message call must have one bounded adjacent matching result",
                    ));
                }
                ScenarioActionV2::MessageSenderResult {
                    call_id, message, ..
                } if call_id.is_empty()
                    || call_id.len() > 256
                    || message.is_empty()
                    || 4 * 1024 < message.len() =>
                {
                    return Err(ClientError::handler(
                        "message result fields must be bounded and nonempty",
                    ));
                }
                ScenarioActionV2::MessageInbound {
                    call_id, message, ..
                } if call_id.is_empty()
                    || call_id.len() > 256
                    || message.is_empty()
                    || 4 * 1024 < message.len() =>
                {
                    return Err(ClientError::handler(
                        "message inbound fields must be bounded and nonempty",
                    ));
                }
                ScenarioActionV2::AgentStartCall {
                    call_id,
                    prompt,
                    role,
                    ..
                } if call_id.is_empty()
                    || call_id.len() > 256
                    || prompt.is_empty()
                    || prompt.len() > 4 * 1024
                    || role.is_empty()
                    || role.len() > 256
                    || !agent_start_call_ids.insert(call_id.as_str())
                    || agent_start_call_ids.len() > MAX_AGENT_START_PAIRS
                    || !matches!(
                        lane.actions.get(action_index + 1),
                        Some(ScenarioActionV2::AgentStartResult {
                            call_id: result_id,
                            ..
                        }) if result_id == call_id
                    ) =>
                {
                    return Err(ClientError::handler(
                        "scenario allows at most two unique bounded adjacent agent_start call/result pairs",
                    ));
                }
                ScenarioActionV2::AgentStartResult { call_id, .. }
                    if call_id.is_empty()
                        || call_id.len() > 256
                        || !matches!(
                            action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                            Some(ScenarioActionV2::AgentStartCall {
                                call_id: request_id,
                                ..
                            }) if request_id == call_id
                        ) =>
                {
                    return Err(ClientError::handler(
                        "agent_start result must have a bounded matching prior call",
                    ));
                }
                ScenarioActionV2::AgentWatchCall { call_id, .. }
                    if call_id.is_empty()
                        || call_id.len() > 256
                        || !agent_watch_call_ids.insert(call_id.as_str())
                        || agent_watch_call_ids.len() > 1
                        || !matches!(
                            lane.actions.get(action_index + 1),
                            Some(ScenarioActionV2::AgentWatchResult {
                                call_id: result_id,
                                ..
                            }) if result_id == call_id
                        ) =>
                {
                    return Err(ClientError::handler(
                        "scenario requires exactly one unique bounded adjacent agent_watch call/result pair",
                    ));
                }
                ScenarioActionV2::AgentWatchResult { call_id, .. }
                    if call_id.is_empty()
                        || call_id.len() > 256
                        || !matches!(
                            action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                            Some(ScenarioActionV2::AgentWatchCall {
                                call_id: request_id,
                                ..
                            }) if request_id == call_id
                        ) =>
                {
                    return Err(ClientError::handler(
                        "agent_watch result must have a bounded matching prior call",
                    ));
                }
                ScenarioActionV2::WatchNotifications { notifications, .. }
                    if notifications.is_empty()
                        || notifications.len() > 4
                        || notifications.iter().any(|notification| match notification {
                            crate::WatchNotificationV2::Response { content }
                            | crate::WatchNotificationV2::Prompt { content } => {
                                content.is_empty() || content.len() > 4 * 1024
                            }
                        }) =>
                {
                    return Err(ClientError::handler(
                        "watch notification batches require 1..=4 bounded notifications",
                    ));
                }
                ScenarioActionV2::WatchNotificationChains {
                    prompt, response, ..
                } if prompt.is_empty()
                    || prompt.len() > 4 * 1024
                    || response.is_empty()
                    || response.len() > 4 * 1024 =>
                {
                    return Err(ClientError::handler(
                        "watch notification chains require bounded prompt and response text",
                    ));
                }
                ScenarioActionV2::ContextOverflow {
                    user_text,
                    removed_user_text,
                    removed_assistant_text,
                    failure_kind,
                } if user_text.is_empty()
                    || user_text.len() > 4 * 1024
                    || removed_user_text.is_empty()
                    || removed_user_text.len() > 4 * 1024
                    || removed_assistant_text.is_empty()
                    || removed_assistant_text.len() > 4 * 1024
                    || *failure_kind != ProviderFailureKind::ContextWindowExceeded
                    || !matches!(
                        action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                        Some(ScenarioActionV2::Text {
                            user_text: prior_user_text,
                            response: prior_response,
                        }) if prior_user_text == removed_user_text
                            && prior_response == removed_assistant_text
                    )
                    || !matches!(
                        lane.actions.get(action_index + 1),
                        Some(ScenarioActionV2::ReactiveOpaqueCompaction {
                            removed_user_text: reactive_removed,
                            removed_assistant_text: reactive_response,
                            overflow_user_text,
                        }) if reactive_removed == removed_user_text
                            && reactive_response == removed_assistant_text
                            && overflow_user_text == user_text
                    ) =>
                {
                    return Err(ClientError::handler(
                        "context overflow must be bounded, canonical, and immediately start reactive opaque compaction",
                    ));
                }
                ScenarioActionV2::ReactiveOpaqueCompaction {
                    removed_user_text,
                    removed_assistant_text,
                    overflow_user_text,
                } if removed_user_text.is_empty()
                    || removed_user_text.len() > 4 * 1024
                    || removed_assistant_text.is_empty()
                    || removed_assistant_text.len() > 4 * 1024
                    || overflow_user_text.is_empty()
                    || overflow_user_text.len() > 4 * 1024
                    || !matches!(
                    action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                    Some(ScenarioActionV2::ContextOverflow {
                        removed_user_text: overflow_removed,
                        removed_assistant_text: overflow_response,
                        user_text,
                        ..
                    }) if overflow_removed == removed_user_text
                        && overflow_response == removed_assistant_text
                        && user_text == overflow_user_text
                    )
                    || !matches!(
                        lane.actions.get(action_index + 1),
                        Some(ScenarioActionV2::ReactiveCompactedOpaqueText {
                            removed_user_text: continued_removed,
                            removed_assistant_text: continued_response,
                            overflow_user_text: continued_overflow,
                            response: _,
                        }) if continued_removed == removed_user_text
                            && continued_response == removed_assistant_text
                            && continued_overflow == overflow_user_text
                    ) =>
                {
                    return Err(ClientError::handler(
                        "reactive opaque compaction must uniquely follow its overflow and precede its continuation",
                    ));
                }
                ScenarioActionV2::ReactiveCompactedOpaqueText {
                    removed_user_text,
                    removed_assistant_text,
                    overflow_user_text,
                    response,
                } if removed_user_text.is_empty()
                    || removed_user_text.len() > 4 * 1024
                    || removed_assistant_text.is_empty()
                    || removed_assistant_text.len() > 4 * 1024
                    || overflow_user_text.is_empty()
                    || overflow_user_text.len() > 4 * 1024
                    || response.is_empty()
                    || response.len() > 4 * 1024
                    || !matches!(
                        action_index.checked_sub(1).and_then(|index| lane.actions.get(index)),
                        Some(ScenarioActionV2::ReactiveOpaqueCompaction {
                            removed_user_text: compacted_removed,
                            removed_assistant_text: compacted_response,
                            overflow_user_text: compacted_overflow,
                        }) if compacted_removed == removed_user_text
                            && compacted_response == removed_assistant_text
                            && compacted_overflow == overflow_user_text
                    ) =>
                {
                    return Err(ClientError::handler(
                        "reactive continuation must be bounded and immediately follow its matching compaction",
                    ));
                }
                ScenarioActionV2::CoreShellWorkdirCall { call_id, .. }
                | ScenarioActionV2::CoreShellResumeEditCall { call_id, .. }
                    if call_id.is_empty()
                        || call_id.len() > 256
                        || !core_call_ids.insert(call_id.as_str()) =>
                {
                    return Err(ClientError::handler(
                        "core-shell call ids must be unique and bounded",
                    ));
                }
                ScenarioActionV2::CoreShellWorkdirResult {
                    call_id,
                    edit_call_id,
                    nonce,
                    ..
                } if !matches!(action_index.checked_sub(1).and_then(|i| lane.actions.get(i)),
                        Some(ScenarioActionV2::CoreShellWorkdirCall { call_id: prior, .. }) if prior == call_id)
                    || edit_call_id.is_empty()
                    || edit_call_id.len() > 256
                    || !core_call_ids.insert(edit_call_id.as_str())
                    || nonce.is_empty()
                    || nonce.len() > 128 =>
                {
                    return Err(ClientError::handler("workdir result must follow its call"));
                }
                ScenarioActionV2::CoreShellCreateResult { call_id, .. }
                    if !matches!(action_index.checked_sub(1).and_then(|i| lane.actions.get(i)),
                        Some(ScenarioActionV2::CoreShellWorkdirResult { edit_call_id: prior, .. }) if prior == call_id) =>
                {
                    return Err(ClientError::handler("create result must follow its call"));
                }
                ScenarioActionV2::CoreShellResumeEditResult { call_id, .. }
                    if !matches!(action_index.checked_sub(1).and_then(|i| lane.actions.get(i)),
                        Some(ScenarioActionV2::CoreShellResumeEditCall { call_id: prior, .. }) if prior == call_id) =>
                {
                    return Err(ClientError::handler(
                        "resume edit result must follow its call",
                    ));
                }
                ScenarioActionV2::HoldUntilCancel { timeout_ms, .. }
                | ScenarioActionV2::StandaloneCompactionHold { timeout_ms }
                    if !(100..=10_000).contains(timeout_ms) =>
                {
                    return Err(ClientError::handler(
                        "hold timeout_ms must be in 100..=10000",
                    ));
                }
                ScenarioActionV2::BarrierText {
                    barrier,
                    participants,
                    ..
                } if lane.actions.len() != 1
                    || barrier.is_empty()
                    || barrier.len() > 64
                    || !(2..=scenario.lanes.len()).contains(participants) =>
                {
                    return Err(ClientError::handler(
                        "a bounded barrier must be the lane's only action",
                    ));
                }
                ScenarioActionV2::BarrierText {
                    barrier,
                    participants,
                    ..
                } => {
                    let entry = barriers
                        .entry(barrier)
                        .or_insert_with(|| (*participants, path_std_collections::HashSet::new()));
                    if entry.0 != *participants {
                        return Err(ClientError::handler(
                            "barrier participant counts must agree",
                        ));
                    }
                    if !entry.1.insert(lane.ctx_id.as_str()) {
                        return Err(ClientError::handler(
                            "a barrier may appear at most once per lane",
                        ));
                    }
                }
                ScenarioActionV2::Error { error, .. }
                | ScenarioActionV2::Disconnect { reason: error, .. }
                | ScenarioActionV2::StandaloneCompactionError {
                    failure_kind: _,
                    error,
                } if error.is_empty() || error.len() > 256 => {
                    return Err(ClientError::handler(
                        "synthetic diagnostic must contain 1..=256 bytes",
                    ));
                }
                _ => {}
            }
        }
    }
    let message_lanes = scenario
        .lanes
        .iter()
        .filter(|lane| {
            lane.actions.iter().any(|action| {
                matches!(
                    action,
                    ScenarioActionV2::MessageCall { .. }
                        | ScenarioActionV2::MessageSenderResult { .. }
                        | ScenarioActionV2::MessageInbound { .. }
                )
            })
        })
        .collect::<Vec<_>>();
    if !message_lanes.is_empty()
        && (scenario.lanes.len() != 2
            || message_lanes.len() != 2
            || !matches!(
                message_lanes[0].actions.as_slice(),
                [ScenarioActionV2::MessageCall {
                    call_id,
                    message: body,
                    ..
                }, ScenarioActionV2::MessageSenderResult {
                    call_id: result_id,
                    message: result_body,
                    ..
                }, ScenarioActionV2::Text { .. }]
                    if call_id == result_id && body == result_body
            )
            || !matches!(
                message_lanes[1].actions.as_slice(),
                [ScenarioActionV2::MessageInbound {
                    call_id: inbound_id,
                    message: inbound_body,
                    ..
                }, ScenarioActionV2::Text { .. }]
                    if matches!(
                        message_lanes[0].actions.first(),
                        Some(ScenarioActionV2::MessageCall {
                            call_id,
                            message,
                            ..
                        }) if call_id == inbound_id && message == inbound_body
                    )
            ))
    {
        return Err(ClientError::handler(
            "message scenario requires one closed main call/result lane and one matching inbound lane",
        ));
    }
    let typed_image_lanes = scenario
        .lanes
        .iter()
        .filter(|lane| {
            lane.actions.iter().any(|action| {
                matches!(
                    action,
                    ScenarioActionV2::TypedImageToolCall { .. }
                        | ScenarioActionV2::TypedImageToolResult { .. }
                        | ScenarioActionV2::TypedImageReplay { .. }
                )
            })
        })
        .collect::<Vec<_>>();
    if !typed_image_lanes.is_empty()
        && (scenario.lanes.len() != 1
            || typed_image_lanes.len() != 1
            || !matches!(
                typed_image_lanes[0].actions.as_slice(),
                [
                    ScenarioActionV2::TypedImageToolCall {
                        user_text: initial_user_text,
                        call_id,
                    },
                    ScenarioActionV2::TypedImageToolResult {
                        call_id: result_id,
                        response: live_response,
                    },
                    ScenarioActionV2::TypedImageReplay {
                        user_text: replay_user_text,
                        call_id: replay_id,
                        response: replay_response,
                    },
                ] if !initial_user_text.is_empty()
                    && initial_user_text.len() <= 4 * 1024
                    && !replay_user_text.is_empty()
                    && replay_user_text.len() <= 4 * 1024
                    && !call_id.is_empty()
                    && call_id.len() <= 256
                    && call_id == result_id
                    && call_id == replay_id
                    && !live_response.is_empty()
                    && live_response.len() <= 4 * 1024
                    && !replay_response.is_empty()
                    && replay_response.len() <= 4 * 1024
            ))
    {
        return Err(ClientError::handler(
            "typed-image scenario requires one bounded call/result/replay lane",
        ));
    }
    if 1 < dummy_call_ids.len()
        && (dummy_call_ids.len() != 2
            || scenario.lanes.len() != 1
            || !is_disconnect_repair_then_success(&scenario.lanes[0].actions))
    {
        return Err(ClientError::handler(
            "two dummy pairs require exact call/repair/call/success lifecycle order",
        ));
    }
    if barriers
        .values()
        .any(|(participants, lanes)| *participants != lanes.len())
    {
        return Err(ClientError::handler(
            "barrier action count must equal participants",
        ));
    }
    Ok(())
}

/// Matches the sole two-pair dummy lifecycle used to prove one live extension
/// disconnect, provider repair, replacement readiness, and later success.
fn is_disconnect_repair_then_success(actions: &[ScenarioActionV2]) -> bool {
    matches!(
        actions,
        [
            ScenarioActionV2::DummyToolCall {
                call_id: first_call, ..
            },
            ScenarioActionV2::DummyToolRepair {
                call_id: first_repair, ..
            },
            ScenarioActionV2::DummyToolCall {
                call_id: second_call, ..
            },
            ScenarioActionV2::DummyToolResult {
                call_id: second_result, ..
            },
        ] if first_call == first_repair
            && second_call == second_result
            && first_call != second_call
    )
}
