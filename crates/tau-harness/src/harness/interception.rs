//! Event-emission interception subsystem.
//!
//! Owns the [`InterceptorRegistry`] (exact + prefix selectors keyed by
//! full `(priority, component_name, connection_id)` registration order), the
//! [`PendingIntercept`] / [`DeferredPublish`]
//! queue state, and the methods that drive the interception chain.
//!
//! Flow: a publish enters via [`Harness::enqueue_publish`]. If no intercept
//! is in flight, [`Harness::dispatch_publish_step`] consults the registry —
//! either dispatching an `InterceptRequest` and parking the publish in
//! `pending_intercept`, or falling through to `commit_event`. While a
//! publish is parked, further publishes queue onto `deferred_publishes` so
//! the log order matches the original publish order.
//!
//! Replies and disconnects feed back through
//! [`Harness::handle_intercept_reply`]
//! / [`Harness::fail_pending_intercept_for_disconnect`], which advance the
//! chain and then drain the deferred queue.

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};

use tau_proto::{
    AgentId, Event, EventName, EventSelector, ExtensionName, HarnessOutputMessage, InterceptAction,
    InterceptReply, InterceptRequest, InterceptionPriority,
};

use crate::harness::InferenceDispatchSelectionError;
use crate::{agent as path_crate_agent, extension as path_crate_extension};

/// One harness-owned full prompt carried from compact-fact admission through
/// its one-shot post-commit delivery.
#[derive(Clone)]
pub(crate) struct PromptDispatchContinuation {
    /// Exact compact fact that owns this continuation.
    pub(crate) started: tau_proto::AgentPromptStarted,
    /// Full transient provider work envelope.
    pub(crate) prompt: std::sync::Arc<tau_proto::AgentPromptCreated>,
    /// Provider route resolved from the captured model at admission.
    pub(crate) provider_connection_id: tau_proto::ConnectionId,
    /// Loaded runtime instance that materialized the request.
    pub(crate) runtime_incarnation: u64,
}

/// Phase of an envelope-bound prompt continuation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum PromptDispatchPhase {
    /// The compact materialization fact has not committed yet.
    Materialization,
    /// The compact fact committed and the full transient envelope may deliver.
    Delivery,
}

/// Exactly one action that may become runnable after a synchronized append.
#[derive(Clone)]
pub(crate) enum PostCommitContinuation {
    /// Agent publication completion.
    AgentPublish(Box<AgentPublishCompletion>),
    /// Compact prompt fact awaiting its authoritative append.
    PromptMaterialization(PromptDispatchContinuation),
    /// Full transient prompt awaiting directed provider delivery.
    PromptDelivery(PromptDispatchContinuation),
    /// Unexpected watched-agent retirement awaiting one exact lifecycle append.
    WatchRetirement(WatchRetirementCompletion),
}

/// Correlation for one watcher delivery in a pending topology-retirement
/// barrier.
#[derive(Clone)]
pub(crate) struct WatchRetirementCompletion {
    /// Endpoint whose watch topology is retiring.
    pub(crate) watched_agent_id: tau_proto::AgentId,
    /// Surviving watcher receiving the lifecycle fact.
    pub(crate) watcher_id: tau_proto::AgentId,
    /// Exact lifecycle message identity.
    pub(crate) message_id: tau_proto::AgentMessageId,
}

use crate::harness::Harness;
use crate::harness::extensions::ExtensionFrameAdmission;

/// Semantic source and durability phase of deferred activation work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredActivationObligation {
    /// Ordinary work waiting for the publish chain to become idle.
    OrdinaryPublishIdle,
    /// Reserved output-length work waiting for its steer to commit.
    OutputLengthPublishIdle,
    /// One exact durable activation watermark.
    Committed,
}

impl DeferredActivationObligation {
    /// Whether this obligation has a durable activating occurrence.
    pub(crate) const fn is_committed(self) -> bool {
        matches!(self, Self::Committed)
    }
}

/// One publish-idle dispatch or one distinct committed activation obligation.
#[derive(Clone)]
pub(crate) struct DeferredPromptDispatch {
    /// Agent whose inference remains pending.
    pub(crate) cid: AgentId,
    /// Closed cut immediately before this committed activation.
    pub(crate) activation_cut: Option<tau_proto::AgentHead>,
    /// Branch watermark containing this committed activation.
    ///
    /// The obligation is runnable only while this watermark is an ancestor of
    /// the selected head. `None` means the owning publish has not committed
    /// yet.
    pub(crate) activation_through: Option<tau_proto::AgentHead>,
    /// Durable activating occurrence whose node may not exist yet.
    pub(crate) activation_source_seq: Option<tau_core::PersistedAgentEventSeq>,
    /// Semantic source and durability phase of this obligation.
    pub(crate) obligation: DeferredActivationObligation,
}

/// Snapshot of a publish that's currently waiting on an interceptor's
/// reply. The harness stops draining further publishes while one of
/// these is alive so the persisted log order matches publish order.
pub(crate) struct PendingIntercept {
    /// Connection that owes us an [`InterceptReply`].
    pub(crate) conn_id: tau_proto::ConnectionId,
    /// Event sent in the [`InterceptRequest`]. Returned to the chain
    /// if the reply is `Pass(None)`, replaced if `Pass(Some(_))`.
    pub(crate) event: Event,
    /// Whether the original publisher requested semantic persistence.
    /// Carried so the eventual commit honours the call site's intent.
    pub(crate) persist: bool,
    /// Immutable source envelope captured when publication entered the generic
    /// queue.
    source: PublicationSource,
    /// If `true`, an interceptor returning `Drop` is overridden:
    /// `tracing::warn!` and continue with the original event.
    pub(crate) must_pass: bool,
    /// Agent that originated this publish, if any. When the
    /// event eventually commits, the harness syncs this
    /// conversation's `head` to the post-fold `tree.head()`. Set
    /// only by `publish_for_agent*`; `publish_event` leaves
    /// it `None`.
    pub(crate) sync_head_for: Option<ConversationHeadSync>,
    /// Cursor for the next interceptor lookup *after* this reply
    /// resolves. Set to the registration we just dispatched to, so
    /// the chain advances strictly past it.
    pub(crate) cursor: InterceptorCursor,
}

impl PendingIntercept {
    /// Return immutable original-route privacy captured before any interceptor
    /// replacement in this publication chain.
    pub(crate) fn original_shell_report_targets_ephemeral(&self) -> bool {
        self.source
            .peer_context
            .extension
            .as_ref()
            .is_some_and(|extension| extension.shell_report_targets_ephemeral)
    }
}

/// Immutable authenticated configured-extension publication identity.
#[derive(Clone)]
pub(crate) struct AuthenticatedExtensionPublication {
    /// Stable configured extension publisher.
    pub(crate) publisher: tau_proto::ExtensionName,
    /// Configured extension connection that authored this publish.
    pub(crate) source: tau_proto::ConnectionId,
    /// Authenticated configured extension kind captured at admission.
    pub(crate) kind: tau_proto::ClientKind,
    /// Optional peer authorities captured at admission.
    pub(crate) capabilities: std::collections::BTreeSet<tau_proto::PeerCapability>,
    /// Stable configured instance identity captured at admission.
    pub(crate) instance_id: tau_proto::ExtensionInstanceId,
    /// Session binding current when the peer frame originally arrived.
    pub(super) admission: ExtensionFrameAdmission,
    /// Whether the original shell-report route targeted an ephemeral agent.
    ///
    /// This immutable bit keeps debug suppression safe when interception
    /// replaces the report's peer-controlled route id.
    pub(crate) shell_report_targets_ephemeral: bool,
    /// Activation-stage reservation made before interception.
    pub(crate) activation_reservation: Option<ActivationReservation>,
}

/// Pre-activation quota reservation for one intercepted declaration.
#[derive(Clone, Copy)]
pub(crate) struct ActivationReservation {
    /// Encoded input-envelope bytes charged before interception.
    pub(crate) encoded_bytes: usize,
    /// Original persistence metadata retained across same-name replacement.
    pub(crate) persist: bool,
    /// Declaration family whose pre-activation pending count owns this charge.
    pub(crate) declaration_family: ActivationDeclarationFamily,
}

/// Pre-activation declaration family bound to one quota reservation.
#[derive(Clone, Copy)]
pub(crate) enum ActivationDeclarationFamily {
    /// Provider model replacement declaration.
    ProviderModels,
    /// Tool registration or unregistration declaration.
    ToolLifecycle,
    /// Complete Action schema snapshot.
    ActionSchema,
    /// Extension-level prompt-fragment declaration.
    PromptFragment,
    /// Session-provider registration, skill, or AGENTS.md declaration.
    SessionDiscovery,
    /// Per-agent context registration or value declaration.
    AgentContext,
}

/// Immutable authenticated metadata carried beside one generic peer publish.
#[derive(Clone, Default)]
pub(crate) struct PeerPublicationContext {
    /// Configured extension identity, when this publish came from one.
    pub(crate) extension: Option<AuthenticatedExtensionPublication>,
}

/// Source envelope retained through generic interception and commit.
struct PublicationSource {
    /// Original connection for persistence and bus delivery metadata.
    connection_id: Option<tau_proto::ConnectionId>,
    /// Immutable authenticated identity captured at admission.
    peer_context: PeerPublicationContext,
}

/// A publish that arrived while another publish was in interception limbo.
pub(crate) struct DeferredPublish {
    /// Immutable source envelope captured at queue admission.
    source: PublicationSource,
    /// Event waiting behind the currently intercepted publish.
    event: Event,
    /// Whether ordinary eligible semantic persistence was requested.
    persist: bool,
    /// Whether an interceptor drop must preserve the original event.
    must_pass: bool,
    /// Conversation cursor synchronized after an ordinary transcript fold.
    sync_head_for: Option<ConversationHeadSync>,
}

impl DeferredPublish {
    /// Borrow the event independent of its eventual commit path.
    pub(crate) fn event(&self) -> &Event {
        &self.event
    }
}

