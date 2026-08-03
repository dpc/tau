//! Runtime state and correlation for the harness-owned wait tool.
//!
//! `WaitTracker` is the sole owner of wait state transitions. A call moves from
//! pending through one terminal state and may be consumed at most once.
//! `call_refs` and `terminal_observations` name durable declaration and
//! terminal occurrences; absence means correlation is unavailable, never a
//! synthetic zero identity. Completion order controls bare-wait delivery, while
//! terminal order only bounds tombstone retention. Ownership changes never
//! invent or rewrite durable identities.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use tau_proto::{
    AgentId, CborValue, ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolCallRef,
    ToolError, ToolName, ToolResult, ToolResultKind, ToolType, ToolUseState, ToolUseStatus,
};

use super::WAIT_TOOL_NAME;

/// Bound retained terminal tombstones while preserving recent duplicate-wait
/// diagnostics and call-ID reuse behavior.
const MAX_WAIT_TERMINAL_TOMBSTONES: usize = 1024;
const ORIGINAL_TOOL_CALL_ID_HEADER: &str = "original_tool_call_id";
const NO_BACKGROUND_WAIT_CANDIDATES: &str = "no background tool calls are running or completed in this conversation; use `wait({\"timeout_minutes\": N})` with a positive integer N to wait for new activating input";
const MAX_INPUT_WAIT_MINUTES: i128 = 24 * 60;

/// Render the normalized input-wait timeout for tool display state.
pub(super) fn wait_timeout_args(timeout: Duration) -> String {
    format!("{}m", timeout.as_secs() / 60)
}

/// Parsed mutually exclusive wait mode.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum WaitTarget {
    /// Wait for one display call ID.
    Exact(
        /// Target display call ID.
        ToolCallId,
    ),
    /// Wait for the oldest owned background completion.
    AnyBackground,
    /// Wait for activating input until the bounded duration elapses.
    AnyInput(
        /// Effective clamped timeout.
        Duration,
    ),
}

/// Runtime lifecycle for one non-wait call.
#[derive(Clone, Debug, PartialEq)]
enum WaitCallState {
    /// Dispatched but not backgrounded or terminal.
    Pending,
    /// Provider round closed with a background placeholder.
    Backgrounded,
    /// Completed in the foreground before any waiter consumed it.
    NormalReturned,
    /// Unconsumed background success.
    BackgroundResult(
        /// Retained source result.
        ToolBackgroundResult,
    ),
    /// Unconsumed background failure or cancellation.
    BackgroundError(
        /// Retained source error.
        ToolBackgroundError,
    ),
    /// Terminal was delivered or deliberately retired.
    Consumed,
}

/// Installed wait invocation and its durable correlation identities.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WaitRequest {
    /// Wait tool display call ID.
    pub(super) call_id: ToolCallId,
    /// Visible wait tool name.
    pub(super) tool_name: ToolName,
    /// Runtime agent that owns the wait.
    pub(super) owner: AgentId,
    /// Empty for exact/bare waits; normalized and bounded `Nm` for input waits.
    pub(super) display_args: String,
    /// Exact declaration of this wait invocation, when provider-declared.
    pub(super) call_ref: Option<ToolCallRef>,
    /// Observation identity allocated before wait resolution.
    pub(super) wait_observation: Option<tau_proto::ObservationId>,
    /// Registration observation allocated when the runtime installs this
    /// waiter.
    pub(super) registration: Option<tau_proto::ObservationId>,
}

/// Installed activating-input wait plus its monotonic deadline.
#[derive(Clone, Debug, PartialEq)]
struct InputWaitRequest {
    /// Common wait invocation and durable identities.
    request: WaitRequest,
    /// Effective bounded deadline.
    deadline: Instant,
}

/// First ordinary-arbitration event retained while a compaction claim is
/// exclusive, for use only if canonical cancellation append rolls back.
#[derive(Clone, Debug, PartialEq)]
enum ClaimedWaitWake {
    /// Activating input won ordinary arbitration while the claim was held.
    Activation(tau_proto::ObservationId),
    /// A matching background completion won ordinary arbitration.
    Completion(
        /// Source call whose completion won.
        ToolCallId,
    ),
    /// The activating-input deadline won ordinary arbitration.
    Timeout,
}

/// Installed waiter held exclusively while manual compaction publishes its
/// canonical cancellation terminal.
#[derive(Clone, Debug, PartialEq)]
enum ClaimedWait {
    /// Exact wait, including the source call whose notification was suppressed.
    Exact {
        /// Source call awaited by the claimed request.
        target: ToolCallId,
        /// Claimed wait request.
        request: WaitRequest,
        /// First activation, completion, or timeout observed while claimed.
        wake: Option<ClaimedWaitWake>,
    },
    /// Bare background-completion wait.
    AnyBackground {
        /// Claimed wait request.
        request: WaitRequest,
        /// First activation, completion, or timeout observed while claimed.
        wake: Option<ClaimedWaitWake>,
    },
    /// Activating-input wait, including its original deadline.
    Input {
        /// Claimed input waiter.
        wait: InputWaitRequest,
        /// First activation, completion, or timeout observed while claimed.
        wake: Option<ClaimedWaitWake>,
    },
}

impl ClaimedWait {
    /// Return the common claimed wait request.
    fn request(&self) -> &WaitRequest {
        match self {
            Self::Exact {
                target: _,
                request,
                wake: _,
            }
            | Self::AnyBackground { request, wake: _ } => request,
            Self::Input { wait, wake: _ } => &wait.request,
        }
    }
}

/// Runtime terminal reply for a wait invocation.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum WaitReplyKind {
    /// Successful wait tool result.
    Result {
        /// Model-visible result payload.
        result: CborValue,
        /// Optional UI state.
        display: Option<ToolUseState>,
    },
    /// Failed or interrupted wait tool result.
    Error {
        /// Model-visible error.
        message: String,
        /// Optional structured details.
        details: Option<CborValue>,
        /// Optional UI state.
        display: Option<ToolUseState>,
    },
}

/// One terminal wait reply plus source suppression changes.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct WaitReply {
    /// Wait call receiving the terminal.
    pub(super) wait_call_id: ToolCallId,
    /// Visible wait tool name.
    pub(super) wait_tool_name: ToolName,
    /// Result or error payload.
    pub(super) kind: WaitReplyKind,
    /// Source completion whose passive prompt must be suppressed.
    pub(super) suppress_call_id: Option<ToolCallId>,
    /// Source completion whose passive prompt must be restored.
    pub(super) unsuppress_call_id: Option<ToolCallId>,
    /// Content-free causal settlement emitted after terminal publication.
    pub(super) settlement: Option<PendingWaitSettlement>,
}

/// Runtime correlation retained until the wait terminal identity is allocated.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingWaitSettlement {
    /// Observation of the parsed wait invocation.
    pub(crate) wait_observation: tau_proto::ObservationId,
    /// Exact declared wait call.
    pub(crate) wait_call: ToolCallRef,
    /// Installed registration, absent for immediate outcomes.
    pub(crate) registration: Option<tau_proto::ObservationId>,
    /// Typed runtime outcome.
    pub(crate) outcome: tau_proto::ToolWaitOutcome,
}

