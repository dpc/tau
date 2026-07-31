//! Bounded presentation-only staging for explicit cold attachment.

use std::collections::{HashMap, HashSet};

use tau_proto::{Event, UnixMicros};

use super::{RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS};

#[cfg(test)]
mod tests;

/// One decoded event plus presentation actions derived from the replay
/// boundary. The renderer applies abandonment before the event and uses the
/// standalone flag only for the annotated historical terminal.
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
    /// Render a historical shell terminal without consuming an active
    /// lifecycle.
    pub(super) standalone_shell_terminal: bool,
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

/// Current presentation behavior for incoming deliveries.
enum StagingPhase {
    /// Plain replay transcript is retained behind current-state catch-up.
    Staging,
    /// Deliveries pass directly through in protocol order; shell reconciliation
    /// remains independently active until replay completion.
    PassThrough,
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
        }
    }

    /// Admits one decoded delivery and returns deliveries ready for rendering.
    pub(super) fn admit(&mut self, mut delivery: RendererDelivery) -> Vec<RendererDelivery> {
        let replay_complete = matches!(delivery.event, Event::SessionReplayComplete(_));
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
                    delivery.standalone_shell_terminal = true;
                    return vec![delivery];
                }
                starts.remove(&finished.command_id);
            }
            (ShellReconciliation::Draining(starts), Event::ShellCommandFinished(finished)) => {
                if delivery.replay && starts.contains(&finished.command_id) {
                    delivery.standalone_shell_terminal = true;
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
        if matches!(self.phase, StagingPhase::PassThrough) {
            return vec![delivery];
        }
        if replay_complete {
            return self.finish_staging(delivery);
        }
        if delivery.replay && is_tool_transcript_event(&delivery.event) {
            // Tool transcript reconstruction has cross-event ordering
            // dependencies. Keep its established protocol order; cold-attach
            // staging intentionally covers the plain prompt/response scenario.
            return self.finish_staging(delivery);
        }
        if delivery.replay && is_transcript_event(&delivery.event) {
            let next_bytes = self.transcript_bytes.saturating_add(delivery.queue_bytes);
            if self.transcript.len() < RENDERER_QUEUE_MAX_ITEMS
                && next_bytes <= RENDERER_QUEUE_MAX_BYTES
            {
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
        self.finish()
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
        standalone_shell_terminal: false,
        abandoned_shell_starts: Vec::new(),
    })
}
