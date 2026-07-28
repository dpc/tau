//! Compact, explicit-observation tool trace projection.

mod toon;

// Keep test-only modules after every production module.
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use tau_core::{AgentJournalSnapshot, PersistedAgentEventSeq};
use tau_proto::{AgentId, CborValue, ContextItem, Event, ObservationId, ToolCallRef, UnixMicros};

use crate::InspectError;

const SCHEMA: &str = "tau.agent_tools";
const LITE_OUTPUT_BYTES: usize = 4 * 1024;

/// First-line metadata shared by both compact encodings.
#[derive(Serialize)]
pub(super) struct Header<'a> {
    /// Stable schema identifier.
    schema: &'static str,
    /// Initial internal schema revision.
    schema_version: u32,
    /// Record discriminator.
    record_type: &'static str,
    /// Requested workflow root.
    root_agent_id: &'a AgentId,
    /// Deterministically selected journal identities.
    included_agent_ids: Vec<&'a AgentId>,
    /// Selected payload detail.
    output: &'static str,
    /// Unit used by qualified intervals.
    time_unit: &'static str,
    /// Origin of timestamps used for intervals.
    timing_basis: &'static str,
    /// Only permitted source of causal edges.
    causality: &'static str,
}

impl<'a> Header<'a> {
    /// Builds the stable compact trace header.
    fn new(
        root: &'a AgentId,
        snapshot: &'a AgentJournalSnapshot,
        mode: super::AgentTraceMode,
    ) -> Self {
        Self {
            schema: SCHEMA,
            schema_version: 0,
            record_type: "header",
            root_agent_id: root,
            included_agent_ids: snapshot.agent_ids().collect(),
            output: mode.label(),
            time_unit: "microseconds",
            timing_basis: "producer_wall_clock_at_observation",
            causality: "explicit_observation_refs_only",
        }
    }
}

#[derive(Clone)]
/// One selected durable occurrence with its owning journal identity.
struct Fact {
    /// Agent journal that owns the occurrence.
    agent_id: AgentId,
    /// Opaque durable occurrence identity.
    id: ObservationId,
    /// Producer wall-clock observation timestamp.
    at: UnixMicros,
    /// Journal-local authoritative sequence.
    seq: PersistedAgentEventSeq,
    /// Complete selected event.
    event: Event,
}

/// A completely materialized semantic record used identically by JSONL and
/// TOON.
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(super) enum Record {
    /// One provider-declared tool call.
    Call(CallRecord),
    /// One accepted inference activation.
    Activation(ActivationRecord),
    /// One explicit correlation edge.
    Relationship(RelationshipRecord),
}

/// Typed resolution of one selected observation reference.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Resolution {
    /// The referenced occurrence is selected in the same agent journal.
    Resolved,
    /// The referenced occurrence is unavailable in this relationship's
    /// journal-local projection.
    SourceNotSelected,
}

/// Typed terminal state for one projected call.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum CallStatus {
    /// Canonical terminal completed successfully.
    Ok,
    /// Canonical terminal records cancellation.
    Cancelled,
    /// Canonical terminal records a tool error.
    Error,
    /// Terminal evidence is incomplete.
    Incomplete,
}

/// Typed registration state for one wait relationship.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RegistrationState {
    /// A selected local registration exists.
    Active,
    /// Runtime settled without installing a waiter.
    Immediate,
    /// Registration evidence is unavailable.
    Unresolved,
}

/// Typed completion state for one wait registration.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RegistrationOutcome {
    /// One fully local settlement references the registration.
    Settled,
    /// No fully local settlement references the registration.
    Incomplete,
}

/// Typed compact call projection.
#[derive(Clone, Serialize)]
pub(super) struct CallRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Journal that owns the provider declaration.
    agent_id: AgentId,
    /// Stable declaration occurrence and output-item index.
    call: ToolCallRef,
    /// Provider display/routing identifier.
    call_id: String,
    /// Declared tool name.
    tool: tau_proto::ToolName,
    /// Lossless JSON-compatible or tagged-CBOR arguments.
    arguments: serde_json::Value,
    /// Shell command extracted from arguments when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// Qualified declaration-to-dispatch interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    declaration_to_dispatch_us: Option<u64>,
    /// Qualified dispatch-to-background interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    dispatch_to_backgrounded_us: Option<u64>,
    /// Coupled terminal, status, timing, and output state.
    #[serde(flatten)]
    lifecycle: CallLifecycleRecord,
}

/// Semantic terminal state of one compact call.
#[derive(Clone, Serialize)]
#[serde(untagged)]
enum CallLifecycleRecord {
    /// No fully selected local canonical terminal exists.
    Incomplete {
        /// Fixed incomplete status.
        status: IncompleteCallStatus,
    },
    /// A local classification references an unavailable canonical terminal.
    Unresolved {
        /// Fixed incomplete status.
        status: IncompleteCallStatus,
        /// Candidate canonical terminal identity.
        terminal: ObservationId,
        /// Runtime terminal classification.
        cause: tau_proto::ToolTerminalCause,
        /// Fixed unavailable resolution.
        terminal_resolution: UnavailableResolution,
    },
    /// One fully selected local canonical terminal owns this call.
    Resolved {
        /// Terminal status derived from the typed cause.
        status: CallStatus,
        /// Canonical terminal occurrence.
        terminal: ObservationId,
        /// Runtime terminal classification.
        cause: tau_proto::ToolTerminalCause,
        /// Fixed local resolution.
        terminal_resolution: LocalResolution,
        /// Qualified dispatch-to-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        dispatch_to_terminal_us: Option<u64>,
        /// Qualified background-to-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        backgrounded_to_terminal_us: Option<u64>,
        /// Source-owned output representation.
        #[serde(flatten)]
        output: CallOutputRecord,
    },
}

/// Fixed `incomplete` status used by incomplete lifecycle variants.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum IncompleteCallStatus {
    /// The call lacks a complete selected local terminal relationship.
    Incomplete,
}

/// Fixed `resolved` marker used by the local terminal variant.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum LocalResolution {
    /// Every terminal endpoint is selected in the declaration journal.
    Resolved,
}

/// Fixed unavailable marker used by unresolved terminal variants.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnavailableResolution {
    /// At least one endpoint is outside the relationship-local selected facts.
    SourceNotSelected,
}

/// Source-owned output attached only to a resolved local terminal.
#[derive(Clone, Serialize)]
#[serde(untagged)]
enum CallOutputRecord {
    /// This record owns no projected output payload.
    None {},
    /// Lite mode retains bounded text and complete counts.
    Lite {
        /// Complete rendered byte count.
        output_bytes: usize,
        /// Complete rendered line count.
        output_lines: usize,
        /// Bounded rendered output.
        output: String,
        /// Whether the bounded output is complete.
        output_complete: bool,
    },
    /// Full mode retains complete rendered output.
    Full {
        /// Complete rendered output.
        output: String,
        /// Fixed completeness marker.
        output_complete: CompleteOutput,
    },
}

/// Serializer for the fixed Boolean `true` output-completeness marker.
#[derive(Clone, Copy)]
struct CompleteOutput;

impl Serialize for CompleteOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(true)
    }
}