/// Effects of invoking a wait before terminal publication.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) struct WaitStart {
    /// Immediate terminal reply, absent for an installed waiter.
    pub(super) reply: Option<WaitReply>,
    /// Source completion prompt to suppress.
    pub(super) suppress_call_id: Option<ToolCallId>,
    /// Registration emitted only after the waiter was actually installed.
    pub(super) registration: Option<(tau_proto::ObservationId, tau_proto::AgentToolWaitRegistered)>,
}

/// Effects of cancelling runtime calls and installed waiters.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) struct WaitCancel {
    /// Wait replies made ready by cancellation.
    pub(super) replies: Vec<WaitReply>,
    /// Source prompts restored after waiter removal.
    pub(super) unsuppress_call_ids: Vec<ToolCallId>,
    /// Source prompts suppressed by delivered cancellation.
    pub(super) suppress_call_ids: Vec<ToolCallId>,
    /// Cancelled wait calls awaiting their canonical terminal identity.
    pub(super) cancelled_waits: Vec<WaitRequest>,
}

/// Runtime wait state machine and bounded durable-correlation cache.
#[derive(Default)]
pub(super) struct WaitTracker {
    /// Runtime state by display call ID.
    calls: HashMap<ToolCallId, WaitCallState>,
    /// Exact waiters by source display call ID.
    waiters: HashMap<ToolCallId, WaitRequest>,
    /// Bare waiters by owning agent.
    any_waiters: HashMap<AgentId, WaitRequest>,
    /// Activating-input waiters by owning agent.
    input_waiters: HashMap<AgentId, InputWaitRequest>,
    /// Owner-scoped wait registrations claimed by manual compaction.
    claimed_waits: HashMap<AgentId, ClaimedWait>,
    /// Runtime call ownership.
    call_owners: HashMap<ToolCallId, AgentId>,
    /// Source tool names retained for wait display.
    call_tool_names: HashMap<ToolCallId, ToolName>,
    /// Exact provider declaration for each tracked runtime call.
    call_refs: HashMap<ToolCallId, ToolCallRef>,
    /// Canonical terminal identity retained for immediate and active delivery.
    terminal_observations: HashMap<ToolCallId, tau_proto::ObservationId>,
    /// Oldest-first deliverable completion order.
    completion_order: VecDeque<ToolCallId>,
    /// Oldest-first bounded order for `NormalReturned` and `Consumed` states.
    terminal_order: VecDeque<ToolCallId>,
}

impl WaitTracker {
    /// Return owners that currently have at least one installed, unclaimed
    /// wait.
    pub(super) fn installed_wait_owners(&self) -> HashSet<AgentId> {
        self.waiters
            .values()
            .map(|wait| wait.owner.clone())
            .chain(self.any_waiters.keys().cloned())
            .chain(self.input_waiters.keys().cloned())
            .chain(self.claimed_waits.keys().cloned())
            .collect()
    }

    /// Atomically claim the installed wait matching this owner and call.
    ///
    /// The harness caller must separately establish that this call is the sole
    /// remaining foreground call before invoking this registration-only check.
    pub(super) fn claim_wait_for_manual_compaction(
        &mut self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        if self.claimed_waits.contains_key(owner) {
            return false;
        }
        let exact_target = self.waiters.iter().find_map(|(target, wait)| {
            (&wait.owner == owner && &wait.call_id == call_id).then(|| target.clone())
        });
        let claimed = if let Some(target) = exact_target {
            self.waiters
                .remove(&target)
                .map(|request| ClaimedWait::Exact {
                    target,
                    request,
                    wake: None,
                })
        } else if self
            .any_waiters
            .get(owner)
            .is_some_and(|wait| &wait.call_id == call_id)
        {
            self.any_waiters
                .remove(owner)
                .map(|request| ClaimedWait::AnyBackground {
                    request,
                    wake: None,
                })
        } else if self
            .input_waiters
            .get(owner)
            .is_some_and(|wait| &wait.request.call_id == call_id)
        {
            self.input_waiters
                .remove(owner)
                .map(|wait| ClaimedWait::Input { wait, wake: None })
        } else {
            None
        };
        if let Some(claimed) = claimed {
            self.claimed_waits.insert(owner.clone(), claimed);
            true
        } else {
            false
        }
    }

    /// Restore a provisionally claimed wait after its cancellation terminal
    /// failed to append.
    pub(super) fn rollback_manual_compaction_claim(
        &mut self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) -> Vec<WaitReply> {
        let Some(claimed) = self.claimed_waits.remove(owner) else {
            return Vec::new();
        };
        if &claimed.request().call_id != call_id {
            self.claimed_waits.insert(owner.clone(), claimed);
            return Vec::new();
        }
        let wake = match claimed {
            ClaimedWait::Exact {
                target,
                request,
                wake,
            } => {
                self.waiters.insert(target, request);
                wake
            }
            ClaimedWait::AnyBackground { request, wake } => {
                self.any_waiters.insert(owner.clone(), request);
                wake
            }
            ClaimedWait::Input { wait, wake } => {
                self.input_waiters.insert(owner.clone(), wait);
                wake
            }
        };
        match wake {
            Some(ClaimedWaitWake::Activation(activation)) => {
                self.activate_waits_for(owner, activation)
            }
            Some(ClaimedWaitWake::Completion(target)) => {
                self.settle_restored_completion(owner, &target)
            }
            Some(ClaimedWaitWake::Timeout) => self.settle_restored_timeout(owner),
            None => self.expire_input_waits(Instant::now()),
        }
    }

    /// Report whether this owner already has the named wait claimed.
    pub(super) fn wait_claimed_for_manual_compaction(
        &self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        self.claimed_waits
            .get(owner)
            .is_some_and(|wait| &wait.request().call_id == call_id)
    }

    /// Replace correlation for a newly dispatched declaration occurrence.
    pub(super) fn reset_call_ref(&mut self, call_id: ToolCallId, call_ref: ToolCallRef) {
        self.terminal_observations.remove(&call_id);
        self.remove_completed(&call_id);
        self.call_refs.insert(call_id, call_ref);
    }

    /// Retain a declaration occurrence without changing runtime call state.
    pub(super) fn retain_call_ref(&mut self, call_id: ToolCallId, call_ref: ToolCallRef) {
        self.call_refs.insert(call_id, call_ref);
    }

    /// Return the retained declaration occurrence.
    pub(super) fn call_ref(&self, call_id: &ToolCallId) -> Option<ToolCallRef> {
        self.call_refs.get(call_id).copied()
    }

    /// Return the retained canonical terminal occurrence.
    pub(super) fn terminal_observation(
        &self,
        call_id: &ToolCallId,
    ) -> Option<tau_proto::ObservationId> {
        self.terminal_observations.get(call_id).copied()
    }

    /// Report whether runtime state contains this call.
    #[cfg(test)]
    pub(super) fn tracks_call(&self, call_id: &ToolCallId) -> bool {
        self.calls.contains_key(call_id)
    }

    /// Report whether an agent owns an installed activating-input waiter.
    #[cfg(test)]
    pub(super) fn input_wait_pending_for(&self, owner: &AgentId) -> bool {
        self.input_waiters.contains_key(owner)
    }