/// Carried on a publish so that, once the event commits and the
/// `AgentTree` fold advances `tree.head()`, the harness can sync
/// the originating conversation's cached `head` to the new node and
/// still attribute conversation-scoped events to the owning agent even
/// if call-level tracking has been cleared while the publish was
/// deferred.
/// Replaces the old "publish then read `tree.head()`" idiom which
/// breaks when an interceptor parks the publish.
#[derive(Clone)]
pub(crate) struct ConversationHeadSync {
    /// Runtime conversation whose durable branch advances on commit.
    pub(crate) cid: AgentId,
    /// Durable agent identity retained if the runtime conversation disappears.
    pub(crate) agent_id: Option<AgentId>,
    /// Harness session generation that owned this publication at enqueue time.
    pub(crate) session_generation: u64,
    /// Exact durable fold parent override for inference-owned completion.
    pub(crate) fold_parent: Option<tau_core::AgentEventParent>,
    /// Suppress the ordinary activation obligation because a stronger
    /// envelope-bound continuation owns this publication batch.
    pub(crate) suppress_activation_dispatch: bool,
    /// Mutually exclusive action that becomes runnable only after this append.
    pub(crate) continuation: Option<PostCommitContinuation>,
    /// Whether successful commit notifies this agent's watchers using the exact
    /// post-interception event text.
    pub(crate) notify_watchers: bool,
}

impl ConversationHeadSync {
    /// Returns the exclusive agent-publication completion, when present.
    pub(crate) fn completion(&self) -> Option<&AgentPublishCompletion> {
        match self.continuation.as_ref() {
            Some(PostCommitContinuation::AgentPublish(completion)) => Some(completion.as_ref()),
            _ => None,
        }
    }

    /// Returns the prompt continuation for either exclusive prompt phase.
    pub(crate) fn prompt_dispatch(&self) -> Option<&PromptDispatchContinuation> {
        match self.continuation.as_ref() {
            Some(
                PostCommitContinuation::PromptMaterialization(continuation)
                | PostCommitContinuation::PromptDelivery(continuation),
            ) => Some(continuation),
            _ => None,
        }
    }

    /// Returns one unexpected watch-retirement delivery correlation.
    pub(crate) fn watch_retirement(&self) -> Option<&WatchRetirementCompletion> {
        match self.continuation.as_ref() {
            Some(PostCommitContinuation::WatchRetirement(completion)) => Some(completion),
            _ => None,
        }
    }

    /// Returns which exclusive prompt continuation phase is present.
    pub(crate) fn prompt_dispatch_phase(&self) -> Option<PromptDispatchPhase> {
        match self.continuation.as_ref() {
            Some(PostCommitContinuation::PromptMaterialization(_)) => {
                Some(PromptDispatchPhase::Materialization)
            }
            Some(PostCommitContinuation::PromptDelivery(_)) => Some(PromptDispatchPhase::Delivery),
            _ => None,
        }
    }
}

/// Harness-owned continuation bound to one exact agent publication envelope.
#[derive(Clone)]
pub(crate) enum DormantOutputLengthCompletion {
    /// Derive the synthetic owner after the dormant steer commits.
    Steer {
        /// Exact planned-response parent retained across append rejection.
        fold_parent: tau_core::AgentEventParent,
    },
    /// Retire the exact dormant activation after its synthetic owner commits.
    Owner {
        /// Exact dormant steer parent retained across append rejection.
        fold_parent: tau_core::AgentEventParent,
        /// Exact closed provider cut captured for the dormant activation.
        activation_cut: tau_proto::AgentHead,
        /// Exact committed steer watermark consumed by the synthetic owner.
        steer: tau_proto::AgentHead,
    },
    /// Derive the owed finish after the synthetic terminal commits.
    Terminal {
        /// Exact synthetic-owner parent retained across append rejection.
        fold_parent: tau_core::AgentEventParent,
    },
    /// Settle runtime and publish the branch-failure notice after owed finish.
    Finish {
        /// Exact dormant terminal parent retained across append rejection.
        fold_parent: tau_core::AgentEventParent,
    },
}

impl DormantOutputLengthCompletion {
    /// Exact explicit parent for this repair fact.
    pub(crate) const fn fold_parent(&self) -> tau_core::AgentEventParent {
        match self {
            Self::Steer { fold_parent }
            | Self::Owner { fold_parent, .. }
            | Self::Terminal { fold_parent }
            | Self::Finish { fold_parent } => *fold_parent,
        }
    }
}