/// Mutable internal accumulator converted into a valid semantic DTO.
struct CallProjection {
    /// Fixed record discriminator.
    record_type: &'static str,
    /// Selected declaration journal.
    agent_id: AgentId,
    /// Exact provider declaration occurrence.
    call: ToolCallRef,
    /// Provider display/routing ID.
    call_id: String,
    /// Provider-declared tool.
    tool: tau_proto::ToolName,
    /// Lossless JSON-compatible or tagged-CBOR arguments.
    arguments: serde_json::Value,
    /// Provisional terminal status.
    status: CallStatus,
    /// Extracted shell command, when applicable.
    command: Option<String>,
    /// Qualified declaration-to-dispatch interval.
    declaration_to_dispatch_us: Option<u64>,
    /// Qualified dispatch-to-background interval.
    dispatch_to_backgrounded_us: Option<u64>,
    /// Canonical terminal observation.
    terminal: Option<ObservationId>,
    /// Producer-classified terminal cause.
    cause: Option<tau_proto::ToolTerminalCause>,
    /// Journal-local terminal availability.
    terminal_resolution: Option<Resolution>,
    /// Qualified dispatch-to-terminal interval.
    dispatch_to_terminal_us: Option<u64>,
    /// Qualified background-to-terminal interval.
    backgrounded_to_terminal_us: Option<u64>,
    /// Complete output byte count in lite mode.
    output_bytes: Option<usize>,
    /// Complete output line count in lite mode.
    output_lines: Option<usize>,
    /// Rendered output, bounded only in lite mode.
    output: Option<String>,
    /// Whether rendered output is complete.
    output_complete: Option<bool>,
}

impl CallProjection {
    fn into_record(self) -> CallRecord {
        let lifecycle = match (self.terminal, self.cause, self.terminal_resolution) {
            (None, None, None) => CallLifecycleRecord::Incomplete {
                status: IncompleteCallStatus::Incomplete,
            },
            (Some(terminal), Some(cause), Some(Resolution::SourceNotSelected)) => {
                CallLifecycleRecord::Unresolved {
                    status: IncompleteCallStatus::Incomplete,
                    terminal,
                    cause,
                    terminal_resolution: UnavailableResolution::SourceNotSelected,
                }
            }
            (Some(terminal), Some(cause), Some(Resolution::Resolved)) => {
                let output = match (
                    self.output_bytes,
                    self.output_lines,
                    self.output,
                    self.output_complete,
                ) {
                    (None, None, None, None) => CallOutputRecord::None {},
                    (
                        Some(output_bytes),
                        Some(output_lines),
                        Some(output),
                        Some(output_complete),
                    ) => CallOutputRecord::Lite {
                        output_bytes,
                        output_lines,
                        output,
                        output_complete,
                    },
                    (None, None, Some(output), Some(true)) => CallOutputRecord::Full {
                        output,
                        output_complete: CompleteOutput,
                    },
                    _ => unreachable!("call output accumulator violated semantic invariants"),
                };
                CallLifecycleRecord::Resolved {
                    status: self.status,
                    terminal,
                    cause,
                    terminal_resolution: LocalResolution::Resolved,
                    dispatch_to_terminal_us: self.dispatch_to_terminal_us,
                    backgrounded_to_terminal_us: self.backgrounded_to_terminal_us,
                    output,
                }
            }
            _ => unreachable!("call lifecycle accumulator violated semantic invariants"),
        };
        CallRecord {
            record_type: self.record_type,
            agent_id: self.agent_id,
            call: self.call,
            call_id: self.call_id,
            tool: self.tool,
            arguments: self.arguments,
            command: self.command,
            declaration_to_dispatch_us: self.declaration_to_dispatch_us,
            dispatch_to_backgrounded_us: self.dispatch_to_backgrounded_us,
            lifecycle,
        }
    }
}

/// Typed compact activation projection.
#[derive(Clone, Serialize)]
pub(super) struct ActivationRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Journal that owns the activation.
    agent_id: AgentId,
    /// Activation observation identity.
    observation_id: ObservationId,
    /// Typed activation class.
    kind: tau_proto::ActivationKind,
    /// Coupled optional source and its resolution.
    #[serde(flatten)]
    source: ActivationSourceRecord,
}

/// Semantic activation source state.
#[derive(Clone, Serialize)]
#[serde(untagged)]
enum ActivationSourceRecord {
    /// The activation has no prior durable source.
    None {
        /// Fixed absent source observation.
        source_observation: Option<ObservationId>,
        /// Fixed absent source call.
        source_call: Option<ToolCallRef>,
        /// Fixed absent resolution.
        source_resolution: Option<Resolution>,
    },
    /// Every selected source endpoint belongs to the activation journal.
    Resolved {
        /// Durable source observation.
        source_observation: ObservationId,
        /// Optional tool call associated with the source.
        source_call: Option<ToolCallRef>,
        /// Fixed local resolution.
        source_resolution: LocalResolution,
        /// Qualified completion-to-queue interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_to_activation_queue_us: Option<u64>,
    },
    /// At least one source endpoint is unavailable to this relationship.
    Unavailable {
        /// Durable source observation reference.
        source_observation: ObservationId,
        /// Optional tool call reference.
        source_call: Option<ToolCallRef>,
        /// Fixed unavailable resolution.
        source_resolution: UnavailableResolution,
    },
}

/// Variant-specific compact relationship projection.
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub(super) enum RelationshipRecord {
    /// Pre-resolution wait observation.
    WaitObservation(WaitObservationRecord),
    /// Installed wait registration.
    WaitRegistration(WaitRegistrationRecord),
    /// Terminal wait settlement.
    WaitSettlement(WaitSettlementRecord),
    /// Accepted cancellation relationship.
    CancellationRequested(CancellationRequestedRecord),
}

/// Compact pre-resolution wait observation.
#[derive(Clone, Serialize)]
pub(super) struct WaitObservationRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Relationship discriminator.
    relationship: &'static str,
    /// Journal owner that defines relationship locality.
    agent_id: AgentId,
    /// Observation identity.
    observation_id: ObservationId,
    /// Declared wait call.
    wait_call: ToolCallRef,
    /// Parsed wait mode.
    mode: tau_proto::ToolWaitMode,
}
/// Compact installed wait registration.
#[derive(Clone, Serialize)]
pub(super) struct WaitRegistrationRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Relationship discriminator.
    relationship: &'static str,
    /// Journal owner that defines relationship locality.
    agent_id: AgentId,
    /// Registration observation identity.
    observation_id: ObservationId,
    /// Pre-resolution wait observation.
    wait_observation: ObservationId,
    /// Declared wait call.
    wait_call: ToolCallRef,
    /// Installed wait mode.
    mode: tau_proto::ToolWaitMode,
    /// Fixed active registration state.
    registration: RegistrationState,
    /// Whether a fully local settlement selected this registration.
    outcome: RegistrationOutcome,
}
/// Compact wait terminal settlement.
#[derive(Clone, Serialize)]
pub(super) struct WaitSettlementRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Relationship discriminator.
    relationship: &'static str,
    /// Journal owner that defines relationship locality.
    agent_id: AgentId,
    /// Settlement observation identity.
    observation_id: ObservationId,
    /// Pre-resolution wait observation.
    wait_observation: ObservationId,
    /// Declared wait call.
    wait_call: ToolCallRef,
    /// Registration state.
    registration: RegistrationState,
    /// Installed registration identity, when present.
    registration_ref: Option<ObservationId>,
    /// Canonical wait terminal reference.
    wait_terminal: ObservationId,
    /// Resolution of the wait terminal relationship.
    wait_terminal_resolution: Resolution,
    /// Typed settlement outcome.
    #[serde(flatten)]
    outcome: WaitOutcomeRecord,
    /// Qualified registration-to-settlement interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    active_wait_us: Option<u64>,
}