    /// Return the retained source tool name for wait interruption display.
    pub(super) fn call_tool_name(&self, call_id: &ToolCallId) -> Option<ToolName> {
        self.call_tool_names.get(call_id).cloned()
    }

    fn attach_completion_settlement(
        &self,
        reply: WaitReply,
        wait: &WaitRequest,
        source_call_id: &ToolCallId,
        source_phase: tau_proto::ToolSourcePhase,
        envelope: tau_proto::ToolOutputEnvelope,
    ) -> WaitReply {
        let Some(source_call) = self.call_refs.get(source_call_id).copied() else {
            return reply;
        };
        let Some(source_terminal) = self.terminal_observations.get(source_call_id).copied() else {
            return reply;
        };
        reply.with_settlement(
            wait,
            tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                source_phase,
                envelope,
            },
        )
    }

    /// Start tracking a non-wait call and clear stale state for a reused
    /// display ID.
    pub(super) fn record_tool_invoke(
        &mut self,
        call_id: ToolCallId,
        tool_name: ToolName,
        owner: AgentId,
    ) {
        if tool_name.as_str() != WAIT_TOOL_NAME {
            self.call_tool_names
                .insert(call_id.clone(), tool_name.clone());
            self.call_owners.insert(call_id.clone(), owner);
            self.terminal_observations.remove(&call_id);
            self.remove_completed(&call_id);
            self.terminal_order.retain(|terminal| terminal != &call_id);
            self.calls.insert(call_id, WaitCallState::Pending);
        }
    }

    /// Parse and resolve a wait immediately or install exactly one owner-scoped
    /// waiter.
    #[cfg(test)]
    pub(super) fn handle_wait_invoke(
        &mut self,
        owner: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        arguments: &CborValue,
        wait_observation: Option<tau_proto::ObservationId>,
    ) -> WaitStart {
        self.handle_wait_invoke_at(
            owner,
            call_id,
            tool_name,
            arguments,
            Instant::now(),
            wait_observation,
        )
    }

    /// Resolve a wait invocation at the supplied monotonic `now`, returning an
    /// immediate outcome or installing a waiter whose input timeout derives
    /// from that exact clock value.
    pub(super) fn handle_wait_invoke_at(
        &mut self,
        owner: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        arguments: &CborValue,
        now: Instant,
        wait_observation: Option<tau_proto::ObservationId>,
    ) -> WaitStart {
        let mut wait = WaitRequest {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            owner: owner.clone(),
            display_args: String::new(),
            call_ref: self.call_refs.get(&call_id).copied(),
            wait_observation,
            registration: None,
        };
        let target = match parse_wait_args(arguments) {
            Ok(target) => target,
            Err(message) => {
                let reply = wait_error_reply(call_id, tool_name, message, Some(arguments.clone()))
                    .with_settlement(
                        &wait,
                        tau_proto::ToolWaitOutcome::Rejected {
                            reason: tau_proto::WaitRejectionReason::InvalidArguments,
                        },
                    );
                return WaitStart::reply(reply);
            }
        };
        wait.display_args = match &target {
            WaitTarget::AnyInput(timeout) => wait_timeout_args(*timeout),
            _ => String::new(),
        };
        match target {
            WaitTarget::Exact(target) => self.start_exact_wait(target, wait),
            WaitTarget::AnyBackground => self.start_any_wait(owner.clone(), wait),
            WaitTarget::AnyInput(timeout) => {
                self.start_input_wait(owner.clone(), wait, now + timeout, timeout)
            }
        }
    }

    fn start_exact_wait(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        if !self.call_is_owned_by(&target, &wait.owner) {
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                format!("unknown tool call: `{target}`"),
                None,
            )
            .with_settlement(
                &wait,
                tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::UnknownTarget,
                },
            );
            return WaitStart::reply(reply);
        }
        if self.waiters.contains_key(&target) {
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                "existing wait for this tool already in progress".to_owned(),
                None,
            )
            .with_settlement(
                &wait,
                tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::DuplicateExactWait,
                },
            );
            return WaitStart::reply(reply);
        }
        let state = self.calls.remove(&target);
        match state {
            Some(WaitCallState::Pending) => {
                self.calls.insert(target.clone(), WaitCallState::Pending);
                let mut wait = wait;
                let Some(target_ref) = self.call_refs.get(&target).copied() else {
                    self.waiters.insert(target, wait);
                    return WaitStart::default();
                };
                let start = self.registered_start(
                    &mut wait,
                    tau_proto::ToolWaitMode::Exact { target: target_ref },
                );
                self.waiters.insert(target, wait);
                start
            }
            Some(WaitCallState::Backgrounded) => {
                self.calls
                    .insert(target.clone(), WaitCallState::Backgrounded);
                let mut wait = wait;
                let mut start = self.call_refs.get(&target).copied().map_or_else(
                    WaitStart::default,
                    |target| {
                        self.registered_start(&mut wait, tau_proto::ToolWaitMode::Exact { target })
                    },
                );
                self.waiters.insert(target.clone(), wait);
                start.suppress_call_id = Some(target);
                start
            }
            Some(WaitCallState::NormalReturned) => {
                self.record_terminal_state(target.clone(), WaitCallState::Consumed);
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                let reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    format!("Tool call {target} returned normally, not backgrounded"),
                    None,
                )
                .with_source_display(source_tool_name, None)
                .with_settlement(
                    &wait,
                    tau_proto::ToolWaitOutcome::Rejected {
                        reason: tau_proto::WaitRejectionReason::TargetReturnedForegroundBeforeWait,
                    },
                );
                WaitStart::reply(reply)
            }
            Some(WaitCallState::BackgroundResult(result)) => {
                self.record_terminal_state(target.clone(), WaitCallState::Consumed);
                self.remove_completed(&target);
                let source_tool_name = Some(result.tool_name.clone());
                let reply = wait_result_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    source_tool_name,
                    result.result,
                    result.display,
                );
                let reply = self.attach_completion_settlement(
                    reply,
                    &wait,
                    &target,
                    tau_proto::ToolSourcePhase::Background,
                    tau_proto::ToolOutputEnvelope::Identity,
                );
                WaitStart::reply_with_suppress(reply, target)
            }
            Some(WaitCallState::BackgroundError(error)) => {
                self.record_terminal_state(target.clone(), WaitCallState::Consumed);
                self.remove_completed(&target);
                let source_tool_name = Some(error.tool_name.clone());
                let reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    error.message,
                    error.details,
                )
                .with_source_display(source_tool_name, error.display);
                let reply = self.attach_completion_settlement(
                    reply,
                    &wait,
                    &target,
                    tau_proto::ToolSourcePhase::Background,
                    tau_proto::ToolOutputEnvelope::Identity,
                );
                WaitStart::reply_with_suppress(reply, target)
            }
            Some(WaitCallState::Consumed) => {
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                let reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    format!("result for tool call `{target}` already consumed"),
                    None,
                )
                .with_source_display(source_tool_name, None)
                .with_settlement(
                    &wait,
                    tau_proto::ToolWaitOutcome::Rejected {
                        reason: tau_proto::WaitRejectionReason::ResultAlreadyConsumed,
                    },
                );
                WaitStart::reply(reply)
            }
            None => {
                let reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    format!("unknown tool call: `{target}`"),
                    None,
                )
                .with_settlement(
                    &wait,
                    tau_proto::ToolWaitOutcome::Rejected {
                        reason: tau_proto::WaitRejectionReason::UnknownTarget,
                    },
                );
                WaitStart::reply(reply)
            }
        }
    }

    /// Return whether the runtime call belongs to the given agent.
    pub(super) fn call_is_owned_by(&self, call_id: &ToolCallId, owner: &AgentId) -> bool {
        self.call_owners.get(call_id) == Some(owner)
    }

    /// Return whether an owned call has one unconsumed completion.
    pub(super) fn completed_call_is_owned_by(&self, call_id: &ToolCallId, owner: &AgentId) -> bool {
        self.call_is_owned_by(call_id, owner) && self.is_completed(call_id)
    }

    /// Consume one completed call after an owning control flow delivered its
    /// terminal payload without a model-declared wait.
    pub(super) fn consume_completed_call(&mut self, call_id: &ToolCallId) {
        self.remove_completed(call_id);
        if self.is_completed(call_id) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
        }
    }

    fn start_any_wait(&mut self, owner: AgentId, wait: WaitRequest) -> WaitStart {
        if self.any_waiters.contains_key(&owner) {
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                "existing wait for a background tool call in this conversation already in progress"
                    .to_owned(),
                None,
            )
            .with_settlement(
                &wait,
                tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::DuplicateAnyWait,
                },
            );
            return WaitStart::reply(reply);
        }
        if let Some(target) = self.oldest_completed_for_owner(&owner) {
            return self.consume_completed_for_any(target, wait);
        }
        if self.has_running_background_for_owner(&owner) {
            let mut wait = wait;
            let start = self.registered_start(&mut wait, tau_proto::ToolWaitMode::NextBackground);
            self.any_waiters.insert(owner, wait);
            return start;
        }
        let reply = wait_error_reply(
            wait.call_id.clone(),
            wait.tool_name.clone(),
            NO_BACKGROUND_WAIT_CANDIDATES.to_owned(),
            None,
        )
        .with_settlement(
            &wait,
            tau_proto::ToolWaitOutcome::Rejected {
                reason: tau_proto::WaitRejectionReason::NoBackgroundCandidate,
            },
        );
        WaitStart::reply(reply)
    }

    fn start_input_wait(
        &mut self,
        owner: AgentId,
        wait: WaitRequest,
        deadline: Instant,
        timeout: Duration,
    ) -> WaitStart {
        if self.input_waiters.contains_key(&owner) {
            let mut reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                "existing input wait for this agent already in progress".to_owned(),
                None,
            );
            if let WaitReplyKind::Error { display, .. } = &mut reply.kind {
                *display = Some(ToolUseState {
                    args: wait.display_args.clone(),
                    status: ToolUseStatus::Error,
                    status_text: "existing input wait for this agent already in progress"
                        .to_owned(),
                    ..Default::default()
                });
            }
            let reply = reply.with_settlement(
                &wait,
                tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::DuplicateInputWait,
                },
            );
            return WaitStart::reply(reply);
        }
        let mut wait = wait;
        let effective_timeout_minutes = u16::try_from(timeout.as_secs() / 60).unwrap_or(u16::MAX);
        let start = self.registered_start(
            &mut wait,
            tau_proto::ToolWaitMode::ActivatingInput {
                effective_timeout_minutes,
            },
        );
        self.input_waiters.insert(
            owner,
            InputWaitRequest {
                request: wait,
                deadline,
            },
        );
        start
    }

    /// Allocate registration identity only for an installed provider-declared
    /// wait.
    fn registered_start(&self, wait: &mut WaitRequest, mode: tau_proto::ToolWaitMode) -> WaitStart {
        let Some(wait_call) = wait.call_ref else {
            return WaitStart::default();
        };
        wait.registration = Some(tau_proto::ObservationId::random());
        let Some(wait_observation) = wait.wait_observation else {
            return WaitStart::default();
        };
        WaitStart {
            registration: Some((
                wait.registration.expect("registration allocated"),
                tau_proto::AgentToolWaitRegistered {
                    wait_observation,
                    wait_call,
                    mode,
                },
            )),
            ..WaitStart::default()
        }
    }

    /// Return the earliest installed activating-input deadline.
    pub(super) fn next_input_wait_deadline(&self) -> Option<Instant> {
        self.input_waiters
            .values()
            .map(|wait| wait.deadline)
            .chain(
                self.claimed_waits
                    .values()
                    .filter_map(|claimed| match claimed {
                        ClaimedWait::Input { wait, wake: None } => Some(wait.deadline),
                        ClaimedWait::Input {
                            wait: _,
                            wake: Some(_),
                        }
                        | ClaimedWait::Exact {
                            target: _,
                            request: _,
                            wake: _,
                        }
                        | ClaimedWait::AnyBackground {
                            request: _,
                            wake: _,
                        } => None,
                    }),
            )
            .min()
    }

    /// Remove due input waiters and return one timeout reply for each.
    pub(super) fn expire_input_waits(&mut self, now: Instant) -> Vec<WaitReply> {
        for claimed in self.claimed_waits.values_mut() {
            if let ClaimedWait::Input { wait, wake } = claimed
                && wait.deadline <= now
                && wake.is_none()
            {
                *wake = Some(ClaimedWaitWake::Timeout);
            }
        }
        let due: Vec<AgentId> = self
            .input_waiters
            .iter()
            .filter(|(_, wait)| wait.deadline <= now)
            .map(|(owner, _)| owner.clone())
            .collect();
        due.into_iter()
            .filter_map(|owner| self.input_waiters.remove(&owner))
            .map(|wait| {
                wait_timed_out_reply(
                    wait.request.call_id.clone(),
                    wait.request.tool_name.clone(),
                    wait.request.display_args.clone(),
                )
                .with_settlement(&wait.request, tau_proto::ToolWaitOutcome::TimedOut)
            })
            .collect()
    }

    fn consume_completed_for_any(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        let Some(state) = self.calls.remove(&target) else {
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                format!("unknown tool call: `{target}`"),
                None,
            )
            .with_settlement(
                &wait,
                tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::UnknownTarget,
                },
            );
            return WaitStart::reply(reply);
        };
        self.remove_completed(&target);
        match state {
            WaitCallState::BackgroundResult(result) => {
                self.record_terminal_state(target.clone(), WaitCallState::Consumed);
                let source_tool_name = Some(result.tool_name.clone());
                let reply = wait_result_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    source_tool_name,
                    result_with_original_tool_call_id(&target, result.result),
                    result.display,
                );
                let reply = self.attach_completion_settlement(
                    reply,
                    &wait,
                    &target,
                    tau_proto::ToolSourcePhase::Background,
                    tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
                );
                WaitStart::reply_with_suppress(reply, target)
            }
            WaitCallState::BackgroundError(error) => {
                self.record_terminal_state(target.clone(), WaitCallState::Consumed);
                let source_tool_name = Some(error.tool_name.clone());
                let reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    error.message,
                    details_with_original_tool_call_id(&target, error.details),
                )
                .with_source_display(source_tool_name, error.display);
                let reply = self.attach_completion_settlement(
                    reply,
                    &wait,
                    &target,
                    tau_proto::ToolSourcePhase::Background,
                    tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
                );
                WaitStart::reply_with_suppress(reply, target)
            }
            other => {
                self.calls.insert(target.clone(), other);
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                WaitStart::reply(
                    wait_error_reply(
                        wait.call_id,
                        wait.tool_name,
                        format!("tool call `{target}` has no completed background result"),
                        None,
                    )
                    .with_source_display(source_tool_name, None),
                )
            }
        }
    }

    /// Record a foreground result and resolve any exact waiter with available
    /// correlation.
    pub(super) fn record_tool_result(
        &mut self,
        result: ToolResult,
        owner: AgentId,
        terminal: Option<tau_proto::ObservationId>,
    ) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        if let Some(terminal) = terminal {
            self.terminal_observations.insert(call_id.clone(), terminal);
        }
        self.call_tool_names
            .insert(call_id.clone(), result.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner);
        if self.is_consumed(&call_id) || self.is_backgrounded(&call_id) {
            return Vec::new();
        }
        if result.kind == ToolResultKind::BackgroundPlaceholder {
            self.calls.insert(call_id, WaitCallState::Backgrounded);
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            let source_tool_name = Some(result.tool_name.clone());
            let reply = wait_result_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                source_tool_name,
                result.result,
                result.display,
            );
            return vec![self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Foreground,
                tau_proto::ToolOutputEnvelope::Identity,
            )];
        }
        self.record_terminal_state(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    /// Record a foreground error and resolve any exact waiter with available
    /// correlation.
    pub(super) fn record_tool_error(
        &mut self,
        error: ToolError,
        owner: AgentId,
        terminal: Option<tau_proto::ObservationId>,
    ) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        if let Some(terminal) = terminal {
            self.terminal_observations.insert(call_id.clone(), terminal);
        }
        self.call_tool_names
            .insert(call_id.clone(), error.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner);
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            let source_tool_name = Some(error.tool_name.clone());
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                error.message,
                error.details,
            )
            .with_source_display(source_tool_name, error.display);
            return vec![self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Foreground,
                tau_proto::ToolOutputEnvelope::Identity,
            )];
        }
        self.record_terminal_state(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    /// Record a background result, then resolve the winning exact or bare
    /// waiter.
    pub(super) fn record_background_result(
        &mut self,
        result: ToolBackgroundResult,
        owner: AgentId,
        terminal: Option<tau_proto::ObservationId>,
    ) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        if let Some(terminal) = terminal {
            self.terminal_observations.insert(call_id.clone(), terminal);
        }
        self.call_tool_names
            .insert(call_id.clone(), result.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(result.tool_name.clone());
            let reply = wait_result_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                source_tool_name,
                result.result,
                result.display,
            )
            .with_suppress(call_id.clone());
            let reply = self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Background,
                tau_proto::ToolOutputEnvelope::Identity,
            );
            let mut replies = vec![reply];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let reply = wait_result_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                Some(result.tool_name.clone()),
                result_with_original_tool_call_id(&call_id, result.result),
                result.display,
            )
            .with_suppress(call_id.clone());
            return vec![self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Background,
                tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
            )];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundResult(result));
        self.push_completed(call_id.clone());
        self.note_claimed_completion(&owner, &call_id);
        Vec::new()
    }

    /// Record a background error, then resolve the winning exact or bare
    /// waiter.
    pub(super) fn record_background_error(
        &mut self,
        error: ToolBackgroundError,
        owner: AgentId,
        terminal: Option<tau_proto::ObservationId>,
    ) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        if let Some(terminal) = terminal {
            self.terminal_observations.insert(call_id.clone(), terminal);
        }
        self.call_tool_names
            .insert(call_id.clone(), error.tool_name.clone());
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(error.tool_name.clone());
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                error.message,
                error.details,
            )
            .with_source_display(source_tool_name, error.display)
            .with_suppress(call_id.clone());
            let reply = self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Background,
                tau_proto::ToolOutputEnvelope::Identity,
            );
            let mut replies = vec![reply];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let source_tool_name = Some(error.tool_name.clone());
            let reply = wait_error_reply(
                wait.call_id.clone(),
                wait.tool_name.clone(),
                error.message,
                details_with_original_tool_call_id(&call_id, error.details),
            )
            .with_source_display(source_tool_name, error.display)
            .with_suppress(call_id.clone());
            return vec![self.attach_completion_settlement(
                reply,
                &wait,
                &call_id,
                tau_proto::ToolSourcePhase::Background,
                tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader,
            )];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundError(error));
        self.push_completed(call_id.clone());
        self.note_claimed_completion(&owner, &call_id);
        Vec::new()
    }

    /// Retain the first completion that would settle a currently claimed wait.
    fn note_claimed_completion(&mut self, owner: &AgentId, call_id: &ToolCallId) {
        let Some(claimed) = self.claimed_waits.get_mut(owner) else {
            return;
        };
        let matches = match claimed {
            ClaimedWait::Exact {
                target,
                request: _,
                wake: _,
            } => target == call_id,
            ClaimedWait::AnyBackground {
                request: _,
                wake: _,
            } => true,
            ClaimedWait::Input { wait: _, wake: _ } => false,
        };
        if matches {
            match claimed {
                ClaimedWait::Exact {
                    target: _,
                    request: _,
                    wake,
                }
                | ClaimedWait::AnyBackground { request: _, wake } => {
                    if wake.is_none() {
                        *wake = Some(ClaimedWaitWake::Completion(call_id.clone()));
                    }
                }
                ClaimedWait::Input { wait: _, wake: _ } => {}
            }
        }
    }

    /// Settle the completion retained as the first rollback winner.
    fn settle_restored_completion(
        &mut self,
        owner: &AgentId,
        target: &ToolCallId,
    ) -> Vec<WaitReply> {
        match self.calls.get(target).cloned() {
            Some(WaitCallState::BackgroundResult(result)) => self.record_background_result(
                result,
                owner.clone(),
                self.terminal_observations.get(target).copied(),
            ),
            Some(WaitCallState::BackgroundError(error)) => self.record_background_error(
                error,
                owner.clone(),
                self.terminal_observations.get(target).copied(),
            ),
            _ => Vec::new(),
        }
    }

    /// Settle the input timeout retained as the first rollback winner.
    fn settle_restored_timeout(&mut self, owner: &AgentId) -> Vec<WaitReply> {
        self.input_waiters
            .remove(owner)
            .map(|wait| {
                wait_timed_out_reply(
                    wait.request.call_id.clone(),
                    wait.request.tool_name.clone(),
                    wait.request.display_args.clone(),
                )
                .with_settlement(&wait.request, tau_proto::ToolWaitOutcome::TimedOut)
            })
            .into_iter()
            .collect()
    }

    /// Retire cancelled calls and waits, returning replies and
    /// prompt-suppression changes.
    pub(super) fn record_tool_cancelled(
        &mut self,
        call_ids: &HashSet<ToolCallId>,
        terminal: Option<(&ToolCallId, tau_proto::ObservationId)>,
    ) -> WaitCancel {
        if call_ids.is_empty() {
            return WaitCancel::default();
        }
        if let Some((call_id, terminal)) = terminal {
            self.terminal_observations.insert(call_id.clone(), terminal);
        }

        let cancelled_owners: HashSet<AgentId> = call_ids
            .iter()
            .filter_map(|call_id| self.call_owners.get(call_id).cloned())
            .collect();
        let mut exact_consumed_cancelled = HashSet::new();
        let mut cancelled = WaitCancel::default();
        let claimed_owners: Vec<_> = self
            .claimed_waits
            .iter()
            .filter(|(_, wait)| call_ids.contains(&wait.request().call_id))
            .map(|(owner, _)| owner.clone())
            .collect();
        for owner in claimed_owners {
            let claimed = self
                .claimed_waits
                .remove(&owner)
                .expect("selected claimed wait exists");
            if let ClaimedWait::Exact {
                target,
                request: _,
                wake: _,
            } = &claimed
            {
                cancelled.unsuppress_call_ids.push(target.clone());
            }
            cancelled.cancelled_waits.push(match claimed {
                ClaimedWait::Exact {
                    target: _,
                    request,
                    wake: _,
                }
                | ClaimedWait::AnyBackground { request, wake: _ } => request,
                ClaimedWait::Input { wait, wake: _ } => wait.request,
            });
        }
        let input_waiters = std::mem::take(&mut self.input_waiters);
        for (owner, wait) in input_waiters {
            if call_ids.contains(&wait.request.call_id) {
                cancelled.cancelled_waits.push(wait.request);
            } else {
                self.input_waiters.insert(owner, wait);
            }
        }
        let waiters = std::mem::take(&mut self.waiters);
        for (target, wait) in waiters {
            let target_cancelled = call_ids.contains(&target);
            let wait_cancelled = call_ids.contains(&wait.call_id);
            let target_was_backgrounded = self.is_backgrounded(&target);

            if wait_cancelled {
                cancelled.cancelled_waits.push(wait);
                if target_was_backgrounded {
                    cancelled.unsuppress_call_ids.push(target.clone());
                }
                continue;
            }
            if target_cancelled {
                let source_tool_name = self.call_tool_names.get(&target).cloned();
                let mut reply = wait_error_reply(
                    wait.call_id.clone(),
                    wait.tool_name.clone(),
                    format!("Tool call `{target}` was cancelled"),
                    None,
                )
                .with_source_display(source_tool_name, None);
                if target_was_backgrounded {
                    reply = reply.with_unsuppress(target.clone());
                }
                reply = self.attach_completion_settlement(
                    reply,
                    &wait,
                    &target,
                    if target_was_backgrounded {
                        tau_proto::ToolSourcePhase::Background
                    } else {
                        tau_proto::ToolSourcePhase::Foreground
                    },
                    tau_proto::ToolOutputEnvelope::Identity,
                );
                exact_consumed_cancelled.insert(target.clone());
                cancelled.replies.push(reply);
            } else {
                self.waiters.insert(target, wait);
            }
        }

        for call_id in call_ids {
            if exact_consumed_cancelled.contains(call_id) {
                self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
                self.remove_completed(call_id);
            } else if self.is_backgrounded(call_id) {
                self.calls.insert(
                    call_id.clone(),
                    WaitCallState::BackgroundError(ToolBackgroundError {
                        call_id: call_id.clone(),
                        tool_name: self
                            .call_tool_names
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| ToolName::new("cancelled")),
                        tool_type: ToolType::Function,
                        message: "Tool call canceled".to_owned(),
                        details: None,
                        originator: tau_proto::PromptOriginator::User,

                        display: None,
                    }),
                );
                self.push_completed(call_id.clone());
            } else {
                self.record_terminal_state(call_id.clone(), WaitCallState::Consumed);
                self.remove_completed(call_id);
            }
        }

        let any_waiters = std::mem::take(&mut self.any_waiters);
        for (owner, wait) in any_waiters {
            if call_ids.contains(&wait.call_id) {
                cancelled.cancelled_waits.push(wait);
                continue;
            }
            if let Some(target) = self.oldest_completed_for_owner(&owner) {
                let start = self.consume_completed_for_any(target, wait);
                if let Some(call_id) = start.suppress_call_id {
                    cancelled.suppress_call_ids.push(call_id);
                }
                cancelled.replies.extend(start.reply);
            } else if self.has_running_background_for_owner(&owner) {
                self.any_waiters.insert(owner, wait);
            } else if cancelled_owners.contains(&owner) {
                let source_tool_name = call_ids.iter().find_map(|call_id| {
                    if self.call_owners.get(call_id) == Some(&owner) {
                        self.call_tool_names.get(call_id).cloned()
                    } else {
                        None
                    }
                });
                cancelled.replies.push(
                    wait_error_reply(
                        wait.call_id.clone(),
                        wait.tool_name.clone(),
                        "background tool call in this conversation was cancelled".to_owned(),
                        None,
                    )
                    .with_source_display(source_tool_name, None)
                    .with_settlement(
                        &wait,
                        tau_proto::ToolWaitOutcome::Rejected {
                            reason: tau_proto::WaitRejectionReason::NoBackgroundCandidate,
                        },
                    ),
                );
            } else {
                self.any_waiters.insert(owner, wait);
            }
        }

        cancelled
    }

    fn interrupt_active_waits_for(
        &mut self,
        owner: &AgentId,
        activation: tau_proto::ObservationId,
    ) -> Vec<WaitReply> {
        let targets: Vec<ToolCallId> = self
            .waiters
            .keys()
            .filter(|target| {
                self.waiters
                    .get(*target)
                    .is_some_and(|wait| &wait.owner == owner)
            })
            .cloned()
            .collect();
        let mut replies: Vec<WaitReply> = targets
            .into_iter()
            .filter_map(|target| {
                self.waiters
                    .remove(&target)
                    .map(|wait| self.interrupted_exact_wait_reply(target, wait, activation))
            })
            .collect();
        if let Some(wait) = self.any_waiters.remove(owner) {
            let reply = wait_interrupted_any_reply(wait.call_id.clone(), wait.tool_name.clone())
                .with_settlement(
                    &wait,
                    tau_proto::ToolWaitOutcome::InterruptedByActivation { activation },
                );
            replies.push(reply);
        }
        replies
    }

    /// Resolve installed waits preempted or satisfied by one activating input.
    pub(super) fn activate_waits_for(
        &mut self,
        owner: &AgentId,
        activation: tau_proto::ObservationId,
    ) -> Vec<WaitReply> {
        if let Some(claimed) = self.claimed_waits.get_mut(owner) {
            match claimed {
                ClaimedWait::Exact {
                    target: _,
                    request: _,
                    wake,
                }
                | ClaimedWait::AnyBackground { request: _, wake }
                | ClaimedWait::Input { wait: _, wake } => {
                    if wake.is_none() {
                        *wake = Some(ClaimedWaitWake::Activation(activation));
                    }
                }
            }
            return Vec::new();
        }
        let mut replies = self.interrupt_active_waits_for(owner, activation);
        if let Some(wait) = self.input_waiters.remove(owner) {
            let reply = wait_input_available_reply(
                wait.request.call_id.clone(),
                wait.request.tool_name.clone(),
                wait.request.display_args.clone(),
            )
            .with_settlement(
                &wait.request,
                tau_proto::ToolWaitOutcome::InputAvailable { activation },
            );
            replies.push(reply);
        }
        replies
    }

    /// Remove an input waiter without publishing a terminal reply.
    pub(super) fn discard_input_wait_for(&mut self, owner: &AgentId) {
        self.input_waiters.remove(owner);
    }

    fn interrupted_exact_wait_reply(
        &self,
        target: ToolCallId,
        wait: WaitRequest,
        activation: tau_proto::ObservationId,
    ) -> WaitReply {
        let source_tool_name = self.call_tool_names.get(&target).cloned();
        let mut reply = wait_interrupted_reply(
            wait.call_id.clone(),
            wait.tool_name.clone(),
            source_tool_name,
            &target,
        )
        .with_settlement(
            &wait,
            tau_proto::ToolWaitOutcome::InterruptedByActivation { activation },
        );
        if self.is_backgrounded(&target) {
            reply = reply.with_unsuppress(target);
        }
        reply
    }

    /// Retire every source call and installed wait owned by an unloading agent.
    ///
    /// Returns every retired source or wait call ID so the harness can clear
    /// its corresponding outer tool tracking.
    pub(super) fn discard_owner(&mut self, owner: &AgentId) -> Vec<ToolCallId> {
        let mut call_ids: Vec<_> = self
            .call_owners
            .iter()
            .filter_map(|(call_id, call_owner)| (call_owner == owner).then_some(call_id.clone()))
            .collect();
        call_ids.extend(
            self.waiters
                .values()
                .filter_map(|wait| (&wait.owner == owner).then_some(wait.call_id.clone())),
        );
        if let Some(wait) = self.any_waiters.remove(owner) {
            call_ids.push(wait.call_id);
        }
        if let Some(wait) = self.input_waiters.remove(owner) {
            call_ids.push(wait.request.call_id);
        }
        if let Some(wait) = self.claimed_waits.remove(owner) {
            call_ids.push(wait.request().call_id.clone());
        }
        self.waiters.retain(|_, wait| &wait.owner != owner);
        call_ids.sort();
        call_ids.dedup();
        for call_id in &call_ids {
            self.calls.remove(call_id);
            self.call_refs.remove(call_id);
            self.terminal_observations.remove(call_id);
            self.call_owners.remove(call_id);
            self.call_tool_names.remove(call_id);
            self.completion_order
                .retain(|completed| completed != call_id);
            self.terminal_order.retain(|terminal| terminal != call_id);
        }
        call_ids
    }

    fn finish_any_waiter_if_no_candidates(&mut self, owner: &AgentId) -> Vec<WaitReply> {
        if self.oldest_completed_for_owner(owner).is_some()
            || self.has_running_background_for_owner(owner)
        {
            return Vec::new();
        }
        let Some(wait) = self.any_waiters.remove(owner) else {
            return Vec::new();
        };
        let reply = wait_error_reply(
            wait.call_id.clone(),
            wait.tool_name.clone(),
            NO_BACKGROUND_WAIT_CANDIDATES.to_owned(),
            None,
        )
        .with_settlement(
            &wait,
            tau_proto::ToolWaitOutcome::Rejected {
                reason: tau_proto::WaitRejectionReason::NoBackgroundCandidate,
            },
        );
        vec![reply]
    }

    /// Return the oldest unconsumed background completion owned by an agent.
    pub(super) fn oldest_completed_for_owner(&self, owner: &AgentId) -> Option<ToolCallId> {
        self.completion_order.iter().find_map(|call_id| {
            (self.call_owners.get(call_id) == Some(owner) && self.is_completed(call_id))
                .then_some(call_id.clone())
        })
    }

    fn has_running_background_for_owner(&self, owner: &AgentId) -> bool {
        self.calls.iter().any(|(call_id, state)| {
            matches!(state, WaitCallState::Backgrounded)
                && self.call_owners.get(call_id) == Some(owner)
        })
    }

    fn push_completed(&mut self, call_id: ToolCallId) {
        if self
            .completion_order
            .iter()
            .all(|existing| existing != &call_id)
        {
            self.completion_order.push_back(call_id);
        }
    }

    fn remove_completed(&mut self, call_id: &ToolCallId) {
        self.completion_order.retain(|existing| existing != call_id);
    }

    fn record_terminal_state(&mut self, call_id: ToolCallId, state: WaitCallState) {
        debug_assert!(matches!(
            state,
            WaitCallState::NormalReturned | WaitCallState::Consumed
        ));
        self.terminal_order.retain(|terminal| terminal != &call_id);
        self.terminal_order.push_back(call_id.clone());
        self.calls.insert(call_id, state);
        while MAX_WAIT_TERMINAL_TOMBSTONES < self.terminal_order.len() {
            let retired = self
                .terminal_order
                .pop_front()
                .expect("terminal order exceeds zero");
            if self.calls.get(&retired).is_some_and(|state| {
                matches!(
                    state,
                    WaitCallState::NormalReturned | WaitCallState::Consumed
                )
            }) {
                self.calls.remove(&retired);
                self.call_refs.remove(&retired);
                self.terminal_observations.remove(&retired);
                self.call_owners.remove(&retired);
                self.call_tool_names.remove(&retired);
            }
        }
    }

    /// Return whether a call is currently waitable in the background.
    pub(super) fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Backgrounded))
    }

    fn is_completed(&self, call_id: &ToolCallId) -> bool {
        self.calls.get(call_id).is_some_and(|state| {
            matches!(
                state,
                WaitCallState::BackgroundResult(_) | WaitCallState::BackgroundError(_)
            )
        })
    }

    fn is_consumed(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Consumed))
    }
}

