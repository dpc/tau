//! Source-bound outbound Slack reaction ownership, replay, and tool execution.
//!
//! This module implements `SPEC-tau-ext-slack-agent-reactions`. Native Slack
//! coordinates stay private, while every operation revalidates the retained
//! Tau-issued target and its current source or configured-destination
//! authority.

use std::collections::{HashMap, HashSet, VecDeque, hash_map as path_std_collections_hash_map};
use std::sync::atomic::Ordering;

use tau_proto::{
    AgentId, CborValue, Event, MessageFactId, ToolProgress, ToolStarted, ToolUseState,
    ToolUseStatus,
};

use super::*;

/// Maximum retained target references.
///
/// Ownership can pin at most [`OWNERSHIP_LIMIT`] targets. The additional
/// send-ledger headroom guarantees every accepted send can activate its target
/// without evicting a pinned reference.
pub(super) const TARGET_LIMIT: usize = OWNERSHIP_LIMIT + SEND_LEDGER_LIMIT;
/// Maximum locally owned reaction tuples.
pub(super) const OWNERSHIP_LIMIT: usize = 1024;
/// Maximum retained tool-call attempts.
pub(super) const ATTEMPT_LIMIT: usize = 256;

/// Slack Web API operation surface used only by the reaction subsystem.
pub(super) trait ReactionClient: Send + Sync + 'static {
    /// Add or remove the bot's reaction on one exact cached Slack item.
    fn react(
        &self,
        cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError>;
}

/// Fail-closed client for extension views that cannot execute reaction tools.
pub(super) struct UnavailableReactionClient;

impl ReactionClient for UnavailableReactionClient {
    fn react(
        &self,
        _cfg: &RuntimeConfig,
        _action: ReactionActionKind,
        _channel_id: &str,
        _message_ts: &str,
        _emoji: &str,
    ) -> Result<(), ReactionApiError> {
        Err(ReactionApiError::OutcomeUnknown)
    }
}
/// Explicit outbound reaction operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReactionActionKind {
    /// Add the named reaction.
    Add,
    /// Remove the named reaction.
    Remove,
}

impl ReactionActionKind {
    /// Parse the exact public action spelling.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            _ => None,
        }
    }

    /// Return the exact public action spelling.
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// Safe, typed outcomes from Slack's reaction methods.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ReactionApiError {
    /// Slack reports the bot already has the reaction.
    AlreadyReacted,
    /// Slack reports the bot has no such reaction.
    NoReaction,
    /// Slack throttled the request for this bounded duration.
    RateLimited(u64),
    /// The app lacks the separately documented write scope.
    MissingScope,
    /// A definitive bounded Slack error category.
    Definitive(&'static str),
    /// The remote effect may have happened.
    OutcomeUnknown,
}

/// Live authority retained for one reaction target.
#[derive(Clone, Eq, PartialEq)]
pub(super) enum ReactionAuthority {
    /// Exact submitted incoming source route.
    Source {
        /// Publisher-scoped message fact id.
        message_id: MessageFactId,
        /// Verified source user bound to the route.
        user_id: String,
    },
    /// Exact operator-configured proactive destination.
    ConfiguredDestination {
        /// Stable model-facing destination alias.
        alias: String,
    },
}

/// One exact Slack item addressable only through a Tau-issued reference.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct ReactionTarget {
    /// Agent that received or authored the message.
    pub(super) agent_id: AgentId,
    /// Exact authenticated conversation route.
    pub(super) conversation: SlackConversation,
    /// Exact item timestamp, which may be a thread child.
    pub(super) message_ts: String,
    /// Exact installation team that minted this private authority.
    pub(super) installation_team_id: String,
    /// Live route authority revalidated on every use.
    pub(super) authority: ReactionAuthority,
}

/// Private semantic identity shared by refs naming the same reaction.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct ReactionKey {
    /// Native conversation identity.
    pub(super) channel_id: String,
    /// Native exact message timestamp.
    pub(super) message_ts: String,
    /// Strict canonical emoji spelling.
    pub(super) emoji: String,
}