/// Variant-specific settlement outcome.
#[derive(Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum WaitOutcomeRecord {
    /// Source-owned output was delivered through the wait.
    CompletionDelivered {
        /// Source call.
        source_call: ToolCallRef,
        /// Source terminal phase.
        source_phase: tau_proto::ToolSourcePhase,
        /// Source-owned canonical output occurrence.
        output_ref: ObservationId,
        /// Envelope applied by the wait.
        envelope: tau_proto::ToolOutputEnvelope,
        /// Resolution of all source endpoints.
        source_resolution: Resolution,
        /// Qualified source-completion-to-delivery interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        completion_to_delivery_us: Option<u64>,
    },
    /// Activating input interrupted a general or exact wait.
    InterruptedByActivation {
        /// Activation observation.
        activation_ref: ObservationId,
        /// Activation relationship resolution.
        source_resolution: Resolution,
        /// Qualified activation-to-wait-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        activation_to_wait_terminal_us: Option<u64>,
    },
    /// Activating input satisfied an input wait.
    InputAvailable {
        /// Activation observation.
        activation_ref: ObservationId,
        /// Activation relationship resolution.
        source_resolution: Resolution,
        /// Qualified activation-to-wait-terminal interval.
        #[serde(skip_serializing_if = "Option::is_none")]
        activation_to_wait_terminal_us: Option<u64>,
    },
    /// Input wait reached its deadline.
    TimedOut,
    /// Runtime rejected the wait.
    Rejected {
        /// Typed rejection reason.
        reason: tau_proto::WaitRejectionReason,
    },
    /// Wait call was cancelled.
    Cancelled,
    /// Owning lifecycle ended before settlement.
    LifecycleAborted,
}

/// Compact accepted cancellation relationship.
#[derive(Clone, Serialize)]
pub(super) struct CancellationRequestedRecord {
    /// Record discriminator.
    record_type: &'static str,
    /// Relationship discriminator.
    relationship: &'static str,
    /// Journal owner that defines relationship locality.
    agent_id: AgentId,
    /// Cancellation observation identity.
    observation_id: ObservationId,
    /// Declared cancel call.
    cancel_call: ToolCallRef,
    /// Declared cancellation target.
    target_call: ToolCallRef,
}

/// Writes compact JSON Lines.
pub(super) fn write_jsonl(
    root: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: super::AgentTraceMode,
    out: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    serde_json::to_writer(&mut *out, &Header::new(root, snapshot, mode)).map_err(json_error)?;
    writeln!(out)?;
    for record in collect(snapshot, mode)? {
        serde_json::to_writer(&mut *out, &record).map_err(json_error)?;
        writeln!(out)?;
    }
    Ok(())
}

/// Writes the semantically identical compact TOON document.
pub(super) fn write_toon(
    root: &AgentId,
    snapshot: &AgentJournalSnapshot,
    mode: super::AgentTraceMode,
    out: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    toon::write(
        &Header::new(root, snapshot, mode),
        collect(snapshot, mode)?,
        out,
    )
}

fn collect(
    snapshot: &AgentJournalSnapshot,
    mode: super::AgentTraceMode,
) -> Result<Vec<Record>, InspectError> {
    let mut facts = Vec::new();
    let mut ids = HashSet::new();
    for agent_id in snapshot.agent_ids() {
        for record in snapshot.records(agent_id)? {
            let record = record?;
            if !ids.insert(record.observation_id) {
                return projection_error(format!(
                    "duplicate observation ID `{}`",
                    record.observation_id
                ));
            }
            facts.push(Fact {
                agent_id: agent_id.clone(),
                id: record.observation_id,
                at: record.recorded_at,
                seq: record.seq,
                event: record.event,
            });
        }
    }
    facts.sort_by(|a, b| (&a.agent_id, a.seq.get()).cmp(&(&b.agent_id, b.seq.get())));
    project_facts(facts, mode)
}