impl WaitReply {
    /// Attach a settlement only when the wait declaration and observation both
    /// exist.
    pub(super) fn with_settlement(
        mut self,
        wait: &WaitRequest,
        outcome: tau_proto::ToolWaitOutcome,
    ) -> Self {
        self.settlement =
            wait.call_ref
                .zip(wait.wait_observation)
                .map(|(wait_call, wait_observation)| PendingWaitSettlement {
                    wait_observation,
                    wait_call,
                    registration: wait.registration,
                    outcome,
                });
        self
    }
    fn with_source_display(
        mut self,
        source_tool_name: Option<ToolName>,
        display: Option<ToolUseState>,
    ) -> Self {
        if let WaitReplyKind::Error {
            message,
            display: dst,
            ..
        } = &mut self.kind
        {
            *dst = Some(wait_display_from_source(
                source_tool_name,
                display,
                ToolUseStatus::Error,
                wait_error_status_text(message),
            ));
        }
        self
    }

    fn with_suppress(mut self, call_id: ToolCallId) -> Self {
        self.suppress_call_id = Some(call_id);
        self
    }

    fn with_unsuppress(mut self, call_id: ToolCallId) -> Self {
        self.unsuppress_call_id = Some(call_id);
        self
    }
}

impl WaitStart {
    /// Construct an immediate wait start with no installed registration.
    pub(super) fn reply(reply: WaitReply) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: None,
            registration: None,
        }
    }

    fn reply_with_suppress(reply: WaitReply, call_id: ToolCallId) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: Some(call_id),
            registration: None,
        }
    }
}

