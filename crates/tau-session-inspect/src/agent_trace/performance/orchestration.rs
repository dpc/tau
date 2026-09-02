//! Content-free orchestration projections derived from durable references.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};
use tau_core::{AgentJournalSnapshot, PersistedAgentEventSeq};
use tau_proto::{
    AgentId, AgentOuterTurnId, AgentPromptId, CompactionTransactionId, Event, ObservationId,
    ProviderAttempt, StandaloneCompactionTrigger, StandaloneExecutionUsage, ToolCallId,
    ToolCallRef, ToolSourcePhase, ToolWaitMode, ToolWaitOutcome, UnixMicros,
};

use super::{observe_clock, projection_error};
use crate::InspectError;

/// One serialized row with deterministic within-agent ordering metadata.
pub(super) struct OrderedRow {
    /// Authoritative start sequence.
    pub(super) journal_seq: u64,
    /// Stable family order for equal starts.
    pub(super) family: u8,
    /// Stable family-local key.
    pub(super) key: String,
    /// Serialized content-free row.
    pub(super) value: Value,
}

/// One selected durable occurrence and its wall-clock comparability epoch.
struct Fact {
    /// Authoritative journal sequence.
    seq: PersistedAgentEventSeq,
    /// Durable observation identity.
    observation_id: ObservationId,
    /// Append-invocation wall sample.
    at: UnixMicros,
    /// Number of prior wall-clock regressions.
    clock_regressions: u64,
    /// Distilled content-free event class and fields.
    kind: FactKind,
}

/// Content-free projection input retained from one durable event.
enum FactKind {
    /// Event is selected but carries no orchestration fields.
    Other,
    /// Provider declaration identities and routing-only call IDs.
    Declaration(Vec<(ToolCallRef, ToolCallId)>),
    /// Canonical terminal call identity and source phase.
    CanonicalTerminal {
        /// Terminal call ID.
        call_id: ToolCallId,
        /// Foreground/background terminal family.
        phase: Option<ToolSourcePhase>,
    },
    /// Accepted cancellation relationship.
    Cancellation(tau_proto::AgentToolCancellationRequested),
    /// Tool dispatch observation.
    Dispatch(tau_proto::AgentToolDispatchObserved),
    /// Tool background transition.
    Backgrounded(tau_proto::AgentToolBackgroundedObserved),
    /// Wait parse observation.
    WaitObserved(tau_proto::AgentToolWaitObserved),
    /// Wait registration.
    WaitRegistered(tau_proto::AgentToolWaitRegistered),
    /// Activating input class.
    Activation(tau_proto::ActivationKind),
    /// Wait settlement.
    WaitSettled(tau_proto::AgentToolWaitSettled),
    /// Canonical-terminal classification.
    TerminalClassified(tau_proto::AgentToolTerminalClassified),
    /// Outer-turn start identifiers.
    OuterStarted {
        /// Stable outer turn.
        outer_turn_id: AgentOuterTurnId,
        /// Owning prompt.
        agent_prompt_id: AgentPromptId,
    },
    /// Outer-turn terminal metadata.
    OuterFinished {
        /// Stable outer turn.
        outer_turn_id: AgentOuterTurnId,
        /// Whether automatic compaction was selected.
        automatic_compaction_decision_present: bool,
    },
    /// Standalone transaction start metadata.
    StandaloneStarted {
        /// Transaction identity.
        transaction_id: CompactionTransactionId,
        /// Compact prompt identity.
        compact_prompt_id: AgentPromptId,
        /// Safe categorical trigger.
        trigger: &'static str,
    },
    /// Successful standalone transaction boundary.
    StandaloneSucceeded(CompactionTransactionId),
    /// Failed standalone transaction boundary.
    StandaloneFailed {
        /// Transaction identity.
        transaction_id: CompactionTransactionId,
        /// Safe categorical reason.
        reason: tau_proto::StandaloneCompactionFailureReason,
    },
    /// Initial standalone accounting.
    StandaloneAccounted(AttemptData),
    /// Correction replacing an awaiting-cancelled sample.
    StandaloneCorrected(AttemptData),
}

/// Content-free standalone attempt data without endpoint or rate metadata.
struct AttemptData {
    /// Provider prompt identity.
    prompt_id: AgentPromptId,
    /// Logical attempt.
    logical_attempt: ProviderAttempt,
    /// Owning transaction.
    transaction_id: CompactionTransactionId,
    /// Provider-qualified model.
    model: tau_proto::ModelId,
    /// Response-local usage scalars retained without provider/cache/session
    /// data.
    usage: AttemptUsage,
    /// Exact stored cost increment.
    cost: Option<tau_proto::EstimatedApiCost>,
    /// Semantic output disposition.
    output: tau_proto::StandaloneExecutionOutput,
}

/// Content-free response-local usage extracted before retaining a fact.
#[derive(Clone, Copy)]
enum AttemptUsage {
    /// Provider supplied no response-local usage.
    Unknown,
    /// Provider supplied the three projected response-local counters.
    Known {
        /// Input tokens sent for this attempt.
        prompt_sent_tokens: u64,
        /// Cache-hit tokens, clamped to sent tokens.
        prompt_cached_tokens: u64,
        /// Output tokens received for this attempt.
        response_received_tokens: u64,
    },
}

impl From<StandaloneExecutionUsage> for AttemptUsage {
    fn from(usage: StandaloneExecutionUsage) -> Self {
        match usage {
            StandaloneExecutionUsage::Known(usage) => Self::Known {
                prompt_sent_tokens: usage.prompt_sent_tokens,
                prompt_cached_tokens: usage.prompt_cached_tokens.min(usage.prompt_sent_tokens),
                response_received_tokens: usage.response_received_tokens,
            },
            StandaloneExecutionUsage::Unknown => Self::Unknown,
        }
    }
}