/// Harness-owned continuation bound to one exact agent publication envelope.
#[derive(Clone)]
pub(crate) enum AgentPublishCompletion {
    /// Report a correlated initial-prompt failure if its canonical submission
    /// cannot commit.
    InitialPromptSubmission {
        /// Accepted initial-prompt identity.
        correlation: crate::agent::InitialPromptCorrelation,
    },
    /// Apply one gated-final disposition only after its durable append
    /// commits.
    GatedFinal {
        /// Selected transcript head that owned the provider response.
        batch_parent: tau_proto::AgentHead,
        /// Exact post-commit terminal or continuation behavior.
        disposition: super::gated_final::GatedFinalDisposition,
        /// Exact interceptor-approved event retained after append rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Start the single output-length successor only after the planned source
    /// response has committed on its owning branch.
    OutputLengthContinuation {
        /// Selected transcript head that owns the source response.
        batch_parent: tau_proto::AgentHead,
        /// Exact planned response retained for post-commit runtime completion.
        response: Box<tau_proto::ProviderResponseFinished>,
        /// Display-only assistant text retained for common terminal handling.
        assistant_text: Option<String>,
        /// Exact interceptor-approved event retained after append rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Retain the exact planned continuation steer until its plan branch
    /// accepts the durable append.
    OutputLengthSteer {
        /// Exact planned-response node that this steer must extend.
        batch_parent: tau_proto::AgentHead,
        /// Exact interceptor-approved steer retained after append rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Publish one reserved successor failure only after its synthetic
    /// prompt-start authority commits.
    OutputLengthPreDeliveryFailure {
        /// Exact owner branch on which prompt-start must commit.
        batch_parent: tau_proto::AgentHead,
        /// Harness-synthesized terminal retained across append rejection.
        response: Box<tau_proto::ProviderResponseFinished>,
        /// Exact interceptor-approved prompt-start retained after rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Advance one explicit-parent dormant output-length repair only after its
    /// exact durable append commits.
    OutputLengthDormantRepair {
        /// Semantically valid next action after commit.
        step: DormantOutputLengthCompletion,
        /// Exact interceptor-approved repair fact retained after rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Start reactive compaction only after its exact rejection response
    /// commits.
    ReactiveContextRecovery {
        /// Durable failed inference owner that the transaction must claim.
        checkpoint: tau_proto::AgentInferenceDispatchStarted,
        /// Provider connection retained only for live attribution.
        source: Option<tau_proto::ConnectionId>,
        /// Exact interceptor-approved rejection retained after append
        /// rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Retain the exact reactive transaction claim until its start commits.
    ReactiveContextRecoveryStart {
        /// Failed inference owner whose selected branch authorizes this claim.
        checkpoint: tau_proto::AgentInferenceDispatchStarted,
        /// Exact failure that must follow this committed synthetic claim.
        failure_after_commit: Option<Box<tau_proto::AgentStandaloneCompactionFailed>>,
        /// Exact interceptor-approved transaction start retained after
        /// rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Retain one reactive recovery failure until its semantic append commits.
    ReactiveContextRecoveryFailure {
        /// Exact committed transaction-start parent of this failure.
        batch_parent: tau_proto::AgentHead,
        /// Exact interceptor-approved failure retained after rejection.
        retry_event: Option<Box<Event>>,
    },
    /// Resume the successful standalone compaction after the final steer in its
    /// completion batch commits.
    StandaloneContinuation {
        /// Durable standalone transaction that owns the continuation.
        transaction_id: tau_proto::CompactionTransactionId,
        /// Provider-qualified model captured by that transaction.
        model: tau_proto::ModelId,
        /// Immutable closed transcript cut immediately before its activation.
        activation_cut: tau_proto::AgentHead,
        /// Selected compaction boundary that must remain an ancestor on retry.
        batch_parent: tau_proto::AgentHead,
        /// Original completion source reused for the checkpoint publication.
        source: Option<tau_proto::ConnectionId>,
        /// Uncommitted suffix beginning with this exact batch publication.
        retry_prompts: Vec<crate::agent::PendingPrompt>,
        /// Whether this member owns continuation execution after commit.
        complete_on_commit: bool,
        /// Exact interceptor-approved steer retained after persistence
        /// rejection.
        approved_retry_event: Option<Box<Event>>,
    },
}

impl AgentPublishCompletion {
    /// Whether this retained completion owns one terminal for the prompt.
    pub(super) fn owns_output_length_terminal(
        &self,
        agent_prompt_id: &tau_proto::AgentPromptId,
    ) -> bool {
        let response = match self {
            Self::OutputLengthContinuation { response, .. }
            | Self::OutputLengthPreDeliveryFailure { response, .. } => Some(response.as_ref()),
            Self::GatedFinal {
                retry_event: Some(event),
                ..
            } => match event.as_ref() {
                Event::ProviderResponseFinished(response) => Some(response),
                _ => None,
            },
            Self::InitialPromptSubmission { .. }
            | Self::GatedFinal { .. }
            | Self::OutputLengthSteer { .. }
            | Self::OutputLengthDormantRepair { .. }
            | Self::ReactiveContextRecovery { .. }
            | Self::ReactiveContextRecoveryStart { .. }
            | Self::ReactiveContextRecoveryFailure { .. }
            | Self::StandaloneContinuation { .. } => None,
        };
        response.is_some_and(|response| {
            &response.agent_prompt_id == agent_prompt_id
                && matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                )
        })
    }

    /// Return the durable transaction shared by every member of this batch.
    fn transaction_id(&self) -> &tau_proto::CompactionTransactionId {
        match self {
            Self::StandaloneContinuation { transaction_id, .. } => transaction_id,
            Self::GatedFinal { .. }
            | Self::OutputLengthContinuation { .. }
            | Self::OutputLengthSteer { .. }
            | Self::OutputLengthPreDeliveryFailure { .. }
            | Self::OutputLengthDormantRepair { .. }
            | Self::ReactiveContextRecovery { .. }
            | Self::ReactiveContextRecoveryStart { .. }
            | Self::ReactiveContextRecoveryFailure { .. }
            | Self::InitialPromptSubmission { .. } => {
                unreachable!("non-standalone completions do not own compaction transactions")
            }
        }
    }
}

/// Event types where a `Drop` reply from an interceptor is
/// overridden into `Pass(None)` with a `tracing::warn!`.
///
/// These events carry state changes the harness can't reasonably
/// continue without — silently dropping an `AgentPromptSubmitted`, for
/// example, would make accepted user input vanish from the transcript.
/// Interceptors that try to
/// drop one of these are almost certainly buggy.
const MUST_PASS_BY_DEFAULT: &[EventName] = &[
    // User-message-bearing events: dropping any of these would
    // make the user's input vanish silently while the harness
    // believes the prompt was delivered.
    EventName::AGENT_PROMPT_SUBMITTED,
    EventName::AGENT_USER_MESSAGE_INJECTED,
    EventName::AGENT_PROMPT_STEERED,
    EventName::AGENT_COMPACTION_TRIGGERED,
    EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
    EventName::AGENT_MANUAL_COMPACTION_REQUEST_FAILED,
    EventName::AGENT_STANDALONE_COMPACTION_STARTED,
    EventName::AGENT_STANDALONE_COMPACTION_FAILED,
    EventName::AGENT_INFERENCE_DISPATCH_STARTED,
    EventName::AGENT_PROMPT_TERMINATED,
    EventName::AGENT_COMPACTED,
    // Session lifecycle facts drive extension/context-provider setup and
    // teardown. Dropping one can wedge startup or leave stale per-session state.
    EventName::SESSION_STARTED,
    EventName::SESSION_SHUTDOWN,
    // Durable session membership facts anchor resume state. Dropping one leaves
    // live session state inconsistent with persisted membership.
    EventName::SESSION_AGENT_LOADED,
    EventName::SESSION_AGENT_UNLOADED,
    // Complete current operational snapshots carry shared navigation authority.
    EventName::AGENT_STATS_UPDATED,
    // Agent creation and message projection facts are harness-validated durable
    // transcript facts. Dropping or rewriting them after validation breaks
    // sender/recipient correlation and resume state.
    EventName::AGENT_STARTED,
    EventName::AGENT_MESSAGE_SENT,
    EventName::AGENT_MESSAGE_RECEIVED,
    EventName::MESSAGE_DELIVERED,
    EventName::MESSAGE_EDITED,
    EventName::MESSAGE_DELETED,
    EventName::MESSAGE_REACTION_ADDED,
    EventName::MESSAGE_REACTION_REMOVED,
    EventName::MESSAGE_SENT,
    // Canonical provider model state is harness-owned current state. Declarations
    // remain mutable and interceptable before this protected projection.
    EventName::PROVIDER_MODELS_UPDATED,
    EventName::AGENT_INITIALIZATION_CONTEXT_SET,
    EventName::HARNESS_AGENT_CONTEXT_INITIALIZED,
    EventName::HARNESS_SESSION_SKILLS_AVAILABLE,
    EventName::TOOL_REGISTER,
    EventName::TOOL_UNREGISTER,
    EventName::TOOL_PROGRESS,
    EventName::ACTION_SCHEMA_PUBLISHED,
    EventName::ACTION_RESULT,
    EventName::ACTION_ERROR,
    // Agent request life-cycle: the agent extension consumes normal
    // `AgentPromptCreated` turns to know when to talk to the LLM. Dropping
    // one wedges the conversation.
    EventName::AGENT_PROMPT_CREATED,
    // Lightweight prompt lifecycle: UIs and notification extensions use this
    // instead of the full provider prompt payload.
    EventName::AGENT_PROMPT_STARTED,
    EventName::AGENT_PROMPT_FAILED,
    EventName::AGENT_PROMPT_REJECTED,
    EventName::AGENT_OUTER_TURN_STARTED,
    EventName::AGENT_OUTER_TURN_FINISHED,
    // Agent response: dropping this would wedge `c.head` /
    // `prompt_agents` bookkeeping and the conversation
    // would never advance.
    EventName::PROVIDER_RESPONSE_FINISHED,
    // Validated ephemeral provider current state must agree between live and
    // late-subscriber projections.
    EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
    // Tool round-trip closure: a missing terminal completion,
    // cancellation, provider result, or background result for a tool
    // that was actually invoked leaves the agent waiting forever.
    EventName::TOOL_RESULT,
    EventName::TOOL_RESULT_DISPLAY,
    EventName::TOOL_ERROR,
    EventName::PROVIDER_TOOL_RESULT,
    EventName::PROVIDER_TOOL_ERROR,
    EventName::TOOL_CANCELLED,
    EventName::TOOL_BACKGROUND_RESULT,
    EventName::TOOL_BACKGROUND_RESULT_DISPLAY,
    EventName::TOOL_BACKGROUND_ERROR,
    // A validated user-shell terminal consumes the harness's pending route.
    // Dropping it would leave every attached UI waiting forever.
    EventName::SHELL_COMMAND_FINISHED,
];

fn mandatory_harness_notice(event: &Event) -> bool {
    matches!(
        event,
        Event::HarnessNotice(info)
            if info.purpose == tau_proto::NoticePurpose::Alert
                || info.level == tau_proto::NoticeLevel::Critical
    )
}

fn event_is_effectively_must_pass(event: &Event, caller_must_pass: bool) -> bool {
    caller_must_pass
        || event_must_pass_by_default(&event.name())
        || mandatory_harness_notice(event)
        || matches!(
            event,
            Event::AgentMetadataSet(set) | Event::AgentMetadataSetRequest(set)
                if set.mutation_id.is_some()
        )
}

/// Return whether a deferred peer publication must cross session rollover.
///
/// Process-global declarations/reports retain their semantic effect for the
/// still-current extension generation. Session-bound observation families
/// retain only their raw committed fact; the downstream admission-generation
/// barrier suppresses stale semantics.
fn rollover_publication_must_commit(event: &Event) -> bool {
    Harness::peer_event_semantics_survive_rollover(event)
        || matches!(
            event,
            Event::ToolRequest(_)
                | Event::StartAgentRequest(_)
                | Event::ExtInternalPromptSubmitRequest(_)
                | Event::ExtensionContextProviderRegister(_)
                | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                | Event::ExtAgentContextPublish(_)
                | Event::ExtensionContextReady(_)
                | Event::ExtensionSessionContextProviderRegister(_)
                | Event::ExtensionSessionDiscoverySnapshotDeclared(_)
                | Event::ExtensionSessionContextReady(_)
        )
}

fn mandatory_harness_notice_was_modified(original: &Event, replacement: &Event) -> bool {
    mandatory_harness_notice(original) && original != replacement
}

fn sanitize_harness_notice_replacement(original: &Event, replacement: &mut Event) {
    if let (Event::HarnessNotice(original), Event::HarnessNotice(replacement)) =
        (original, replacement)
    {
        replacement.kind.clone_from(&original.kind);
        replacement.level = original.level;
        replacement.purpose = original.purpose;
    }
}

fn preserve_agent_metadata_mutation_id(original: &Event, replacement: &mut Event) {
    let (original, replacement) = match (original, replacement) {
        (Event::AgentMetadataSet(original), Event::AgentMetadataSet(replacement))
        | (Event::AgentMetadataSetRequest(original), Event::AgentMetadataSetRequest(replacement)) => {
            (original, replacement)
        }
        _ => return,
    };
    if original.mutation_id.is_some() {
        replacement.agent_id = original.agent_id.clone();
        replacement.key = original.key.clone();
        replacement.inheritable = original.inheritable;
    }
    replacement.mutation_id = original.mutation_id.clone();
}

fn preserve_shell_command_identity(original: &Event, replacement: &mut Event) {
    match (original, replacement) {
        (Event::ShellCommandProgress(original), Event::ShellCommandProgress(replacement)) => {
            replacement.command_id = original.command_id.clone();
            replacement.target_agent_id = original.target_agent_id.clone();
        }
        (Event::ShellCommandFinished(original), Event::ShellCommandFinished(replacement)) => {
            replacement.command_id = original.command_id.clone();
            replacement.session_id = original.session_id.clone();
            replacement.command.clone_from(&original.command);
            replacement.include_in_context = original.include_in_context;
            replacement.target_agent_id = original.target_agent_id.clone();
        }
        _ => {}
    }
}

/// Reject a replacement that cannot carry tool-call correlation.
fn invalid_tool_request_replacement(event: &Event) -> bool {
    matches!(event, Event::ToolRequest(request) if request.call_id.is_empty())
}

pub(super) fn immutable_protected_fact_was_modified(original: &Event, replacement: &Event) -> bool {
    matches!(
        original,
        Event::AgentStarted(_)
            | Event::AgentUserInteractionRecorded(_)
            | Event::AgentMessageSent(_)
            | Event::AgentMessageReceived(_)
            | Event::MessageDelivered(_)
            | Event::MessageEdited(_)
            | Event::MessageDeleted(_)
            | Event::MessageReactionAdded(_)
            | Event::MessageReactionRemoved(_)
            | Event::MessageSent(_)
            | Event::ProviderModelsUpdated(_)
            | Event::AgentInitializationContextSet(_)
            | Event::HarnessAgentContextInitialized(_)
            | Event::HarnessSessionSkillsAvailable(_)
            | Event::ToolRegister(_)
            | Event::ToolUnregister(_)
            | Event::ToolProgress(_)
            | Event::ActionSchemaPublished(_)
            | Event::ActionResult(_)
            | Event::ActionError(_)
            | Event::SessionStarted(_)
            | Event::SessionShutdown(_)
            | Event::SessionAgentLoaded(_)
            | Event::SessionAgentUnloaded(_)
            | Event::AgentStatsUpdated(_)
            | Event::AgentCompactionTriggered(_)
            | Event::AgentCompacted(_)
            | Event::AgentStandaloneCompactionStarted(_)
            | Event::AgentStandaloneCompactionFailed(_)
            | Event::AgentInferenceDispatchStarted(_)
            | Event::AgentPromptCreated(_)
            | Event::AgentPromptStarted(_)
            | Event::AgentPromptFailed(_)
            | Event::AgentPromptRejected(_)
            | Event::AgentPromptTerminated(_)
            | Event::AgentOuterTurnStarted(_)
            | Event::AgentOuterTurnFinished(_)
            | Event::ProviderResponseFinished(_)
            | Event::HarnessProviderQuotaChanged(_)
            | Event::ToolResult(_)
            | Event::ToolResultDisplay(_)
            | Event::ToolError(_)
            | Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::ToolCancelled(_)
            | Event::ToolBackgroundResult(_)
            | Event::ToolBackgroundResultDisplay(_)
            | Event::ToolBackgroundError(_)
            | Event::ShellCommandFinished(_)
    ) && original != replacement
}

pub(super) fn event_must_pass_by_default(name: &EventName) -> bool {
    MUST_PASS_BY_DEFAULT.contains(name)
}

fn protected_prompt_fields_were_modified(original: &Event, replacement: &Event) -> bool {
    match (original, replacement) {
        (Event::AgentPromptSubmitted(original), Event::AgentPromptSubmitted(replacement)) => {
            original.agent_id != replacement.agent_id
                || original.inference_activation != replacement.inference_activation
                || original.message_class != replacement.message_class
                || original.internal_kind != replacement.internal_kind
                || (matches!(
                    original.internal_kind,
                    Some(
                        tau_proto::InternalPromptKind::ContextSizeAlert
                            | tau_proto::InternalPromptKind::BackgroundToolCompletion
                    )
                ) && original.text != replacement.text)
                || original.originator != replacement.originator
                || original.submission_source != replacement.submission_source
        }
        (
            Event::AgentUserMessageInjected(original),
            Event::AgentUserMessageInjected(replacement),
        ) => {
            original.agent_id != replacement.agent_id
                || original.inference_activation != replacement.inference_activation
                || original.message_class != replacement.message_class
        }
        (Event::AgentPromptSteered(original), Event::AgentPromptSteered(replacement)) => {
            original.agent_id != replacement.agent_id
                || original.inference_activation != replacement.inference_activation
                || original.submission_source != replacement.submission_source
                || original.message_class != replacement.message_class
                || original.internal_kind != replacement.internal_kind
                || original.self_compaction_terminal != replacement.self_compaction_terminal
                || (original.self_compaction_terminal.is_some()
                    && original.text != replacement.text)
                || (matches!(
                    original.internal_kind,
                    Some(
                        tau_proto::InternalPromptKind::ContextSizeAlert
                            | tau_proto::InternalPromptKind::BackgroundToolCompletion
                    )
                ) && original.text != replacement.text)
        }
        _ => false,
    }
}

/// Cursor pointing just past the interceptor registration that last handled a
/// parked publish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InterceptorCursor {
    /// Selector set that produced the parked interceptor. Exact selectors are
    /// exhausted before prefix selectors, so prefix chaining uses an
    /// independent cursor after the exact set is done.
    set: InterceptorSet,
    /// Full registration key used for same-set continuation.
    registration: InterceptorRegistration,
}

/// Which selector set matched an interceptor registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterceptorSet {
    /// Exact event-name selector.
    Exact,
    /// Prefix selector.
    Prefix,
}

/// Registry lookup result with the selector set that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct InterceptorMatch {
    /// Selector set used for cursor continuation.
    set: InterceptorSet,
    /// Matching registration.
    registration: InterceptorRegistration,
}

/// Interceptor registration ordered by priority, component name, then
/// connection id.
#[derive(Clone, Debug, Eq, PartialEq)]
struct InterceptorRegistration {
    priority: InterceptionPriority,
    component_name: ExtensionName,
    connection_id: tau_proto::ConnectionId,
}

impl Ord for InterceptorRegistration {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(test)]
        test_registration_order_comparison();
        self.priority
            .cmp(&other.priority)
            .then_with(|| {
                self.component_name
                    .as_str()
                    .cmp(other.component_name.as_str())
            })
            .then_with(|| self.connection_id.as_str().cmp(&other.connection_id))
    }
}

impl PartialOrd for InterceptorRegistration {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(crate) struct InterceptorRegistry {
    exact: BTreeMap<tau_proto::EventName, BTreeSet<InterceptorRegistration>>,
    prefix: BTreeMap<String, BTreeSet<InterceptorRegistration>>,
}

impl InterceptorRegistry {
    pub(crate) fn replace_for_connection(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        component_name: ExtensionName,
        selectors: Vec<EventSelector>,
        priority: InterceptionPriority,
    ) {
        self.remove_connection(connection_id);
        let registration = InterceptorRegistration {
            priority,
            component_name,
            connection_id: connection_id.clone(),
        };
        for selector in selectors {
            match selector {
                EventSelector::Exact(name) => {
                    self.exact
                        .entry(name)
                        .or_default()
                        .insert(registration.clone());
                }
                EventSelector::Prefix(prefix) => {
                    self.prefix
                        .entry(prefix)
                        .or_default()
                        .insert(registration.clone());
                }
            }
        }
    }

