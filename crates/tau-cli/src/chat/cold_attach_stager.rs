//! Bounded presentation-only staging for explicit cold attachment.

use std::collections::{HashMap, HashSet};

use tau_proto::{Event, UnixMicros};

use super::{RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS};

#[cfg(test)]
mod tests;

/// One decoded event plus presentation actions derived from the replay
/// boundary. The renderer applies abandonment before the event and interprets
/// the event according to `presentation`.
pub(super) struct RendererDelivery {
    /// Typed payload interpreted by the event renderer.
    pub(super) event: Event,
    /// Whether the harness marked this delivery as replay/catch-up.
    replay: bool,
    /// Harness-provided observation time.
    pub(super) recorded_at: UnixMicros,
    /// Encoded frame bytes charged to bounded staging or renderer admission.
    pub(super) queue_bytes: usize,
    /// Process-local correlation retained from socket decode through rendering.
    pub(super) delivery_id: u64,
    /// Presentation-only event interpretation derived by cold-attach staging.
    pub(super) presentation: RendererPresentation,
    /// Starts shown before catch-up but absent from its authoritative snapshot.
    pub(super) abandoned_shell_starts: Vec<ShellStartPresentation>,
}

/// Presentation owner retained for a start that may need boundary cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShellStartPresentation {
    /// Public lifecycle id.
    pub(crate) command_id: tau_proto::ShellCommandId,
    /// Canonical target when known.
    pub(crate) target_agent_id: Option<tau_proto::AgentId>,
}

/// Presentation-only interpretation attached to one typed renderer delivery.
pub(super) enum RendererPresentation {
    /// Process the event through its ordinary renderer path.
    Ordinary,
    /// Render a historical shell terminal without consuming an active
    /// lifecycle.
    StandaloneShellTerminal,
    /// Select the validated transcript owner before rendering a pending start.
    ReconstructedToolStart {
        /// Loaded current-session transcript owner validated by the replay
        /// fold.
        owner: tau_proto::AgentId,
    },
}

/// Current presentation behavior for incoming deliveries.
enum StagingPhase {
    /// Plain replay transcript is retained behind current-state catch-up.
    Staging,
    /// Deliveries pass directly through in protocol order; shell reconciliation
    /// remains independently active until replay completion.
    PassThrough,
}

/// Outcome of folding one delivery into bounded tool-reconstruction state.
enum ToolFold {
    /// The delivery was retained or absorbed.
    Consumed,
    /// The delivery does not participate in reconstruction.
    Forward(RendererDelivery),
    /// Retaining the delivery would exceed the shared cold-attach bound.
    Overflow(RendererDelivery),
}

/// Historical tool reconstruction exists only until one replay boundary.
enum ToolReconciliation {
    /// Retain the bounded inputs needed to derive a pending baseline.
    Active(ToolReconstructionState),
    /// Historical input exceeded the bound; suppress replay starts until the
    /// boundary rather than publishing an incomplete pending baseline.
    FailedClosed,
    /// Forward all tool lifecycle events in protocol order.
    Disabled,
}

/// One retained reconstruction fact and its aggregate byte charge.
struct Charged<T> {
    /// Typed fact used by the ownership join.
    value: T,
    /// Encoded source bytes charged to the staging budget.
    bytes: usize,
}

impl<T> Charged<T> {
    /// Couples a retained value to its source-frame charge.
    fn new(value: T, bytes: usize) -> Self {
        Self { value, bytes }
    }
}

/// Aggregate usage across staged deliveries and reconstruction facts.
#[derive(Debug, Default, Eq, PartialEq)]
struct RetainedUsage {
    /// Retained delivery and metadata entry count.
    items: usize,
    /// Retained encoded-frame byte charge.
    bytes: usize,
}

/// Bounded state consumed atomically when tool reconstruction ends.
struct ToolReconstructionState {
    /// Durable dispatched starts not yet closed by a canonical terminal.
    pending_starts: HashMap<tau_proto::ToolCallId, RendererDelivery>,
    /// Live tool frames held until the historical lifecycle fold is complete.
    buffered_live: Vec<RendererDelivery>,
    /// Current replayed session identity used to scope loaded-agent facts.
    current_session_id: Option<Charged<tau_proto::SessionId>>,
    /// Current loaded-agent session membership observed during catch-up.
    loaded_agents: HashMap<tau_proto::AgentId, Charged<tau_proto::SessionId>>,
    /// Transcript tool-call ownership declared by replayed provider responses.
    transcript_tool_owners: HashMap<tau_proto::ToolCallId, Charged<tau_proto::AgentId>>,
}