fn wait_result_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    source_tool_name: Option<ToolName>,
    result: CborValue,
    display: Option<ToolUseState>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Result {
            result,
            display: Some(wait_display_from_source(
                source_tool_name,
                display,
                ToolUseStatus::Success,
                "ok".to_owned(),
            )),
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
        settlement: None,
    }
}

fn wait_display_from_source(
    source_tool_name: Option<ToolName>,
    display: Option<ToolUseState>,
    default_status: ToolUseStatus,
    default_status_text: String,
) -> ToolUseState {
    // The waited tool's descriptor describes the payload returned to the model.
    // Rendering that descriptor under the `wait` tool makes the UI surface
    // arbitrary command/path labels when the source tool happened to provide
    // them. Keep the source tool name plus completion severity for the `wait`
    // call itself.
    let (display_args, status, status_text) = display
        .map(|display| (display.args, display.status, display.status_text))
        .unwrap_or((String::new(), default_status, default_status_text));
    ToolUseState {
        args: source_tool_name
            .map(|tool_name| tool_name.to_string())
            .unwrap_or(display_args),
        status,
        status_text: wait_display_status_text(status, status_text),
        ..Default::default()
    }
}

fn wait_display_status_text(status: ToolUseStatus, status_text: String) -> String {
    if !status_text.trim().is_empty() {
        return status_text;
    }
    match status {
        ToolUseStatus::Success => "ok".to_owned(),
        ToolUseStatus::Warning => "warning".to_owned(),
        ToolUseStatus::Error => "err".to_owned(),
        ToolUseStatus::InProgress => tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
    }
}

