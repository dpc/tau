//! Bounded, cancellable, at-least-once Slack send delivery.

mod scheduler;
mod wire;

#[cfg(test)]
pub(super) use scheduler::ImmediateSendScheduler;
pub(super) use scheduler::{SendScheduler, SendWake, SystemSendScheduler};
pub(super) use wire::{
    FrozenPostBody, InternalSourceMention, MIN_RETRY_DELAY, PostAttemptFailure, PostAttemptOutcome,
    SendFailureCategory, SlackApiError, SlackPostMode, classify_api_error, classify_post_api_error,
    parse_retry_after, retry_delay,
};
#[cfg(test)]
pub(super) use wire::{MAX_RETRY_AFTER, PostCompositionError, retry_jitter};

use super::*;

/// Exact lifecycle and configuration authority frozen before Slack I/O.
#[derive(Clone)]
pub(super) struct FrozenSendAuthority {
    /// Monotonic reservation preventing stale workers from owning a reused
    /// call.
    pub(super) token: u64,
    /// Harness session generation that admitted the call.
    pub(super) session_generation: u64,
    /// Extension connection/lifecycle epoch that admitted the call.
    pub(super) ingress_epoch: u64,
    /// Configuration/credential generation that admitted the call.
    pub(super) config_generation: u64,
    /// Agent lifecycle generation that admitted the call.
    pub(super) agent_generation: u64,
    /// Typed capability registration generation that admitted the call.
    pub(super) capability_generation: u64,
    /// Exact extension instance that owns the scoped tool.
    pub(super) instance_name: Option<tau_proto::ExtensionName>,
    /// Exact scoped tool lease name from the authorized ToolStarted.
    pub(super) send_tool: tau_proto::ToolName,
    /// Exact bot identity paired with the installation team.
    pub(super) bot_user_id: ObservedSlackBotId,
    /// Exact installation team paired with the bot observation.
    pub(super) installation_team_id: String,
}

/// Validated bot identity observed during optional Slack preflight.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct ObservedSlackBotId(String);

impl ObservedSlackBotId {
    /// Capture an id that already passed Slack user-id validation.
    fn from_validated(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// Return the validated native id for exact ambient comparison.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fully prepared exact send retained for replay and retry.
#[derive(Clone)]
pub(super) struct PreparedSend {
    /// Original invocation and exact argument fingerprint.
    pub(super) invoke: ToolStarted,
    /// Frozen resolved configuration.
    pub(super) cfg: RuntimeConfig,
    /// Exact canonical source or configured destination route.
    pub(super) route: SlackConversation,
    /// Harness-owned authorization selector.
    pub(super) authorization: tau_proto::TransportSendAuthorization,
    /// Canonical outgoing endpoint.
    pub(super) endpoint: MessageEndpoint,
    /// Canonical outgoing conversation.
    pub(super) conversation: LegacyMessageConversation,
    /// Policy status committed with the outgoing fact.
    pub(super) policy_status: SenderPolicyStatus,
    /// Private reaction authority installed only after completion.
    pub(super) reaction_authority: ReactionAuthority,
    /// Opaque source reply selector for completion.
    pub(super) reply_to: Option<MessageId>,
    /// Exact final wire body reused for both attempts.
    pub(super) body: FrozenPostBody,
    /// Exact final logical text placed in the outgoing durable draft.
    pub(super) text: String,
    /// Latest instant at which the sole retry may begin.
    pub(super) retry_deadline: Instant,
    /// Frozen lifecycle, configuration, session, and tool authority.
    pub(super) authority: FrozenSendAuthority,
}

/// Bounded non-evicting state for one accepted send intent.
#[derive(Clone)]
pub(super) enum SendLedgerDisposition {
    /// Reserved before its bounded worker starts.
    Reserved,
    /// Initial Slack I/O is in progress.
    InFlight {
        /// Semantic attempt currently performing provider I/O.
        attempt: SendAttempt,
        /// Remote copy range inherited from earlier attempts.
        prior_copies: RemoteCopyPossibility,
    },
    /// One and only retry is waiting for its event-driven deadline.
    RetryScheduled {
        /// Remote copy range inherited from attempt one.
        prior_copies: RemoteCopyPossibility,
        /// Safe reason that authorized the retry.
        reason: SendFailureCategory,
    },
    /// Slack accepted and this exact completion awaits Tau's durable decision.
    AwaitingCompletion {
        /// Exact completion resubmitted on authorized replay.
        request: Box<CompleteTransportSendRequest>,
        /// Stable remote copy range after success.
        copies: RemoteCopyPossibility,
    },
    /// Terminal failure with no unresolved provider attempt.
    DefinitiveFailure {
        /// Safe failure category.
        category: SendFailureCategory,
        /// Stable remote copy range.
        copies: RemoteCopyPossibility,
    },
    /// Terminal failure with at least one unresolved provider attempt.
    ExhaustedUnknown {
        /// Last safe failure category.
        category: SendFailureCategory,
        /// Stable remote copy range across both attempts.
        copies: RemoteCopyPossibility,
    },
    /// Lifecycle authority was revoked; an already-started attempt may exist.
    Cancelled {
        /// Stable remote copy range when cancellation won.
        copies: RemoteCopyPossibility,
    },
    /// Tau durably accepted the outgoing fact and tool result.
    Completed {
        /// Stable tool result returned for an exact replay.
        result: Box<ToolResult>,
        /// Exact cumulative remote-copy range.
        copies: RemoteCopyPossibility,
    },
}

/// Semantic provider attempt within the fixed budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SendAttempt {
    /// First and always-present attempt.
    Initial,
    /// Sole optional retry.
    Retry,
}

/// Conservative remote copy range retained across attempt transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RemoteCopyPossibility {
    /// No attempt that could produce a copy has completed.
    None,
    /// Zero or one copies may exist.
    UpToOne,
    /// Exactly one validated copy exists.
    One,
    /// One or two copies may exist after ambiguous-first then success.
    OneOrTwo,
    /// Zero, one, or two copies may exist after two ambiguous attempts.
    UpToTwo,
}