/// Local ownership for one unambiguously added reaction.
pub(super) struct ReactionOwner {
    /// Agent allowed to remove the reaction.
    pub(super) agent_id: AgentId,
    /// Reference pinned while ownership remains live.
    pub(super) message_ref: MessageFactId,
}

/// One exact in-flight reservation protected against late completion races.
pub(super) struct ReactionReservation {
    /// Agent whose call owns the reservation.
    pub(super) agent_id: AgentId,
    /// Monotonic token unique within this extension process.
    pub(super) token: SlackReactionReservation,
    /// Target reference pinned until the call finishes.
    pub(super) message_ref: MessageFactId,
    /// Whether this is an unowned add counted against ownership capacity.
    pub(super) unowned_add: bool,
}

/// Frozen authority and reservation carried from Slack I/O through confirmed
/// terminal submission.
struct PreparedReaction {
    /// Configuration frozen before the remote attempt.
    cfg: RuntimeConfig,
    /// Exact retained target used by the remote attempt.
    target: ReactionTarget,
    /// Semantic reaction tuple reserved by this call.
    key: ReactionKey,
    /// Target reference presented by the caller.
    message_ref: MessageFactId,
    /// Configuration generation at preparation.
    generation: SlackConfigGeneration,
    /// Reaction lifecycle epoch at preparation.
    epoch: SlackReactionEpoch,
    /// Exact in-flight reservation token.
    reservation: SlackReactionReservation,
    /// Whether the caller already owned this tuple before the attempt.
    owned_before: bool,
}

impl PreparedReaction {
    /// Return whether this completion retains its exact reservation, target,
    /// configuration, and lifecycle authority.
    fn is_current(&self, state: &State, invoke: &ToolStarted) -> bool {
        state.configuration.config_generation == self.generation
            && state.ingress.reactions.epoch == self.epoch
            && state
                .ingress
                .reactions
                .in_flight
                .get(&self.key)
                .is_some_and(|current| current.token == self.reservation)
            && state
                .configuration
                .config
                .as_ref()
                .is_some_and(|current_cfg| {
                    state
                        .ingress
                        .reactions
                        .targets
                        .get(&self.message_ref)
                        .is_some_and(|current_target| {
                            current_target == &self.target
                                && current_target.agent_id == invoke.agent_id
                                && reaction_target_authorized(state, current_cfg, current_target)
                        })
                })
    }
}

/// Reaction state-machine output before generic tool dispatch.
enum ReactionExecution {
    /// Generic dispatch still owes this terminal.
    Pending(Event),
    /// Reaction completion already submitted this terminal synchronously.
    Submitted(Event),
    /// Reaction completion retained ownership because terminal output failed.
    Failed(Event, ClientError),
}

/// Terminal disposition retained for same-process tool-call replay.
#[derive(Clone)]
pub(super) enum ReactionAttemptDisposition {
    /// Authorized call awaits Slack or confirmed terminal output.
    InFlight,
    /// Structured successful result.
    Success(CborValue),
    /// Stable bounded error.
    Error(String),
}

/// Fingerprint and terminal result for one reaction call.
#[derive(Clone)]
pub(super) struct ReactionAttempt {
    /// Exact calling agent.
    pub(super) agent_id: AgentId,
    /// Exact invocation arguments.
    pub(super) arguments: CborValue,
    /// Terminal result returned without repeating Slack I/O.
    pub(super) disposition: ReactionAttemptDisposition,
}

/// Bounded runtime state for target authority, ownership, reservations, and
/// replay.
#[derive(Default)]
pub(super) struct ReactionState {
    /// Tau-issued fact refs mapped to private exact targets.
    pub(super) targets: HashMap<MessageFactId, ReactionTarget>,
    /// Oldest-first target insertion order.
    pub(super) target_order: VecDeque<MessageFactId>,
    /// Locally owned bot reactions.
    pub(super) owners: HashMap<ReactionKey, ReactionOwner>,
    /// Reaction tuples reserved through Slack I/O and terminal confirmation.
    pub(super) in_flight: HashMap<ReactionKey, ReactionReservation>,
    /// Monotonic token preventing late calls from clearing newer reservations.
    pub(super) next_reservation: SlackReactionReservation,
    /// Lifecycle epoch preventing late calls from mutating restored state.
    pub(super) epoch: SlackReactionEpoch,
    /// Same-process terminal reaction attempts.
    pub(super) attempts: HashMap<tau_proto::ToolCallId, ReactionAttempt>,
    /// Oldest-first attempt insertion order.
    pub(super) attempt_order: VecDeque<tau_proto::ToolCallId>,
}

