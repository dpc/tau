//! Strict, versioned scenarios understood by the deterministic fake provider.

use serde::{Deserialize, Serialize};
use tau_proto::{ProviderFailureKind, ToolCallId, ToolName};

/// Fully-qualified model published by every deterministic scenario.
pub const FAKE_MODEL_ID: &str = "fake/test";

/// Version-one deterministic provider scenario.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioV1 {
    /// Schema version. Version one is the only accepted value.
    pub version: u8,
    /// Stable diagnostic scenario name.
    pub name: String,
    /// Exact FIFO turns expected from the harness.
    pub turns: Vec<ScenarioTurnV1>,
}

/// Version-two scenario with independent exact correlation lanes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioV2 {
    /// Schema version. Must be two.
    pub version: u8,
    /// Stable diagnostic scenario name.
    pub name: String,
    /// Independently consumed lanes keyed by exact initial-prompt correlation
    /// id.
    pub lanes: Vec<ScenarioLaneV2>,
}

/// One independent provider lane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioLaneV2 {
    /// Exact `ctx_id` copied from the lane's initial UI submission. A one-lane
    /// public-PTY scenario may bind its first agent when the UI supplies no id.
    pub ctx_id: String,
    /// Exact ordered actions for this lane.
    pub actions: Vec<ScenarioActionV2>,
}

/// One bounded provider action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScenarioActionV2 {
    /// Complete normally with assistant text.
    Text {
        /// Exact latest user text.
        user_text: String,
        /// Complete assistant response.
        response: String,
    },
    /// Request the one allowlisted no-side-effect dummy tool.
    DummyToolCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
    },
    /// Accept the correlated successful dummy result and finish with text.
    DummyToolResult {
        /// Exact latest user text retained in the provider continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Complete assistant response after the tool result.
        response: String,
    },
    /// Complete with a typed terminal provider error.
    Error {
        /// Exact latest user text.
        user_text: String,
        /// Safe machine-readable failure classification.
        failure_kind: ProviderFailureKind,
        /// Bounded synthetic diagnostic.
        error: String,
    },
    /// Remain pending until an exact prompt cancellation, with a hard timeout.
    HoldUntilCancel {
        /// Exact latest user text.
        user_text: String,
        /// Maximum hold duration before a typed timeout error is emitted.
        timeout_ms: u64,
    },
    /// Deliberately disconnect the provider after accepting the prompt.
    Disconnect {
        /// Exact latest user text.
        user_text: String,
        /// Bounded synthetic disconnect reason.
        reason: String,
    },
    /// Wait for all named barrier participants, then complete each lane.
    BarrierText {
        /// Exact latest user text.
        user_text: String,
        /// Shared bounded barrier name.
        barrier: String,
        /// Exact participant count.
        participants: usize,
        /// Lane-local assistant response.
        response: String,
    },
}

impl ScenarioV2 {
    /// Creates a scenario from explicitly keyed lanes.
    #[must_use]
    pub fn new(name: impl Into<String>, lanes: Vec<ScenarioLaneV2>) -> Self {
        Self {
            version: 2,
            name: name.into(),
            lanes,
        }
    }
}

/// One expected prompt and its deterministic provider response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScenarioTurnV1 {
    /// Match a user prompt and finish with streamed assistant text.
    Text {
        /// Exact final user-authored text.
        user_text: String,
        /// Append-only assistant deltas.
        deltas: Vec<String>,
        /// Complete durable assistant response.
        response: String,
    },
    /// Match a user prompt and request the deterministic dummy tool.
    ToolCall {
        /// Exact final user-authored text.
        user_text: String,
        /// Exact visible tool name.
        tool_name: ToolName,
        /// Stable provider-authored call identifier.
        call_id: ToolCallId,
    },
    /// Match the tool-result continuation and finish the interaction.
    ToolResult {
        /// Call identifier that must bind the result to the prior request.
        call_id: ToolCallId,
        /// Complete final assistant response.
        response: String,
    },
}

impl ScenarioV1 {
    /// Creates the standard streaming-text acceptance scenario.
    #[must_use]
    pub fn text_v1(user_text: impl Into<String>, response: impl Into<String>) -> Self {
        let response = response.into();
        let split = response.len().min(response.len() / 2 + response.len() % 2);
        let split = response
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= split)
            .last()
            .unwrap_or(0);
        Self {
            version: 1,
            name: "text_v1".to_owned(),
            turns: vec![ScenarioTurnV1::Text {
                user_text: user_text.into(),
                deltas: vec![response[..split].to_owned(), response[split..].to_owned()],
                response,
            }],
        }
    }

    /// Creates the standard successful `restart_test_dummy` tool round.
    #[must_use]
    pub fn dummy_tool_round_v1(user_text: impl Into<String>) -> Self {
        Self {
            version: 1,
            name: "dummy_tool_round_v1".to_owned(),
            turns: vec![
                ScenarioTurnV1::ToolCall {
                    user_text: user_text.into(),
                    tool_name: ToolName::new(tau_ext_test_dummy::RESTART_TEST_DUMMY_TOOL_NAME),
                    call_id: "fake-call-1".into(),
                },
                ScenarioTurnV1::ToolResult {
                    call_id: "fake-call-1".into(),
                    response: "tool completed".to_owned(),
                },
            ],
        }
    }
}