/// Semantic terminal failure class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SendFailureDisposition {
    /// No unresolved attempt precedes the failure.
    Definitive,
    /// At least one attempt remains unresolved.
    ExhaustedUnknown,
}

/// Result of atomically entering one provider attempt.
enum BeginSendAttempt {
    /// The ledger transitioned to in-flight.
    Started,
    /// Exact frozen authority is no longer current.
    Revoked,
    /// The retry could not begin inside its absolute horizon.
    RetryHorizonExpired,
}

impl RemoteCopyPossibility {
    /// Return a stable low-cardinality tracing label.
    pub(super) fn trace_label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UpToOne => "up_to_one",
            Self::One => "one",
            Self::OneOrTwo => "one_or_two",
            Self::UpToTwo => "up_to_two",
        }
    }
    /// Include one additional ambiguous attempt.
    fn after_ambiguous(self) -> Self {
        match self {
            Self::None => Self::UpToOne,
            Self::UpToOne => Self::UpToTwo,
            Self::One | Self::OneOrTwo | Self::UpToTwo => Self::UpToTwo,
        }
    }

    /// Include one additional validated successful attempt.
    fn after_success(self) -> Self {
        match self {
            Self::None => Self::One,
            Self::UpToOne => Self::OneOrTwo,
            Self::One | Self::OneOrTwo | Self::UpToTwo => Self::OneOrTwo,
        }
    }

    /// Return a stable user-visible copy caveat when ambiguity remains.
    pub(super) fn caveat(self) -> Option<&'static str> {
        match self {
            Self::None | Self::One => None,
            Self::UpToOne => Some("zero or one Slack copies may exist"),
            Self::OneOrTwo => Some("one or two Slack copies may exist"),
            Self::UpToTwo => Some("zero, one, or two Slack copies may exist"),
        }
    }
}

/// Fingerprint, frozen authority, and disposition for one accepted tool call.
#[derive(Clone)]
pub(super) struct SendLedgerEntry {
    /// Agent and arguments are also present in `prepared.invoke`; keeping the
    /// prepared intent whole prevents route/body resampling during replay.
    pub(super) prepared: PreparedSend,
    /// Current retry/completion state.
    pub(super) disposition: SendLedgerDisposition,
}

/// Token-scoped membership in one native channel's logical-call FIFO.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct SendQueueReservation {
    call_id: tau_proto::ToolCallId,
    token: u64,
}

/// One exact completion-publication owner and its cached harness request.
pub(super) struct CompletionOutput {
    owner: SendQueueReservation,
    request: Box<CompleteTransportSendRequest>,
}

/// Closed replay decision for one incoming ToolCallId.
pub(super) enum SendReplay {
    /// No reservation exists; validate and reserve a new intent.
    New,
    /// An identical delivery owns provider I/O, or its exact completion output
    /// is queued or active.
    Coalesced,
    /// Return one stable terminal event without provider I/O.
    Event(Box<Event>),
    /// Resubmit one stable completion through a background confirmed writer.
    Completion(CompletionOutput),
}

impl State {
    /// Validate one frozen send against current lifecycle, configuration,
    /// capability, route, and non-reusable ledger reservation.
    fn send_authority_is_current(&self, prepared: &PreparedSend) -> bool {
        let FrozenSendAuthority {
            token,
            session_generation,
            ingress_epoch,
            config_generation,
            agent_generation,
            capability_generation,
            instance_name,
            send_tool: _,
            bot_user_id,
            installation_team_id,
        } = &prepared.authority;
        if !self.capability_active
            || self.session_generation != *session_generation
            || self.ingress_epoch != *ingress_epoch
            || self.config_generation != *config_generation
            || self.send_agent_generation(&prepared.invoke.agent_id) != *agent_generation
            || self.capability_generation != *capability_generation
            || self.instance_name.as_ref() != instance_name.as_ref()
            || self.bot_user_id.as_deref() != Some(bot_user_id.as_str())
            || self.installation_team_id.as_deref() != Some(installation_team_id.as_str())
            || self
                .send_ledger
                .get(&prepared.invoke.call_id)
                .is_none_or(|entry| entry.prepared.authority.token != *token)
        {
            return false;
        }
        let Some(cfg) = self.config.as_ref() else {
            return false;
        };
        match &prepared.authorization {
            tau_proto::TransportSendAuthorization::Reply { message_id } => {
                self.registered_agents.contains(&prepared.invoke.agent_id)
                    && self.reply_routes.get(message_id).is_some_and(|route| {
                        route.agent_id == prepared.invoke.agent_id
                            && route.conversation == prepared.route
                            && is_route_authorized(self, cfg, &route.conversation, &route.user_id)
                    })
            }
            tau_proto::TransportSendAuthorization::ConfiguredDestination { alias } => {
                cfg.proactive_aliases.contains(alias)
                    && cfg.conversations.get(alias).is_some_and(|destination| {
                        destination.conversation_id == prepared.route.channel_id
                            && destination.thread_ts == prepared.route.thread_ts
                            && destination.kind == prepared.route.kind
                            && destination.alias == prepared.route.alias
                    })
            }
        }
    }
}