impl ReactionState {
    /// Insert a target, evicting only the oldest target without live ownership.
    pub(super) fn insert_target(
        &mut self,
        message_ref: MessageFactId,
        target: ReactionTarget,
    ) -> bool {
        if let path_std_collections_hash_map::Entry::Occupied(mut entry) =
            self.targets.entry(message_ref.clone())
        {
            if entry.get() == &target {
                entry.insert(target);
                return true;
            }
            return false;
        }
        while self.targets.len() >= TARGET_LIMIT {
            let Some(index) = self.target_order.iter().position(|candidate| {
                !self
                    .owners
                    .values()
                    .any(|owner| &owner.message_ref == candidate)
                    && !self
                        .in_flight
                        .values()
                        .any(|reservation| &reservation.message_ref == candidate)
            }) else {
                return false;
            };
            if let Some(evicted) = self.target_order.remove(index) {
                self.targets.remove(&evicted);
            }
        }
        self.target_order.push_back(message_ref.clone());
        self.targets.insert(message_ref, target);
        true
    }

    /// Store one bounded terminal reaction attempt.
    pub(super) fn remember_attempt(
        &mut self,
        invoke: &ToolStarted,
        disposition: ReactionAttemptDisposition,
    ) -> bool {
        if let Some(existing) = self.attempts.get_mut(&invoke.call_id) {
            if existing.agent_id != invoke.agent_id || existing.arguments != invoke.arguments {
                return false;
            }
            existing.disposition = disposition;
            return true;
        }
        while self.attempts.len() >= ATTEMPT_LIMIT {
            let Some(index) = self.attempt_order.iter().position(|call_id| {
                self.attempts.get(call_id).is_some_and(|attempt| {
                    !matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                })
            }) else {
                return false;
            };
            if let Some(evicted) = self.attempt_order.remove(index) {
                self.attempts.remove(&evicted);
            }
        }
        self.attempt_order
            .retain(|call_id| call_id != &invoke.call_id);
        self.attempt_order.push_back(invoke.call_id.clone());
        self.attempts.insert(
            invoke.call_id.clone(),
            ReactionAttempt {
                agent_id: invoke.agent_id.clone(),
                arguments: invoke.arguments.clone(),
                disposition,
            },
        );
        true
    }