fn wait_error_status_text(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("err")
        .to_owned()
}

/// Construct a wait error reply without durable settlement metadata.
pub(super) fn wait_error_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    message: String,
    details: Option<CborValue>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Error {
            message,
            details,
            display: None,
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
        settlement: None,
    }
}

/// Construct an exact-wait activation interruption reply.
pub(super) fn wait_interrupted_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    source_tool_name: Option<ToolName>,
    target_call_id: &ToolCallId,
) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        source_tool_name,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for tool call `{target_call_id}` was interrupted because new input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

/// Construct a bare-wait activation interruption reply.
pub(super) fn wait_interrupted_any_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        None,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for a background tool call in this conversation was interrupted because new input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

fn result_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    result: CborValue,
) -> CborValue {
    let header = original_tool_call_id_entry(original_call_id);
    match result {
        CborValue::Map(mut entries) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        other => CborValue::Map(vec![header, (CborValue::Text("output".to_owned()), other)]),
    }
}

fn details_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    details: Option<CborValue>,
) -> Option<CborValue> {
    let header = original_tool_call_id_entry(original_call_id);
    Some(match details {
        Some(CborValue::Map(mut entries)) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        Some(other) => CborValue::Map(vec![header, (CborValue::Text("details".to_owned()), other)]),
        None => CborValue::Map(vec![header]),
    })
}