/// Collects all content-free orchestration rows for one selected journal.
pub(super) fn collect(
    snapshot: &AgentJournalSnapshot,
    agent_id: &AgentId,
    origin: Option<UnixMicros>,
) -> Result<Vec<OrderedRow>, InspectError> {
    let facts = read_facts(snapshot, agent_id)?;
    let by_id = observation_index(agent_id, &facts)?;
    let mut rows = Vec::new();
    rows.extend(tool_rows(agent_id, origin, &facts, &by_id)?);
    rows.extend(wait_rows(agent_id, origin, &facts, &by_id)?);
    rows.extend(outer_turn_rows(agent_id, origin, &facts)?);
    rows.extend(standalone_rows(agent_id, origin, &facts)?);
    Ok(rows)
}

/// Reads one validated journal while assigning wall-clock regression epochs.
fn read_facts(
    snapshot: &AgentJournalSnapshot,
    agent_id: &AgentId,
) -> Result<Vec<Fact>, InspectError> {
    let mut facts = Vec::new();
    let mut previous = None;
    let mut regressions = 0;
    for record in snapshot.records(agent_id)? {
        let record = record?;
        observe_clock(&mut previous, &mut regressions, record.recorded_at);
        let kind = distill_event(record.observation_id, record.event);
        facts.push(Fact {
            seq: record.seq,
            observation_id: record.observation_id,
            at: record.recorded_at,
            clock_regressions: regressions,
            kind,
        });
    }
    Ok(facts)
}

/// Drops all payload-bearing fields before retaining one projection fact.
fn distill_event(observation_id: ObservationId, event: Event) -> FactKind {
    match event {
        Event::ProviderResponseFinished(value) => FactKind::Declaration(
            value
                .output_items
                .into_iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    tau_proto::ContextItem::ToolCall(call) => Some((
                        ToolCallRef {
                            declaration: observation_id,
                            item_index: u32::try_from(index).ok()?,
                        },
                        call.call_id,
                    )),
                    _ => None,
                })
                .collect(),
        ),
        Event::ProviderToolResult(value) if value.kind == tau_proto::ToolResultKind::Final => {
            FactKind::CanonicalTerminal {
                call_id: value.call_id,
                phase: Some(ToolSourcePhase::Foreground),
            }
        }
        Event::ProviderToolError(value) => FactKind::CanonicalTerminal {
            call_id: value.call_id,
            phase: Some(ToolSourcePhase::Foreground),
        },
        Event::ToolBackgroundResult(value) => FactKind::CanonicalTerminal {
            call_id: value.call_id,
            phase: Some(ToolSourcePhase::Background),
        },
        Event::ToolBackgroundError(value) => FactKind::CanonicalTerminal {
            call_id: value.call_id,
            phase: Some(ToolSourcePhase::Background),
        },
        Event::ToolCancelled(value) => FactKind::CanonicalTerminal {
            call_id: value.call_id,
            phase: None,
        },
        Event::AgentToolCancellationRequested(value) => FactKind::Cancellation(value),
        Event::AgentToolDispatchObserved(value) => FactKind::Dispatch(value),
        Event::AgentToolBackgroundedObserved(value) => FactKind::Backgrounded(value),
        Event::AgentToolWaitObserved(value) => FactKind::WaitObserved(value),
        Event::AgentToolWaitRegistered(value) => FactKind::WaitRegistered(value),
        Event::AgentActivationQueued(value) => FactKind::Activation(value.kind),
        Event::AgentToolWaitSettled(value) => FactKind::WaitSettled(value),
        Event::AgentToolTerminalClassified(value) => FactKind::TerminalClassified(value),
        Event::AgentOuterTurnStarted(value) => FactKind::OuterStarted {
            outer_turn_id: value.outer_turn_id,
            agent_prompt_id: value.agent_prompt_id,
        },
        Event::AgentOuterTurnFinished(value) => FactKind::OuterFinished {
            outer_turn_id: value.outer_turn_id,
            automatic_compaction_decision_present: value.automatic_compaction_decision.is_some(),
        },
        Event::AgentStandaloneCompactionStarted(value) => FactKind::StandaloneStarted {
            transaction_id: value.transaction_id,
            compact_prompt_id: value.compact_prompt_id,
            trigger: trigger_kind(&value.trigger),
        },
        Event::AgentCompacted(value) => value
            .transaction_id
            .map_or(FactKind::Other, FactKind::StandaloneSucceeded),
        Event::AgentStandaloneCompactionFailed(value) => FactKind::StandaloneFailed {
            transaction_id: value.transaction_id,
            reason: value.reason,
        },
        Event::ProviderStandaloneExecutionAccounted(value) => {
            FactKind::StandaloneAccounted(AttemptData {
                prompt_id: value.agent_prompt_id,
                logical_attempt: value.logical_attempt,
                transaction_id: value.transaction_id,
                model: value.model,
                usage: value.usage.into(),
                cost: value.estimated_api_cost_increment,
                output: value.output,
            })
        }
        Event::ProviderStandaloneExecutionAccountingCorrected(value) => {
            FactKind::StandaloneCorrected(AttemptData {
                prompt_id: value.agent_prompt_id,
                logical_attempt: value.logical_attempt,
                transaction_id: value.transaction_id,
                model: value.model,
                usage: value.usage.into(),
                cost: value.estimated_api_cost_increment,
                output: value.output,
            })
        }
        _ => FactKind::Other,
    }
}