    /// Clear all reaction target, ownership, in-flight, and replay state.
    pub(super) fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_next();
        self.targets.clear();
        self.target_order.clear();
        self.owners.clear();
        self.in_flight.clear();
        self.attempts.clear();
        self.attempt_order.clear();
    }

    /// Remove all reaction state belonging to one unloaded agent.
    pub(super) fn remove_agent(&mut self, agent_id: &AgentId) {
        self.targets
            .retain(|_, target| &target.agent_id != agent_id);
        self.target_order
            .retain(|message_ref| self.targets.contains_key(message_ref));
        self.owners.retain(|_, owner| &owner.agent_id != agent_id);
        self.attempts
            .retain(|_, attempt| &attempt.agent_id != agent_id);
        self.attempt_order
            .retain(|call_id| self.attempts.contains_key(call_id));
        self.in_flight
            .retain(|_, reservation| &reservation.agent_id != agent_id);
    }

    /// Revoke source-authorized targets for an unregistered agent, preserving
    /// proactive targets.
    pub(super) fn remove_agent_sources(&mut self, agent_id: &AgentId) {
        let revoked = self
            .targets
            .iter()
            .filter(|(_, target)| {
                &target.agent_id == agent_id
                    && matches!(target.authority, ReactionAuthority::Source { .. })
            })
            .map(|(message_ref, _)| message_ref.clone())
            .collect::<HashSet<_>>();
        self.targets
            .retain(|message_ref, _| !revoked.contains(message_ref));
        self.target_order
            .retain(|message_ref| !revoked.contains(message_ref));
        self.owners
            .retain(|_, owner| !revoked.contains(&owner.message_ref));
        self.in_flight
            .retain(|_, reservation| !revoked.contains(&reservation.message_ref));
        // Attempt fingerprints survive unregister so late calls terminalize
        // safely and same-call replay cannot regain Slack I/O authority.
    }

    /// Revoke every target, ownership, and reservation keyed to one message
    /// fact.
    pub(super) fn revoke_message(&mut self, message_id: &MessageFactId) {
        self.targets.remove(message_id);
        self.target_order
            .retain(|candidate| candidate != message_id);
        self.owners
            .retain(|_, owner| &owner.message_ref != message_id);
        self.in_flight
            .retain(|_, reservation| &reservation.message_ref != message_id);
    }

    /// Return whether live ownership or I/O pins one source reply route.
    pub(super) fn source_route_is_pinned(&self, message_id: &MessageFactId) -> bool {
        self.owners
            .values()
            .map(|owner| &owner.message_ref)
            .chain(
                self.in_flight
                    .values()
                    .map(|reservation| &reservation.message_ref),
            )
            .any(|message_ref| {
                self.targets.get(message_ref).is_some_and(|target| {
                    matches!(
                        &target.authority,
                        ReactionAuthority::Source {
                            message_id: owned_id,
                            ..
                        } if owned_id == message_id
                    )
                })
            })
    }

    /// Return whether an identical call is awaiting Slack or confirmed output.
    pub(super) fn identical_call_in_flight(&self, invoke: &ToolStarted) -> bool {
        self.attempts.get(&invoke.call_id).is_some_and(|attempt| {
            attempt.agent_id == invoke.agent_id
                && attempt.arguments == invoke.arguments
                && matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
        })
    }
}

/// Fixed schema for explicit source-bound Slack reaction mutations.
pub(super) fn react_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(REACT_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(REACT_TOOL_NAME)),
        description: Some(
            "Add or remove one emoji reaction on an exact Slack message reference issued by Tau. Channel IDs and timestamps are not accepted as separate route arguments; aliases, toggle, list, and discovery are rejected."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "message_ref": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 128,
                    "description": "Tau-issued message fact ID from a locally submitted Slack report or successful slack_send result"
                },
                "emoji": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 77,
                    "pattern": "^[a-z0-9_+-]{1,64}(::skin-tone-[2-6])?$",
                    "description": "Slack emoji name without surrounding colons"
                },
                "action": { "type": "string", "enum": ["add", "remove"] }
            },
            "required": ["message_ref", "emoji", "action"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![tau_proto::ToolTag::new(REACT_TOOL_TAG)],
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "react-eyes".to_owned(),
            title: Some("Add an eyes reaction".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("message_ref", example_text("slack-message:0123456789abcdef")),
                example_field("emoji", example_text("eyes")),
                example_field("action", example_text("add")),
            ]),
            note: Some("Use action=remove only for a reaction this agent added.".to_owned()),
            subcommand: None,
        }],
    }
}

impl Extension {
    /// Execute one separately authorized Slack reaction.
    ///
    /// Fresh remote successes synchronously write and flush their terminal
    /// result. Output failure retires the complete Slack session before return.
    pub(super) fn handle_react(&self, invoke: ToolStarted) -> ToolTerminalSubmission {
        match self.execute_react(invoke) {
            ReactionExecution::Pending(event) => ToolTerminalSubmission::pending(event),
            ReactionExecution::Submitted(event) => {
                drop(event);
                ToolTerminalSubmission::Confirmed
            }
            ReactionExecution::Failed(event, error) => {
                drop(event);
                ToolTerminalSubmission::Failed(error)
            }
        }
    }

    /// Return only the reaction event for direct state-machine tests.
    #[cfg(test)]
    pub(super) fn handle_react_event(&self, invoke: ToolStarted) -> Event {
        match self.execute_react(invoke) {
            ReactionExecution::Pending(event)
            | ReactionExecution::Submitted(event)
            | ReactionExecution::Failed(event, _) => event,
        }
    }