    pub(crate) fn remove_connection(&mut self, connection_id: &tau_proto::ConnectionId) {
        for registrations in self.exact.values_mut() {
            registrations.retain(|r| r.connection_id != *connection_id);
        }
        self.exact
            .retain(|_, registrations| !registrations.is_empty());
        for registrations in self.prefix.values_mut() {
            registrations.retain(|r| r.connection_id != *connection_id);
        }
        self.prefix
            .retain(|_, registrations| !registrations.is_empty());
    }

    fn next_for(
        &self,
        event: &Event,
        cursor: Option<&InterceptorCursor>,
    ) -> Option<InterceptorMatch> {
        let name = event.name();
        if cursor.is_none_or(|cursor| cursor.set == InterceptorSet::Exact) {
            let exact_cursor = cursor
                .filter(|cursor| cursor.set == InterceptorSet::Exact)
                .map(|cursor| &cursor.registration);
            if let Some(registration) = self.next_in_set(self.exact.get(&name), exact_cursor) {
                return Some(InterceptorMatch {
                    set: InterceptorSet::Exact,
                    registration,
                });
            }
        }

        let prefix_cursor = cursor
            .filter(|cursor| cursor.set == InterceptorSet::Prefix)
            .map(|cursor| &cursor.registration);
        self.prefix
            .iter()
            .filter(|(prefix, _)| name.matches_prefix(prefix))
            .filter_map(|(_, registrations)| self.next_in_set(Some(registrations), prefix_cursor))
            .min()
            .map(|registration| InterceptorMatch {
                set: InterceptorSet::Prefix,
                registration,
            })
    }

