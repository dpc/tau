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
    /// Original decoded event allocation interpreted by the renderer. Staging
    /// and enqueueing move this box intact; replay normalization may replace
    /// only its contained event.
    pub(super) event: Box<Event>,
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
    /// Returns whether reconstruction still owns one process-local delivery.
    ///
    /// This linear probe is used only by guarded decoded-memory measurement.
    pub(super) fn retains_delivery(&self, delivery_id: u64) -> bool {
        self.transcript
            .iter()
            .any(|delivery| delivery.delivery_id == delivery_id)
            || matches!(
                &self.tool_reconciliation,
                ToolReconciliation::Active(state)
                    if state
                        .pending_starts
                        .values()
                        .chain(state.buffered_live.iter())
                        .any(|delivery| delivery.delivery_id == delivery_id)
            )
    }

    /// Returns every process-local delivery currently retained by
    /// reconstruction.
    ///
    /// The caller invokes this linear diagnostic probe only while
    /// decoded-memory measurement is enabled.
    pub(super) fn retained_delivery_ids(&self) -> Vec<u64> {
        let mut ids = self
            .transcript
            .iter()
            .map(|delivery| delivery.delivery_id)
            .collect::<Vec<_>>();
        if let ToolReconciliation::Active(state) = &self.tool_reconciliation {
            ids.extend(
                state
                    .pending_starts
                    .values()
                    .chain(state.buffered_live.iter())
                    .map(|delivery| delivery.delivery_id),
            );
        }
        ids
    }

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
            && let Event::ProviderToolError(error) = delivery.event.as_ref()
        {
            *delivery.event = Event::ToolError(error.clone());
        }
        if !self.observe_tool_reconstruction_scope(&delivery) {
            return self.finish_tool_reconstruction(Some(delivery), true);
        }
        let replay_complete = matches!(delivery.event.as_ref(), Event::SessionReplayComplete(_));
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
        match (&mut self.shell_reconciliation, delivery.event.as_ref()) {
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
                delivery.event.as_ref(),
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
        if delivery.replay && is_tool_transcript_event(delivery.event.as_ref()) {
            // Tool transcript reconstruction has cross-event ordering
            // dependencies. Keep its established protocol order; cold-attach
            // staging intentionally covers the plain prompt/response scenario.
            return self.finish_staging(delivery);
        }
        if delivery.replay && is_transcript_event(delivery.event.as_ref()) {
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
        match delivery.event.as_ref() {
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
                if delivery.replay && matches!(delivery.event.as_ref(), Event::ToolStarted(_)) =>
            {
                return ToolFold::Consumed;
            }
            ToolReconciliation::FailedClosed | ToolReconciliation::Disabled => {
                return ToolFold::Forward(delivery);
            }
            ToolReconciliation::Active(_) => {}
        }
        if !delivery.replay && is_tool_lifecycle_event(delivery.event.as_ref()) {
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
        match delivery.event.as_ref() {
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

    /// Stops reconstruction after deriving an owner-aware baseline for buffered
    /// live frames and an optional overflowing delivery.
    ///
    /// The first pass walks buffered frames in order and determines whether a
    /// held replay start needs materializing: it survives a buffered terminal
    /// only when an earlier buffered progress frame needs that start as its
    /// renderer owner. It ignores starts and progress after a terminal. The
    /// second pass emits starts and progress only with an active owner,
    /// preserves the first terminal even without one, and suppresses every
    /// later lifecycle frame. This preserves a valid progress/terminal
    /// transition without creating an ownerless progress block after replay
    /// failure, ownership rejection, or a late report.
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
                    !delivery.replay || !matches!(delivery.event.as_ref(), Event::ToolStarted(_))
                })
                .collect();
        };
        if discard_baseline {
            self.tool_reconciliation = ToolReconciliation::FailedClosed;
        }
        let mut buffered_live_starts = HashSet::new();
        let mut buffered_terminals = HashSet::new();
        let mut progress_before_terminal = HashSet::new();
        let mut first_pass_settled_calls = HashSet::new();
        for buffered in state.buffered_live.iter().chain(overflow.iter()) {
            match tool_lifecycle_frame(&buffered.event) {
                Some(ToolLifecycleFrame::Started(call_id))
                    if !first_pass_settled_calls.contains(call_id) =>
                {
                    buffered_live_starts.insert(call_id.clone());
                }
                Some(ToolLifecycleFrame::Progress(call_id))
                    if !first_pass_settled_calls.contains(call_id) =>
                {
                    progress_before_terminal.insert(call_id.clone());
                }
                Some(ToolLifecycleFrame::Terminal(call_id)) => {
                    first_pass_settled_calls.insert(call_id.clone());
                    buffered_terminals.insert(call_id.clone());
                }
                _ => {}
            }
        }
        let mut ready = self.finish();
        let mut starts = state
            .pending_starts
            .drain()
            .filter(|(call_id, _)| {
                !discard_baseline
                    && !buffered_live_starts.contains(call_id)
                    && (!buffered_terminals.contains(call_id)
                        || progress_before_terminal.contains(call_id))
            })
            .filter_map(|(call_id, mut delivery)| {
                let Event::ToolStarted(started) = delivery.event.as_ref() else {
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
                let current_session_id =
                    state.current_session_id.as_ref().map(|entry| &entry.value);
                let authorized = transcript_owned == Some(&started.agent_id)
                    && matches!(
                        (loaded_session, current_session_id),
                        (Some(loaded), Some(current)) if loaded == current
                    );
                if authorized {
                    delivery.presentation = RendererPresentation::ReconstructedToolStart {
                        owner: started.agent_id.clone(),
                    };
                    Some((call_id, delivery))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        starts.sort_by_key(|(_, delivery)| delivery.delivery_id);
        let mut active_tool_calls = starts
            .iter()
            .map(|(call_id, _)| call_id.clone())
            .collect::<HashSet<_>>();
        let mut settled_tool_calls = HashSet::new();
        ready.extend(starts.into_iter().map(|(_, delivery)| delivery));
        ready.extend(state.buffered_live.drain(..).filter(|delivery| {
            preserve_buffered_tool_lifecycle(
                delivery.event.as_ref(),
                &mut active_tool_calls,
                &mut settled_tool_calls,
            )
        }));
        ready.extend(overflow.into_iter().filter(|delivery| {
            (!discard_baseline
                || !delivery.replay
                || !matches!(delivery.event.as_ref(), Event::ToolStarted(_)))
                && preserve_buffered_tool_lifecycle(
                    delivery.event.as_ref(),
                    &mut active_tool_calls,
                    &mut settled_tool_calls,
                )
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

/// One buffered event that needs an active tool presentation owner.
enum ToolLifecycleFrame<'a> {
    /// A tool lifecycle begins with this id.
    Started(&'a tau_proto::ToolCallId),
    /// A tool lifecycle has an in-flight update for this id.
    Progress(&'a tau_proto::ToolCallId),
    /// A tool lifecycle is terminal for this id.
    Terminal(&'a tau_proto::ToolCallId),
}

/// Classifies buffered frames whose projection depends on a live tool owner.
fn tool_lifecycle_frame(event: &Event) -> Option<ToolLifecycleFrame<'_>> {
    match event {
        Event::ToolStarted(event) => Some(ToolLifecycleFrame::Started(&event.call_id)),
        Event::ToolProgress(event) => Some(ToolLifecycleFrame::Progress(&event.call_id)),
        event => tool_terminal_id(event).map(ToolLifecycleFrame::Terminal),
    }
}

/// Retains starts and progress only with a materialized owner, preserves the
/// first terminal as the visible call outcome, and suppresses later lifecycle
/// frames so progress cannot become an orphan row.
fn preserve_buffered_tool_lifecycle(
    event: &Event,
    active_tool_calls: &mut HashSet<tau_proto::ToolCallId>,
    settled_tool_calls: &mut HashSet<tau_proto::ToolCallId>,
) -> bool {
    match tool_lifecycle_frame(event) {
        Some(ToolLifecycleFrame::Started(call_id)) => {
            if settled_tool_calls.contains(call_id) {
                false
            } else {
                active_tool_calls.insert(call_id.clone());
                true
            }
        }
        Some(ToolLifecycleFrame::Progress(call_id)) => {
            active_tool_calls.contains(call_id) && !settled_tool_calls.contains(call_id)
        }
        Some(ToolLifecycleFrame::Terminal(call_id)) => {
            if !settled_tool_calls.insert(call_id.clone()) {
                return false;
            }
            active_tool_calls.remove(call_id);
            true
        }
        None => true,
    }
}

/// Converts one delivery while suppressing replayed terminal side effects.
///
/// Retains the socket decoder's event box for all admitted deliveries.
pub(super) fn renderer_event_from_delivery(
    delivery: tau_proto::EventDelivery,
    queue_bytes: usize,
    delivery_id: u64,
) -> Option<RendererDelivery> {
    let tau_proto::EventDelivery {
        event,
        replay,
        recorded_at,
    } = delivery;
    if replay
        && matches!(
            event.as_ref(),
            Event::Osc1337SetUserVar(_) | Event::TermBell(_)
        )
    {
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