impl Extension {
    /// Establish the mandatory bot/workspace pair before any send reservation.
    ///
    /// `auth.test` is read-only. The generation check prevents a late preflight
    /// from binding replacement credentials or configuration.
    fn ensure_send_installation(&self) -> Result<(), String> {
        let (cfg, generation) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.installation_mismatch {
                return Err(
                    "Slack installation identity changed; restart Tau before sending".to_owned(),
                );
            }
            if state.bot_user_id.is_some() && state.installation_team_id.is_some() {
                return Ok(());
            }
            if state.bot_user_id.is_some() || state.installation_team_id.is_some() {
                return Err("Slack installation identity is incomplete".to_owned());
            }
            (
                state
                    .config
                    .clone()
                    .ok_or_else(|| "slack extension is not configured".to_owned())?,
                state.config_generation,
            )
        };
        let installation = self.authenticated_installation(&cfg)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.config_generation != generation || state.config.is_none() {
            return Err("Slack installation preflight became stale".to_owned());
        }
        state
            .install_or_match_installation(installation.bot_user_id, installation.team_id)
            .map(|_| ())
    }

    /// Reserve one exact send intent and move all remote I/O off tau-client's
    /// serialized reader.
    pub(super) fn handle_send(&self, invoke: ToolStarted) -> Option<Event> {
        let send_tool = self.output.wire_tool_name(SEND_TOOL_NAME);
        if let Err(message) = validate_object_fields(
            &invoke.arguments,
            &["message", "reply_to", "destination", "mention_source_user"],
        ) {
            return Some(tool_error(invoke, message));
        }
        let message = match cbor_string_field(&invoke.arguments, "message") {
            Ok(message) => message,
            Err(message) => return Some(tool_error(invoke, message)),
        };
        let reply_to = cbor_optional_string_field(&invoke.arguments, "reply_to");
        let destination_alias = cbor_optional_string_field(&invoke.arguments, "destination");
        let mention_source_user =
            match cbor_optional_bool_field(&invoke.arguments, "mention_source_user") {
                Ok(value) => value.unwrap_or(false),
                Err(message) => return Some(tool_error(invoke, message)),
            };
        let (reply_to, destination_alias) = match (reply_to, destination_alias) {
            (Ok(Some(reply)), Ok(None)) => (Some(MessageId::new(reply)), None),
            (Ok(None), Ok(Some(alias))) => (None, Some(alias)),
            (Ok(Some(_)), Ok(Some(_))) | (Ok(None), Ok(None)) => {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} requires exactly one of `reply_to` or `destination`"),
                ));
            }
            (Err(message), _) | (_, Err(message)) => return Some(tool_error(invoke, message)),
        };
        if message.trim().is_empty() {
            return Some(tool_error(invoke, "`message` must not be empty".to_owned()));
        }
        if mention_source_user && reply_to.is_none() {
            return Some(tool_error(
                invoke,
                "mention_source_user=true requires reply_to and is not supported with destination"
                    .to_owned(),
            ));
        }
        let mut installation_preflight_attempted = false;
        let prepared = loop {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            match self.replay_send_locked(&mut state, &invoke, &send_tool) {
                SendReplay::New => {}
                SendReplay::Coalesced => return None,
                SendReplay::Event(event) => return Some(*event),
                SendReplay::Completion(output) => {
                    // Thread-spawn failure retires send state synchronously, so
                    // the non-reentrant state lock must be released first.
                    drop(state);
                    self.spawn_completion_output(output);
                    return None;
                }
            }
            if state.send_ledger.len() >= SEND_LEDGER_LIMIT {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} delivery ledger is full for this session"),
                ));
            }
            let Some(cfg) = state.config.clone() else {
                return Some(tool_error(
                    invoke,
                    "slack extension is not configured".to_owned(),
                ));
            };
            if message.len() > cfg.max_message_bytes {
                return Some(tool_error(
                    invoke,
                    "`message` exceeds slack max_message_bytes".to_owned(),
                ));
            }
            if state.pending_posts.len() >= PENDING_SEND_LIMIT {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} has too many completions awaiting Tau; try again later"),
                ));
            }
            if state.active_send_workers >= ACTIVE_SEND_WORKER_LIMIT {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} delivery workers are busy; try again later"),
                ));
            }
            if self.output_failed.load(Ordering::Acquire) {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} completion output is unavailable"),
                ));
            }
            if !state.capability_active {
                return Some(tool_error(
                    invoke,
                    format!("{send_tool} transport capability is not active"),
                ));
            }

            let (
                route,
                authorization,
                endpoint,
                conversation,
                policy_status,
                reaction_authority,
                source_mention,
            ) = if let Some(reply_to) = &reply_to {
                if !state.registered_agents.contains(&invoke.agent_id) {
                    let register = self.output.wire_tool_name(REGISTER_TOOL_NAME);
                    return Some(tool_error(
                        invoke,
                        format!("Slack reply requires {register}(enabled: true) first"),
                    ));
                }
                let Some(route) = state.reply_routes.get(reply_to).cloned() else {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} reply_to is unknown or stale"),
                    ));
                };
                if route.agent_id != invoke.agent_id {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} reply_to belongs to another agent"),
                    ));
                }
                if !is_route_authorized(&state, &cfg, &route.conversation, &route.user_id) {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} originating conversation is no longer authorized"),
                    ));
                }
                if state.installation_team_id.as_deref()
                    != Some(route.installation_team_id.as_str())
                {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} originating installation is no longer active"),
                    ));
                }
                let source_mention = if mention_source_user {
                    if route.user_id == "USLACKBOT"
                        || state.bot_user_id.as_deref() == Some(route.user_id.as_str())
                    {
                        return Some(tool_error(
                            invoke,
                            "Slack source mention is unavailable or unauthorized".to_owned(),
                        ));
                    }
                    match InternalSourceMention::new(&route.user_id) {
                        Ok(mention) => Some(mention),
                        Err(error) => {
                            return Some(tool_error(invoke, error.to_string()));
                        }
                    }
                } else {
                    None
                };
                (
                    route.conversation.clone(),
                    tau_proto::TransportSendAuthorization::Reply {
                        message_id: reply_to.clone(),
                    },
                    external_endpoint_for_route(&route),
                    message_conversation(&route.conversation),
                    route.policy_status,
                    ReactionAuthority::Source {
                        message_id: reply_to.clone(),
                        user_id: route.user_id.clone(),
                    },
                    source_mention,
                )
            } else {
                let alias = destination_alias.as_ref().expect("exclusive selector");
                if !valid_conversation_alias(alias) {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} destination is unknown or unauthorized"),
                    ));
                }
                let Some(destination) = cfg
                    .proactive_aliases
                    .contains(alias)
                    .then(|| cfg.conversations.get(alias))
                    .flatten()
                else {
                    return Some(tool_error(
                        invoke,
                        format!("{send_tool} destination is unknown or unauthorized"),
                    ));
                };
                (
                    SlackConversation {
                        channel_id: destination.conversation_id.clone(),
                        thread_ts: destination.thread_ts.clone(),
                        kind: destination.kind,
                        alias: destination.alias.clone(),
                    },
                    tau_proto::TransportSendAuthorization::ConfiguredDestination {
                        alias: alias.clone(),
                    },
                    send_destination_endpoint(destination),
                    send_destination_conversation(destination),
                    SenderPolicyStatus::Internal,
                    ReactionAuthority::ConfiguredDestination {
                        alias: alias.clone(),
                    },
                    None,
                )
            };
            let text = if cfg.prefix_agent_id {
                format!("[{}] {message}", invoke.agent_id.as_ref())
            } else {
                message.clone()
            };
            let mode = match SlackPostMode::agent(text.clone(), source_mention.as_ref()) {
                Ok(mode) => mode,
                Err(error) => return Some(tool_error(invoke, error.to_string())),
            };
            let body = FrozenPostBody::new(&route.channel_id, route.thread_ts.as_deref(), &mode);
            let (bot_user_id, installation_team_id) =
                match (&state.bot_user_id, &state.installation_team_id) {
                    (Some(bot_user_id), Some(installation_team_id)) => (
                        ObservedSlackBotId::from_validated(bot_user_id),
                        installation_team_id.clone(),
                    ),
                    (None, None) if !installation_preflight_attempted => {
                        drop(state);
                        if let Err(error) = self.ensure_send_installation() {
                            return Some(tool_error(invoke, error));
                        }
                        installation_preflight_attempted = true;
                        continue;
                    }
                    (None, None) | (Some(_), None) | (None, Some(_)) => {
                        return Some(tool_error(
                            invoke,
                            format!("{send_tool} installation identity is unavailable"),
                        ));
                    }
                };
            state.config_frozen = true;
            state.next_send_reservation = state.next_send_reservation.wrapping_add(1);
            let authority = FrozenSendAuthority {
                token: state.next_send_reservation,
                session_generation: state.session_generation,
                ingress_epoch: state.ingress_epoch,
                config_generation: state.config_generation,
                agent_generation: state.send_agent_generation(&invoke.agent_id),
                capability_generation: state.capability_generation,
                instance_name: state.instance_name.clone(),
                send_tool: send_tool.clone(),
                bot_user_id,
                installation_team_id,
            };
            let prepared_at = Instant::now();
            state
                .channel_send_queues
                .entry(route.channel_id.clone())
                .or_default()
                .push_back(SendQueueReservation {
                    call_id: invoke.call_id.clone(),
                    token: authority.token,
                });
            let prepared = PreparedSend {
                invoke: invoke.clone(),
                cfg,
                route,
                authorization,
                endpoint,
                conversation,
                policy_status,
                reaction_authority,
                reply_to,
                body,
                text,
                retry_deadline: prepared_at
                    .checked_add(SEND_ATTEMPT_HORIZON)
                    .unwrap_or(prepared_at),
                authority,
            };
            state.active_send_workers += 1;
            state.send_ledger.insert(
                invoke.call_id.clone(),
                SendLedgerEntry {
                    prepared: prepared.clone(),
                    disposition: SendLedgerDisposition::Reserved,
                },
            );
            break prepared;
        };
        self.spawn_send_worker(prepared);
        None
    }

    /// Evaluate a replay while the caller owns the state lock.
    fn replay_send_locked(
        &self,
        state: &mut State,
        invoke: &ToolStarted,
        send_tool: &tau_proto::ToolName,
    ) -> SendReplay {
        let Some(entry) = state.send_ledger.get(&invoke.call_id) else {
            return SendReplay::New;
        };
        if entry.prepared.invoke.agent_id != invoke.agent_id
            || entry.prepared.invoke.arguments != invoke.arguments
        {
            return SendReplay::Event(Box::new(tool_error(
                invoke.clone(),
                format!("{send_tool} call id was replayed with conflicting arguments"),
            )));
        }
        match &entry.disposition {
            SendLedgerDisposition::Reserved => SendReplay::Coalesced,
            SendLedgerDisposition::InFlight {
                attempt,
                prior_copies,
            } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    attempt = ?attempt,
                    prior_copies = prior_copies.trace_label(),
                    "coalesced Slack send replay"
                );
                SendReplay::Coalesced
            }
            SendLedgerDisposition::RetryScheduled {
                prior_copies,
                reason,
            } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    prior_copies = prior_copies.trace_label(),
                    reason = reason.trace_label(),
                    "coalesced Slack send replay"
                );
                SendReplay::Coalesced
            }
            SendLedgerDisposition::AwaitingCompletion { request, copies: _ } => {
                let output = CompletionOutput {
                    owner: SendQueueReservation {
                        call_id: invoke.call_id.clone(),
                        token: entry.prepared.authority.token,
                    },
                    request: request.clone(),
                };
                if !state.completion_resubmitting.insert(output.owner.clone()) {
                    return SendReplay::Coalesced;
                }
                if state.active_send_workers >= ACTIVE_SEND_WORKER_LIMIT {
                    state.pending_completion_outputs.push_back(output);
                    return SendReplay::Coalesced;
                }
                state.active_send_workers += 1;
                SendReplay::Completion(output)
            }
            SendLedgerDisposition::Completed { result, copies } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    copies = copies.trace_label(),
                    "replayed durably completed Slack send"
                );
                SendReplay::Event(Box::new(Event::ToolResult((**result).clone())))
            }
            SendLedgerDisposition::DefinitiveFailure { category, copies }
            | SendLedgerDisposition::ExhaustedUnknown { category, copies } => {
                SendReplay::Event(Box::new(tool_error(
                    invoke.clone(),
                    copies.caveat().map_or_else(
                        || category.to_string(),
                        |caveat| format!("{category}; {caveat}"),
                    ),
                )))
            }
            SendLedgerDisposition::Cancelled { copies } => SendReplay::Event(Box::new(tool_error(
                invoke.clone(),
                copies.caveat().map_or_else(
                    || "Slack delivery was cancelled before I/O".to_owned(),
                    |caveat| format!("Slack delivery was cancelled; {caveat}"),
                ),
            ))),
        }
    }

    /// Spawn a bounded delivery worker and terminalize safely if the OS
    /// refuses.
    fn spawn_send_worker(&self, prepared: PreparedSend) {
        let call_id = prepared.invoke.call_id.clone();
        let token = prepared.authority.token;
        let worker = SendDeliveryWorker::from_extension(self);
        let panic_worker = worker.clone();
        let panic_prepared = prepared.clone();
        let spawn_failure_prepared = prepared.clone();
        let spawn = std::thread::Builder::new()
            .name("tau-slack-send".to_owned())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.execute_send(prepared);
                }))
                .is_err()
                {
                    panic_worker.finish_send_failure(
                        &panic_prepared,
                        SendFailureCategory::WorkerUnavailable,
                        SendFailureDisposition::ExhaustedUnknown,
                        RemoteCopyPossibility::UpToTwo,
                    );
                }
                worker.release_send_worker(&panic_prepared);
            });
        if spawn.is_err() {
            let invoke = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.send_ledger.get_mut(&call_id).and_then(|entry| {
                    if entry.prepared.authority.token != token {
                        return None;
                    }
                    entry.disposition = SendLedgerDisposition::DefinitiveFailure {
                        category: SendFailureCategory::WorkerUnavailable,
                        copies: RemoteCopyPossibility::None,
                    };
                    Some(entry.prepared.invoke.clone())
                })
            };
            SendDeliveryWorker::from_extension(self).release_send_worker(&spawn_failure_prepared);
            if let Some(invoke) = invoke {
                self.output.emit(tool_error(
                    invoke,
                    SendFailureCategory::WorkerUnavailable.to_string(),
                ));
            }
        }
    }

    /// Resubmit a cached completion without blocking tau-client's reader.
    fn spawn_completion_output(&self, output: CompletionOutput) {
        SendDeliveryWorker::from_extension(self).spawn_reserved_completion_output(output);
    }
}