    /// Execute the reaction state machine and classify terminal ownership.
    fn execute_react(&self, invoke: ToolStarted) -> ReactionExecution {
        macro_rules! pending {
            ($event:expr) => {
                return ReactionExecution::Pending($event)
            };
        }
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(attempt) = state.ingress.reactions.attempts.get(&invoke.call_id) {
                if attempt.agent_id != invoke.agent_id || attempt.arguments != invoke.arguments {
                    pending!(tool_error(
                        invoke,
                        "slack_react call id was replayed with conflicting arguments".to_owned(),
                    ));
                }
                pending!(match &attempt.disposition {
                    ReactionAttemptDisposition::InFlight => reaction_coalesced(invoke),
                    ReactionAttemptDisposition::Success(result) => {
                        structured_tool_result(invoke, result.clone())
                    }
                    ReactionAttemptDisposition::Error(message) => {
                        tool_error(invoke, message.clone())
                    }
                });
            }
        }
        let parsed = (|| {
            validate_object_fields(&invoke.arguments, &["message_ref", "emoji", "action"])?;
            let message_ref = cbor_string_field(&invoke.arguments, "message_ref")?;
            let emoji = cbor_string_field(&invoke.arguments, "emoji")?;
            let action_text = cbor_string_field(&invoke.arguments, "action")?;
            if message_ref.is_empty() || message_ref.len() > 128 {
                return Err("`message_ref` must contain 1 to 128 bytes".to_owned());
            }
            if !valid_outbound_emoji(&emoji) {
                return Err("`emoji` must be a valid lowercase Slack emoji name".to_owned());
            }
            let action = ReactionActionKind::parse(&action_text)
                .ok_or_else(|| "`action` must be `add` or `remove`".to_owned())?;
            Ok((MessageFactId::new(message_ref), emoji, action))
        })();
        let (message_ref, emoji, action) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => pending!(self.finish_reaction_error(invoke, message)),
        };
        let prepared = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if self.output_failed.load(Ordering::Acquire)
                || self.shutdown.is_requested()
                || !state.socket.session_active
            {
                pending!(self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                ));
            }
            let Some(cfg) = state.configuration.config.clone() else {
                pending!(self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                ));
            };
            let Some(target) = state.ingress.reactions.targets.get(&message_ref).cloned() else {
                pending!(self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                ));
            };
            if target.agent_id != invoke.agent_id
                || !reaction_target_authorized(&state, &cfg, &target)
            {
                pending!(self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack message reference is unknown, stale, or unauthorized".to_owned(),
                ));
            }
            if let Some(attempt) = state.ingress.reactions.attempts.get(&invoke.call_id) {
                if attempt.agent_id == invoke.agent_id && attempt.arguments == invoke.arguments {
                    pending!(reaction_coalesced(invoke));
                }
                pending!(tool_error(
                    invoke,
                    "slack_react call id was replayed with conflicting arguments".to_owned(),
                ));
            }
            if state.ingress.reactions.attempts.len() >= ATTEMPT_LIMIT
                && state.ingress.reactions.attempts.values().all(|attempt| {
                    matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                })
            {
                pending!(tool_error(
                    invoke,
                    "Slack reaction attempt capacity is full".to_owned(),
                ));
            }
            let key = ReactionKey {
                channel_id: target.conversation.channel_id.clone(),
                message_ts: target.message_ts.clone(),
                emoji: emoji.clone(),
            };
            if state.ingress.reactions.in_flight.contains_key(&key) {
                pending!(self.finish_reaction_error_locked(
                    &mut state,
                    invoke,
                    "Slack reaction is already in progress".to_owned(),
                ));
            }
            let owner_agent = state
                .ingress
                .reactions
                .owners
                .get(&key)
                .map(|owner| owner.agent_id.clone());
            let owned_before = owner_agent.as_ref() == Some(&invoke.agent_id);
            match action {
                ReactionActionKind::Add => {
                    if owner_agent
                        .as_ref()
                        .is_some_and(|owner| owner != &invoke.agent_id)
                    {
                        pending!(self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction is owned by another agent".to_owned(),
                        ));
                    }
                    if owner_agent.is_none()
                        && state.ingress.reactions.owners.len()
                            + state
                                .ingress
                                .reactions
                                .in_flight
                                .values()
                                .filter(|reservation| reservation.unowned_add)
                                .count()
                            >= OWNERSHIP_LIMIT
                    {
                        pending!(self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction ownership capacity is full".to_owned(),
                        ));
                    }
                }
                ReactionActionKind::Remove => {
                    if !owned_before {
                        pending!(self.finish_reaction_error_locked(
                            &mut state,
                            invoke,
                            "Slack reaction is not owned by this agent".to_owned(),
                        ));
                    }
                }
            }
            state.ingress.reactions.next_reservation =
                state.ingress.reactions.next_reservation.wrapping_next();
            let reservation = state.ingress.reactions.next_reservation;
            state.ingress.reactions.in_flight.insert(
                key.clone(),
                ReactionReservation {
                    agent_id: invoke.agent_id.clone(),
                    token: reservation,
                    message_ref: message_ref.clone(),
                    unowned_add: action == ReactionActionKind::Add && !owned_before,
                },
            );
            let remembered = state
                .ingress
                .reactions
                .remember_attempt(&invoke, ReactionAttemptDisposition::InFlight);
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(remembered);
            state.configuration.config_frozen = true;
            PreparedReaction {
                cfg,
                target,
                key,
                message_ref,
                generation: state.configuration.config_generation,
                epoch: state.ingress.reactions.epoch,
                reservation,
                owned_before,
            }
        };
        let outcome = self.reaction_client.react(
            &prepared.cfg,
            action,
            &prepared.target.conversation.channel_id,
            &prepared.target.message_ts,
            &emoji,
        );
        let submission = self
            .output_submission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let current = prepared.is_current(&state, &invoke);
            if !current {
                if state
                    .ingress
                    .reactions
                    .in_flight
                    .get(&prepared.key)
                    .is_some_and(|current| current.token == prepared.reservation)
                {
                    state.ingress.reactions.in_flight.remove(&prepared.key);
                }
                drop(submission);
                let message =
                    "Slack message reference is unknown, stale, or unauthorized".to_owned();
                if state
                    .ingress
                    .reactions
                    .attempts
                    .get(&invoke.call_id)
                    .is_some_and(|attempt| {
                        attempt.agent_id == invoke.agent_id
                            && attempt.arguments == invoke.arguments
                            && matches!(attempt.disposition, ReactionAttemptDisposition::InFlight)
                    })
                {
                    pending!(self.finish_reaction_error_locked(&mut state, invoke, message));
                }
                pending!(tool_error(invoke, message));
            }
            let successful_remote_outcome = match (&action, &outcome) {
                (ReactionActionKind::Add, Ok(()))
                | (ReactionActionKind::Add, Err(ReactionApiError::AlreadyReacted))
                    if prepared.owned_before =>
                {
                    true
                }
                (ReactionActionKind::Remove, Ok(()))
                | (ReactionActionKind::Remove, Err(ReactionApiError::NoReaction))
                    if prepared.owned_before =>
                {
                    true
                }
                (ReactionActionKind::Add, Ok(())) => true,
                _ => false,
            };
            if !successful_remote_outcome {
                state.ingress.reactions.in_flight.remove(&prepared.key);
                let message =
                    reaction_error_message(outcome.err(), action, current, prepared.owned_before);
                drop(submission);
                pending!(self.finish_reaction_error_locked(&mut state, invoke, message));
            }
        }
        let result = CborValue::Map(vec![
            example_field("status", example_text("ok")),
            example_field("action", example_text(action.as_str())),
            example_field("emoji", example_text(&emoji)),
        ]);
        let Event::ToolResult(tool_result) = structured_tool_result(invoke.clone(), result.clone())
        else {
            unreachable!("structured reaction result is terminal success");
        };
        let result_sent = self
            .output
            .report_tool_result_confirmed(tool_result.clone());
        if result_sent.is_err() {
            self.output_failed.store(true, Ordering::Release);
        }
        #[cfg(test)]
        if result_sent.is_ok() {
            run_blocking_test_hook(&self.test_hooks.reaction_result_boundary);
        }
        if result_sent.is_ok() {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if prepared.is_current(&state, &invoke) {
                match action {
                    ReactionActionKind::Add if !prepared.owned_before => {
                        state.ingress.reactions.owners.insert(
                            prepared.key.clone(),
                            ReactionOwner {
                                agent_id: invoke.agent_id.clone(),
                                message_ref: prepared.message_ref.clone(),
                            },
                        );
                    }
                    ReactionActionKind::Remove => {
                        state.ingress.reactions.owners.remove(&prepared.key);
                    }
                    ReactionActionKind::Add => {}
                }
                state.ingress.reactions.in_flight.remove(&prepared.key);
                let remembered = state
                    .ingress
                    .reactions
                    .remember_attempt(&invoke, ReactionAttemptDisposition::Success(result));
                // ast-grep-ignore: debug-assert-expression-must-not-mutate
                debug_assert!(remembered);
            }
        }
        drop(submission);
        if let Err(error) = result_sent {
            self.retire_after_output_failure();
            return ReactionExecution::Failed(Event::ToolResult(tool_result), error);
        }
        ReactionExecution::Submitted(Event::ToolResult(tool_result))
    }

    /// Store and return one terminal reaction error after acquiring state.
    fn finish_reaction_error(&self, invoke: ToolStarted, message: String) -> Event {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.finish_reaction_error_locked(&mut state, invoke, message)
    }

    /// Store and return one terminal reaction error while state is locked.
    fn finish_reaction_error_locked(
        &self,
        state: &mut State,
        invoke: ToolStarted,
        message: String,
    ) -> Event {
        if state
            .ingress
            .reactions
            .remember_attempt(&invoke, ReactionAttemptDisposition::Error(message.clone()))
        {
            tool_error(invoke, message)
        } else {
            tool_error(
                invoke,
                "slack_react call id was replayed with conflicting arguments".to_owned(),
            )
        }
    }
}

