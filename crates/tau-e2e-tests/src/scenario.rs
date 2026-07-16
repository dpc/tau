//! Strict, versioned scenarios understood by the deterministic fake provider.

use serde::{Deserialize, Serialize};
use tau_proto::{ToolCallId, ToolName};

/// Fully-qualified model published by every phase-one scenario.
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
