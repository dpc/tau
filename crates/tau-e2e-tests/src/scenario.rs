//! Strict, versioned scenarios understood by the deterministic fake provider.

use serde::{Deserialize, Serialize};
use tau_proto::{ProviderFailureKind, ToolCallId, ToolName};

/// Fully-qualified model published by every deterministic scenario.
pub const FAKE_MODEL_ID: &str = "fake/test";

/// Exact provider-owned syntax emitted by the canonical opaque-compaction
/// acceptance case.
pub const CANONICAL_OPAQUE_COMPACTION_JSON: &str =
    r#"{"type":"compaction","id":"cmp-e2e","encrypted_content":"opaque-e2e"}"#;

/// Version-one deterministic provider scenario.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioV1 {
    /// Schema version, fixed at zero by `GATE-no-backward-compatibility`.
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
    /// Schema version, fixed at zero by `GATE-no-backward-compatibility`.
    pub version: u8,
    /// Stable scenario identity used for diagnostics and narrowly allowlisted
    /// fixture capabilities such as public-PTY `ui-prompt-*` lane binding.
    pub name: String,
    /// Independently consumed lanes keyed by exact initial-prompt correlation
    /// id.
    pub lanes: Vec<ScenarioLaneV2>,
}

/// One independent provider lane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioLaneV2 {
    /// Exact `ctx_id` copied from the lane's initial UI submission. A named,
    /// one-lane public-PTY scenario may instead bind its first agent from a
    /// harness-minted `ui-prompt-*` id; a multi-lane session-restore scenario
    /// may bind only the exact harness-minted child by unique configured
    /// first-prompt text.
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
    /// Reach the output limit with one replay-safe full reasoning item and no
    /// assistant message or tool call.
    OutputLengthReasoning {
        /// Exact initial user text.
        user_text: String,
        /// Exact nonempty full reasoning text retained for one continuation.
        reasoning: String,
        /// Whether this response reports deterministic nonzero token usage.
        report_usage: bool,
    },
    /// Complete the sole output-length continuation after validating exact
    /// replay.
    OutputLengthContinuation {
        /// Exact initial user text retained from the source request.
        user_text: String,
        /// Exact full reasoning text retained from the source response.
        reasoning: String,
        /// Complete normal assistant response.
        response: String,
        /// Whether this response reports deterministic nonzero token usage.
        report_usage: bool,
    },
    /// Return the private typed narrative output consumed by the harness.
    StandaloneCompaction {
        /// Exact bounded narrative carried in the private output envelope.
        narrative: String,
    },
    /// Return one fixed canonical opaque provider compaction item.
    StandaloneOpaqueCompaction,
    /// Reject one ordinary inference with the sole canonical context-window
    /// failure that may authorize reactive compaction.
    ContextOverflow {
        /// Exact latest user text on the rejected ordinary inference.
        user_text: String,
        /// Earlier compactable user text that the reactive replacement must
        /// remove from its continuation context.
        removed_user_text: String,
        /// Assistant text that closes the pre-cut user round.
        removed_assistant_text: String,
        /// Safe machine-readable failure classification, fixed to
        /// `ContextWindowExceeded` by the closed grammar.
        failure_kind: ProviderFailureKind,
    },
    /// Return one fixed canonical opaque replacement for the reactive
    /// compaction transaction caused by the immediately preceding overflow.
    ReactiveOpaqueCompaction {
        /// Earlier user text that the replacement boundary must remove from its
        /// one following continuation.
        removed_user_text: String,
        /// Assistant text that closes the pre-cut user round.
        removed_assistant_text: String,
        /// Exact ordinary inference prompt retained after replacement.
        overflow_user_text: String,
    },
    /// Complete the one ordinary continuation after a reactive opaque
    /// replacement is visible to the provider.
    ReactiveCompactedOpaqueText {
        /// Earlier compacted user text that the replacement must remove from
        /// continuation context.
        removed_user_text: String,
        /// Assistant text that closes the pre-cut user round.
        removed_assistant_text: String,
        /// Exact ordinary inference prompt retained after replacement.
        overflow_user_text: String,
        /// Complete assistant response after recovery.
        response: String,
    },
    /// Return text only after the prior standalone replacement is visible in
    /// the next ordinary provider prompt.
    CompactedText {
        /// Exact latest user text.
        user_text: String,
        /// Exact composite checkpoint that must survive in replacement context.
        checkpoint: String,
        /// Earlier user text that the replacement must remove from context.
        removed_user_text: String,
        /// Complete assistant response.
        response: String,
    },
    /// Return text only after the exact canonical opaque replacement is visible
    /// in the next ordinary provider prompt.
    CompactedOpaqueText {
        /// Exact latest user text.
        user_text: String,
        /// Earlier user text that the replacement must remove from context.
        removed_user_text: String,
        /// Complete assistant response.
        response: String,
    },
    /// Complete a standalone compaction request with one terminal provider
    /// error.
    StandaloneCompactionError {
        /// Safe machine-readable failure classification.
        failure_kind: ProviderFailureKind,
        /// Bounded synthetic diagnostic.
        error: String,
    },
    /// Remain in a standalone compaction request until exact cancellation.
    StandaloneCompactionHold {
        /// Maximum hold duration before a typed timeout error is emitted.
        timeout_ms: u64,
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
    /// Request the closed dummy mode that returns one fixed typed image.
    TypedImageToolCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
    },
    /// Accept the exact typed image from the immediately preceding tool call.
    TypedImageToolResult {
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Complete assistant response after the live image result.
        response: String,
    },
    /// Complete only when cold replay retains the exact prior typed image once.
    TypedImageReplay {
        /// Exact later user text.
        user_text: String,
        /// Exact provider-authored call identity for the retained image.
        call_id: ToolCallId,
        /// Complete assistant response after replay validation.
        response: String,
    },
    /// Accept the harness's exact synthetic interrupted-tool error and
    /// terminalize the resumed provider continuation.
    DummyToolRepair {
        /// Exact latest user text retained in the provider continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact harness-generated restart/possible-side-effect diagnostic.
        diagnostic: String,
        /// Complete assistant response after accepting the repair.
        response: String,
    },
    /// Request the production harness-owned `message` tool for the sole
    /// test-driver-created worker in this closed scenario.
    MessageCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact body delivered to the worker.
        message: String,
    },
    /// Accept the sender's correlated compact message-tool result.
    MessageSenderResult {
        /// Exact provider-authored message-call identity.
        call_id: ToolCallId,
        /// Exact body delivered to the worker.
        message: String,
        /// Complete assistant response.
        response: String,
    },
    /// Accept the recipient's sole canonical inbound message wrapper.
    MessageInbound {
        /// Exact provider-authored message-call identity shared with the
        /// sender.
        call_id: ToolCallId,
        /// Exact body delivered to the worker.
        message: String,
        /// Complete worker response.
        response: String,
    },
    /// Accept one exact inbound wrapper after one known held user prompt.
    MessageInboundAfterHeld {
        /// Exact provider-authored message-call identity.
        call_id: ToolCallId,
        /// Exact body delivered to the worker.
        message: String,
        /// Exact held user prompt preceding the wrapper.
        held_user_text: String,
        /// Complete worker response.
        response: String,
    },
    /// Invoke the configured test-only raw-message tool for the message-call
    /// recipient selected earlier in this lane.
    ProviderContextRawMessageCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact activating raw delivered-message body.
        raw_text: String,
    },
    /// Accept the raw-message tool result and complete the sender turn.
    ProviderContextRawMessageResult {
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact activating raw delivered-message body.
        raw_text: String,
        /// Complete sender response.
        response: String,
    },
    /// Accept one typed inbound wrapper and one raw delivered-message
    /// projection after a known held user prompt.
    MessageAndRawInboundAfterHeld {
        /// Exact provider-authored message-call identity.
        call_id: ToolCallId,
        /// Exact typed body delivered through the production message tool.
        message: String,
        /// Exact activating raw delivered-message body.
        raw_text: String,
        /// Exact held user prompt preceding both deferred inputs.
        held_user_text: String,
        /// Complete worker response.
        response: String,
    },
    /// Accept typed and raw deferred inputs after one complete parallel dummy
    /// tool aggregate released from a provider barrier.
    MessageAndRawInboundAfterParallelTools {
        /// Exact provider-authored message-call identity.
        call_id: ToolCallId,
        /// Exact typed body delivered through the production message tool.
        message: String,
        /// Exact activating raw delivered-message body.
        raw_text: String,
        /// Exact held user prompt preceding the tool-bearing response.
        held_user_text: String,
        /// Exact parallel dummy call identities.
        tool_call_ids: Vec<ToolCallId>,
        /// Complete worker response.
        response: String,
    },
    /// Request the production harness-owned `agent_start` tool.
    AgentStartCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact self-contained child prompt.
        prompt: String,
        /// Exact child role.
        role: String,
    },
    /// Accept the correlated immediate `agent_start` result and finish with
    /// text.
    AgentStartResult {
        /// Exact latest user text retained in the provider continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Complete assistant response after the tool result.
        response: String,
    },
    /// Enable the production harness-owned `agent_watch` tool for the child
    /// learned from the validated `agent_start` result.
    AgentWatchCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
    },
    /// Accept the correlated successful `agent_watch` result and finish with
    /// text.
    AgentWatchResult {
        /// Exact latest user text retained in the provider continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact sanitized successful-result shape.
        expectation: AgentWatchResultExpectationV2,
        /// Complete assistant response after the tool result.
        response: String,
    },
    /// Match one complete model-visible batch of automatic watch notifications.
    WatchNotifications {
        /// Ordered logical batch consumed one notification per provider prompt.
        notifications: Vec<WatchNotificationV2>,
        /// Complete assistant response after consuming the batch.
        response: String,
    },
    /// Match the two independent prompt/response and running/idle watch chains
    /// without imposing a cross-stream total order.
    WatchNotificationChains {
        /// Exact direct user prompt reported for the watched child.
        prompt: String,
        /// Exact final response reported for the watched child.
        response: String,
        /// Complete assistant response after all four notifications.
        completion: String,
    },
    /// Request the closed Gate 2 relative `workdir("project")` operation.
    CoreShellWorkdirCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
    },
    /// Accept the successful workdir result before dependent filesystem work.
    CoreShellWorkdirResult {
        /// Exact latest user text retained in the continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Exact identity for the dependent relative edit.
        edit_call_id: ToolCallId,
        /// Per-run nonce embedded only in file contents.
        nonce: String,
    },
    /// Accept creation and terminate Boot A with an exact marker.
    CoreShellCreateResult {
        /// Exact latest user text retained in the continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Complete assistant marker.
        response: String,
    },
    /// Replace the restored sentinel through the same fixed relative path.
    CoreShellResumeEditCall {
        /// Exact latest user text.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Per-run nonce that must already be present in restored context.
        nonce: String,
    },
    /// Accept the resumed edit and terminate Boot B with an exact marker.
    CoreShellResumeEditResult {
        /// Exact latest user text retained in the continuation.
        user_text: String,
        /// Exact provider-authored call identity.
        call_id: ToolCallId,
        /// Complete assistant marker.
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
    ///
    /// After its wait worker starts, the fake emits exactly one
    /// prompt-correlated `hold_ready` semantic trace record and info-level
    /// notice. These are live fixture readiness, not provider acknowledgement.
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
    /// Wait at a named barrier, then return one provider response containing
    /// parallel deterministic dummy-tool calls.
    BarrierParallelDummyTools {
        /// Exact latest user text.
        user_text: String,
        /// Fixture-global barrier name.
        barrier: String,
        /// Exact number of participants required to release the barrier.
        participants: usize,
        /// Exact parallel dummy-tool call identities.
        tool_call_ids: Vec<ToolCallId>,
    },
}