/// Validate the strict outbound emoji grammar without normalization.
pub(super) fn valid_outbound_emoji(value: &str) -> bool {
    let (base, tone) = match value.split_once("::") {
        Some((base, tone)) => (base, Some(tone)),
        None => (value, None),
    };
    (1..=64).contains(&base.len())
        && base.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_+-".contains(&byte)
        })
        && tone.is_none_or(|tone| {
            matches!(
                tone.as_bytes(),
                [
                    b's',
                    b'k',
                    b'i',
                    b'n',
                    b'-',
                    b't',
                    b'o',
                    b'n',
                    b'e',
                    b'-',
                    b'2'..=b'6'
                ]
            )
        })
}

/// Revalidate the exact current route authority for one cached target.
fn reaction_target_authorized(state: &State, cfg: &RuntimeConfig, target: &ReactionTarget) -> bool {
    if state.socket.installation_team_id.as_deref() != Some(target.installation_team_id.as_str()) {
        return false;
    }
    match &target.authority {
        ReactionAuthority::Source {
            message_id,
            user_id,
        } => {
            state.agents.registered_agents.contains(&target.agent_id)
                && state
                    .ingress
                    .reply_routes
                    .get(message_id)
                    .is_some_and(|route| {
                        route.agent_id == target.agent_id
                            && route.user_id == *user_id
                            && route.conversation == target.conversation
                            && state.socket.installation_team_id.as_deref()
                                == Some(route.installation_team_id.as_str())
                    })
                && is_route_authorized(state, cfg, &target.conversation, user_id)
        }
        ReactionAuthority::ConfiguredDestination { alias } => cfg
            .proactive_aliases
            .contains(alias)
            .then(|| cfg.conversations.get(alias))
            .flatten()
            .is_some_and(|policy| {
                policy.conversation_id == target.conversation.channel_id
                    && policy.thread_ts == target.conversation.thread_ts
                    && policy.kind == target.conversation.kind
            }),
    }
}