    fn next_in_set(
        &self,
        registrations: Option<&BTreeSet<InterceptorRegistration>>,
        cursor: Option<&InterceptorRegistration>,
    ) -> Option<InterceptorRegistration> {
        let registrations = registrations?;
        match cursor {
            Some(cursor) => registrations
                .range((Excluded(cursor), Unbounded))
                .next()
                .cloned(),
            None => registrations.first().cloned(),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Counts ordering comparisons for the current registry test thread.
static REGISTRATION_ORDER_COMPARISONS: std::cell::Cell<usize> = const {
        Cell::new(0)
    };
}

#[cfg(test)]
fn test_registration_order_comparison() {
    REGISTRATION_ORDER_COMPARISONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn reset_registration_order_comparisons() {
    REGISTRATION_ORDER_COMPARISONS.with(|count| count.set(0));
}

#[cfg(test)]
fn registration_order_comparisons() -> usize {
    REGISTRATION_ORDER_COMPARISONS.with(Cell::get)
}

impl Harness {
    fn is_synchronized_agent_checkpoint_or_completion(
        event: &Event,
        sync: Option<&ConversationHeadSync>,
    ) -> bool {
        sync.is_some_and(|sync| {
            sync.continuation.is_some() || matches!(event, Event::AgentInferenceDispatchStarted(_))
        })
    }

    fn suspend_interceptor_after_destructive_cancel(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) {
        self.suspended_interceptor_connections
            .insert(connection_id.clone());
    }

    /// Remove not-yet-started publications from the same rejected completion
    /// batch. The retained envelope republishes their exact prompt suffix.
    pub(crate) fn discard_deferred_agent_publish_batch(
        &mut self,
        cid: &AgentId,
        completion: &AgentPublishCompletion,
    ) {
        let transaction_id = completion.transaction_id();
        self.deferred_publishes.retain(|publish| {
            publish
                .sync_head_for
                .as_ref()
                .filter(|sync| &sync.cid == cid)
                .and_then(ConversationHeadSync::completion)
                .is_none_or(|queued| queued.transaction_id() != transaction_id)
        });
    }

    /// Cancel synchronized checkpoints/completions owned by one unloading
    /// agent, suspend an in-flight responder, and resume unrelated FIFO
    /// work.
    pub(crate) fn cancel_agent_synchronized_publications(&mut self, cid: &AgentId) {
        let mut canceled_prompt_ids = Vec::new();
        let mut canceled_initial_prompts = Vec::new();
        let mut canceled_watch_retirements = Vec::new();
        let removed_pending = self.pending_intercept.as_ref().is_some_and(|pending| {
            pending
                .sync_head_for
                .as_ref()
                .is_some_and(|sync| &sync.cid == cid)
                && Self::is_synchronized_agent_checkpoint_or_completion(
                    &pending.event,
                    pending.sync_head_for.as_ref(),
                )
        });
        if removed_pending {
            let pending = self
                .pending_intercept
                .take()
                .expect("matched pending intercept");
            if let Some(prompt_id) = pending
                .sync_head_for
                .as_ref()
                .and_then(ConversationHeadSync::prompt_dispatch)
                .map(|continuation| continuation.started.agent_prompt_id.clone())
            {
                canceled_prompt_ids.push(prompt_id);
            }
            if let Some(AgentPublishCompletion::InitialPromptSubmission { correlation }) = pending
                .sync_head_for
                .as_ref()
                .and_then(ConversationHeadSync::completion)
            {
                canceled_initial_prompts.push(correlation.clone());
            }
            if let Some(completion) = pending
                .sync_head_for
                .as_ref()
                .and_then(ConversationHeadSync::watch_retirement)
            {
                canceled_watch_retirements.push(completion.clone());
            }
            self.suspend_interceptor_after_destructive_cancel(&pending.conn_id);
            self.rollback_rejected_activation_successor(&pending.event);
        }
        self.deferred_publishes.retain(|publish| {
            let canceled = publish
                .sync_head_for
                .as_ref()
                .is_some_and(|sync| &sync.cid == cid)
                && Self::is_synchronized_agent_checkpoint_or_completion(
                    &publish.event,
                    publish.sync_head_for.as_ref(),
                );
            if canceled
                && let Some(prompt_id) = publish
                    .sync_head_for
                    .as_ref()
                    .and_then(ConversationHeadSync::prompt_dispatch)
                    .map(|continuation| continuation.started.agent_prompt_id.clone())
            {
                canceled_prompt_ids.push(prompt_id);
            }
            if canceled
                && let Some(AgentPublishCompletion::InitialPromptSubmission { correlation }) =
                    publish
                        .sync_head_for
                        .as_ref()
                        .and_then(ConversationHeadSync::completion)
            {
                canceled_initial_prompts.push(correlation.clone());
            }
            if canceled
                && let Some(completion) = publish
                    .sync_head_for
                    .as_ref()
                    .and_then(ConversationHeadSync::watch_retirement)
            {
                canceled_watch_retirements.push(completion.clone());
            }
            !canceled
        });
        for prompt_id in canceled_prompt_ids {
            self.dispose_prompt_dispatch_bookkeeping(&prompt_id);
        }
        for correlation in canceled_initial_prompts {
            self.publish_initial_prompt_failed(
                correlation,
                tau_proto::AgentPromptFailureStage::LifecycleTeardown,
                "agent teardown discarded initial prompt submission",
            );
        }
        for completion in &canceled_watch_retirements {
            self.finish_watch_retirement_delivery(completion, false);
        }
        if removed_pending {
            // Canceling one agent's intercepted completion unblocks the global
            // FIFO. Preserve and resume every publication not owned by that
            // completion; another agent's durable work must not disappear with
            // the unloading owner.
            self.drain_deferred_publishes();
            self.drain_publish_idle_dispatches();
        }
    }

    /// Quiesce synchronized publications for rollover: cancel old-session
    /// checkpoints/completions, retain required publications, run
    /// Drop-equivalent cleanup, suspend in-flight responders, and drain the
    /// retained FIFO.
    pub(crate) fn quiesce_synchronized_publications_for_rollover(&mut self) {
        if let Some(pending) = self.pending_intercept.take() {
            self.suspend_interceptor_after_destructive_cancel(&pending.conn_id);
            if Self::is_synchronized_agent_checkpoint_or_completion(
                &pending.event,
                pending.sync_head_for.as_ref(),
            ) {
                if let Some(prompt_id) = pending
                    .sync_head_for
                    .as_ref()
                    .and_then(ConversationHeadSync::prompt_dispatch)
                    .map(|continuation| continuation.started.agent_prompt_id.clone())
                {
                    self.dispose_prompt_dispatch_bookkeeping(&prompt_id);
                }
                if let Some(AgentPublishCompletion::InitialPromptSubmission { correlation }) =
                    pending
                        .sync_head_for
                        .as_ref()
                        .and_then(ConversationHeadSync::completion)
                {
                    self.publish_initial_prompt_failed(
                        correlation.clone(),
                        tau_proto::AgentPromptFailureStage::LifecycleTeardown,
                        "session switch discarded initial prompt submission",
                    );
                }
                self.rollback_rejected_activation_successor(&pending.event);
            } else {
                // The switch already advanced session generation. Commit the
                // accepted observation through its normal path; stale admission
                // can no longer create or retarget work in the replacement
                // session.
                self.advance_pending_intercept(pending, InterceptAction::Pass(None));
            }
        }
        // Specialized session teardown has already completed admission, ACK,
        // shell, and peer failure paths. Retain mandatory terminal/lifecycle
        // publications, including SessionShutdown, and force their interception
        // chains to completion before changing the bound session.
        let mut retained = VecDeque::with_capacity(self.deferred_publishes.len());
        while let Some(publish) = self.deferred_publishes.pop_front() {
            if let Some(AgentPublishCompletion::InitialPromptSubmission { correlation }) = publish
                .sync_head_for
                .as_ref()
                .and_then(ConversationHeadSync::completion)
            {
                let correlation = correlation.clone();
                self.discard_deferred_publish(
                    publish,
                    "session rollover canceled initial prompt submission",
                );
                self.publish_initial_prompt_failed(
                    correlation,
                    tau_proto::AgentPromptFailureStage::LifecycleTeardown,
                    "session switch discarded initial prompt submission",
                );
                continue;
            }
            if event_is_effectively_must_pass(&publish.event, publish.must_pass)
                || rollover_publication_must_commit(&publish.event)
            {
                retained.push_back(publish);
            } else {
                self.discard_deferred_publish(publish, "session rollover canceled publication");
            }
        }
        self.deferred_publishes = retained;
        loop {
            self.drain_deferred_publishes();
            let Some(pending_must_pass) = self.pending_intercept.take() else {
                break;
            };
            self.suspend_interceptor_after_destructive_cancel(&pending_must_pass.conn_id);
            self.advance_pending_intercept(pending_must_pass, InterceptAction::Pass(None));
        }
    }

    /// Commit an already interceptor-approved retry event without restarting
    /// the interception chain.
    pub(crate) fn commit_approved_agent_retry(
        &mut self,
        cid: &AgentId,
        event: Event,
        completion: AgentPublishCompletion,
    ) {
        let notify_watchers = match &completion {
            AgentPublishCompletion::StandaloneContinuation { retry_prompts, .. } => retry_prompts
                .first()
                .is_some_and(path_crate_agent::PendingPrompt::should_notify_watchers),
            AgentPublishCompletion::GatedFinal { .. } => false,
            AgentPublishCompletion::OutputLengthContinuation { .. } => false,
            AgentPublishCompletion::OutputLengthSteer { .. } => false,
            AgentPublishCompletion::OutputLengthPreDeliveryFailure { .. } => false,
            AgentPublishCompletion::OutputLengthDormantRepair { .. } => false,
            AgentPublishCompletion::ReactiveContextRecovery { .. } => false,
            AgentPublishCompletion::ReactiveContextRecoveryStart { .. } => false,
            AgentPublishCompletion::ReactiveContextRecoveryFailure { .. } => false,
            AgentPublishCompletion::InitialPromptSubmission { .. } => false,
        };
        let fold_parent = match &completion {
            AgentPublishCompletion::OutputLengthDormantRepair { step, .. } => {
                Some(step.fold_parent())
            }
            AgentPublishCompletion::OutputLengthSteer { batch_parent, .. } => {
                Some(tau_core::AgentEventParent::from_head(*batch_parent))
            }
            AgentPublishCompletion::ReactiveContextRecovery { checkpoint, .. }
            | AgentPublishCompletion::ReactiveContextRecoveryStart { checkpoint, .. } => {
                Some(tau_core::AgentEventParent::from_head(checkpoint.through))
            }
            AgentPublishCompletion::ReactiveContextRecoveryFailure { batch_parent, .. } => {
                Some(tau_core::AgentEventParent::from_head(*batch_parent))
            }
            _ => None,
        };
        let agent_id = self.agent_id_for_event(&event).or_else(|| {
            self.agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .map(crate::parse_agent_id)
        });
        self.commit_event(
            None,
            &PeerPublicationContext::default(),
            event.clone(),
            event.defaults_to_persist(),
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id,
                session_generation: self.current_session_generation,
                fold_parent,
                suppress_activation_dispatch: true,
                continuation: Some(PostCommitContinuation::AgentPublish(Box::new(completion))),
                notify_watchers,
            }),
        );
        self.drain_deferred_publishes();
    }

    /// Rewrite queued canonical model state to an empty snapshot after its
    /// provider generation disconnects.
    pub(crate) fn clear_parked_provider_model_updates(
        &mut self,
        publisher: &tau_proto::ExtensionName,
    ) -> bool {
        let mut cleared = false;
        if let Some(Event::ProviderModelsUpdated(update)) = self
            .pending_intercept
            .as_mut()
            .map(|pending| &mut pending.event)
            && &update.publisher_extension_id == publisher
        {
            update.models.clear();
            cleared = true;
        }
        for deferred in &mut self.deferred_publishes {
            if let Event::ProviderModelsUpdated(update) = &mut deferred.event
                && &update.publisher_extension_id == publisher
            {
                update.models.clear();
                cleared = true;
            }
        }
        cleared
    }

    /// Remove canceled peer receives from current and deferred interception
    /// without exposing their content to another interceptor or commit path.
    pub(crate) fn discard_canceled_peer_receive_publishes(
        &mut self,
        canceled: &std::collections::HashSet<tau_proto::AgentMessageId>,
    ) {
        if self.pending_intercept.as_ref().is_some_and(|pending| {
            matches!(
                &pending.event,
                Event::AgentMessageReceived(received)
                    if canceled.contains(&received.message_id)
            )
        }) {
            let pending = self.pending_intercept.take().expect("matched peer receive");
            self.suspend_interceptor_after_destructive_cancel(&pending.conn_id);
            self.fail_pending_external_receive(
                &pending.event,
                "target session changed before receive commit",
                tau_proto::ExternalAgentMessageFailure::TargetSessionChanged,
            );
            self.discard_peer_activation_reservation(&pending.source.peer_context);
        }
        let mut retained = VecDeque::with_capacity(self.deferred_publishes.len());
        while let Some(deferred) = self.deferred_publishes.pop_front() {
            if matches!(
                deferred.event(),
                Event::AgentMessageReceived(received)
                    if canceled.contains(&received.message_id)
            ) {
                self.discard_deferred_publish(
                    deferred,
                    "target session changed before receive commit",
                );
            } else {
                retained.push_back(deferred);
            }
        }
        self.deferred_publishes = retained;
        self.drain_deferred_publishes();
        self.drain_publish_idle_dispatches();
    }

    /// Cancel one queued publication through the same reservation/ACK cleanup
    /// owned by an interceptor Drop, without exposing it to another
    /// interceptor.
    fn discard_deferred_publish(&mut self, deferred: DeferredPublish, reason: &str) {
        let DeferredPublish {
            source,
            event,
            persist: _,
            must_pass: _,
            sync_head_for,
        } = deferred;
        if let Some(prompt_id) = sync_head_for
            .as_ref()
            .and_then(ConversationHeadSync::prompt_dispatch)
            .map(|continuation| continuation.started.agent_prompt_id.clone())
        {
            self.dispose_prompt_dispatch_bookkeeping(&prompt_id);
        }
        if Self::pending_external_receive_message_id(&event)
            .is_some_and(|id| self.pending_external_receive_acks.contains_key(id))
        {
            self.fail_pending_external_receive(
                &event,
                reason,
                tau_proto::ExternalAgentMessageFailure::Rejected,
            );
        }
        if let Event::ShellCommandProgress(progress) = &event {
            self.discard_uncommitted_shell_canonical_marker(&progress.command_id);
        }
        self.discard_peer_activation_reservation(&source.peer_context);
    }

    /// True when no event is parked in interception and no publish is
    /// queued behind one.
    pub(super) fn publish_chain_is_idle(&self) -> bool {
        self.pending_intercept.is_none() && self.deferred_publishes.is_empty()
    }

    /// True when `cid` already has a prompt dispatch waiting for a
    /// publish/interception condition.
    pub(crate) fn has_deferred_prompt_dispatch_for(&self, cid: &AgentId) -> bool {
        self.pending_publish_idle_dispatches.iter().any(|queued| {
            &queued.cid == cid
                && (!queued.obligation.is_committed()
                    || queued.activation_through.is_none()
                    || self.deferred_activation_is_selected(queued))
        })
    }

    /// Send `cid`'s prompt now if the publish chain is idle; otherwise
    /// park it until interception and deferred publishes fully drain.
    pub(crate) fn dispatch_prompt_after_publish_idle(&mut self, cid: &AgentId) {
        self.dispatch_or_defer_prompt(cid);
    }

    /// Wait for the whole publish batch, then run activation compaction before
    /// inference using the final active fact's parent as the immutable cut.
    pub(crate) fn dispatch_activation_after_publish_idle(&mut self, cid: &AgentId) {
        if !self.publish_chain_is_idle() {
            self.defer_prompt_dispatch(cid.clone());
            return;
        }
        if !self
            .pending_publish_idle_dispatches
            .iter()
            .any(|deferred| deferred.cid == *cid && deferred.obligation.is_committed())
        {
            self.enqueue_committed_activation_dispatch(
                cid.clone(),
                self.activation_cut_before_current_head(cid),
                self.selected_head_for_agent(cid),
            );
        }
        self.drain_publish_idle_dispatches();
    }

    fn dispatch_or_defer_prompt(&mut self, cid: &AgentId) {
        if !self.publish_chain_is_idle() {
            self.defer_prompt_dispatch(cid.clone());
            return;
        }
        if !self.agent_context_ready_for(cid) {
            self.defer_prompt_dispatch(cid.clone());
            return;
        }
        self.checkpoint_or_send_prompt(cid, None);
    }

    /// Commit an immutable inference watermark before live inference dispatch.
    ///
    /// Standalone compact operations already have their own durable start and
    /// are sent directly. Ordinary inference first enters
    /// `AwaitingCheckpoint`; only the checkpoint's post-commit reaction sends
    /// the exact reserved prompt id and head.
    fn checkpoint_or_send_prompt(
        &mut self,
        cid: &AgentId,
        captured_activation_cut: Option<tau_proto::AgentHead>,
    ) {
        let _ = self.ensure_agent_id_for_agent(cid);
        let state = self
            .agents
            .get(cid)
            .map(|agent| agent.activation_dispatch.clone());
        if matches!(
            state,
            Some(crate::agent::ActivationDispatchState::Running { .. })
        ) {
            let _ = self.send_prompt_to_agent_for(cid);
            return;
        }
        if !matches!(state, Some(crate::agent::ActivationDispatchState::None)) {
            return;
        }
        if !self.agent_can_start_deferred_inference_dispatch(cid) {
            return;
        }
        let output_length_owner_ready = self.agents.get(cid).is_some_and(|agent| {
            matches!(
                agent.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
            )
        });
        if !output_length_owner_ready && !self.validate_prompt_render_for_dispatch(cid) {
            return;
        }
        let selection = match self.select_inference_dispatch(cid, captured_activation_cut) {
            Ok(selection) => selection,
            Err(InferenceDispatchSelectionError::MissingModel) => {
                let role_name = self.role_name_for_agent_id(cid);
                self.emit_info(&format!(
                    "role `{role_name}` has no available model — use :role to pick a role, :model <provider>/<model> to pick an agent model, or enable a provider"
                ));
                self.set_agent_turn_state(cid, path_crate_agent::AgentTurnState::Idle);
                return;
            }
            Err(InferenceDispatchSelectionError::OutputLengthBranchInvalid) => {
                self.repair_dormant_output_length_lineage(cid);
                return;
            }
            Err(InferenceDispatchSelectionError::MissingActivationCut) => return,
        };
        let Some(checkpoint) = self.claim_inference_checkpoint(cid, selection) else {
            return;
        };
        self.publish_for_agent(
            cid,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: checkpoint.durable_agent_id,
                transaction_id: None,
                agent_prompt_id: checkpoint.agent_prompt_id,
                through: checkpoint.through,
                model: Some(checkpoint.selection.model),
                operation: Some(checkpoint.selection.operation),
                activation_cut: Some(checkpoint.selection.activation_cut),
                output_length_continuation: checkpoint.output_length_continuation,
            }),
        );
    }

