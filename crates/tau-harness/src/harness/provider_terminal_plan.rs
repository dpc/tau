//! Short-lived classification and eager execution state for provider terminals.

use tau_proto::ConnectionId;

/// Exhaustive classification at each incrementally typed provider-terminal
/// family boundary.
pub(crate) enum ProviderTerminalPlan {
    /// The terminal settles one standalone-compaction transaction.
    StandaloneCompaction(StandaloneCompactionTerminalPlan),
    /// The terminal is an eligible ordinary-inference context rejection.
    ReactiveContextRecovery(Box<ReactiveContextRecoveryPlan>),
    /// The terminal is governed by the agent's final-status contract.
    FinalStatusGated(FinalStatusGatedPlan),
    /// The terminal is commit-gated by an automatic compaction decision or a
    /// pending side-conversation message wake.
    AutomaticCompactionOrPendingMessageWake(AutomaticCompactionOrPendingMessageWakePlan),
    /// The terminal is a committed source for one output-length continuation.
    OutputLengthContinuationSource(OutputLengthContinuationSourcePlan),
    /// The terminal settles one reserved output-length continuation successor.
    OutputLengthContinuationTerminal(OutputLengthContinuationTerminalPlan),
    /// The terminal completes one extension-originated side conversation.
    SideConversation(SideConversationTerminalPlan),
    /// The terminal belongs to another provider-terminal family.
    Other,
}

/// Exact terminal decision for one standalone-compaction provider response.
pub(crate) enum StandaloneCompactionTerminalPlan {
    /// The provider returned a structurally valid replacement window.
    Accepted(tau_proto::ValidatedCompactionWindow),
    /// The provider terminal cannot commit a replacement window.
    Rejected(StandaloneCompactionRejection),
}

/// The only locally classified reasons a standalone provider terminal can fail.
#[derive(Clone, Copy)]
pub(crate) enum StandaloneCompactionRejection {
    /// The provider explicitly reported a terminal failure.
    ProviderError,
    /// Bounded retries exhausted with reliable no-semantic-output evidence.
    TransientCompactionExhausted,
    /// A completed private local summary contained no usable narrative.
    LocalSummaryUnusable,
    /// The provider canonically rejected an output-free request for context
    /// size.
    ContextWindowExceeded,
    /// The provider rejected the selected encoded message size without output.
    MessageTooLarge,
    /// The provider did not report a completed terminal turn.
    InvalidStop,
    /// The provider exhausted its output-token budget.
    OutputLengthExceeded,
    /// The provider did not return an acceptable replacement window.
    InvalidWindow,
}

impl StandaloneCompactionRejection {
    /// Convert this local classification to the durable transaction failure
    /// reason.
    pub(super) fn durable_reason(self) -> tau_proto::StandaloneCompactionFailureReason {
        match self {
            Self::ProviderError => tau_proto::StandaloneCompactionFailureReason::ProviderError,
            Self::TransientCompactionExhausted => {
                tau_proto::StandaloneCompactionFailureReason::TransientCompactionExhausted
            }
            Self::LocalSummaryUnusable => {
                tau_proto::StandaloneCompactionFailureReason::LocalSummaryUnusable
            }
            Self::ContextWindowExceeded => {
                tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
            }
            Self::MessageTooLarge => tau_proto::StandaloneCompactionFailureReason::MessageTooLarge,
            Self::OutputLengthExceeded => {
                tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded
            }
            Self::InvalidStop | Self::InvalidWindow => {
                tau_proto::StandaloneCompactionFailureReason::InvalidWindow
            }
        }
    }
}

/// Exact eager reducer decision for one side-conversation terminal.
pub(crate) struct SideConversationTerminalPlan {
    /// Extension that receives the completed side-conversation result.
    pub(super) name: tau_proto::ExtensionName,
    /// Extension-owned query correlation.
    pub(super) query_id: String,
}

/// Complete semantic input for classifying one possible side-conversation
/// terminal.
pub(crate) struct SideConversationTerminalClassification<'a> {
    /// Conversation whose active originator owns classification authority.
    pub(super) cid: &'a tau_proto::AgentId,
    /// Fully normalized canonical provider terminal.
    pub(super) response: &'a tau_proto::ProviderResponseFinished,
    /// Whether the terminal requests tool calls after stop reconciliation.
    pub(super) requested_tool_calls: bool,
    /// Whether this is an extension query without tool authority.
    pub(super) is_non_tool_ext_query: bool,
}

/// Exact post-commit reducer selected for a reserved output-length successor.
pub(crate) struct OutputLengthContinuationTerminalPlan {
    /// Typed reducer retained by the existing gated-final publication envelope.
    pub(super) reducer: super::gated_final::CommittedGatedFinalReducer,
}

/// Complete semantic input for classifying one reserved output-length
/// successor.
pub(crate) struct OutputLengthContinuationTerminalClassification<'a> {
    /// Whether final-status challenge authority already owns this terminal.
    pub(super) final_status_challenged: bool,
    /// Exact fully prepared provider response.
    pub(super) response: &'a tau_proto::ProviderResponseFinished,
}

/// Exact post-commit reducer selected for an output-length continuation source.
pub(crate) struct OutputLengthContinuationSourcePlan {
    /// Typed reducer retained across publication and interception.
    pub(super) reducer:
        super::output_length_continuation_reducer::CommittedOutputLengthContinuation,
}

/// Complete semantic input for classifying one output-length continuation
/// source.
pub(crate) struct OutputLengthContinuationSourceClassification<'a> {
    /// Exact fully prepared provider response.
    pub(super) response: &'a tau_proto::ProviderResponseFinished,
    /// Display-only assistant text retained for common terminal handling.
    pub(super) assistant_text: Option<&'a str>,
}

/// Exact authority needed to execute one reactive context-recovery terminal.
pub(crate) struct ReactiveContextRecoveryPlan {
    /// Durable failed inference owner that the transaction must claim.
    pub(super) checkpoint: tau_proto::AgentInferenceDispatchStarted,
    /// Provider connection retained only for live attribution.
    pub(super) source: Option<ConnectionId>,
}

/// Exact final-status decision retained through eager terminal preparation.
pub(crate) enum FinalStatusGatedPlan {
    /// Queue the captured status reminder after the response commits.
    Challenge {
        /// Validated unresolved status captured with the response.
        challenge: crate::agent::FinalStatusChallenge,
    },
    /// Apply ordinary committed terminal projection after the response commits.
    Accept,
}

/// Exact deferred tool effect for an automatic-compaction-owned or pending
/// message-wake terminal.
pub(crate) struct AutomaticCompactionOrPendingMessageWakePlan {
    /// Tool effect withheld until the response commits.
    pub(super) tool_effect: super::CommittedOutputLengthToolEffect,
}

/// Complete semantic input for classifying an automatic-compaction-owned or
/// pending-message-wake terminal.
pub(crate) struct AutomaticCompactionOrPendingMessageWakeClassification {
    /// Whether final-status ownership already claimed this terminal.
    pub(super) final_status_owned: bool,
    /// Whether the eager automatic-compaction policy owns this terminal.
    pub(super) automatic_compaction_owned: bool,
    /// Whether a pending side-conversation message must wake after this
    /// terminal.
    pub(super) continues_for_pending_message_wake: bool,
    /// Exact deferred tool effect, including its normalized call payload.
    pub(super) tool_effect: super::CommittedOutputLengthToolEffect,
}