/// Convert typed reaction failures to bounded non-sensitive terminal text.
pub(super) fn reaction_error_message(
    error: Option<ReactionApiError>,
    action: ReactionActionKind,
    current: bool,
    owned_before: bool,
) -> String {
    if !current {
        return "Slack message reference is unknown, stale, or unauthorized".to_owned();
    }
    match error {
        Some(ReactionApiError::AlreadyReacted) if action == ReactionActionKind::Add => {
            if owned_before {
                "Slack reaction replay could not be confirmed".to_owned()
            } else {
                "Slack reaction already exists but is not owned by this agent".to_owned()
            }
        }
        Some(ReactionApiError::AlreadyReacted) => {
            "Slack reaction failed: already_reacted".to_owned()
        }
        Some(ReactionApiError::NoReaction) => {
            "Slack reaction does not exist or is not locally owned".to_owned()
        }
        Some(ReactionApiError::RateLimited(seconds)) => {
            format!("Slack reactions are rate limited; retry after {seconds}s")
        }
        Some(ReactionApiError::MissingScope) => {
            "Slack reactions require the reactions:write scope; add it and reinstall the Slack app"
                .to_owned()
        }
        Some(ReactionApiError::Definitive(category)) => {
            format!("Slack reaction failed: {category}")
        }
        Some(ReactionApiError::OutcomeUnknown) | None => {
            "Slack reaction outcome is unknown; the request was not retried".to_owned()
        }
    }
}