    fn defer_prompt_dispatch(&mut self, cid: AgentId) {
        if self.has_deferred_prompt_dispatch_for(&cid) {
            tracing::debug!(
                target: "tau_harness::interception",
                conversation_id = %cid,
                "prompt dispatch already deferred; skipping duplicate",
            );
            return;
        }
        let output_length_continuation = self.agents.get(&cid).is_some_and(|agent| {
            matches!(
                agent.output_length_continuation,
                crate::agent::OutputLengthContinuationState::Planned(_)
                    | crate::agent::OutputLengthContinuationState::OwnerReady(_)
            )
        });
        self.pending_publish_idle_dispatches
            .push_back(DeferredPromptDispatch {
                cid,
                activation_cut: None,
                activation_through: None,
                activation_source_seq: None,
                obligation: if output_length_continuation {
                    DeferredActivationObligation::OutputLengthPublishIdle
                } else {
                    DeferredActivationObligation::OrdinaryPublishIdle
                },
            });
    }

    /// Whether an ordinary deferred obligation may create its durable
    /// checkpoint.
    ///
    /// A committed provider response can synchronously activate a continuation
    /// before its outer handler changes the live turn projection from
    /// `AgentThinking`; that response has already released dispatch ownership,
    /// so requiring a projected `Idle` state here would strand valid
    /// message and tool-completion continuations. A foreground tool round
    /// is different: `send_prompt_to_agent_for` explicitly refuses it, so
    /// checkpointing would create a durable dispatch with no possible
    /// provider request.
    fn agent_can_start_deferred_inference_dispatch(&self, cid: &AgentId) -> bool {
        self.agent_context_ready_for(cid)
            && self.agents.get(cid).is_some_and(|agent| {
                !agent.terminating
                    && matches!(
                        agent.outer_turn,
                        path_crate_agent::OuterTurnRuntimeState::None
                            | path_crate_agent::OuterTurnRuntimeState::Active(_)
                    )
                    && matches!(
                        agent.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                    )
                    && !matches!(
                        agent.turn_state,
                        crate::agent::AgentTurnState::ToolsRunning { .. }
                    )
                    && !self.agent_has_open_foreground_tool_round(cid)
            })
    }

    fn same_deferred_prompt_dispatch(
        left: &DeferredPromptDispatch,
        right: &DeferredPromptDispatch,
    ) -> bool {
        left.cid == right.cid
            && left.activation_cut == right.activation_cut
            && left.activation_through == right.activation_through
            && left.activation_source_seq == right.activation_source_seq
            && left.obligation == right.obligation
    }

    fn deferred_prompt_dispatch_is_actionable(&self, deferred: &DeferredPromptDispatch) -> bool {
        let selected =
            !deferred.obligation.is_committed() || self.deferred_activation_is_selected(deferred);
        if !selected {
            return false;
        }
        if !self.agents.contains_key(&deferred.cid) {
            return true;
        }
        let owner_can_advance = self.agents.get(&deferred.cid).is_some_and(|agent| {
            (matches!(
                agent.activation_dispatch,
                crate::agent::ActivationDispatchState::Running { .. }
            ) && !agent.terminating
                && self.agent_context_ready_for(&deferred.cid))
                || self.agent_can_start_deferred_inference_dispatch(&deferred.cid)
        });
        owner_can_advance
            && !self
                .pending_agent_publish_completions
                .contains_key(&deferred.cid)
    }

    /// Returns the runtime-selected durable head for one loaded agent.
    pub(super) fn selected_head_for_agent(&self, cid: &AgentId) -> Option<tau_proto::AgentHead> {
        self.agents.get(cid).map(|agent| {
            agent
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
        })
    }

    /// Returns whether one branch-owned deferred activation is selected now.
    fn deferred_activation_is_selected(&self, deferred: &DeferredPromptDispatch) -> bool {
        let Some(owner) = deferred.activation_through else {
            return false;
        };
        let Some(agent) = self.agents.get(&deferred.cid) else {
            return false;
        };
        let Some(tree) = agent
            .agent_id
            .as_deref()
            .and_then(|agent_id| self.agent_store.agent(agent_id))
        else {
            return false;
        };
        let selected = agent
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        tree.is_ancestor_head(owner, selected)
    }

    /// Queue one exact committed activation obligation.
    ///
    /// Comparable obligations remain distinct here. A later committed successor
    /// acknowledges every covered watermark on its selected branch.
    pub(crate) fn enqueue_committed_activation_dispatch(
        &mut self,
        cid: AgentId,
        activation_cut: Option<tau_proto::AgentHead>,
        activation_through: Option<tau_proto::AgentHead>,
    ) {
        self.pending_publish_idle_dispatches
            .retain(|deferred| deferred.cid != cid || deferred.obligation.is_committed());
        self.pending_publish_idle_dispatches
            .push_back(DeferredPromptDispatch {
                cid,
                activation_cut,
                activation_through,
                activation_source_seq: None,
                obligation: DeferredActivationObligation::Committed,
            });
    }