/// Builds the explicit-reference lookup and rejects ambiguous observations.
fn observation_index<'a>(
    agent_id: &AgentId,
    facts: &'a [Fact],
) -> Result<HashMap<ObservationId, &'a Fact>, InspectError> {
    let mut by_id = HashMap::new();
    for fact in facts {
        if by_id.insert(fact.observation_id, fact).is_some() {
            return Err(projection_error(format!(
                "agent `{agent_id}` has duplicate observation `{}`",
                fact.observation_id
            )));
        }
    }
    Ok(by_id)
}

/// Projects dispatched calls without retaining declarations, names, or
/// payloads.
fn tool_rows(
    agent_id: &AgentId,
    origin: Option<UnixMicros>,
    facts: &[Fact],
    by_id: &HashMap<ObservationId, &Fact>,
) -> Result<Vec<OrderedRow>, InspectError> {
    let mut dispatches = HashMap::<ToolCallRef, &Fact>::new();
    let mut backgrounded = HashMap::<ToolCallRef, &Fact>::new();
    let mut terminals =
        HashMap::<ToolCallRef, (&Fact, &tau_proto::AgentToolTerminalClassified)>::new();
    let calls = declared_calls(facts)?;
    for fact in facts {
        match &fact.kind {
            FactKind::Dispatch(value) => {
                require_declared(agent_id, value.call, &calls, "tool dispatch")?;
                unique(&mut dispatches, value.call, fact, agent_id, "tool dispatch")?;
            }
            FactKind::Backgrounded(value) => {
                require_declared(agent_id, value.call, &calls, "tool background")?;
                unique(
                    &mut backgrounded,
                    value.call,
                    fact,
                    agent_id,
                    "tool background",
                )?;
            }
            FactKind::TerminalClassified(value)
                if terminals.insert(value.call, (fact, value)).is_some() =>
            {
                return Err(projection_error(format!(
                    "agent `{agent_id}` call {:?} has multiple terminal classifications",
                    value.call
                )));
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    for (call, dispatch) in dispatches {
        let background = backgrounded.get(&call).copied();
        let classification = terminals.get(&call).copied();
        let terminal = classification.and_then(|(_, value)| by_id.get(&value.terminal).copied());
        let classification_resolved = classification
            .map(|(_, classification)| {
                validate_classification(agent_id, classification, terminal, &calls, by_id)
            })
            .transpose()?
            .unwrap_or(false);
        let status = if classification.is_none() {
            "incomplete"
        } else if classification_resolved {
            "completed"
        } else {
            "source_not_selected"
        };
        let mut row = base("tool_call", agent_id, dispatch, origin, "dispatch_at_us");
        row.insert(
            "call".into(),
            serde_json::to_value(call).expect("call serializes"),
        );
        row.insert("status".into(), json!(status));
        if let Some((_, value)) = classification {
            row.insert(
                "cause".into(),
                serde_json::to_value(&value.cause).expect("cause serializes"),
            );
        }
        add_fact_endpoint(&mut row, "backgrounded", background, origin);
        add_fact_endpoint(&mut row, "terminal", terminal, origin);
        add_interval(
            &mut row,
            "dispatch_to_backgrounded_us",
            Some(dispatch),
            background,
        );
        add_interval(
            &mut row,
            "backgrounded_to_terminal_us",
            background,
            terminal,
        );
        add_interval(
            &mut row,
            "dispatch_to_terminal_us",
            Some(dispatch),
            terminal,
        );
        rows.push(OrderedRow {
            journal_seq: dispatch.seq.get(),
            family: 1,
            key: format!("{:?}", call),
            value: Value::Object(row),
        });
    }
    Ok(rows)
}

/// Collects declared calls while retaining no names, arguments, or response
/// body.
fn declared_calls(facts: &[Fact]) -> Result<HashMap<ToolCallRef, ToolCallId>, InspectError> {
    let mut calls = HashMap::new();
    for fact in facts {
        let FactKind::Declaration(declared) = &fact.kind else {
            continue;
        };
        for (call, call_id) in declared {
            if calls.insert(*call, call_id.clone()).is_some() {
                return Err(projection_error(format!(
                    "call {call:?} has multiple declarations"
                )));
            }
        }
    }
    Ok(calls)
}

/// Requires a content-free observation to refer to one selected declaration.
fn require_declared(
    agent_id: &AgentId,
    call: ToolCallRef,
    calls: &HashMap<ToolCallRef, ToolCallId>,
    label: &str,
) -> Result<(), InspectError> {
    if !calls.contains_key(&call) {
        return Err(projection_error(format!(
            "agent `{agent_id}` {label} refers to undeclared call {call:?}"
        )));
    }
    Ok(())
}

/// Validates one selected terminal classification and cancellation reference.
fn validate_classification(
    agent_id: &AgentId,
    classification: &tau_proto::AgentToolTerminalClassified,
    terminal: Option<&Fact>,
    calls: &HashMap<ToolCallRef, ToolCallId>,
    by_id: &HashMap<ObservationId, &Fact>,
) -> Result<bool, InspectError> {
    require_declared(
        agent_id,
        classification.call,
        calls,
        "terminal classification",
    )?;
    let Some(terminal) = terminal else {
        return Ok(false);
    };
    let FactKind::CanonicalTerminal { call_id, .. } = &terminal.kind else {
        return Err(projection_error(format!(
            "agent `{agent_id}` classification terminal `{}` is not canonical",
            classification.terminal
        )));
    };
    if calls.get(&classification.call) != Some(call_id) {
        return Err(projection_error(format!(
            "agent `{agent_id}` classification terminal owns a different call"
        )));
    }
    if let tau_proto::ToolTerminalCause::Cancellation { request } = classification.cause {
        let Some(request) = by_id.get(&request).copied() else {
            return Ok(false);
        };
        let FactKind::Cancellation(request) = &request.kind else {
            return Err(projection_error(format!(
                "agent `{agent_id}` cancellation cause does not reference a cancellation request"
            )));
        };
        if request.target_call != classification.call {
            return Err(projection_error(format!(
                "agent `{agent_id}` cancellation request targets a different call"
            )));
        }
    }
    Ok(true)
}

/// Validates exact-wait declaration ownership without parsing provider
/// arguments.
fn validate_wait_mode(
    agent_id: &AgentId,
    wait_call: ToolCallRef,
    mode: &ToolWaitMode,
    calls: &HashMap<ToolCallRef, ToolCallId>,
) -> Result<(), InspectError> {
    if let ToolWaitMode::Exact { target } = mode {
        require_declared(agent_id, *target, calls, "exact wait target")?;
        if *target == wait_call {
            return Err(projection_error(format!(
                "agent `{agent_id}` exact wait targets itself"
            )));
        }
    }
    if let ToolWaitMode::ExactAll { targets } = mode {
        for target in targets {
            require_declared(agent_id, *target, calls, "exact-all wait target")?;
            if *target == wait_call {
                return Err(projection_error(format!(
                    "agent `{agent_id}` exact-all wait targets itself"
                )));
            }
        }
    }
    Ok(())
}

/// Validates every selected endpoint required by one typed wait settlement.
fn validate_wait_settlement(
    agent_id: &AgentId,
    observed: &tau_proto::AgentToolWaitObserved,
    settlement: &tau_proto::AgentToolWaitSettled,
    terminal: Option<&Fact>,
    calls: &HashMap<ToolCallRef, ToolCallId>,
    by_id: &HashMap<ObservationId, &Fact>,
) -> Result<(), InspectError> {
    if settlement.wait_call != observed.wait_call {
        return Err(projection_error(format!(
            "agent `{agent_id}` wait settlement refers to a different call"
        )));
    }
    if !wait_mode_allows_outcome(&observed.mode, settlement) {
        return Err(projection_error(format!(
            "agent `{agent_id}` wait mode and outcome contradict"
        )));
    }
    if let Some(registration) = settlement.registration
        && let Some(registration) = by_id.get(&registration).copied()
    {
        let FactKind::WaitRegistered(registration) = &registration.kind else {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait registration reference has the wrong event type"
            )));
        };
        if registration.wait_observation != settlement.wait_observation
            || registration.wait_call != settlement.wait_call
            || registration.mode != observed.mode
        {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait registration contradicts its observation"
            )));
        }
    }
    if let Some(terminal) = terminal {
        let FactKind::CanonicalTerminal { call_id, .. } = &terminal.kind else {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait terminal reference is not canonical"
            )));
        };
        if calls.get(&settlement.wait_call) != Some(call_id) {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait terminal owns a different call"
            )));
        }
    }
    match &settlement.outcome {
        ToolWaitOutcome::CompletionDelivered {
            source_call,
            source_terminal,
            source_phase,
            ..
        } => {
            require_declared(agent_id, *source_call, calls, "wait source")?;
            if let ToolWaitMode::Exact { target } = observed.mode
                && target != *source_call
            {
                return Err(projection_error(format!(
                    "agent `{agent_id}` exact wait delivered a different source"
                )));
            }
            if let Some(source) = by_id.get(source_terminal).copied() {
                let FactKind::CanonicalTerminal { call_id, phase } = &source.kind else {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` wait source terminal is not canonical"
                    )));
                };
                if calls.get(source_call) != Some(call_id)
                    || phase.is_some_and(|phase| phase != *source_phase)
                {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` wait source terminal contradicts its call or phase"
                    )));
                }
            }
        }
        ToolWaitOutcome::CompletionsDelivered { sources } => {
            for source in sources {
                require_declared(agent_id, source.source_call, calls, "wait source")?;
                if let Some(terminal) = by_id.get(&source.source_terminal).copied() {
                    let FactKind::CanonicalTerminal { call_id, phase } = &terminal.kind else {
                        return Err(projection_error(format!(
                            "agent `{agent_id}` wait source terminal is not canonical"
                        )));
                    };
                    if calls.get(&source.source_call) != Some(call_id)
                        || phase.is_some_and(|phase| phase != source.source_phase)
                    {
                        return Err(projection_error(format!(
                            "agent `{agent_id}` wait source terminal contradicts its call or phase"
                        )));
                    }
                }
            }
        }
        ToolWaitOutcome::InterruptedByActivation { activation }
        | ToolWaitOutcome::InputAvailable { activation } => {
            if let Some(activation) = by_id.get(activation).copied()
                && !matches!(activation.kind, FactKind::Activation(_))
            {
                return Err(projection_error(format!(
                    "agent `{agent_id}` wait activation reference has the wrong event type"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Returns whether one settlement is valid for its parsed mode and
/// registration.
fn wait_mode_allows_outcome(
    mode: &ToolWaitMode,
    settlement: &tau_proto::AgentToolWaitSettled,
) -> bool {
    use tau_proto::{
        ToolOutputEnvelope as Envelope, ToolWaitMode as Mode, ToolWaitOutcome as Outcome,
        WaitRejectionReason as Reject,
    };
    let registered = settlement.registration.is_some();
    match (mode, &settlement.outcome) {
        (
            Mode::Exact { target },
            Outcome::CompletionDelivered {
                source_call,
                envelope,
                ..
            },
        ) => target == source_call && *envelope == Envelope::Identity,
        (Mode::ExactAll { targets }, Outcome::CompletionsDelivered { sources }) => {
            targets.len() == sources.len()
                && targets.iter().zip(sources).all(|(target, source)| {
                    target == &source.source_call && source.envelope == Envelope::Identity
                })
        }
        (
            Mode::NextBackground,
            Outcome::CompletionDelivered {
                source_phase,
                envelope,
                ..
            },
        ) => {
            *source_phase == ToolSourcePhase::Background
                && *envelope == Envelope::OriginalToolCallIdHeader
        }
        (
            Mode::Exact { .. } | Mode::ExactAll { .. } | Mode::NextBackground,
            Outcome::InterruptedByActivation { .. },
        )
        | (Mode::ActivatingInput { .. }, Outcome::InputAvailable { .. }) => true,
        (Mode::ActivatingInput { .. }, Outcome::TimedOut)
        | (
            Mode::Exact { .. }
            | Mode::ExactAll { .. }
            | Mode::NextBackground
            | Mode::ActivatingInput { .. },
            Outcome::Cancelled | Outcome::LifecycleAborted,
        ) => registered,
        (
            Mode::Exact { .. } | Mode::ExactAll { .. },
            Outcome::Rejected {
                reason:
                    Reject::DuplicateExactWait
                    | Reject::TargetReturnedForegroundBeforeWait
                    | Reject::ResultAlreadyConsumed,
            },
        )
        | (
            Mode::NextBackground,
            Outcome::Rejected {
                reason: Reject::DuplicateAnyWait,
            },
        )
        | (
            Mode::ActivatingInput { .. },
            Outcome::Rejected {
                reason: Reject::DuplicateInputWait,
            },
        )
        | (
            Mode::ExactUnresolved,
            Outcome::Rejected {
                reason: Reject::UnknownTarget,
            },
        )
        | (
            Mode::ExactAllUnresolved,
            Outcome::Rejected {
                reason: Reject::UnknownTarget,
            },
        )
        | (
            Mode::InvalidArguments,
            Outcome::Rejected {
                reason: Reject::InvalidArguments,
            },
        ) => !registered,
        (
            Mode::NextBackground,
            Outcome::Rejected {
                reason: Reject::NoBackgroundCandidate,
            },
        ) => true,
        _ => false,
    }
}

/// Projects typed wait registration, settlement, and referenced timing.
fn wait_rows(
    agent_id: &AgentId,
    origin: Option<UnixMicros>,
    facts: &[Fact],
    by_id: &HashMap<ObservationId, &Fact>,
) -> Result<Vec<OrderedRow>, InspectError> {
    let calls = declared_calls(facts)?;
    let mut observed = HashMap::<ToolCallRef, (&Fact, &tau_proto::AgentToolWaitObserved)>::new();
    let mut registered =
        HashMap::<ToolCallRef, (&Fact, &tau_proto::AgentToolWaitRegistered)>::new();
    let mut settled = HashMap::<ToolCallRef, (&Fact, &tau_proto::AgentToolWaitSettled)>::new();
    for fact in facts {
        match &fact.kind {
            FactKind::WaitObserved(value) => {
                require_declared(agent_id, value.wait_call, &calls, "wait observation")?;
                validate_wait_mode(agent_id, value.wait_call, &value.mode, &calls)?;
                if observed.insert(value.wait_call, (fact, value)).is_some() {
                    return Err(duplicate(agent_id, value.wait_call, "wait observation"));
                }
            }
            FactKind::WaitRegistered(value) => {
                require_declared(agent_id, value.wait_call, &calls, "wait registration")?;
                if registered.insert(value.wait_call, (fact, value)).is_some() {
                    return Err(duplicate(agent_id, value.wait_call, "wait registration"));
                }
            }
            FactKind::WaitSettled(value)
                if settled.insert(value.wait_call, (fact, value)).is_some() =>
            {
                return Err(duplicate(agent_id, value.wait_call, "wait settlement"));
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    for (wait_call, (start, observation)) in observed {
        let registration = registered.get(&wait_call).copied();
        let settlement = settled.get(&wait_call).copied();
        if registration.is_some_and(|(_, value)| {
            value.wait_observation != start.observation_id || value.mode != observation.mode
        }) {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait {wait_call:?} has contradictory registration"
            )));
        }
        if settlement.is_some_and(|(_, value)| value.wait_observation != start.observation_id) {
            return Err(projection_error(format!(
                "agent `{agent_id}` wait {wait_call:?} has contradictory settlement"
            )));
        }
        let terminal = settlement.and_then(|(_, value)| by_id.get(&value.wait_terminal).copied());
        if let Some((_, value)) = settlement {
            validate_wait_settlement(agent_id, observation, value, terminal, &calls, by_id)?;
        }
        let registration_status = match settlement {
            Some((_, value)) if value.registration.is_none() => "immediate",
            Some((_, value)) if value.registration.and_then(|id| by_id.get(&id)).is_some() => {
                "active"
            }
            None if registration.is_some() => "active",
            _ => "unresolved",
        };
        let mut row = base("wait", agent_id, start, origin, "observed_at_us");
        row.insert(
            "wait_call".into(),
            serde_json::to_value(wait_call).expect("call serializes"),
        );
        add_wait_mode(&mut row, &observation.mode);
        row.insert("registration".into(), json!(registration_status));
        add_fact_endpoint(&mut row, "terminal", terminal, origin);
        if let Some((_, value)) = settlement {
            add_wait_outcome(&mut row, value, by_id)?;
            if let Some(registration_id) = value.registration {
                add_interval(
                    &mut row,
                    "active_wait_us",
                    by_id.get(&registration_id).copied(),
                    terminal,
                );
            }
            match &value.outcome {
                ToolWaitOutcome::CompletionDelivered {
                    source_terminal, ..
                } => add_interval(
                    &mut row,
                    "completion_to_delivery_us",
                    by_id.get(source_terminal).copied(),
                    terminal,
                ),
                ToolWaitOutcome::InterruptedByActivation { activation }
                | ToolWaitOutcome::InputAvailable { activation } => add_interval(
                    &mut row,
                    "activation_to_wait_terminal_us",
                    by_id.get(activation).copied(),
                    terminal,
                ),
                _ => {}
            }
        } else {
            row.insert("outcome".into(), json!("incomplete"));
        }
        rows.push(OrderedRow {
            journal_seq: start.seq.get(),
            family: 2,
            key: format!("{:?}", wait_call),
            value: Value::Object(row),
        });
    }
    Ok(rows)
}

/// Projects explicit outer-turn start and finish boundaries.
fn outer_turn_rows(
    agent_id: &AgentId,
    origin: Option<UnixMicros>,
    facts: &[Fact],
) -> Result<Vec<OrderedRow>, InspectError> {
    let mut starts = BTreeMap::<AgentOuterTurnId, (&Fact, &AgentPromptId)>::new();
    let mut finishes = HashMap::<AgentOuterTurnId, (&Fact, bool)>::new();
    for fact in facts {
        match &fact.kind {
            FactKind::OuterStarted {
                outer_turn_id,
                agent_prompt_id,
            } => {
                if starts
                    .insert(outer_turn_id.clone(), (fact, agent_prompt_id))
                    .is_some()
                {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` outer turn `{}` has multiple starts",
                        outer_turn_id
                    )));
                }
            }
            FactKind::OuterFinished {
                outer_turn_id,
                automatic_compaction_decision_present,
            } if finishes
                .insert(
                    outer_turn_id.clone(),
                    (fact, *automatic_compaction_decision_present),
                )
                .is_some() =>
            {
                return Err(projection_error(format!(
                    "agent `{agent_id}` outer turn `{}` has multiple finishes",
                    outer_turn_id
                )));
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    for (outer_turn_id, (start, agent_prompt_id)) in starts {
        let finish = finishes.get(&outer_turn_id).copied();
        let mut row = base("outer_turn", agent_id, start, origin, "started_at_us");
        row.insert("outer_turn_id".into(), json!(outer_turn_id));
        row.insert("agent_prompt_id".into(), json!(agent_prompt_id));
        row.insert(
            "status".into(),
            json!(if finish.is_some() {
                "settled"
            } else {
                "incomplete"
            }),
        );
        add_fact_endpoint(&mut row, "terminal", finish.map(|(fact, _)| fact), origin);
        add_interval(
            &mut row,
            "recorded_at_wall_elapsed_us",
            Some(start),
            finish.map(|(fact, _)| fact),
        );
        if let Some((_, automatic_compaction_decision_present)) = finish {
            row.insert(
                "automatic_compaction_decision_present".into(),
                json!(automatic_compaction_decision_present),
            );
        }
        rows.push(OrderedRow {
            journal_seq: start.seq.get(),
            family: 3,
            key: outer_turn_id.to_string(),
            value: Value::Object(row),
        });
    }
    Ok(rows)
}

/// Effective standalone attempt evidence after any cancellation correction.
struct Attempt<'a> {
    /// Accounting occurrence owning the effective values.
    fact: &'a Fact,
    /// Distilled accounting data.
    data: &'a AttemptData,
    /// Whether this row replaced the awaiting-cancelled observation.
    corrected: bool,
}

/// Projects standalone transaction boundaries and correction-folded attempts.
fn standalone_rows(
    agent_id: &AgentId,
    origin: Option<UnixMicros>,
    facts: &[Fact],
) -> Result<Vec<OrderedRow>, InspectError> {
    let mut starts =
        BTreeMap::<CompactionTransactionId, (&Fact, &AgentPromptId, &'static str)>::new();
    let mut terminals = HashMap::<
        CompactionTransactionId,
        (&Fact, Option<tau_proto::StandaloneCompactionFailureReason>),
    >::new();
    let mut attempts = HashMap::<(AgentPromptId, ProviderAttempt), Attempt<'_>>::new();
    for fact in facts {
        match &fact.kind {
            FactKind::StandaloneStarted {
                transaction_id,
                compact_prompt_id,
                trigger,
            } => {
                if starts
                    .insert(transaction_id.clone(), (fact, compact_prompt_id, *trigger))
                    .is_some()
                {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` standalone transaction `{}` has multiple starts",
                        transaction_id
                    )));
                }
            }
            FactKind::StandaloneSucceeded(transaction_id)
                if terminals
                    .insert(transaction_id.clone(), (fact, None))
                    .is_some() =>
            {
                return Err(projection_error(format!(
                    "agent `{agent_id}` standalone transaction `{transaction_id}` has multiple terminals"
                )));
            }
            FactKind::StandaloneFailed {
                transaction_id,
                reason,
            } => {
                if terminals
                    .insert(transaction_id.clone(), (fact, Some(*reason)))
                    .is_some()
                {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` standalone transaction `{}` has multiple terminals",
                        transaction_id
                    )));
                }
            }
            FactKind::StandaloneAccounted(value) => {
                let key = (value.prompt_id.clone(), value.logical_attempt);
                if attempts
                    .insert(
                        key,
                        Attempt {
                            fact,
                            data: value,
                            corrected: false,
                        },
                    )
                    .is_some()
                {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` has duplicate standalone accounting"
                    )));
                }
            }
            FactKind::StandaloneCorrected(value) => {
                let key = (value.prompt_id.clone(), value.logical_attempt);
                let Some(previous) = attempts.get(&key) else {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` standalone correction has no initial accounting"
                    )));
                };
                if previous.corrected {
                    return Err(projection_error(format!(
                        "agent `{agent_id}` has duplicate standalone correction"
                    )));
                }
                attempts.insert(
                    key,
                    Attempt {
                        fact,
                        data: value,
                        corrected: true,
                    },
                );
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    for (transaction_id, (start, compact_prompt_id, trigger)) in starts {
        let terminal = terminals.get(&transaction_id).copied();
        let mut transaction_attempts = attempts
            .values()
            .filter(|attempt| attempt.data.transaction_id == transaction_id)
            .collect::<Vec<_>>();
        transaction_attempts.sort_by_key(|attempt| attempt.data.logical_attempt.get());
        let mut row = base(
            "standalone_compaction",
            agent_id,
            start,
            origin,
            "started_at_us",
        );
        row.insert("transaction_id".into(), json!(transaction_id));
        row.insert("compact_prompt_id".into(), json!(compact_prompt_id));
        row.insert("trigger".into(), json!(trigger));
        row.insert(
            "status".into(),
            json!(match terminal {
                Some((_, None)) => "succeeded",
                Some((_, Some(_))) => "failed",
                None => "incomplete",
            }),
        );
        if let Some((_, Some(failure))) = terminal {
            row.insert(
                "failure_reason".into(),
                serde_json::to_value(failure).expect("reason serializes"),
            );
        }
        add_fact_endpoint(&mut row, "terminal", terminal.map(|(fact, _)| fact), origin);
        add_interval(
            &mut row,
            "recorded_at_wall_elapsed_us",
            Some(start),
            terminal.map(|(fact, _)| fact),
        );
        row.insert("attempt_count".into(), json!(transaction_attempts.len()));
        row.insert(
            "attempts".into(),
            Value::Array(
                transaction_attempts
                    .into_iter()
                    .map(|attempt| attempt_value(attempt, origin))
                    .collect(),
            ),
        );
        rows.push(OrderedRow {
            journal_seq: start.seq.get(),
            family: 4,
            key: transaction_id.to_string(),
            value: Value::Object(row),
        });
    }
    Ok(rows)
}

/// Serializes one effective standalone attempt without rates or provider
/// payloads.
fn attempt_value(attempt: &Attempt<'_>, origin: Option<UnixMicros>) -> Value {
    let mut value = Map::new();
    value.insert("agent_prompt_id".into(), json!(attempt.data.prompt_id));
    value.insert(
        "logical_attempt".into(),
        json!(attempt.data.logical_attempt.get()),
    );
    value.insert("model".into(), json!(attempt.data.model));
    value.insert(
        "accounting_journal_seq".into(),
        json!(attempt.fact.seq.get()),
    );
    if let Some(at) = relative_time(attempt.fact.at, origin) {
        value.insert("accounting_at_us".into(), json!(at));
    }
    value.insert("corrected".into(), json!(attempt.corrected));
    value.insert(
        "output".into(),
        serde_json::to_value(attempt.data.output).expect("output serializes"),
    );
    match attempt.data.usage {
        AttemptUsage::Known {
            prompt_sent_tokens,
            prompt_cached_tokens,
            response_received_tokens,
        } => {
            value.insert("usage_known".into(), json!(true));
            value.insert("prompt_sent_tokens".into(), json!(prompt_sent_tokens));
            value.insert("prompt_cached_tokens".into(), json!(prompt_cached_tokens));
            value.insert(
                "response_received_tokens".into(),
                json!(response_received_tokens),
            );
        }
        AttemptUsage::Unknown => {
            value.insert("usage_known".into(), json!(false));
        }
    }
    if let Some(cost) = attempt.data.cost {
        value.insert(
            "estimated_api_cost_picodollars".into(),
            json!(cost.as_picodollars()),
        );
    }
    Value::Object(value)
}

/// Adds the common occurrence identity and relative start time.
fn base(
    record_type: &'static str,
    agent_id: &AgentId,
    start: &Fact,
    origin: Option<UnixMicros>,
    start_time_field: &'static str,
) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("record_type".into(), json!(record_type));
    row.insert("agent_id".into(), json!(agent_id));
    row.insert("journal_seq".into(), json!(start.seq.get()));
    if let Some(at) = relative_time(start.at, origin) {
        row.insert(start_time_field.into(), json!(at));
    }
    row
}

/// Adds a selected endpoint's sequence and relative time.
fn add_fact_endpoint(
    row: &mut Map<String, Value>,
    prefix: &str,
    fact: Option<&Fact>,
    origin: Option<UnixMicros>,
) {
    let Some(fact) = fact else {
        return;
    };
    row.insert(format!("{prefix}_journal_seq"), json!(fact.seq.get()));
    if let Some(at) = relative_time(fact.at, origin) {
        row.insert(format!("{prefix}_at_us"), json!(at));
    }
}

/// Adds a qualified interval only within one non-regressing clock epoch.
fn add_interval(
    row: &mut Map<String, Value>,
    field: &str,
    start: Option<&Fact>,
    terminal: Option<&Fact>,
) {
    let Some((start, terminal)) = start.zip(terminal) else {
        return;
    };
    if start.clock_regressions == terminal.clock_regressions
        && start.at.get() != 0
        && let Some(elapsed) = terminal.at.get().checked_sub(start.at.get())
    {
        row.insert(field.into(), json!(elapsed));
    }
}

/// Adds the typed wait mode while exposing only the effective input timeout.
fn add_wait_mode(row: &mut Map<String, Value>, mode: &ToolWaitMode) {
    match mode {
        ToolWaitMode::Exact { target } => {
            row.insert("mode".into(), json!("exact"));
            row.insert(
                "target_call".into(),
                serde_json::to_value(target).expect("call serializes"),
            );
        }
        ToolWaitMode::ExactAll { targets } => {
            row.insert("mode".into(), json!("exact_all"));
            row.insert(
                "target_calls".into(),
                serde_json::to_value(targets).expect("calls serialize"),
            );
        }
        ToolWaitMode::ExactUnresolved => {
            row.insert("mode".into(), json!("exact_unresolved"));
        }
        ToolWaitMode::ExactAllUnresolved => {
            row.insert("mode".into(), json!("exact_all_unresolved"));
        }
        ToolWaitMode::NextBackground => {
            row.insert("mode".into(), json!("next_background"));
        }
        ToolWaitMode::ActivatingInput {
            effective_timeout_minutes,
        } => {
            row.insert("mode".into(), json!("activating_input"));
            row.insert(
                "effective_timeout_minutes".into(),
                json!(effective_timeout_minutes),
            );
        }
        ToolWaitMode::InvalidArguments => {
            row.insert("mode".into(), json!("invalid_arguments"));
        }
    }
}

/// Adds a typed wait outcome and selected content-free references.
fn add_wait_outcome(
    row: &mut Map<String, Value>,
    settlement: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
) -> Result<(), InspectError> {
    match &settlement.outcome {
        ToolWaitOutcome::CompletionDelivered {
            source_call,
            source_terminal,
            source_phase,
            envelope,
        } => {
            row.insert("outcome".into(), json!("completion_delivered"));
            row.insert(
                "source_call".into(),
                serde_json::to_value(source_call).expect("call serializes"),
            );
            row.insert("source_terminal".into(), json!(source_terminal));
            row.insert(
                "source_phase".into(),
                serde_json::to_value(source_phase).expect("phase serializes"),
            );
            row.insert(
                "envelope".into(),
                serde_json::to_value(envelope).expect("envelope serializes"),
            );
        }
        ToolWaitOutcome::CompletionsDelivered { sources } => {
            row.insert("outcome".into(), json!("completions_delivered"));
            row.insert(
                "sources".into(),
                serde_json::to_value(sources).expect("sources serialize"),
            );
        }
        ToolWaitOutcome::InterruptedByActivation { activation }
        | ToolWaitOutcome::InputAvailable { activation } => {
            row.insert(
                "outcome".into(),
                json!(if matches!(
                    settlement.outcome,
                    ToolWaitOutcome::InterruptedByActivation { .. }
                ) {
                    "interrupted_by_activation"
                } else {
                    "input_available"
                }),
            );
            row.insert("activation".into(), json!(activation));
            if let Some(Fact {
                kind: FactKind::Activation(kind),
                ..
            }) = by_id.get(activation).copied()
            {
                row.insert(
                    "activation_kind".into(),
                    serde_json::to_value(kind).expect("kind serializes"),
                );
            }
        }
        ToolWaitOutcome::TimedOut => {
            row.insert("outcome".into(), json!("timed_out"));
        }
        ToolWaitOutcome::Rejected { reason } => {
            row.insert("outcome".into(), json!("rejected"));
            row.insert(
                "rejection_reason".into(),
                serde_json::to_value(reason).expect("reason serializes"),
            );
        }
        ToolWaitOutcome::Cancelled => {
            row.insert("outcome".into(), json!("cancelled"));
        }
        ToolWaitOutcome::LifecycleAborted => {
            row.insert("outcome".into(), json!("lifecycle_aborted"));
        }
    }
    Ok(())
}

/// Converts a rich trigger to its safe categorical discriminator.
fn trigger_kind(trigger: &StandaloneCompactionTrigger) -> &'static str {
    match trigger {
        StandaloneCompactionTrigger::Manual => "manual",
        StandaloneCompactionTrigger::AutomaticThreshold => "automatic_threshold",
        StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. } => {
            "automatic_threshold_evidence"
        }
        StandaloneCompactionTrigger::AutomaticContinuation { .. } => "automatic_continuation",
        StandaloneCompactionTrigger::AutomaticContextRetreat { .. } => "automatic_context_retreat",
        StandaloneCompactionTrigger::AutomaticPreflightFailure { .. } => {
            "automatic_preflight_failure"
        }
        StandaloneCompactionTrigger::AutomaticPolicy { .. } => "automatic_policy",
        StandaloneCompactionTrigger::ManualAgentTool { .. } => "manual_agent_tool",
        StandaloneCompactionTrigger::ManualUi { .. } => "manual_ui",
        StandaloneCompactionTrigger::ReactiveContextOverflow { .. } => "reactive_context_overflow",
        StandaloneCompactionTrigger::ReactivePreflightFailure { .. } => {
            "reactive_preflight_failure"
        }
    }
}

/// Inserts one unique keyed fact.
fn unique<'a, K: Eq + std::hash::Hash>(
    map: &mut HashMap<K, &'a Fact>,
    key: K,
    fact: &'a Fact,
    agent_id: &AgentId,
    label: &str,
) -> Result<(), InspectError> {
    if map.insert(key, fact).is_some() {
        return Err(projection_error(format!(
            "agent `{agent_id}` has duplicate {label}"
        )));
    }
    Ok(())
}

/// Builds a duplicate-fact diagnostic.
fn duplicate(agent_id: &AgentId, call: ToolCallRef, label: &str) -> InspectError {
    projection_error(format!(
        "agent `{agent_id}` call {call:?} has multiple {label} facts"
    ))
}

/// Computes trace-relative time for one nonzero sample.
fn relative_time(at: UnixMicros, origin: Option<UnixMicros>) -> Option<u64> {
    (at.get() != 0)
        .then_some(at)
        .zip(origin)
        .and_then(|(at, origin)| at.get().checked_sub(origin.get()))
}