/// One closed sanitized result expected from `agent_watch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWatchResultExpectationV2 {
    /// The watch was enabled without a current provider-work status.
    Enabled,
    /// The watch was enabled while the target had uncertain unknown-category
    /// provider dispatch.
    DispatchUncertainUnknown,
}

/// One closed automatic watch notification expected by a V2 action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WatchNotificationV2 {
    /// The watched child produced its final response.
    Response {
        /// Exact unescaped child response content.
        content: String,
    },
    /// The watched child accepted a direct user prompt.
    Prompt {
        /// Exact unescaped child prompt content.
        content: String,
    },
}

impl ScenarioV2 {
    /// Creates a scenario from explicitly keyed lanes.
    #[must_use]
    pub fn new(name: impl Into<String>, lanes: Vec<ScenarioLaneV2>) -> Self {
        Self {
            version: 0,
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
    /// Request Working status and substantive dummy work in one parallel batch.
    StatusPolicyToolCall {
        /// Exact final user-authored text.
        user_text: String,
        /// Exact provider output order.
        order: StatusToolOrder,
        /// Whether the initial Working call is accepted or rejected.
        initial_status: InitialStatusOutcome,
        /// Phase used to release the later Working-final challenge.
        terminal_phase: StatusTerminalPhase,
    },
    /// Accept both successful parallel results and request more work while
    /// Working.
    StatusPolicyToolResult {
        /// Whether the initial Working call is accepted or rejected.
        initial_status: InitialStatusOutcome,
        /// Phase used to release the Working-final challenge.
        terminal_phase: StatusTerminalPhase,
    },
    /// Accept later substantive work without a repeated start reminder, then
    /// attempt a final while Working.
    WorkingFollowupToolResult {
        /// Whether this continuation carries follow-up work or recovery status.
        initial_status: InitialStatusOutcome,
        /// Phase used to release the Working-final challenge.
        terminal_phase: StatusTerminalPhase,
    },
    /// Match the Working-final challenge and request its terminal status.
    WorkingFinalStatusCall {
        /// Phase used to release the Working-final challenge.
        terminal_phase: StatusTerminalPhase,
    },
    /// Accept the terminal status result and complete normally.
    TerminalStatusResult {
        /// Exact accepted terminal phase.
        terminal_phase: StatusTerminalPhase,
        /// Complete final assistant response.
        response: String,
    },
}

/// Closed provider order for status plus substantive work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusToolOrder {
    /// Emit Working status before substantive work.
    StatusFirst,
    /// Emit substantive work before Working status.
    WorkFirst,
}

/// Closed initial status-call outcome for policy coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialStatusOutcome {
    /// A valid accepted Working report.
    AcceptedWorking,
    /// A well-formed call whose state value is rejected.
    Rejected,
}