impl ToolReconstructionState {
    /// Creates empty, active reconstruction state.
    fn new() -> Self {
        Self {
            pending_starts: HashMap::new(),
            buffered_live: Vec::new(),
            current_session_id: None,
            loaded_agents: HashMap::new(),
            transcript_tool_owners: HashMap::new(),
        }
    }
}

/// Shell lifecycle reconciliation around one replay boundary.
enum ShellReconciliation {
    /// Record live/snapshot starts until replay completes.
    Collecting {
        /// Every start already admitted before the boundary.
        starts: HashMap<tau_proto::ShellCommandId, Option<tau_proto::AgentId>>,
        /// Starts confirmed by a current-state snapshot.
        snapshotted: HashSet<tau_proto::ShellCommandId>,
    },
    /// Suppress queued duplicate starts, preserve historical terminals as
    /// standalone, and retain each snapshot-confirmed id until its live
    /// terminal settles it; disable reconciliation once no confirmed ids
    /// remain.
    Draining(HashSet<tau_proto::ShellCommandId>),
    /// Preserve steady-state delivery without filtering.
    Disabled,
}

/// Bounded UI-local staging that places cold-attach state before transcript
/// rows and reconciles duplicate shell-start observations.
///
/// A historical terminal that reuses an active id renders as a standalone old
/// lifecycle without consuming the current row.
pub(super) struct ColdAttachStager {
    /// Current presentation behavior.
    phase: StagingPhase,
    /// Historical transcript deliveries withheld until current-state catch-up
    /// ends.
    transcript: Vec<RendererDelivery>,
    /// Bytes retained in `transcript`.
    transcript_bytes: usize,
    /// Active public shell lifecycle ids already admitted to the renderer.
    shell_reconciliation: ShellReconciliation,
    /// One-shot historical tool reconstruction state.
    tool_reconciliation: ToolReconciliation,
}

impl ColdAttachStager {
    /// Creates staging for an explicit attach.
    pub(super) fn staging() -> Self {
        Self {
            phase: StagingPhase::Staging,
            transcript: Vec::new(),
            transcript_bytes: 0,
            shell_reconciliation: ShellReconciliation::Collecting {
                starts: HashMap::new(),
                snapshotted: HashSet::new(),
            },
            tool_reconciliation: ToolReconciliation::Active(ToolReconstructionState::new()),
        }
    }

    /// Creates protocol-order pass-through for new and resumed owning UIs.
    pub(super) fn pass_through() -> Self {
        Self {
            phase: StagingPhase::PassThrough,
            transcript: Vec::new(),
            transcript_bytes: 0,
            shell_reconciliation: ShellReconciliation::Collecting {
                starts: HashMap::new(),
                snapshotted: HashSet::new(),
            },
            tool_reconciliation: ToolReconciliation::Disabled,
        }
    }