/// Narrow runtime owned by one bounded background delivery task.
#[derive(Clone)]
struct SendDeliveryWorker {
    state: Arc<Mutex<State>>,
    completion_publication_gate: Arc<Mutex<()>>,
    client: Arc<dyn SlackClient>,
    output: Output,
    wake: Arc<SendWake>,
    scheduler: Arc<dyn SendScheduler>,
    shutdown: Arc<ShutdownSignal>,
    output_failed: Arc<AtomicBool>,
    completion_output_spawner: Arc<dyn CompletionOutputSpawner>,
}

impl SendDeliveryWorker {
    /// Capture only the dependencies needed after delivery admission.
    fn from_extension(extension: &Extension) -> Self {
        Self {
            state: Arc::clone(&extension.state),
            completion_publication_gate: Arc::clone(&extension.completion_publication_gate),
            client: Arc::clone(&extension.client),
            output: extension.output.clone(),
            wake: Arc::clone(&extension.send_wake),
            scheduler: Arc::clone(&extension.send_scheduler),
            shutdown: Arc::clone(&extension.shutdown),
            output_failed: Arc::clone(&extension.output_failed),
            completion_output_spawner: Arc::clone(&extension.completion_output_spawner),
        }
    }

    /// Release one bounded delivery-worker slot after spawn failure or exit.
    fn release_send_worker(&self, prepared: &PreparedSend) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.active_send_workers = state.active_send_workers.saturating_sub(1);
        state.completion_resubmitting.remove(&SendQueueReservation {
            call_id: prepared.invoke.call_id.clone(),
            token: prepared.authority.token,
        });
        let channel_id = &prepared.route.channel_id;
        let remove_queue = if let Some(queue) = state.channel_send_queues.get_mut(channel_id) {
            queue.retain(|reservation| {
                reservation.call_id != prepared.invoke.call_id
                    || reservation.token != prepared.authority.token
            });
            queue.is_empty()
        } else {
            false
        };
        if remove_queue {
            state.channel_send_queues.remove(channel_id);
        }
        let pending = state.reserve_pending_completion_output();
        drop(state);
        self.wake.notify_progress();
        if let Some(request) = pending {
            self.spawn_reserved_completion_output(request);
        }
    }

    /// Release one replay-completion slot from the shared worker bound.
    fn release_completion_worker(&self, owner: &SendQueueReservation) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.active_send_workers = state.active_send_workers.saturating_sub(1);
        state.completion_resubmitting.remove(owner);
        let pending = state.reserve_pending_completion_output();
        drop(state);
        self.wake.notify_progress();
        if let Some(output) = pending {
            self.spawn_reserved_completion_output(output);
        }
    }

    /// Spawn one completion whose shared worker slot is already reserved.
    fn spawn_reserved_completion_output(&self, output: CompletionOutput) {
        let worker = self.clone();
        let fallback = self.clone();
        let fallback_owner = output.owner.clone();
        let worker_owner = output.owner.clone();
        if self
            .completion_output_spawner
            .spawn(Box::new(move || {
                worker.confirm_completion_output(output);
                worker.release_completion_worker(&worker_owner);
            }))
            .is_err()
        {
            fallback.retire_after_output_failure();
            fallback.release_completion_worker(&fallback_owner);
        }
    }

    /// Run the initial attempt and at most one retry of the exact frozen body.
    fn execute_send(&self, prepared: PreparedSend) {
        if !self.acquire_channel_turn(&prepared) {
            self.finish_send_cancelled(&prepared, RemoteCopyPossibility::None);
            return;
        }
        let initial_not_before = self.channel_attempt_barrier(&prepared);
        if !self.wait_until_authorized(&prepared, initial_not_before) {
            self.finish_send_cancelled(&prepared, RemoteCopyPossibility::None);
            return;
        }
        match self.begin_send_attempt(&prepared, SendAttempt::Initial, RemoteCopyPossibility::None)
        {
            BeginSendAttempt::Started => {}
            BeginSendAttempt::Revoked | BeginSendAttempt::RetryHorizonExpired => {
                self.finish_send_cancelled(&prepared, RemoteCopyPossibility::None);
                return;
            }
        }
        let first = self.post_attempt(&prepared);
        if !self.send_authority_is_current(&prepared) {
            self.finish_send_cancelled(&prepared, RemoteCopyPossibility::UpToOne);
            return;
        }
        match first {
            PostAttemptOutcome::Accepted(posted) => {
                self.finish_send_success(&prepared, posted, RemoteCopyPossibility::None);
            }
            PostAttemptOutcome::DefinitiveFailure(category) => {
                let copies = if category == SendFailureCategory::ConflictingRoute {
                    RemoteCopyPossibility::One
                } else {
                    RemoteCopyPossibility::None
                };
                self.finish_send_failure(
                    &prepared,
                    category,
                    SendFailureDisposition::Definitive,
                    copies,
                );
            }
            PostAttemptOutcome::OutcomeUnknown(category) => {
                self.schedule_send_retry(
                    &prepared,
                    category,
                    Duration::from_secs(1),
                    RemoteCopyPossibility::UpToOne,
                );
            }
            PostAttemptOutcome::RateLimited(delay) => {
                self.schedule_send_retry(
                    &prepared,
                    SendFailureCategory::RateLimited,
                    delay,
                    RemoteCopyPossibility::None,
                );
            }
        }
    }

    /// Mark one attempt started only after exact current-authority validation.
    fn begin_send_attempt(
        &self,
        prepared: &PreparedSend,
        attempt: SendAttempt,
        prior_copies: RemoteCopyPossibility,
    ) -> BeginSendAttempt {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.output_failed.load(Ordering::Acquire) {
            return BeginSendAttempt::Revoked;
        }
        if !state.send_authority_is_current(prepared) {
            return BeginSendAttempt::Revoked;
        }
        if state
            .channel_send_queues
            .get(&prepared.route.channel_id)
            .and_then(|queue| queue.front())
            != Some(&SendQueueReservation {
                call_id: prepared.invoke.call_id.clone(),
                token: prepared.authority.token,
            })
        {
            return BeginSendAttempt::Revoked;
        }
        let Some(entry) = state.send_ledger.get_mut(&prepared.invoke.call_id) else {
            return BeginSendAttempt::Revoked;
        };
        if entry.prepared.authority.token != prepared.authority.token {
            return BeginSendAttempt::Revoked;
        }
        if attempt == SendAttempt::Retry && Instant::now() > entry.prepared.retry_deadline {
            return BeginSendAttempt::RetryHorizonExpired;
        }
        entry.disposition = SendLedgerDisposition::InFlight {
            attempt,
            prior_copies,
        };
        let now = Instant::now();
        state.channel_attempt_deadlines.insert(
            prepared.route.channel_id.clone(),
            now.checked_add(MIN_RETRY_DELAY).unwrap_or(now),
        );
        BeginSendAttempt::Started
    }

    /// Execute one typed provider attempt and emit payload-free timing.
    fn post_attempt(&self, prepared: &PreparedSend) -> PostAttemptOutcome<PostedMessage> {
        let post_class = if matches!(
            prepared.authorization,
            tau_proto::TransportSendAuthorization::ConfiguredDestination { alias: _ }
        ) {
            "proactive"
        } else {
            "agent_reply"
        };
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            post_class,
            "slack.api.post_message_started"
        );
        let started = Instant::now();
        let outcome = self.client.post_message(&prepared.cfg, &prepared.body);
        tracing::trace!(
            target: LOG_TARGET,
            schema = LATENCY_SCHEMA,
            post_class,
            duration_us = elapsed_us(started),
            outcome = outcome.trace_label(),
            "slack.api.post_message_finished"
        );
        outcome
    }

    /// Schedule the sole retry with per-channel ordering and event-driven
    /// lifecycle cancellation.
    fn schedule_send_retry(
        &self,
        prepared: &PreparedSend,
        reason: SendFailureCategory,
        base_delay: Duration,
        prior_copies: RemoteCopyPossibility,
    ) {
        let deadline = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.send_authority_is_current(prepared) {
                drop(state);
                self.finish_send_cancelled(prepared, prior_copies);
                return;
            }
            let delay = retry_delay(
                base_delay,
                prepared.invoke.call_id.as_str(),
                &prepared.route.channel_id,
            );
            let now = Instant::now();
            let candidate = now.checked_add(delay).unwrap_or(now);
            let deadline = state
                .channel_attempt_deadlines
                .get(&prepared.route.channel_id)
                .copied()
                .map_or(candidate, |previous| previous.max(candidate));
            if deadline > prepared.retry_deadline {
                drop(state);
                self.finish_send_failure(
                    prepared,
                    reason,
                    if prior_copies == RemoteCopyPossibility::None {
                        SendFailureDisposition::Definitive
                    } else {
                        SendFailureDisposition::ExhaustedUnknown
                    },
                    prior_copies,
                );
                return;
            }
            let Some(entry) = state.send_ledger.get_mut(&prepared.invoke.call_id) else {
                return;
            };
            entry.disposition = SendLedgerDisposition::RetryScheduled {
                prior_copies,
                reason,
            };
            deadline
        };
        if !self.wait_until_authorized(prepared, deadline) {
            self.finish_send_cancelled(prepared, prior_copies);
            return;
        }
        match self.begin_send_attempt(prepared, SendAttempt::Retry, prior_copies) {
            BeginSendAttempt::Started => {}
            BeginSendAttempt::Revoked => {
                self.finish_send_cancelled(prepared, prior_copies);
                return;
            }
            BeginSendAttempt::RetryHorizonExpired => {
                self.finish_send_failure(
                    prepared,
                    reason,
                    if prior_copies == RemoteCopyPossibility::None {
                        SendFailureDisposition::Definitive
                    } else {
                        SendFailureDisposition::ExhaustedUnknown
                    },
                    prior_copies,
                );
                return;
            }
        }
        let second = self.post_attempt(prepared);
        if !self.send_authority_is_current(prepared) {
            self.finish_send_cancelled(prepared, prior_copies.after_ambiguous());
            return;
        }
        match second {
            PostAttemptOutcome::Accepted(posted) => {
                self.finish_send_success(prepared, posted, prior_copies);
            }
            PostAttemptOutcome::DefinitiveFailure(category) => {
                let copies = if category == SendFailureCategory::ConflictingRoute {
                    prior_copies.after_success()
                } else {
                    prior_copies
                };
                self.finish_send_failure(
                    prepared,
                    category,
                    if prior_copies == RemoteCopyPossibility::None {
                        SendFailureDisposition::Definitive
                    } else {
                        SendFailureDisposition::ExhaustedUnknown
                    },
                    copies,
                );
            }
            PostAttemptOutcome::OutcomeUnknown(category) => {
                self.finish_send_failure(
                    prepared,
                    category,
                    SendFailureDisposition::ExhaustedUnknown,
                    prior_copies.after_ambiguous(),
                );
            }
            PostAttemptOutcome::RateLimited(_) => {
                self.finish_send_failure(
                    prepared,
                    SendFailureCategory::RateLimited,
                    if prior_copies == RemoteCopyPossibility::None {
                        SendFailureDisposition::Definitive
                    } else {
                        SendFailureDisposition::ExhaustedUnknown
                    },
                    prior_copies,
                );
            }
        }
    }

    /// Revalidate exact lifecycle, route, configuration, and ledger authority.
    fn send_authority_is_current(&self, prepared: &PreparedSend) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.send_authority_is_current(prepared)
    }

    /// Wait event-first until this logical call owns the front of its channel.
    fn acquire_channel_turn(&self, prepared: &PreparedSend) -> bool {
        loop {
            let generation = self.wake.generation();
            let current_and_front = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.send_authority_is_current(prepared)
                    && state
                        .channel_send_queues
                        .get(&prepared.route.channel_id)
                        .and_then(|queue| queue.front())
                        == Some(&SendQueueReservation {
                            call_id: prepared.invoke.call_id.clone(),
                            token: prepared.authority.token,
                        })
            };
            if current_and_front {
                return true;
            }
            if !self.send_authority_is_current(prepared) {
                return false;
            }
            // Queue progress always notifies. The bounded timeout is only a
            // fail-safe against a panicking predecessor.
            let _ = self
                .wake
                .wait(generation, crate::send_delivery::wire::MAX_RETRY_AFTER);
        }
    }

    /// Read the live pacing barrier only after this call owns its channel turn.
    fn channel_attempt_barrier(&self, prepared: &PreparedSend) -> Instant {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .channel_attempt_deadlines
            .get(&prepared.route.channel_id)
            .copied()
            .unwrap_or_else(Instant::now)
    }

    /// Wait until an absolute attempt deadline, treating every wake as a reason
    /// to revalidate rather than as cancellation by itself.
    fn wait_until_authorized(&self, prepared: &PreparedSend, deadline: Instant) -> bool {
        loop {
            // Capture before validation so a revocation racing the check makes
            // the following wait return immediately.
            let generation = self.wake.generation();
            if !self.send_authority_is_current(prepared) {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            if !self.scheduler.wait(&self.wake, generation, remaining) {
                return self.send_authority_is_current(prepared);
            }
        }
    }

    /// Admit one exact owner against session retirement, then write and flush
    /// its completion. A publication admitted before retirement is already
    /// started; a later or reused owner fails closed without writing.
    fn confirm_completion_output(&self, output: CompletionOutput) {
        {
            let _publication = self
                .completion_publication_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let owner_is_current = state.completion_resubmitting.contains(&output.owner)
                && state
                    .send_ledger
                    .get(&output.owner.call_id)
                    .is_some_and(|entry| {
                        entry.prepared.authority.token == output.owner.token
                            && matches!(
                                entry.disposition,
                                SendLedgerDisposition::AwaitingCompletion {
                                    request: _,
                                    copies: _
                                }
                            )
                    });
            if !owner_is_current {
                return;
            }
        }
        let sent = self
            .output
            .send_confirmed(HarnessInputMessage::CompleteTransportSend(output.request));
        if !sent {
            self.retire_after_output_failure();
        }
    }

    /// Stop all later Slack effects after a confirmed completion write fails.
    fn retire_after_output_failure(&self) {
        self.output_failed.store(true, Ordering::Release);
        retire_send_state(&self.state, &self.completion_publication_gate, &self.wake);
        self.shutdown.request();
    }

    /// Store and emit one stable terminal failure.
    fn finish_send_failure(
        &self,
        prepared: &PreparedSend,
        category: SendFailureCategory,
        disposition: SendFailureDisposition,
        copies: RemoteCopyPossibility,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        {
            let current = state.send_authority_is_current(prepared);
            let Some(entry) = state.send_ledger.get_mut(&prepared.invoke.call_id) else {
                return;
            };
            if entry.prepared.authority.token != prepared.authority.token {
                return;
            }
            if matches!(
                entry.disposition,
                SendLedgerDisposition::AwaitingCompletion {
                    request: _,
                    copies: _
                } | SendLedgerDisposition::DefinitiveFailure {
                    category: _,
                    copies: _
                } | SendLedgerDisposition::ExhaustedUnknown {
                    category: _,
                    copies: _
                } | SendLedgerDisposition::Cancelled { copies: _ }
                    | SendLedgerDisposition::Completed {
                        result: _,
                        copies: _
                    }
            ) {
                return;
            }
            let message = if !current {
                entry.disposition = SendLedgerDisposition::Cancelled { copies };
                copies.caveat().map_or_else(
                    || "Slack delivery was cancelled before I/O".to_owned(),
                    |caveat| format!("Slack delivery was cancelled; {caveat}"),
                )
            } else if disposition == SendFailureDisposition::ExhaustedUnknown {
                entry.disposition = SendLedgerDisposition::ExhaustedUnknown { category, copies };
                copies.caveat().map_or_else(
                    || category.to_string(),
                    |caveat| format!("{category}; {caveat}"),
                )
            } else {
                entry.disposition = SendLedgerDisposition::DefinitiveFailure { category, copies };
                copies.caveat().map_or_else(
                    || category.to_string(),
                    |caveat| format!("{category}; {caveat}"),
                )
            };
            self.output
                .emit(tool_error(entry.prepared.invoke.clone(), message));
        }
    }

    /// Store stable cancellation without recreating revoked authority.
    fn finish_send_cancelled(&self, prepared: &PreparedSend, copies: RemoteCopyPossibility) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        {
            let Some(entry) = state.send_ledger.get_mut(&prepared.invoke.call_id) else {
                return;
            };
            if entry.prepared.authority.token != prepared.authority.token {
                return;
            }
            if matches!(
                entry.disposition,
                SendLedgerDisposition::AwaitingCompletion {
                    request: _,
                    copies: _
                } | SendLedgerDisposition::DefinitiveFailure {
                    category: _,
                    copies: _
                } | SendLedgerDisposition::ExhaustedUnknown {
                    category: _,
                    copies: _
                } | SendLedgerDisposition::Cancelled { copies: _ }
                    | SendLedgerDisposition::Completed {
                        result: _,
                        copies: _
                    }
            ) {
                return;
            }
            entry.disposition = SendLedgerDisposition::Cancelled { copies };
            self.output.emit(tool_error(
                entry.prepared.invoke.clone(),
                copies.caveat().map_or_else(
                    || "Slack delivery was cancelled before I/O".to_owned(),
                    |caveat| format!("Slack delivery was cancelled; {caveat}"),
                ),
            ));
        }
    }

    /// Build and retain the exact harness completion after a validated Slack
    /// success and a final authority check.
    fn finish_send_success(
        &self,
        prepared: &PreparedSend,
        posted: PostedMessage,
        prior_copies: RemoteCopyPossibility,
    ) {
        let copies = prior_copies.after_success();
        if posted.channel_id != prepared.route.channel_id
            || posted.thread_ts != prepared.route.thread_ts
        {
            self.finish_send_failure(
                prepared,
                SendFailureCategory::ConflictingRoute,
                if prior_copies == RemoteCopyPossibility::None {
                    SendFailureDisposition::Definitive
                } else {
                    SendFailureDisposition::ExhaustedUnknown
                },
                copies,
            );
            return;
        }
        let request_id = format!(
            "slack-send-{}-{}-{}",
            prepared.invoke.call_id.as_str(),
            prepared.authority.session_generation,
            prepared.authority.token
        );
        let operation = MessageOperation::Create {
            payload: MessagePayload::Text {
                text: prepared.text.clone(),
                format: TextFormat::Plain,
            },
        };
        let mut draft = transport_draft(
            prepared.endpoint.clone(),
            &prepared.route,
            operation,
            false,
            ExternalMessageIdentity {
                event_id: None,
                message_id: Some(posted.ts.clone()),
                revision_id: None,
            },
            prepared.policy_status,
            prepared.authority.send_tool.clone(),
        );
        draft.conversation = Some(prepared.conversation.clone());
        let message_ref = mint_message_ref();
        let mut result = successful_tool_result(&prepared.invoke, "");
        result.result = CborValue::Map(vec![
            example_field("status", example_text("sent")),
            example_field("message_ref", example_text(&message_ref)),
            example_field(
                "delivery_copies",
                example_text(match copies {
                    RemoteCopyPossibility::One => "one",
                    RemoteCopyPossibility::OneOrTwo => "one_or_two_possible",
                    RemoteCopyPossibility::None
                    | RemoteCopyPossibility::UpToOne
                    | RemoteCopyPossibility::UpToTwo => "unknown",
                }),
            ),
        ]);
        let completion = Box::new(CompleteTransportSendRequest {
            request_id: request_id.clone(),
            call_id: prepared.invoke.call_id.clone(),
            agent_id: prepared.invoke.agent_id.clone(),
            in_reply_to: prepared.reply_to.clone(),
            authorization: prepared.authorization.clone(),
            draft,
            acceptance: MessageTransportAcceptance::SubmittedToTransport,
            tool_result: result,
        });
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.send_authority_is_current(prepared) {
                drop(state);
                self.finish_send_cancelled(prepared, copies);
                return;
            }
            let Some(entry) = state.send_ledger.get_mut(&prepared.invoke.call_id) else {
                return;
            };
            if entry.prepared.authority.token != prepared.authority.token {
                return;
            }
            entry.disposition = SendLedgerDisposition::AwaitingCompletion {
                request: completion.clone(),
                copies,
            };
            state.pending_posts.insert(
                request_id,
                PendingPostedMessage {
                    conversation: prepared.route.clone(),
                    posted,
                    agent_id: prepared.invoke.agent_id.clone(),
                    invoke: prepared.invoke.clone(),
                    message_ref,
                    authority: prepared.reaction_authority.clone(),
                    send_authority: prepared.authority.clone(),
                },
            );
            state.completion_resubmitting.insert(SendQueueReservation {
                call_id: prepared.invoke.call_id.clone(),
                token: prepared.authority.token,
            });
        }
        self.confirm_completion_output(CompletionOutput {
            owner: SendQueueReservation {
                call_id: prepared.invoke.call_id.clone(),
                token: prepared.authority.token,
            },
            request: completion,
        });
    }
}
