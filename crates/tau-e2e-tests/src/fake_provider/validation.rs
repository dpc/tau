//! Closed grammar and resource validation for fake-provider scenarios.

use super::*;

pub(super) fn validate_v1(scenario: &ScenarioV1) -> ClientResult<()> {
    if scenario.version != 1 {
        return Err(ClientError::handler("ScenarioV1 version must be 1"));
    }
    if scenario.turns.is_empty() || scenario.turns.len() > MAX_TURNS {
        return Err(ClientError::handler("scenario must contain 1..=8 turns"));
    }
    let total_text = serde_json::to_vec(scenario)
        .map_err(|error| ClientError::handler(error.to_string()))?
        .len();
    if total_text > MAX_SCENARIO_BYTES {
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
                "scenario grammar must be one text turn or one matching tool call/result pair",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_v2(scenario: &ScenarioV2) -> ClientResult<()> {
    if scenario.version != 2 {
        return Err(ClientError::handler("ScenarioV2 version must be 2"));
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
    let mut ids = std::collections::HashSet::new();
    let mut barriers: HashMap<&str, (usize, std::collections::HashSet<&str>)> = HashMap::new();
    for lane in &scenario.lanes {
        if lane.ctx_id.is_empty() || lane.ctx_id.len() > 64 || !ids.insert(lane.ctx_id.as_str()) {
            return Err(ClientError::handler(
                "lane ctx_id must be unique and contain 1..=64 bytes",
            ));
        }
        if lane.actions.is_empty() || lane.actions.len() > MAX_TURNS {
            return Err(ClientError::handler("lane must contain 1..=8 actions"));
        }
        for action in &lane.actions {
            match action {
                ScenarioActionV2::HoldUntilCancel { timeout_ms, .. }
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
                        .or_insert_with(|| (*participants, std::collections::HashSet::new()));
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
                    if error.is_empty() || error.len() > 256 =>
                {
                    return Err(ClientError::handler(
                        "synthetic diagnostic must contain 1..=256 bytes",
                    ));
                }
                _ => {}
            }
        }
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