fn original_tool_call_id_entry(original_call_id: &ToolCallId) -> (CborValue, CborValue) {
    (
        CborValue::Text(ORIGINAL_TOOL_CALL_ID_HEADER.to_owned()),
        CborValue::Text(original_call_id.to_string()),
    )
}

/// Parse mutually exclusive exact, bare-background, or activating-input
/// arguments.
pub(super) fn parse_wait_args(arguments: &CborValue) -> Result<WaitTarget, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut tool_call_id_value = None;
    let mut timeout_minutes_value = None;
    let mut legacy_any_input = false;
    let mut tool_call_id_count = 0_u8;
    let mut timeout_minutes_count = 0_u8;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "tool_call_id" => {
                tool_call_id_count = tool_call_id_count.saturating_add(1);
                tool_call_id_value.get_or_insert(v);
            }
            "timeout_minutes" => {
                timeout_minutes_count = timeout_minutes_count.saturating_add(1);
                timeout_minutes_value.get_or_insert(v);
            }
            "any_input" => legacy_any_input = true,
            _ => {}
        }
    }
    if legacy_any_input {
        return Err(
            "`any_input` is no longer supported; use `timeout_minutes` with a positive integer"
                .to_owned(),
        );
    }
    if tool_call_id_value.is_some() && timeout_minutes_value.is_some() {
        return Err("`tool_call_id` and `timeout_minutes` are mutually exclusive".to_owned());
    }
    if 1 < tool_call_id_count {
        return Err("`tool_call_id` must not be repeated".to_owned());
    }
    if 1 < timeout_minutes_count {
        return Err("`timeout_minutes` must not be repeated".to_owned());
    }
    if let Some(value) = tool_call_id_value {
        return match value {
            CborValue::Text(text) if text.trim().is_empty() => {
                Err("`tool_call_id` must not be empty".to_owned())
            }
            CborValue::Text(text) => Ok(WaitTarget::Exact(text.trim().to_owned().into())),
            _ => Err("`tool_call_id` must be a string".to_owned()),
        };
    }
    match timeout_minutes_value {
        Some(CborValue::Integer(value)) => {
            let minutes: i128 = (*value).into();
            if minutes < 1 {
                return Err("`timeout_minutes` must be at least 1".to_owned());
            }
            let effective_minutes = minutes.min(MAX_INPUT_WAIT_MINUTES) as u64;
            Ok(WaitTarget::AnyInput(Duration::from_secs(
                effective_minutes * 60,
            )))
        }
        Some(_) => Err("`timeout_minutes` must be an integer".to_owned()),
        None => Ok(WaitTarget::AnyBackground),
    }
}