fn project_facts(
    facts: Vec<Fact>,
    mode: super::AgentTraceMode,
) -> Result<Vec<Record>, InspectError> {
    let by_id: HashMap<_, _> = facts.iter().map(|f| (f.id, f)).collect();
    let mut calls = HashMap::<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>::new();
    let mut call_order = Vec::new();
    for fact in &facts {
        if let Event::ProviderResponseFinished(response) = &fact.event {
            for (index, item) in response.output_items.iter().enumerate() {
                if let ContextItem::ToolCall(call) = item {
                    let item_index = u32::try_from(index).map_err(|_| {
                        InspectError::Trace(crate::AgentTraceError::Projection(
                            "tool item index exceeds u32".into(),
                        ))
                    })?;
                    let call_ref = ToolCallRef {
                        declaration: fact.id,
                        item_index,
                    };
                    calls.insert(call_ref, (fact, index, call));
                    call_order.push(call_ref);
                }
            }
        }
    }
    validate_observation_integrity(&facts, &by_id, &calls)?;
    let dispatch: HashMap<_, _> = facts
        .iter()
        .filter_map(|f| {
            if let Event::AgentToolDispatchObserved(e) = &f.event {
                calls
                    .get(&e.call)
                    .filter(|(declaration, _, _)| declaration.agent_id == f.agent_id)
                    .map(|_| (e.call, f))
            } else {
                None
            }
        })
        .collect();
    let backgrounded: HashMap<_, _> = facts
        .iter()
        .filter_map(|f| {
            if let Event::AgentToolBackgroundedObserved(e) = &f.event {
                calls
                    .get(&e.call)
                    .filter(|(declaration, _, _)| declaration.agent_id == f.agent_id)
                    .map(|_| (e.call, f))
            } else {
                None
            }
        })
        .collect();
    let mut terminals = HashMap::new();
    for fact in &facts {
        let Event::AgentToolTerminalClassified(classification) = &fact.event else {
            continue;
        };
        if !call_is_selected_local(fact, classification.call, &calls) {
            continue;
        }
        let replacement_committed =
            classification_terminal_committed(fact, classification, &by_id, &calls);
        let current_committed = terminals.get(&classification.call).is_some_and(
            |(current_fact, current): &(&Fact, &tau_proto::AgentToolTerminalClassified)| {
                classification_terminal_committed(current_fact, current, &by_id, &calls)
            },
        );
        if replacement_committed || !current_committed {
            terminals.insert(classification.call, (fact, classification));
        }
    }
    let mut records = Vec::new();
    let wait_settlements = facts
        .iter()
        .filter_map(|fact| match &fact.event {
            Event::AgentToolWaitSettled(e)
                if settlement_is_fully_local(fact, e, &by_id, &calls) =>
            {
                Some((e.wait_call, e))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut wait_calls = facts
        .iter()
        .filter_map(|fact| match &fact.event {
            Event::AgentToolWaitObserved(e)
                if call_is_selected_local(fact, e.wait_call, &calls)
                    && wait_mode_is_fully_local(fact, &e.mode, &calls) =>
            {
                Some(e.wait_call)
            }
            Event::AgentToolWaitRegistered(e)
                if wait_registration_event_is_fully_local(fact, e, &by_id, &calls) =>
            {
                Some(e.wait_call)
            }
            Event::AgentToolWaitSettled(e)
                if settlement_is_fully_local(fact, e, &by_id, &calls) =>
            {
                Some(e.wait_call)
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    wait_calls.extend(facts.iter().filter_map(|fact| match &fact.event {
        Event::AgentToolWaitSettled(settled)
            if call_is_selected_local(fact, settled.wait_call, &calls)
                && matches!(
                    settled.outcome,
                    tau_proto::ToolWaitOutcome::CompletionDelivered { .. }
                ) =>
        {
            Some(settled.wait_call)
        }
        _ => None,
    }));
    for call_ref in call_order {
        let Some((declaration, _, call)) = calls.get(&call_ref) else {
            return projection_error("declared call disappeared".to_owned());
        };
        let terminal = terminals.get(&call_ref);
        let selected_terminal = terminal.and_then(|(_, e)| by_id.get(&e.terminal));
        let classification_fully_local = terminal.is_some_and(|(fact, classification)| {
            classification_is_fully_local(fact, classification, &by_id, &calls)
        });
        let resolved_terminal = selected_terminal
            .filter(|owner| classification_fully_local && owner.agent_id == declaration.agent_id);
        let status = terminal
            .filter(|_| resolved_terminal.is_some())
            .map_or(CallStatus::Incomplete, |(_, e)| terminal_status(&e.cause));
        let status = if wait_calls.contains(&call_ref) && !wait_settlements.contains_key(&call_ref)
        {
            CallStatus::Incomplete
        } else {
            status
        };
        let mut value = CallProjection {
            record_type: "call",
            agent_id: declaration.agent_id.clone(),
            call: call_ref,
            call_id: call.call_id.to_string(),
            tool: call.name.clone(),
            arguments: arguments(&call.arguments),
            status,
            command: shell_command(&call.name, &call.arguments),
            declaration_to_dispatch_us: None,
            dispatch_to_backgrounded_us: None,
            terminal: None,
            cause: None,
            terminal_resolution: None,
            dispatch_to_terminal_us: None,
            backgrounded_to_terminal_us: None,
            output_bytes: None,
            output_lines: None,
            output: None,
            output_complete: None,
        };
        if let Some(dispatch) = dispatch.get(&call_ref) {
            interval(&mut value.declaration_to_dispatch_us, declaration, dispatch);
            if let Some(backgrounded) = backgrounded.get(&call_ref) {
                interval(
                    &mut value.dispatch_to_backgrounded_us,
                    dispatch,
                    backgrounded,
                );
            }
        }
        if let Some((_classification, terminal_event)) = terminal {
            value.terminal = Some(terminal_event.terminal);
            value.cause = Some(terminal_event.cause.clone());
            match selected_terminal {
                Some(owner) => {
                    value.terminal_resolution = Some(
                        if classification_fully_local && owner.agent_id == declaration.agent_id {
                            Resolution::Resolved
                        } else {
                            // Foreign or missing endpoints can occur in selected
                            // subsets and incomplete or historical journals.
                            Resolution::SourceNotSelected
                        },
                    );
                    if let Some(dispatch) = dispatch.get(&call_ref)
                        && classification_fully_local
                        && owner.agent_id == declaration.agent_id
                    {
                        interval(&mut value.dispatch_to_terminal_us, dispatch, owner);
                    }
                    if let Some(backgrounded) = backgrounded.get(&call_ref)
                        && classification_fully_local
                        && owner.agent_id == declaration.agent_id
                    {
                        interval(&mut value.backgrounded_to_terminal_us, backgrounded, owner);
                    }
                    let wait_owns_output =
                        wait_settlements.get(&call_ref).is_some_and(|settlement| {
                            !matches!(
                                settlement.outcome,
                                tau_proto::ToolWaitOutcome::CompletionDelivered { .. }
                            )
                        });
                    if classification_fully_local
                        && owner.agent_id == declaration.agent_id
                        && (!wait_calls.contains(&call_ref) || wait_owns_output)
                    {
                        add_owned_output(&mut value, &owner.event, mode);
                    }
                }
                None => value.terminal_resolution = Some(Resolution::SourceNotSelected),
            }
        }
        records.push(Record::Call(value.into_record()));
    }
    let settled_registrations = facts
        .iter()
        .filter_map(|fact| match &fact.event {
            Event::AgentToolWaitSettled(settled)
                if settlement_is_fully_local(fact, settled, &by_id, &calls) =>
            {
                settled
                    .registration
                    .map(|registration| (fact.agent_id.clone(), registration))
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    for fact in &facts {
        match &fact.event {
            Event::AgentActivationQueued(e) => {
                let source_is_fully_local = e.source_observation.is_some_and(|id| {
                    observation_is_selected_local(fact, id, &by_id)
                        && e.source_call
                            .is_none_or(|call| call_is_selected_local(fact, call, &calls))
                });
                let mut completion_to_activation_queue_us = None;
                if let (Some(source_id), Some(source_call)) = (e.source_observation, e.source_call)
                    && source_is_fully_local
                    && let Some(source) = by_id
                        .get(&source_id)
                        .filter(|source| source.agent_id == fact.agent_id)
                    && let Some((_, _, declared)) = calls.get(&source_call)
                    && canonical_terminal_call_id(&source.event) == Some(&declared.call_id)
                {
                    interval(&mut completion_to_activation_queue_us, source, fact);
                }
                let source = match (e.source_observation, source_is_fully_local) {
                    (None, _) => ActivationSourceRecord::None {
                        source_observation: None,
                        source_call: None,
                        source_resolution: None,
                    },
                    (Some(source_observation), true) => ActivationSourceRecord::Resolved {
                        source_observation,
                        source_call: e.source_call,
                        source_resolution: LocalResolution::Resolved,
                        completion_to_activation_queue_us,
                    },
                    (Some(source_observation), false) => ActivationSourceRecord::Unavailable {
                        source_observation,
                        source_call: e.source_call,
                        source_resolution: UnavailableResolution::SourceNotSelected,
                    },
                };
                let record = ActivationRecord {
                    record_type: "activation",
                    agent_id: fact.agent_id.clone(),
                    observation_id: fact.id,
                    kind: e.kind,
                    source,
                };
                records.push(Record::Activation(record));
            }
            Event::AgentToolWaitObserved(e) => {
                records.push(Record::Relationship(RelationshipRecord::WaitObservation(
                    WaitObservationRecord {
                        record_type: "relationship",
                        relationship: "wait_observation",
                        agent_id: fact.agent_id.clone(),
                        observation_id: fact.id,
                        wait_call: e.wait_call,
                        mode: projected_wait_mode(fact, e.wait_call, &e.mode, &calls),
                    },
                )));
            }
            Event::AgentToolWaitRegistered(e) => {
                let is_fully_local =
                    wait_registration_event_is_fully_local(fact, e, &by_id, &calls);
                let outcome = if is_fully_local
                    && settled_registrations.contains(&(fact.agent_id.clone(), fact.id))
                {
                    RegistrationOutcome::Settled
                } else {
                    RegistrationOutcome::Incomplete
                };
                records.push(Record::Relationship(RelationshipRecord::WaitRegistration(
                    WaitRegistrationRecord {
                        record_type: "relationship",
                        relationship: "wait_registration",
                        agent_id: fact.agent_id.clone(),
                        observation_id: fact.id,
                        wait_observation: e.wait_observation,
                        wait_call: e.wait_call,
                        mode: projected_wait_mode(fact, e.wait_call, &e.mode, &calls),
                        registration: RegistrationState::Active,
                        outcome,
                    },
                )));
            }
            Event::AgentToolWaitSettled(e) => {
                let is_fully_local = settlement_is_fully_local(fact, e, &by_id, &calls);
                let registration = match (is_fully_local, e.registration) {
                    (false, Some(_)) => RegistrationState::Unresolved,
                    (false, None) => RegistrationState::Immediate,
                    (true, None) => RegistrationState::Immediate,
                    (true, Some(id))
                        if by_id
                            .get(&id)
                            .is_some_and(|registration| registration.agent_id == fact.agent_id) =>
                    {
                        RegistrationState::Active
                    }
                    (true, Some(_)) => RegistrationState::Unresolved,
                };
                let terminal_resolution = match (is_fully_local, by_id.get(&e.wait_terminal)) {
                    (false, _) => Resolution::SourceNotSelected,
                    (true, Some(terminal)) if terminal.agent_id == fact.agent_id => {
                        Resolution::Resolved
                    }
                    (true, Some(_) | None) => Resolution::SourceNotSelected,
                };
                let (outcome, active_wait_us) = wait_relationship(e, &by_id, fact, !is_fully_local);
                records.push(Record::Relationship(RelationshipRecord::WaitSettlement(
                    WaitSettlementRecord {
                        record_type: "relationship",
                        relationship: "wait_settlement",
                        agent_id: fact.agent_id.clone(),
                        observation_id: fact.id,
                        wait_observation: e.wait_observation,
                        wait_call: e.wait_call,
                        registration,
                        registration_ref: e.registration,
                        wait_terminal: e.wait_terminal,
                        wait_terminal_resolution: terminal_resolution,
                        outcome,
                        active_wait_us,
                    },
                )));
            }
            Event::AgentToolCancellationRequested(e) => {
                records.push(Record::Relationship(
                    RelationshipRecord::CancellationRequested(CancellationRequestedRecord {
                        record_type: "relationship",
                        relationship: "cancellation_requested",
                        agent_id: fact.agent_id.clone(),
                        observation_id: fact.id,
                        cancel_call: e.cancel_call,
                        target_call: e.target_call,
                    }),
                ));
            }
            _ => {}
        }
    }
    Ok(records)
}

/// Reject ambiguous or contradictory explicit observations before projection.
fn validate_observation_integrity(
    facts: &[Fact],
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> Result<(), InspectError> {
    let mut dispatches = HashSet::new();
    let mut backgrounded = HashSet::new();
    let mut wait_observations = HashSet::new();
    let mut registrations = HashMap::new();
    let mut settlements = HashSet::new();
    let mut committed_classifications = HashSet::new();
    for fact in facts {
        match &fact.event {
            Event::AgentActivationQueued(e)
                if let (Some(source_id), Some(source_call)) =
                    (e.source_observation, e.source_call)
                    && observation_is_selected_local(fact, source_id, by_id)
                    && call_is_selected_local(fact, source_call, calls) =>
            {
                let source = by_id
                    .get(&source_id)
                    .expect("selected-local activation source exists");
                let declared = calls
                    .get(&source_call)
                    .expect("selected-local activation call exists")
                    .2;
                if canonical_terminal_call_id(&source.event) != Some(&declared.call_id) {
                    return projection_error(format!(
                        "activation source `{source_id}` does not own call {source_call:?}"
                    ));
                }
            }
            Event::AgentToolDispatchObserved(e)
                if call_is_selected_local(fact, e.call, calls) && !dispatches.insert(e.call) =>
            {
                return projection_error(format!(
                    "duplicate dispatch observation for {:?}",
                    e.call
                ));
            }
            Event::AgentToolBackgroundedObserved(e)
                if call_is_selected_local(fact, e.call, calls) && !backgrounded.insert(e.call) =>
            {
                return projection_error(format!(
                    "duplicate background observation for {:?}",
                    e.call
                ));
            }
            Event::AgentToolWaitObserved(e)
                if call_is_selected_local(fact, e.wait_call, calls)
                    && wait_mode_is_fully_local(fact, &e.mode, calls)
                    && !wait_observations.insert(e.wait_call) =>
            {
                return projection_error(format!(
                    "duplicate wait observation for {:?}",
                    e.wait_call
                ));
            }
            Event::AgentToolWaitRegistered(e) => {
                if !wait_registration_event_is_fully_local(fact, e, by_id, calls) {
                    continue;
                }
                validate_wait_observation_ref(
                    fact,
                    e.wait_observation,
                    e.wait_call,
                    Some(&e.mode),
                    by_id,
                    calls,
                )?;
                if registrations.insert(fact.id, e.wait_call).is_some() {
                    return projection_error(format!("duplicate wait registration `{}`", fact.id));
                }
            }
            Event::AgentToolWaitSettled(e) => {
                if !settlement_endpoints_are_local(fact, e, by_id, calls) {
                    continue;
                }
                validate_wait_observation_ref(
                    fact,
                    e.wait_observation,
                    e.wait_call,
                    None,
                    by_id,
                    calls,
                )?;
                validate_selected_settlement_consistency(fact, e, by_id, calls)?;
                if !settlements.insert(e.wait_call) {
                    return projection_error(format!(
                        "duplicate wait settlement for {:?}",
                        e.wait_call
                    ));
                }
                if let Some(registration) = e.registration {
                    match by_id
                        .get(&registration)
                        .filter(|registration| registration.agent_id == fact.agent_id)
                        .map(|fact| &fact.event)
                    {
                        Some(Event::AgentToolWaitRegistered(registered))
                            if registered.wait_call == e.wait_call => {}
                        Some(_) => {
                            return projection_error(format!(
                                "wait settlement references contradictory registration `{registration}`"
                            ));
                        }
                        None => {}
                    }
                }
                if let Some(terminal) = by_id
                    .get(&e.wait_terminal)
                    .filter(|terminal| terminal.agent_id == fact.agent_id)
                {
                    let Some((_, _, wait_call)) = calls.get(&e.wait_call) else {
                        return projection_error(format!(
                            "wait terminal `{}` resolves without a selected wait declaration",
                            e.wait_terminal
                        ));
                    };
                    if canonical_terminal_call_id(&terminal.event) != Some(&wait_call.call_id) {
                        return projection_error(format!(
                            "wait terminal `{}` does not own wait call {:?}",
                            e.wait_terminal, e.wait_call
                        ));
                    }
                }
                validate_wait_outcome(fact, e, by_id, calls)?;
            }
            Event::AgentToolTerminalClassified(e) => {
                if !classification_is_fully_local(fact, e, by_id, calls) {
                    continue;
                }
                if classification_terminal_committed(fact, e, by_id, calls)
                    && !committed_classifications.insert(e.call)
                {
                    return projection_error(format!(
                        "multiple committed terminal classifications for {:?}",
                        e.call
                    ));
                }
                if let Some(terminal) = by_id
                    .get(&e.terminal)
                    .filter(|terminal| terminal.agent_id == fact.agent_id)
                {
                    let Some((_, _, declared)) = calls.get(&e.call) else {
                        return projection_error(format!(
                            "terminal `{}` resolves without a selected declaration",
                            e.terminal
                        ));
                    };
                    if canonical_terminal_call_id(&terminal.event) != Some(&declared.call_id) {
                        return projection_error(format!(
                            "terminal `{}` does not own call {:?}",
                            e.terminal, e.call
                        ));
                    }
                }
                if let tau_proto::ToolTerminalCause::Cancellation { request } = e.cause
                    && let Some(request_fact) = by_id
                        .get(&request)
                        .filter(|request| request.agent_id == fact.agent_id)
                    && !matches!(
                        &request_fact.event,
                        Event::AgentToolCancellationRequested(requested)
                            if requested.target_call == e.call
                    )
                {
                    return projection_error(format!(
                        "terminal cancellation request `{request}` does not target call {:?}",
                        e.call
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn call_is_selected_local(
    owner: &Fact,
    call: ToolCallRef,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    calls
        .get(&call)
        .is_some_and(|(declaration, _, _)| declaration.agent_id == owner.agent_id)
}

fn observation_is_selected_local(
    owner: &Fact,
    observation: ObservationId,
    by_id: &HashMap<ObservationId, &Fact>,
) -> bool {
    by_id
        .get(&observation)
        .is_some_and(|endpoint| endpoint.agent_id == owner.agent_id)
}

fn wait_mode_is_fully_local(
    fact: &Fact,
    mode: &tau_proto::ToolWaitMode,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    match mode {
        tau_proto::ToolWaitMode::Exact { target } => call_is_selected_local(fact, *target, calls),
        _ => true,
    }
}

fn wait_observation_is_fully_local(
    fact: &Fact,
    observation: ObservationId,
    _wait_call: ToolCallRef,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    by_id.get(&observation).is_some_and(|observed| {
        if observed.agent_id != fact.agent_id {
            return false;
        }
        match &observed.event {
            Event::AgentToolWaitObserved(value) => {
                call_is_selected_local(fact, value.wait_call, calls)
                    && wait_mode_is_fully_local(fact, &value.mode, calls)
            }
            _ => true,
        }
    })
}

fn wait_registration_is_fully_local(
    fact: &Fact,
    registration: ObservationId,
    _wait_observation: ObservationId,
    _wait_call: ToolCallRef,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    by_id.get(&registration).is_some_and(|registered| {
        if registered.agent_id != fact.agent_id {
            return false;
        }
        match &registered.event {
            Event::AgentToolWaitRegistered(value) => {
                call_is_selected_local(fact, value.wait_call, calls)
                    && wait_mode_is_fully_local(fact, &value.mode, calls)
                    && wait_observation_is_fully_local(
                        fact,
                        value.wait_observation,
                        value.wait_call,
                        by_id,
                        calls,
                    )
            }
            _ => true,
        }
    })
}

fn wait_registration_event_is_fully_local(
    fact: &Fact,
    registration: &tau_proto::AgentToolWaitRegistered,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    call_is_selected_local(fact, registration.wait_call, calls)
        && wait_mode_is_fully_local(fact, &registration.mode, calls)
        && wait_observation_is_fully_local(
            fact,
            registration.wait_observation,
            registration.wait_call,
            by_id,
            calls,
        )
}

fn settlement_is_fully_local(
    fact: &Fact,
    settlement: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    settlement_endpoints_are_local(fact, settlement, by_id, calls)
}

/// Return whether every endpoint needed to validate or project a settlement is
/// present in the selected journal. This deliberately does not test semantic
/// agreement: callers validate contradictions only after this predicate holds.
fn settlement_endpoints_are_local(
    fact: &Fact,
    settlement: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    call_is_selected_local(fact, settlement.wait_call, calls)
        && wait_observation_is_fully_local(
            fact,
            settlement.wait_observation,
            settlement.wait_call,
            by_id,
            calls,
        )
        && settlement.registration.is_none_or(|id| {
            wait_registration_is_fully_local(
                fact,
                id,
                settlement.wait_observation,
                settlement.wait_call,
                by_id,
                calls,
            )
        })
        && observation_is_selected_local(fact, settlement.wait_terminal, by_id)
        && match &settlement.outcome {
            tau_proto::ToolWaitOutcome::CompletionDelivered {
                source_call,
                source_terminal,
                ..
            } => {
                call_is_selected_local(fact, *source_call, calls)
                    && observation_is_selected_local(fact, *source_terminal, by_id)
            }
            tau_proto::ToolWaitOutcome::InterruptedByActivation { activation }
            | tau_proto::ToolWaitOutcome::InputAvailable { activation } => {
                observation_is_selected_local(fact, *activation, by_id)
            }
            _ => true,
        }
}

fn classification_is_fully_local(
    fact: &Fact,
    classification: &tau_proto::AgentToolTerminalClassified,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    call_is_selected_local(fact, classification.call, calls)
        && observation_is_selected_local(fact, classification.terminal, by_id)
        && match classification.cause {
            tau_proto::ToolTerminalCause::Cancellation { request } => {
                observation_is_selected_local(fact, request, by_id)
            }
            _ => true,
        }
}

fn projected_wait_mode(
    fact: &Fact,
    wait_call: ToolCallRef,
    mode: &tau_proto::ToolWaitMode,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> tau_proto::ToolWaitMode {
    match mode {
        tau_proto::ToolWaitMode::Exact { target }
            if !call_is_selected_local(fact, wait_call, calls)
                || !call_is_selected_local(fact, *target, calls) =>
        {
            tau_proto::ToolWaitMode::ExactUnresolved
        }
        _ => mode.clone(),
    }
}

fn validate_wait_outcome(
    settlement_fact: &Fact,
    settlement: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> Result<(), InspectError> {
    if !settlement_endpoints_are_local(settlement_fact, settlement, by_id, calls) {
        return Ok(());
    }
    let observed_mode = by_id
        .get(&settlement.wait_observation)
        .and_then(|fact| match &fact.event {
            Event::AgentToolWaitObserved(observed) => Some(&observed.mode),
            _ => None,
        })
        .expect("fully local settlement has a wait observation");
    if !wait_mode_allows_outcome(observed_mode, settlement) {
        return projection_error("wait mode and selected-local outcome contradict".to_owned());
    }
    match &settlement.outcome {
        tau_proto::ToolWaitOutcome::CompletionDelivered {
            source_call,
            source_terminal,
            source_phase,
            ..
        } => {
            if let Some(terminal) = by_id
                .get(source_terminal)
                .filter(|terminal| terminal.agent_id == settlement_fact.agent_id)
                && let Some((_, _, declared)) = calls
                    .get(source_call)
                    .filter(|(declaration, _, _)| declaration.agent_id == settlement_fact.agent_id)
                && canonical_terminal_call_id(&terminal.event) != Some(&declared.call_id)
            {
                return projection_error(format!(
                    "wait source terminal `{source_terminal}` does not own call {source_call:?}"
                ));
            }
            if let Some(terminal) = by_id.get(source_terminal) {
                let phase_matches = match source_phase {
                    tau_proto::ToolSourcePhase::Foreground => matches!(
                        terminal.event,
                        Event::ProviderToolResult(_)
                            | Event::ProviderToolError(_)
                            | Event::ToolCancelled(_)
                    ),
                    tau_proto::ToolSourcePhase::Background => matches!(
                        terminal.event,
                        Event::ToolBackgroundResult(_)
                            | Event::ToolBackgroundError(_)
                            | Event::ToolCancelled(_)
                    ),
                };
                if !phase_matches {
                    return projection_error(
                        "wait source phase contradicts canonical terminal family".to_owned(),
                    );
                }
            }
        }
        tau_proto::ToolWaitOutcome::InterruptedByActivation { activation }
        | tau_proto::ToolWaitOutcome::InputAvailable { activation } => {
            if let Some(source) = by_id
                .get(activation)
                .filter(|source| source.agent_id == settlement_fact.agent_id)
                && !matches!(source.event, Event::AgentActivationQueued(_))
            {
                return projection_error(format!(
                    "wait activation reference `{activation}` is not an activation"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn wait_mode_allows_outcome(
    mode: &tau_proto::ToolWaitMode,
    settlement: &tau_proto::AgentToolWaitSettled,
) -> bool {
    use tau_proto::{
        ToolWaitMode as Mode, ToolWaitOutcome as Outcome, WaitRejectionReason as Reject,
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
        ) => target == source_call && *envelope == tau_proto::ToolOutputEnvelope::Identity,
        (
            Mode::NextBackground,
            Outcome::CompletionDelivered {
                source_phase,
                envelope,
                ..
            },
        ) => {
            *source_phase == tau_proto::ToolSourcePhase::Background
                && *envelope == tau_proto::ToolOutputEnvelope::OriginalToolCallIdHeader
        }
        (Mode::Exact { .. } | Mode::NextBackground, Outcome::InterruptedByActivation { .. })
        | (Mode::ActivatingInput { .. }, Outcome::InputAvailable { .. }) => true,
        (Mode::ActivatingInput { .. }, Outcome::TimedOut)
        | (
            Mode::Exact { .. } | Mode::NextBackground | Mode::ActivatingInput { .. },
            Outcome::Cancelled | Outcome::LifecycleAborted,
        ) => registered,
        (
            Mode::Exact { .. },
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

fn validate_selected_settlement_consistency(
    fact: &Fact,
    settlement: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> Result<(), InspectError> {
    if !settlement_endpoints_are_local(fact, settlement, by_id, calls) {
        return Ok(());
    }
    let observed = by_id
        .get(&settlement.wait_observation)
        .filter(|observed| observed.agent_id == fact.agent_id)
        .and_then(|observed| match &observed.event {
            Event::AgentToolWaitObserved(observed)
                if observed.wait_call == settlement.wait_call =>
            {
                Some(observed)
            }
            _ => None,
        });
    if let Some(registration_id) = settlement.registration
        && let Some(registration_fact) = by_id
            .get(&registration_id)
            .filter(|registration| registration.agent_id == fact.agent_id)
    {
        let Some(registration) = (match &registration_fact.event {
            Event::AgentToolWaitRegistered(registration) => Some(registration),
            _ => None,
        }) else {
            return projection_error(format!(
                "wait settlement references contradictory registration `{registration_id}`"
            ));
        };
        if registration.wait_call != settlement.wait_call
            || registration.wait_observation != settlement.wait_observation
            || observed.is_some_and(|observed| observed.mode != registration.mode)
        {
            return projection_error(format!(
                "wait settlement references contradictory registration `{registration_id}`"
            ));
        }
    }
    if let (
        Some(tau_proto::AgentToolWaitObserved {
            mode: tau_proto::ToolWaitMode::Exact { target },
            ..
        }),
        tau_proto::ToolWaitOutcome::CompletionDelivered { source_call, .. },
    ) = (observed, &settlement.outcome)
        && target != source_call
    {
        return projection_error(
            "exact wait settlement delivered a different selected source call".to_owned(),
        );
    }
    Ok(())
}

/// Reject a selected wait-observation reference that resolves to another call
/// or event family while allowing an omitted selected-cut endpoint.
fn validate_wait_observation_ref(
    dependent: &Fact,
    observation: ObservationId,
    wait_call: ToolCallRef,
    expected_mode: Option<&tau_proto::ToolWaitMode>,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> Result<(), InspectError> {
    if !call_is_selected_local(dependent, wait_call, calls)
        || !observation_is_selected_local(dependent, observation, by_id)
    {
        return Ok(());
    }
    let Some(observed) = by_id.get(&observation) else {
        return Ok(());
    };
    if observed.agent_id != dependent.agent_id {
        return Ok(());
    }
    let Event::AgentToolWaitObserved(value) = &observed.event else {
        return projection_error(format!(
            "wait observation `{observation}` contradicts wait call {wait_call:?}"
        ));
    };
    if !wait_mode_is_fully_local(dependent, &value.mode, calls)
        || expected_mode.is_some_and(|mode| !wait_mode_is_fully_local(dependent, mode, calls))
    {
        return Ok(());
    }
    if !matches!(
        &observed.event,
        Event::AgentToolWaitObserved(value)
            if value.wait_call == wait_call
                && expected_mode.is_none_or(|mode| mode == &value.mode)
    ) {
        return projection_error(format!(
            "wait observation `{observation}` contradicts wait call {wait_call:?}"
        ));
    }
    Ok(())
}

fn canonical_terminal_call_id(event: &Event) -> Option<&tau_proto::ToolCallId> {
    match event {
        Event::ProviderToolResult(e) if e.kind == tau_proto::ToolResultKind::Final => {
            Some(&e.call_id)
        }
        Event::ProviderToolError(e) => Some(&e.call_id),
        Event::ToolBackgroundResult(e) => Some(&e.call_id),
        Event::ToolBackgroundError(e) => Some(&e.call_id),
        Event::ToolCancelled(e) => Some(&e.call_id),
        _ => None,
    }
}

fn classification_terminal_committed(
    classification_fact: &Fact,
    classification: &tau_proto::AgentToolTerminalClassified,
    by_id: &HashMap<ObservationId, &Fact>,
    calls: &HashMap<ToolCallRef, (&Fact, usize, &tau_proto::ToolCallItem)>,
) -> bool {
    if !classification_is_fully_local(classification_fact, classification, by_id, calls) {
        return false;
    }
    let Some((_, _, declared)) = calls.get(&classification.call) else {
        return false;
    };
    by_id
        .get(&classification.terminal)
        .and_then(|terminal| canonical_terminal_call_id(&terminal.event))
        == Some(&declared.call_id)
}

fn terminal_status(cause: &tau_proto::ToolTerminalCause) -> CallStatus {
    match cause {
        tau_proto::ToolTerminalCause::Completed => CallStatus::Ok,
        tau_proto::ToolTerminalCause::Cancellation { .. } => CallStatus::Cancelled,
        tau_proto::ToolTerminalCause::ToolError => CallStatus::Error,
        _ => CallStatus::Incomplete,
    }
}
fn interval(slot: &mut Option<u64>, first: &Fact, second: &Fact) {
    *slot = second.at.get().checked_sub(first.at.get());
}
fn wait_relationship(
    e: &tau_proto::AgentToolWaitSettled,
    by_id: &HashMap<ObservationId, &Fact>,
    settled: &Fact,
    has_foreign_endpoint: bool,
) -> (WaitOutcomeRecord, Option<u64>) {
    use tau_proto::ToolWaitOutcome::*;
    let outcome = match &e.outcome {
        CompletionDelivered {
            source_call,
            source_terminal,
            source_phase,
            envelope,
        } => {
            let (source_resolution, completion_to_delivery_us) =
                match (has_foreign_endpoint, by_id.get(source_terminal)) {
                    (true, _) => (Resolution::SourceNotSelected, None),
                    (false, Some(source)) if source.agent_id == settled.agent_id => {
                        let mut interval_us = None;
                        interval(&mut interval_us, source, settled);
                        (Resolution::Resolved, interval_us)
                    }
                    (false, Some(_) | None) => (Resolution::SourceNotSelected, None),
                };
            WaitOutcomeRecord::CompletionDelivered {
                source_call: *source_call,
                source_phase: *source_phase,
                output_ref: *source_terminal,
                envelope: *envelope,
                source_resolution,
                completion_to_delivery_us,
            }
        }
        InterruptedByActivation { activation } | InputAvailable { activation } => {
            let (source_resolution, activation_to_wait_terminal_us) =
                match (has_foreign_endpoint, by_id.get(activation)) {
                    (true, _) => (Resolution::SourceNotSelected, None),
                    (false, Some(a)) if a.agent_id == settled.agent_id => {
                        let mut interval_us = None;
                        if let Some(wait_terminal) = by_id
                            .get(&e.wait_terminal)
                            .filter(|terminal| terminal.agent_id == settled.agent_id)
                        {
                            interval(&mut interval_us, a, wait_terminal)
                        }
                        (Resolution::Resolved, interval_us)
                    }
                    (false, Some(_) | None) => (Resolution::SourceNotSelected, None),
                };
            if matches!(&e.outcome, InterruptedByActivation { .. }) {
                WaitOutcomeRecord::InterruptedByActivation {
                    activation_ref: *activation,
                    source_resolution,
                    activation_to_wait_terminal_us,
                }
            } else {
                WaitOutcomeRecord::InputAvailable {
                    activation_ref: *activation,
                    source_resolution,
                    activation_to_wait_terminal_us,
                }
            }
        }
        TimedOut => WaitOutcomeRecord::TimedOut,
        Rejected { reason } => WaitOutcomeRecord::Rejected { reason: *reason },
        Cancelled => WaitOutcomeRecord::Cancelled,
        LifecycleAborted => WaitOutcomeRecord::LifecycleAborted,
    };
    let mut active_wait_us = None;
    if !has_foreign_endpoint
        && let Some(registration) = e
            .registration
            .and_then(|id| by_id.get(&id))
            .filter(|registration| registration.agent_id == settled.agent_id)
    {
        interval(&mut active_wait_us, registration, settled);
    }
    (outcome, active_wait_us)
}
fn add_owned_output(value: &mut CallProjection, event: &Event, mode: super::AgentTraceMode) {
    let rendered = match event {
        Event::ProviderToolResult(e) => {
            Some(tau_proto::ToolResponse::from_cbor(&e.result).render())
        }
        Event::ToolBackgroundResult(e) => {
            Some(tau_proto::ToolResponse::from_cbor(&e.result).render())
        }
        Event::ProviderToolError(e) => Some(render_error(&e.message, e.details.as_ref())),
        Event::ToolBackgroundError(e) => Some(render_error(&e.message, e.details.as_ref())),
        Event::ToolCancelled(_) => Some(render_cancelled()),
        _ => None,
    };
    if let Some(rendered) = rendered {
        match mode {
            super::AgentTraceMode::Lite => {
                value.output_bytes = Some(rendered.len());
                value.output_lines = Some(rendered.lines().count());
                let (s, complete) = lite_output(&rendered);
                value.output = Some(s.to_owned());
                value.output_complete = Some(complete)
            }
            super::AgentTraceMode::Full => {
                value.output = Some(rendered);
                value.output_complete = Some(true)
            }
        }
    }
}
fn lite_output(output: &str) -> (&str, bool) {
    if output.len() <= LITE_OUTPUT_BYTES {
        return (output, true);
    }
    let mut end = LITE_OUTPUT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1
    }
    (&output[..end], false)
}
fn arguments(value: &CborValue) -> serde_json::Value {
    faithful_json(value).unwrap_or_else(|| crate::lossless_json::typed_cbor(value))
}
fn faithful_json(value: &CborValue) -> Option<serde_json::Value> {
    match value {
        CborValue::Null => Some(serde_json::Value::Null),
        CborValue::Bool(v) => Some((*v).into()),
        CborValue::Integer(v) => {
            let v: i128 = (*v).into();
            i64::try_from(v)
                .ok()
                .map(Into::into)
                .or_else(|| u64::try_from(v).ok().map(Into::into))
        }
        CborValue::Text(v) => Some(v.clone().into()),
        CborValue::Array(v) => v
            .iter()
            .map(faithful_json)
            .collect::<Option<Vec<_>>>()
            .map(Into::into),
        CborValue::Map(entries) => {
            let mut o = serde_json::Map::new();
            for (k, v) in entries {
                let CborValue::Text(k) = k else { return None };
                if o.contains_key(k) {
                    return None;
                }
                o.insert(k.clone(), faithful_json(v)?);
            }
            Some(o.into())
        }
        _ => None,
    }
}
fn shell_command(tool: &tau_proto::ToolName, args: &CborValue) -> Option<String> {
    if !matches!(tool.as_str(), "shell" | "shell_command" | "gpt_shell") {
        return None;
    }
    let CborValue::Text(v) = tau_proto::cbor_field(args, "command")? else {
        return None;
    };
    Some(v.clone())
}
fn render_error(message: &str, details: Option<&CborValue>) -> String {
    let mut r = tau_proto::ToolResponse::from_cbor(details.unwrap_or(&CborValue::Null));
    r.headers.insert(
        0,
        tau_proto::ToolResponseHeader {
            key: "error".into(),
            value: message.into(),
        },
    );
    r.render()
}
fn render_cancelled() -> String {
    tau_proto::ToolResponse {
        raw: CborValue::Null,
        headers: vec![tau_proto::ToolResponseHeader {
            key: "cancelled".into(),
            value: "cancelled".into(),
        }],
        body: String::new(),
    }
    .render()
}
fn projection_error<T>(message: String) -> Result<T, InspectError> {
    Err(InspectError::Trace(crate::AgentTraceError::Projection(
        message,
    )))
}
fn json_error(error: serde_json::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize compact agent tool trace: {error}"
    )))
}