    /// Admits one decoded delivery and returns deliveries ready for rendering.
    pub(super) fn admit(&mut self, mut delivery: RendererDelivery) -> Vec<RendererDelivery> {
        if delivery.replay
            && let Event::ProviderToolError(error) = &delivery.event
        {
            delivery.event = Event::ToolError(error.clone());
        }
        if !self.observe_tool_reconstruction_scope(&delivery) {
            return self.finish_tool_reconstruction(Some(delivery), true);
        }
        let replay_complete = matches!(delivery.event, Event::SessionReplayComplete(_));
        if !replay_complete {
            delivery = match self.fold_tool_delivery(delivery) {
                ToolFold::Consumed => return Vec::new(),
                ToolFold::Forward(delivery) => delivery,
                ToolFold::Overflow(delivery) => {
                    let discard_baseline = delivery.replay;
                    return self.finish_tool_reconstruction(Some(delivery), discard_baseline);
                }
            };
        }
        if replay_complete {
            delivery.abandoned_shell_starts = self.finish_shell_reconciliation();
        }
        match (&mut self.shell_reconciliation, &delivery.event) {
            (
                ShellReconciliation::Collecting {
                    starts,
                    snapshotted,
                },
                Event::UiShellCommand(command),
            ) => {
                if delivery.replay {
                    snapshotted.insert(command.command_id.clone());
                }
                let already_started = starts
                    .insert(command.command_id.clone(), command.target_agent_id.clone())
                    .is_some();
                if already_started {
                    return Vec::new();
                }
            }
            (ShellReconciliation::Draining(starts), Event::UiShellCommand(command)) => {
                if starts.contains(&command.command_id) {
                    return Vec::new();
                }
            }
            (
                ShellReconciliation::Collecting { starts, .. },
                Event::ShellCommandFinished(finished),
            ) => {
                if delivery.replay && starts.contains_key(&finished.command_id) {
                    delivery.presentation = RendererPresentation::StandaloneShellTerminal;
                    return vec![delivery];
                }
                starts.remove(&finished.command_id);
            }
            (ShellReconciliation::Draining(starts), Event::ShellCommandFinished(finished)) => {
                if delivery.replay && starts.contains(&finished.command_id) {
                    delivery.presentation = RendererPresentation::StandaloneShellTerminal;
                    return vec![delivery];
                }
                starts.remove(&finished.command_id);
                if starts.is_empty()
                    && matches!(self.shell_reconciliation, ShellReconciliation::Draining(_))
                {
                    self.shell_reconciliation = ShellReconciliation::Disabled;
                }
            }
            _ => {}
        }
        if replay_complete {
            if matches!(
                &delivery.event,
                Event::SessionReplayComplete(complete) if complete.error.is_some()
            ) && let ToolReconciliation::Active(state) = &mut self.tool_reconciliation
            {
                state.pending_starts.clear();
            }
            let mut ready = self.finish_tool_reconstruction(None, false);
            ready.push(delivery);
            return ready;
        }
        if matches!(self.phase, StagingPhase::PassThrough) {
            return vec![delivery];
        }
        if delivery.replay && is_tool_transcript_event(&delivery.event) {
            // Tool transcript reconstruction has cross-event ordering
            // dependencies. Keep its established protocol order; cold-attach
            // staging intentionally covers the plain prompt/response scenario.
            return self.finish_staging(delivery);
        }
        if delivery.replay && is_transcript_event(&delivery.event) {
            if self.can_retain(&delivery) {
                let next_bytes = self.transcript_bytes.saturating_add(delivery.queue_bytes);
                self.transcript.push(delivery);
                self.transcript_bytes = next_bytes;
                return Vec::new();
            }

            // Preserve retained relative order if an unusually large catch-up
            // exceeds the UI-local presentation budget.
            return self.finish_staging(delivery);
        }
        vec![delivery]
    }