/// Validates wait arguments and returns the effective activating-input timeout
/// in minutes when that mode was selected.
///
/// # Errors
///
/// Returns the same validation error used by wait invocation when arguments are
/// malformed, conflicting, repeated, or otherwise unsupported.
pub(super) fn normalized_wait_timeout_minutes_inner(
    arguments: &CborValue,
) -> Result<Option<u64>, String> {
    match parse_wait_args(arguments)? {
        WaitTarget::AnyInput(timeout) => Ok(Some(timeout.as_secs() / 60)),
        _ => Ok(None),
    }
}

/// Construct the successful reply for activating-input delivery.
pub(super) fn wait_input_available_reply(
    call_id: ToolCallId,
    tool_name: ToolName,
    display_args: String,
) -> WaitReply {
    wait_result_reply(
        call_id,
        tool_name,
        None,
        CborValue::Map(vec![(
            CborValue::Text("input_available".to_owned()),
            CborValue::Bool(true),
        )]),
        Some(ToolUseState {
            args: display_args,
            status: ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
    )
}

fn wait_timed_out_reply(
    call_id: ToolCallId,
    tool_name: ToolName,
    display_args: String,
) -> WaitReply {
    WaitReply {
        wait_call_id: call_id,
        wait_tool_name: tool_name,
        kind: WaitReplyKind::Result {
            result: CborValue::Map(vec![(
                CborValue::Text("timed_out".to_owned()),
                CborValue::Bool(true),
            )]),
            display: Some(wait_display_from_source(
                None,
                Some(ToolUseState {
                    args: display_args,
                    status: ToolUseStatus::Warning,
                    status_text: "timeout".to_owned(),
                    ..Default::default()
                }),
                ToolUseStatus::Warning,
                "timeout".to_owned(),
            )),
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
        settlement: None,
    }
}

#[cfg(test)]
mod tests;