/// Return a non-terminal progress event when a duplicate shares active I/O.
fn reaction_coalesced(invoke: ToolStarted) -> Event {
    Event::ToolProgressReported(ToolProgress {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        message: Some("identical slack_react call is already in progress".to_owned()),
        progress: None,
        display: Some(ToolUseState {
            status: ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
    })
}

impl HttpSlackClient {
    /// Call one reaction method with typed, body-safe failure handling.
    fn post_reaction(
        &self,
        cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError> {
        let method = match action {
            ReactionActionKind::Add => "reactions.add",
            ReactionActionKind::Remove => "reactions.remove",
        };
        let url = format!("{}/{method}", cfg.api_base);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", cfg.bot_token))
            .content_type("application/json")
            .send(
                serde_json::json!({
                    "channel": channel_id,
                    "timestamp": message_ts,
                    "name": emoji
                })
                .to_string(),
            )
            .map_err(|_| ReactionApiError::OutcomeUnknown)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.clamp(1, 3_600));
        if status == 429 {
            return Err(ReactionApiError::RateLimited(retry_after.unwrap_or(1)));
        }
        if 500 <= status {
            return Err(ReactionApiError::OutcomeUnknown);
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_SLACK_API_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|_| ReactionApiError::OutcomeUnknown)?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| ReactionApiError::OutcomeUnknown)?;
        if (200..300).contains(&status)
            && value.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        {
            return Ok(());
        }
        let code = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown_error");
        Err(match code {
            "already_reacted" => ReactionApiError::AlreadyReacted,
            "no_reaction" => ReactionApiError::NoReaction,
            "ratelimited" => ReactionApiError::RateLimited(retry_after.unwrap_or(1)),
            "missing_scope" => ReactionApiError::MissingScope,
            "fatal_error" | "internal_error" | "request_timeout" | "service_unavailable" => {
                ReactionApiError::OutcomeUnknown
            }
            "invalid_name" => ReactionApiError::Definitive("invalid emoji name"),
            "too_many_emoji" | "too_many_reactions" => {
                ReactionApiError::Definitive("reaction limit reached")
            }
            "is_archived" | "message_not_found" | "channel_not_found" | "not_found"
            | "thread_locked" | "not_reactable" => {
                ReactionApiError::Definitive("target unavailable")
            }
            "not_in_channel" | "restricted_action" | "missing_permission" => {
                ReactionApiError::Definitive("permission denied")
            }
            "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" => {
                ReactionApiError::Definitive("authentication failed")
            }
            _ => ReactionApiError::Definitive("request rejected"),
        })
    }
}

impl ReactionClient for HttpSlackClient {
    fn react(
        &self,
        cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError> {
        self.post_reaction(cfg, action, channel_id, message_ts, emoji)
    }
}