    /// Records the current-session, loaded-agent, transcript-call join used to
    /// filter reconstructed starts at the replay boundary.
    fn observe_tool_reconstruction_scope(&mut self, delivery: &RendererDelivery) -> bool {
        if !matches!(self.tool_reconciliation, ToolReconciliation::Active(_)) {
            return true;
        }
        let charge = delivery.queue_bytes.max(1);
        match &delivery.event {
            Event::SessionStarted(started) => {
                let (adds_item, old_charge) = match &self.tool_reconciliation {
                    ToolReconciliation::Active(state) => (
                        state.current_session_id.is_none(),
                        state
                            .current_session_id
                            .as_ref()
                            .map_or(0, |entry| entry.bytes),
                    ),
                    ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                        unreachable!("checked active state")
                    }
                };
                if !self.can_replace_metadata(adds_item, old_charge, charge) {
                    return false;
                }
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                state.current_session_id = Some(Charged::new(started.session_id.clone(), charge));
            }
            Event::SessionAgentLoaded(loaded) => {
                let (adds_item, old_charge) = match &self.tool_reconciliation {
                    ToolReconciliation::Active(state) => (
                        !state.loaded_agents.contains_key(&loaded.agent_id),
                        state
                            .loaded_agents
                            .get(&loaded.agent_id)
                            .map_or(0, |entry| entry.bytes),
                    ),
                    ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                        unreachable!("checked active state")
                    }
                };
                if !self.can_replace_metadata(adds_item, old_charge, charge) {
                    return false;
                }
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                state.loaded_agents.insert(
                    loaded.agent_id.clone(),
                    Charged::new(loaded.session_id.clone(), charge),
                );
            }
            Event::HarnessAgentContextInitialized(initialized) => {
                let (adds_item, old_charge) = match &self.tool_reconciliation {
                    ToolReconciliation::Active(state) => (
                        !state.loaded_agents.contains_key(&initialized.agent_id),
                        state
                            .loaded_agents
                            .get(&initialized.agent_id)
                            .map_or(0, |entry| entry.bytes),
                    ),
                    ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                        unreachable!("checked active state")
                    }
                };
                if !self.can_replace_metadata(adds_item, old_charge, charge) {
                    return false;
                }
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                state.loaded_agents.insert(
                    initialized.agent_id.clone(),
                    Charged::new(initialized.session_id.clone(), charge),
                );
            }
            Event::SessionAgentUnloaded(unloaded) => {
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                let matches_loaded_session = state
                    .loaded_agents
                    .get(&unloaded.agent_id)
                    .is_some_and(|entry| entry.value == unloaded.session_id);
                if matches_loaded_session {
                    state.loaded_agents.remove(&unloaded.agent_id);
                }
            }
            Event::ProviderResponseFinished(finished) if delivery.replay => {
                for item in &finished.output_items {
                    if let tau_proto::ContextItem::ToolCall(call) = item {
                        let (adds_item, old_charge) = match &self.tool_reconciliation {
                            ToolReconciliation::Active(state) => (
                                !state.transcript_tool_owners.contains_key(&call.call_id),
                                state
                                    .transcript_tool_owners
                                    .get(&call.call_id)
                                    .map_or(0, |entry| entry.bytes),
                            ),
                            ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                                unreachable!("checked active state")
                            }
                        };
                        if !self.can_replace_metadata(adds_item, old_charge, charge) {
                            return false;
                        }
                        let ToolReconciliation::Active(state) = &mut self.tool_reconciliation
                        else {
                            unreachable!("checked active state");
                        };
                        state.transcript_tool_owners.insert(
                            call.call_id.clone(),
                            Charged::new(finished.agent_id.clone(), charge),
                        );
                    }
                }
            }
            _ => {}
        }
        true
    }

    /// Folds replayed dispatched starts against terminals and withholds live
    /// tool frames until replay establishes the pending baseline.
    fn fold_tool_delivery(&mut self, delivery: RendererDelivery) -> ToolFold {
        match self.tool_reconciliation {
            ToolReconciliation::FailedClosed
                if delivery.replay && matches!(delivery.event, Event::ToolStarted(_)) =>
            {
                return ToolFold::Consumed;
            }
            ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                return ToolFold::Forward(delivery);
            }
            ToolReconciliation::Active(_) => {}
        }
        if !delivery.replay && is_tool_lifecycle_event(&delivery.event) {
            if !self.can_retain(&delivery) {
                return ToolFold::Overflow(delivery);
            }
            let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                unreachable!("checked active state");
            };
            state.buffered_live.push(delivery);
            return ToolFold::Consumed;
        }
        if !delivery.replay {
            return ToolFold::Forward(delivery);
        }
        match &delivery.event {
            Event::ToolStarted(started) => {
                let duplicate = match &self.tool_reconciliation {
                    ToolReconciliation::Active(state) => {
                        state.pending_starts.contains_key(&started.call_id)
                    }
                    ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                        unreachable!("checked active state")
                    }
                };
                if duplicate {
                    return ToolFold::Consumed;
                }
                if !self.can_retain(&delivery) {
                    return ToolFold::Overflow(delivery);
                }
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                state
                    .pending_starts
                    .insert(started.call_id.clone(), delivery);
                ToolFold::Consumed
            }
            event if tool_terminal_id(event).is_some() => {
                let call_id = tool_terminal_id(event).expect("matched terminal");
                let ToolReconciliation::Active(state) = &mut self.tool_reconciliation else {
                    unreachable!("checked active state");
                };
                state.pending_starts.remove(call_id);
                ToolFold::Forward(delivery)
            }
            _ => ToolFold::Forward(delivery),
        }
    }

    /// Returns whether one more retained delivery fits the aggregate bound.
    fn can_retain(&self, delivery: &RendererDelivery) -> bool {
        let usage = self.retained_usage();
        usage.items < RENDERER_QUEUE_MAX_ITEMS
            && usage.bytes.saturating_add(delivery.queue_bytes) <= RENDERER_QUEUE_MAX_BYTES
    }

    /// Returns whether replacing or adding one reconstruction index entry fits
    /// the same aggregate budget as retained deliveries.
    fn can_replace_metadata(&self, adds_item: bool, old_charge: usize, new_charge: usize) -> bool {
        let usage = self.retained_usage();
        usage.items.saturating_add(usize::from(adds_item)) <= RENDERER_QUEUE_MAX_ITEMS
            && usage
                .bytes
                .saturating_sub(old_charge)
                .saturating_add(new_charge)
                <= RENDERER_QUEUE_MAX_BYTES
    }

    /// Computes aggregate retained delivery and reconstruction-index usage.
    fn retained_usage(&self) -> RetainedUsage {
        let ToolReconciliation::Active(state) = &self.tool_reconciliation else {
            return RetainedUsage {
                items: self.transcript.len(),
                bytes: self.transcript_bytes,
            };
        };
        let metadata_items = usize::from(state.current_session_id.is_some())
            .saturating_add(state.loaded_agents.len())
            .saturating_add(state.transcript_tool_owners.len());
        let retained_items = self
            .transcript
            .len()
            .saturating_add(state.pending_starts.len())
            .saturating_add(state.buffered_live.len())
            .saturating_add(metadata_items);
        let metadata_bytes = state
            .current_session_id
            .as_ref()
            .map_or(0, |entry| entry.bytes)
            .saturating_add(state.loaded_agents.values().map(|entry| entry.bytes).sum())
            .saturating_add(
                state
                    .transcript_tool_owners
                    .values()
                    .map(|entry| entry.bytes)
                    .sum(),
            );
        let retained_bytes = self
            .transcript_bytes
            .saturating_add(
                state
                    .pending_starts
                    .values()
                    .map(|delivery| delivery.queue_bytes)
                    .sum(),
            )
            .saturating_add(
                state
                    .buffered_live
                    .iter()
                    .map(|delivery| delivery.queue_bytes)
                    .sum(),
            )
            .saturating_add(metadata_bytes);
        RetainedUsage {
            items: retained_items,
            bytes: retained_bytes,
        }
    }

    /// Stops reconstruction and drains the retained baseline before buffered
    /// live frames and an optional overflowing delivery.
    fn finish_tool_reconstruction(
        &mut self,
        overflow: Option<RendererDelivery>,
        discard_baseline: bool,
    ) -> Vec<RendererDelivery> {
        let previous =
            std::mem::replace(&mut self.tool_reconciliation, ToolReconciliation::Disabled);
        let ToolReconciliation::Active(mut state) = previous else {
            return overflow
                .into_iter()
                .filter(|delivery| {
                    !delivery.replay || !matches!(delivery.event, Event::ToolStarted(_))
                })
                .collect();
        };
        if discard_baseline {
            self.tool_reconciliation = ToolReconciliation::FailedClosed;
        }
        for buffered in state.buffered_live.iter().chain(overflow.iter()) {
            if let Some(call_id) = tool_terminal_id(&buffered.event) {
                state.pending_starts.remove(call_id);
            } else if let Event::ToolStarted(started) = &buffered.event {
                state.pending_starts.remove(&started.call_id);
            }
        }
        let mut ready = self.finish();
        let mut starts = if discard_baseline {
            Vec::new()
        } else {
            state.pending_starts.drain().collect::<Vec<_>>()
        };
        starts.sort_by_key(|(_, delivery)| delivery.delivery_id);
        ready.extend(starts.into_iter().filter_map(|(call_id, mut delivery)| {
            let Event::ToolStarted(started) = &delivery.event else {
                return None;
            };
            let transcript_owned = state
                .transcript_tool_owners
                .get(&call_id)
                .map(|entry| &entry.value);
            let loaded_session = state
                .loaded_agents
                .get(&started.agent_id)
                .map(|entry| &entry.value);
            let current_session_id = state.current_session_id.as_ref().map(|entry| &entry.value);
            let authorized = transcript_owned == Some(&started.agent_id)
                && matches!(
                    (loaded_session, current_session_id),
                    (Some(loaded), Some(current)) if loaded == current
                );
            if authorized {
                delivery.presentation = RendererPresentation::ReconstructedToolStart {
                    owner: started.agent_id.clone(),
                };
                Some(delivery)
            } else {
                None
            }
        }));
        ready.append(&mut state.buffered_live);
        ready.extend(overflow.into_iter().filter(|delivery| {
            !discard_baseline
                || !delivery.replay
                || !matches!(delivery.event, Event::ToolStarted(_))
        }));
        ready
    }

    /// Flushes retained transcript and permanently resumes protocol order
    /// without ending the independent shell-reconciliation phase.
    fn finish_staging(&mut self, delivery: RendererDelivery) -> Vec<RendererDelivery> {
        let mut ready = self.finish();
        ready.push(delivery);
        ready
    }

    /// Close collection at replay completion while retaining only ids whose
    /// already-queued duplicate start or live terminal still needs draining.
    fn finish_shell_reconciliation(&mut self) -> Vec<ShellStartPresentation> {
        let (next, abandoned) = match std::mem::replace(
            &mut self.shell_reconciliation,
            ShellReconciliation::Disabled,
        ) {
            ShellReconciliation::Collecting {
                starts,
                snapshotted,
            } => {
                let abandoned = starts
                    .into_iter()
                    .filter(|(command_id, _)| !snapshotted.contains(command_id))
                    .map(|(command_id, target_agent_id)| ShellStartPresentation {
                        command_id,
                        target_agent_id,
                    })
                    .collect::<Vec<_>>();
                let next = if snapshotted.is_empty() {
                    ShellReconciliation::Disabled
                } else {
                    ShellReconciliation::Draining(snapshotted)
                };
                (next, abandoned)
            }
            _ => (ShellReconciliation::Disabled, Vec::new()),
        };
        self.shell_reconciliation = next;
        abandoned
    }

    /// Ends staging and returns retained transcript in relative order.
    fn finish(&mut self) -> Vec<RendererDelivery> {
        self.phase = StagingPhase::PassThrough;
        self.transcript_bytes = 0;
        std::mem::take(&mut self.transcript)
    }

    /// Drains retained history before a remote termination is enqueued.
    pub(super) fn finish_before_disconnect(&mut self) -> Vec<RendererDelivery> {
        self.finish_tool_reconstruction(None, false)
    }
}