    /// Queue one committed activation by durable occurrence until it
    /// materializes.
    pub(crate) fn enqueue_committed_activation_occurrence(
        &mut self,
        cid: AgentId,
        source_seq: tau_core::PersistedAgentEventSeq,
        activation_through: Option<tau_proto::AgentHead>,
    ) {
        let activation_cut = activation_through.and_then(|_| {
            self.agents
                .get(&cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| self.agent_store.agent(agent_id))
                .and_then(|tree| {
                    tree.node_for_durable_event_seq(source_seq)
                        .and_then(|node_id| tree.node(node_id))
                })
                .map(|node| {
                    node.parent_id
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
                })
        });
        self.pending_publish_idle_dispatches
            .retain(|deferred| deferred.cid != cid || deferred.obligation.is_committed());
        self.pending_publish_idle_dispatches
            .push_back(DeferredPromptDispatch {
                cid,
                activation_cut,
                activation_through,
                activation_source_seq: Some(source_seq),
                obligation: DeferredActivationObligation::Committed,
            });
    }

    /// Retire the exact committed activation that cannot materialize.
    ///
    /// Ordinary deferred dispatches and other branch watermarks remain
    /// retryable.
    pub(super) fn retire_deferred_activation(
        &mut self,
        cid: &AgentId,
        activation_through: Option<tau_proto::AgentHead>,
    ) {
        self.pending_publish_idle_dispatches.retain(|deferred| {
            deferred.cid != *cid
                || !deferred.obligation.is_committed()
                || deferred.activation_through != activation_through
        });
    }