/// Closed phase that permits a Working outer turn to finish.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusTerminalPhase {
    /// Finish with Done.
    Done,
    /// Finish with Blocked.
    Blocked,
}

impl StatusTerminalPhase {
    /// Return the model-visible status argument.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
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
            version: 0,
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
            version: 0,
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

    /// Creates the complete current-status policy path: parallel Working plus
    /// work, challenged final, terminal transition, and accepted final.
    #[must_use]
    pub fn status_policy_round_v1(
        user_text: impl Into<String>,
        order: StatusToolOrder,
        initial_status: InitialStatusOutcome,
        terminal_phase: StatusTerminalPhase,
    ) -> Self {
        Self {
            version: 0,
            name: "status_policy_round_v1".to_owned(),
            turns: vec![
                ScenarioTurnV1::StatusPolicyToolCall {
                    user_text: user_text.into(),
                    order,
                    initial_status,
                    terminal_phase,
                },
                ScenarioTurnV1::StatusPolicyToolResult {
                    initial_status,
                    terminal_phase,
                },
                ScenarioTurnV1::WorkingFollowupToolResult {
                    initial_status,
                    terminal_phase,
                },
                ScenarioTurnV1::WorkingFinalStatusCall { terminal_phase },
                ScenarioTurnV1::TerminalStatusResult {
                    terminal_phase,
                    response: "status policy completed".to_owned(),
                },
            ],
        }
    }

    /// Return whether this scenario requires status and parallel tool calls.
    #[must_use]
    pub fn uses_status_policy(&self) -> bool {
        self.turns
            .iter()
            .any(|turn| matches!(turn, ScenarioTurnV1::StatusPolicyToolCall { .. }))
    }
}