/// Identifies replay rows that constitute visible transcript history.
fn is_transcript_event(event: &Event) -> bool {
    matches!(
        event,
        Event::UiPromptSubmitted(_)
            | Event::AgentPromptSubmitted(_)
            | Event::ProviderResponseFinished(_)
    )
}

/// Identifies tool-bearing catch-up whose renderer dependencies require wire
/// order.
fn is_tool_transcript_event(event: &Event) -> bool {
    match event {
        Event::ProviderResponseFinished(finished) => finished.output_items.iter().any(|item| {
            matches!(
                item,
                tau_proto::ContextItem::ToolCall(_) | tau_proto::ContextItem::ToolResult(_)
            )
        }),
        Event::ToolStarted(_)
        | Event::ToolResultDisplay(_)
        | Event::ToolResult(_)
        | Event::ProviderToolResult(_)
        | Event::ToolError(_)
        | Event::ProviderToolError(_) => true,
        _ => false,
    }
}

/// Identifies frames whose ordering depends on the replayed tool baseline.
fn is_tool_lifecycle_event(event: &Event) -> bool {
    matches!(event, Event::ToolStarted(_) | Event::ToolProgress(_))
        || tool_terminal_id(event).is_some()
}

/// Returns the call id for canonical UI lifecycle terminals.
fn tool_terminal_id(event: &Event) -> Option<&tau_proto::ToolCallId> {
    match event {
        Event::ToolRejected(event) => Some(&event.call_id),
        Event::ToolResultDisplay(event) if event.kind == tau_proto::ToolResultKind::Final => {
            Some(&event.call_id)
        }
        Event::ToolError(event) => Some(&event.call_id),
        Event::ToolCancelled(event) => Some(&event.call_id),
        Event::ToolBackgroundResultDisplay(event) => Some(&event.call_id),
        Event::ToolBackgroundError(event) => Some(&event.call_id),
        _ => None,
    }
}

/// Converts one delivery while suppressing replayed terminal side effects.
pub(super) fn renderer_event_from_delivery(
    delivery: tau_proto::EventDelivery,
    queue_bytes: usize,
    delivery_id: u64,
) -> Option<RendererDelivery> {
    let (event, replay, recorded_at) = delivery.into_parts();
    if replay && matches!(event, Event::Osc1337SetUserVar(_) | Event::TermBell(_)) {
        return None;
    }
    Some(RendererDelivery {
        event,
        replay,
        recorded_at: recorded_at.unwrap_or_else(UnixMicros::now),
        queue_bytes,
        delivery_id,
        presentation: RendererPresentation::Ordinary,
        abandoned_shell_starts: Vec::new(),
    })
}