    /// Transfer every selected-branch activation obligation covered by a
    /// committed inference checkpoint or standalone-compaction start.
    pub(crate) fn acknowledge_deferred_activations_through(
        &mut self,
        cid: &AgentId,
        through: tau_proto::AgentHead,
    ) {
        let tree = self
            .agents
            .get(cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .and_then(|agent_id| self.agent_store.agent(agent_id));
        let Some(tree) = tree else {
            return;
        };
        self.pending_publish_idle_dispatches.retain(|deferred| {
            deferred.cid != *cid
                || !deferred.obligation.is_committed()
                || deferred
                    .activation_through
                    .is_none_or(|owner| !tree.is_ancestor_head(owner, through))
        });
    }

    /// Retire both possible runtime forms of one exact dormant output-length
    /// activation after its synthetic owner commits.
    pub(crate) fn retire_dormant_output_length_activation(
        &mut self,
        cid: &AgentId,
        activation_cut: tau_proto::AgentHead,
        steer: tau_proto::AgentHead,
    ) {
        self.pending_publish_idle_dispatches.retain(|deferred| {
            if deferred.cid != *cid {
                return true;
            }
            if deferred.obligation.is_committed() {
                return deferred.activation_through != Some(steer);
            }
            deferred.obligation != DeferredActivationObligation::OutputLengthPublishIdle
                || deferred
                    .activation_cut
                    .is_some_and(|cut| cut != activation_cut)
        });
    }

    /// Computes the closed provider prefix immediately before the selected
    /// head.
    ///
    /// Callers capture this only while the selected head is the activation's
    /// exact committed watermark. Returns `None` if the runtime agent is
    /// absent, selects Root, lacks a durable id, its tree is unloaded, or
    /// its selected node is missing from that tree.
    pub(crate) fn activation_cut_before_current_head(
        &self,
        cid: &AgentId,
    ) -> Option<tau_proto::AgentHead> {
        let agent = self.agents.get(cid)?;
        let head = agent.head?;
        let tree = self.agent_store.agent(agent.agent_id.as_deref()?)?;
        let provisional = tree
            .node(head)?
            .parent_id
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        Some(self.closed_provider_prefix_for_agent(agent.agent_id.as_deref()?, provisional))
    }

    /// Entry point for any publish call. Defers if interception is
    /// in flight; otherwise drives the publish through the
    /// interception chain and into the bus.
    pub(crate) fn enqueue_publish(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
        persist: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        self.enqueue_publish_inner(source, event, persist, must_pass, sync_head_for, None);
    }

    /// Enqueue a peer publication with its immutable frame-admission session.
    pub(super) fn enqueue_publish_with_admission(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
        persist: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
        admission: ExtensionFrameAdmission,
    ) {
        self.enqueue_publish_inner(
            source,
            event,
            persist,
            must_pass,
            sync_head_for,
            Some(admission),
        );
    }

    fn enqueue_publish_inner(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: Event,
        persist: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
        admission: Option<ExtensionFrameAdmission>,
    ) {
        let shell_report_targets_ephemeral = match &event {
            Event::ShellCommandProgressReported(progress) => Some(&progress.command_id),
            Event::ShellCommandFinishedReported(finished) => Some(&finished.command_id),
            _ => None,
        }
        .is_some_and(|command_id| self.ephemeral_ui_shell_route_ids.contains(command_id));
        let extension = source.and_then(|source_id| self.extensions.entries.get(source_id));
        let activation_reservation = extension
            .filter(|entry| entry.state != path_crate_extension::ExtensionState::Ready)
            .and_then(|_| {
                let declaration_family = match event {
                    Event::ProviderModelsDeclared(_) => ActivationDeclarationFamily::ProviderModels,
                    Event::ToolRegistrationDeclared(_) | Event::ToolUnregistrationDeclared(_) => {
                        ActivationDeclarationFamily::ToolLifecycle
                    }
                    Event::ActionSchemaDeclared(_) => ActivationDeclarationFamily::ActionSchema,
                    Event::ExtPromptFragmentPublish(_) => {
                        ActivationDeclarationFamily::PromptFragment
                    }
                    Event::ExtensionSessionContextProviderRegister(_)
                    | Event::ExtensionSessionDiscoverySnapshotDeclared(_) => {
                        ActivationDeclarationFamily::SessionDiscovery
                    }
                    Event::ExtensionContextProviderRegister(_)
                    | Event::ExtensionAgentDiscoverySnapshotDeclared(_)
                    | Event::ExtAgentContextPublish(_) => ActivationDeclarationFamily::AgentContext,
                    _ => return None,
                };
                Some(ActivationReservation {
                    encoded_bytes: Self::encoded_emit_size(&event, persist),
                    persist,
                    declaration_family,
                })
            });
        let peer_context = PeerPublicationContext {
            extension: extension.map(|entry| AuthenticatedExtensionPublication {
                publisher: entry.name.clone(),
                source: entry.connection_id.clone(),
                kind: entry.kind.clone(),
                capabilities: entry.peer_capabilities.clone(),
                instance_id: entry.instance_id,
                admission: admission.unwrap_or_else(|| ExtensionFrameAdmission {
                    session_id: self.current_session_id.clone(),
                    session_generation: self.current_session_generation,
                }),
                shell_report_targets_ephemeral,
                activation_reservation,
            }),
        };
        let source = PublicationSource {
            connection_id: source.cloned(),
            peer_context,
        };
        if self.pending_intercept.is_some() {
            self.deferred_publishes.push_back(DeferredPublish {
                source,
                event,
                persist,
                must_pass,
                sync_head_for,
            });
            return;
        }
        self.dispatch_publish_step(source, event, persist, must_pass, sync_head_for, None);
    }

    /// Return the encoded input-envelope size charged for one emitted event.
    pub(super) fn encoded_emit_size(event: &Event, persist: bool) -> usize {
        let mut encoded = Vec::new();
        ciborium::into_writer(
            &tau_proto::HarnessInputMessage::emit_with_persist(event.clone(), persist),
            &mut encoded,
        )
        .expect("an admitted event remains encodable");
        encoded.len()
    }

    /// One step through the interception chain for a single publish.
    ///
    /// `cursor` is `None` on the first dispatch and `Some` on subsequent steps
    /// so lookup advances strictly past the interceptor that just replied.
    /// Exact registrations are considered before prefix registrations; once
    /// exact registrations are exhausted, prefix lookup starts with an
    /// independent full-registration cursor. If a matching interceptor is
    /// found and the request is delivered, the publish parks in
    /// `pending_intercept` waiting for its reply. If delivery fails, that
    /// interceptor is removed/skipped and the chain continues. If no
    /// further interceptor matches, the event commits.
    fn dispatch_publish_step(
        &mut self,
        source: PublicationSource,
        event: Event,
        persist: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
        mut cursor: Option<InterceptorCursor>,
    ) {
        loop {
            let Some(interceptor_match) = self.interceptors.next_for(&event, cursor.as_ref())
            else {
                self.commit_event(
                    source.connection_id.as_ref(),
                    &source.peer_context,
                    event,
                    persist,
                    sync_head_for,
                );
                return;
            };
            let interceptor = interceptor_match.registration;
            if self
                .suspended_interceptor_connections
                .contains(&interceptor.connection_id)
            {
                cursor = Some(InterceptorCursor {
                    set: interceptor_match.set,
                    registration: interceptor,
                });
                continue;
            }
            tracing::debug!(
                target: "tau_harness::interception",
                event = %event.name(),
                priority = interceptor.priority.get(),
                component = %interceptor.component_name,
                connection_id = %interceptor.connection_id,
                "intercepting event emission"
            );
            let conn_id = interceptor.connection_id.to_owned();
            let report = self.bus.send_to(
                &conn_id,
                None,
                HarnessOutputMessage::InterceptRequest(InterceptRequest {
                    event: Box::new(event.clone()),
                    persist,
                }),
            );
            let delivered = report
                .as_ref()
                .is_ok_and(|report| report.delivered_to.iter().any(|id| id == &conn_id));
            if delivered {
                self.pending_intercept = Some(PendingIntercept {
                    conn_id: conn_id.clone(),
                    event,
                    persist,
                    source,
                    must_pass,
                    sync_head_for,
                    cursor: InterceptorCursor {
                        set: interceptor_match.set,
                        registration: interceptor,
                    },
                });
                return;
            }
            tracing::warn!(
                target: "tau_harness::interception",
                event = %event.name(),
                connection_id = %conn_id,
                error = ?report.err(),
                "interceptor request delivery failed; skipping interceptor"
            );
            self.interceptors.remove_connection(&conn_id);
            cursor = Some(InterceptorCursor {
                set: interceptor_match.set,
                registration: interceptor,
            });
        }
    }

    /// Resolve a parked interception with the extension's reply.
    /// Advances the chain (next interceptor, or commit), then drains publishes
    /// that arrived while waiting until completion or a downstream failure.
    ///
    /// # Errors
    ///
    /// Returns an error when committing the resolved publish or a deferred
    /// publish triggers a fatal extension-activation failure.
    pub(crate) fn handle_intercept_reply(
        &mut self,
        conn_id: &tau_proto::ConnectionId,
        reply: InterceptReply,
    ) -> Result<(), crate::HarnessError> {
        if self.suspended_interceptor_connections.remove(conn_id) {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = %conn_id,
                "consuming stale reply for destructively canceled intercept request"
            );
            return Ok(());
        }
        let Some(pending) = self.pending_intercept.take() else {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = %conn_id,
                "InterceptReply received without a pending intercept; ignoring",
            );
            return Ok(());
        };
        if pending.conn_id != *conn_id {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = %conn_id,
                expected = %pending.conn_id,
                "InterceptReply from unexpected connection; ignoring and \
                 continuing to wait",
            );
            // Restore — we're still waiting on the original responder.
            self.pending_intercept = Some(pending);
            return Ok(());
        }
        self.advance_pending_intercept(pending, reply.action);
        self.take_pending_publish_error()?;
        self.drain_deferred_publishes();
        self.take_pending_publish_error()?;
        self.drain_publish_idle_dispatches();
        Ok(())
    }

    /// Resolve a pending intercept whose responder disconnected.
    /// Defaults to `Pass(None)` so the original event still flows —
    /// extensions cannot wedge the harness by going away mid-reply.
    pub(crate) fn fail_pending_intercept_for_disconnect(
        &mut self,
        conn_id: &tau_proto::ConnectionId,
    ) {
        let Some(pending) = self.pending_intercept.take() else {
            return;
        };
        if pending.conn_id != *conn_id {
            self.pending_intercept = Some(pending);
            return;
        }
        tracing::warn!(
            target: "tau_harness::interception",
            connection_id = %conn_id,
            "interceptor disconnected mid-reply; treating as Pass(None)",
        );
        self.advance_pending_intercept(pending, InterceptAction::Pass(None));
        if self.pending_publish_error.is_none() {
            self.drain_deferred_publishes();
            self.drain_publish_idle_dispatches();
        }
    }

    /// Apply an [`InterceptAction`] to a pending intercept and drive
    /// the next chain step (or commit, or drop).
    fn advance_pending_intercept(&mut self, pending: PendingIntercept, action: InterceptAction) {
        let PendingIntercept {
            conn_id: _,
            event: original_event,
            persist,
            source,
            must_pass,
            sync_head_for,
            cursor,
        } = pending;

        let event_name = original_event.name();
        let shell_progress_command_id = match &original_event {
            Event::ShellCommandProgress(progress) => Some(progress.command_id.clone()),
            _ => None,
        };
        let next_event = match action {
            InterceptAction::Pass(None) => Some(original_event),
            InterceptAction::Pass(Some(boxed)) => {
                let mut new_event = *boxed;
                if new_event.name() != event_name {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        original = %event_name,
                        replacement = %new_event.name(),
                        "interceptor returned a different event type; \
                         falling back to the original",
                    );
                    Some(original_event)
                } else if mandatory_harness_notice_was_modified(&original_event, &new_event) {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        event = %event_name,
                        "interceptor tried to modify a mandatory harness.notice; \
                         publishing original instead",
                    );
                    Some(original_event)
                } else {
                    sanitize_harness_notice_replacement(&original_event, &mut new_event);
                    preserve_agent_metadata_mutation_id(&original_event, &mut new_event);
                    preserve_shell_command_identity(&original_event, &mut new_event);
                    if protected_prompt_fields_were_modified(&original_event, &new_event) {
                        tracing::warn!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            "interceptor tried to modify protected prompt fields; \
                             publishing original instead",
                        );
                        Some(original_event)
                    } else if immutable_protected_fact_was_modified(&original_event, &new_event) {
                        tracing::warn!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            "interceptor tried to modify an immutable protected fact; \
                             publishing original instead",
                        );
                        Some(original_event)
                    } else if invalid_tool_request_replacement(&new_event) {
                        tracing::warn!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            "interceptor returned a tool request with an empty call id; \
                             publishing original instead",
                        );
                        Some(original_event)
                    } else if let Err(error) =
                        self.validate_agent_metadata_interceptor_replacement(&new_event)
                    {
                        tracing::warn!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            %error,
                            "interceptor returned invalid agent metadata; \
                             publishing original instead",
                        );
                        Some(original_event)
                    } else {
                        Some(new_event)
                    }
                }
            }
            InterceptAction::Drop => {
                if Harness::pending_external_receive_message_id(&original_event)
                    .is_some_and(|id| self.pending_external_receive_acks.contains_key(id))
                {
                    self.fail_pending_external_receive(
                        &original_event,
                        "peer receive projection was rejected by interception",
                        tau_proto::ExternalAgentMessageFailure::Rejected,
                    );
                    None
                } else {
                    let must_pass_default = event_is_effectively_must_pass(&original_event, false);
                    if event_is_effectively_must_pass(&original_event, must_pass) {
                        tracing::warn!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            must_pass_caller = must_pass,
                            must_pass_default = must_pass_default,
                            "interceptor tried to Drop a must-pass event; \
                             publishing original instead",
                        );
                        Some(original_event)
                    } else {
                        tracing::debug!(
                            target: "tau_harness::interception",
                            event = %event_name,
                            "interceptor dropped event",
                        );
                        None
                    }
                }
            }
        };

        let Some(event) = next_event else {
            if let Some(command_id) = shell_progress_command_id.as_ref() {
                self.discard_uncommitted_shell_canonical_marker(command_id);
            }
            self.discard_peer_activation_reservation(&source.peer_context);
            return;
        };

        self.dispatch_publish_step(
            source,
            event,
            persist,
            must_pass,
            sync_head_for,
            Some(cursor),
        );
    }

    /// Drain `deferred_publishes` until either it's empty or one of
    /// them parks a new intercept.
    fn drain_deferred_publishes(&mut self) {
        while self.pending_intercept.is_none() {
            let Some(deferred) = self.deferred_publishes.pop_front() else {
                break;
            };
            let DeferredPublish {
                source,
                event,
                persist,
                must_pass,
                sync_head_for,
            } = deferred;
            self.dispatch_publish_step(source, event, persist, must_pass, sync_head_for, None);
        }
    }

    /// Dispatch selected branch obligations only after publication becomes
    /// idle.
    ///
    /// Dormant sibling obligations remain queued. Not-ready agents, blocked
    /// ownership, and checkpoint ownership remain queued without consumption,
    /// while runnable obligations for other agents continue in the same bounded
    /// scan.
    /// A committed activation remains queued until its checkpoint or standalone
    /// start commits and acknowledges every covered selected-branch watermark;
    /// a rejected successor therefore remains retryable.
    pub(crate) fn drain_publish_idle_dispatches(&mut self) {
        let mut attempted = Vec::new();
        while self.publish_chain_is_idle() {
            for index in 0..self.pending_publish_idle_dispatches.len() {
                let needs_binding = self.pending_publish_idle_dispatches[index]
                    .obligation
                    .is_committed()
                    && self.pending_publish_idle_dispatches[index]
                        .activation_through
                        .is_none();
                if !needs_binding {
                    continue;
                }
                let cid = self.pending_publish_idle_dispatches[index].cid.clone();
                if let Some(source_seq) =
                    self.pending_publish_idle_dispatches[index].activation_source_seq
                {
                    let materialized = self
                        .agents
                        .get(&cid)
                        .and_then(|agent| agent.agent_id.as_deref())
                        .and_then(|agent_id| self.agent_store.agent(agent_id))
                        .and_then(|tree| {
                            tree.node_for_durable_event_seq(source_seq)
                                .and_then(|node_id| {
                                    tree.node(node_id).map(|node| (node_id, node.parent_id))
                                })
                        });
                    let Some((node_id, parent_id)) = materialized else {
                        continue;
                    };
                    self.pending_publish_idle_dispatches[index].activation_through =
                        Some(tau_proto::AgentHead::Node(node_id));
                    self.pending_publish_idle_dispatches[index].activation_cut = Some(
                        parent_id.map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    );
                    continue;
                }
                let through = self.selected_head_for_agent(&cid);
                let cut = self.activation_cut_before_current_head(&cid);
                self.pending_publish_idle_dispatches[index].activation_through = through;
                self.pending_publish_idle_dispatches[index].activation_cut = cut;
            }
            let Some(index) = self
                .pending_publish_idle_dispatches
                .iter()
                .enumerate()
                .find_map(|(index, deferred)| {
                    if attempted
                        .iter()
                        .any(|prior| Self::same_deferred_prompt_dispatch(prior, deferred))
                    {
                        return None;
                    }
                    self.deferred_prompt_dispatch_is_actionable(deferred)
                        .then_some(index)
                })
            else {
                break;
            };
            let deferred = self.pending_publish_idle_dispatches[index].clone();
            let cid = deferred.cid.clone();
            if !self.agents.contains_key(&cid) {
                self.pending_publish_idle_dispatches.remove(index);
                continue;
            }
            if !deferred.obligation.is_committed() {
                self.pending_publish_idle_dispatches.remove(index);
            }
            if deferred.obligation.is_committed()
                && self.schedule_standalone_auto_compaction_for_activation(
                    &cid,
                    true,
                    deferred
                        .activation_cut
                        .or_else(|| self.activation_cut_before_current_head(&cid)),
                )
            {
                if self.pending_publish_idle_dispatches.iter().any(|queued| {
                    queued.cid == cid
                        && queued.obligation.is_committed()
                        && queued.activation_through == deferred.activation_through
                }) {
                    // A synchronous persistence rejection left the branch
                    // obligation unclaimed. Stop this drain pass instead of
                    // immediately retrying the same failed successor forever.
                    break;
                }
                continue;
            }
            self.checkpoint_or_send_prompt(&cid, deferred.activation_cut);
            if deferred.obligation.is_committed()
                && self
                    .pending_publish_idle_dispatches
                    .iter()
                    .any(|queued| Self::same_deferred_prompt_dispatch(queued, &deferred))
            {
                attempted.push(deferred);
            }
        }
    }
}

#[cfg(test)]
mod tests;
