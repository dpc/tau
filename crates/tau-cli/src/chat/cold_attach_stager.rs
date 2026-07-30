//! Bounded presentation-only staging for explicit cold attachment.

use tau_proto::{Event, UnixMicros};

use super::{RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS};

#[cfg(test)]
mod tests;

/// One decoded event retaining the delivery phase needed by cold-attach
/// staging.
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
}

/// Current presentation behavior for incoming deliveries.
enum StagingPhase {
    /// Plain replay transcript is retained behind current-state catch-up.
    Staging,
    /// Deliveries pass directly through in protocol order.
    PassThrough,
}

/// Bounded UI-local staging that places cold-attach state before transcript
/// rows.
pub(super) struct ColdAttachStager {
    /// Current presentation behavior.
    phase: StagingPhase,
    /// Historical transcript deliveries withheld until current-state catch-up
    /// ends.
    transcript: Vec<RendererDelivery>,
    /// Bytes retained in `transcript`.
    transcript_bytes: usize,
}

impl ColdAttachStager {
    /// Creates staging for an explicit attach.
    pub(super) fn staging() -> Self {
        Self {
            phase: StagingPhase::Staging,
            transcript: Vec::new(),
            transcript_bytes: 0,
        }
    }

    /// Creates protocol-order pass-through for new and resumed owning UIs.
    pub(super) fn pass_through() -> Self {
        Self {
            phase: StagingPhase::PassThrough,
            transcript: Vec::new(),
            transcript_bytes: 0,
        }
    }

    /// Admits one decoded delivery and returns deliveries ready for rendering.
    pub(super) fn admit(&mut self, delivery: RendererDelivery) -> Vec<RendererDelivery> {
        if matches!(self.phase, StagingPhase::PassThrough) {
            return vec![delivery];
        }
        if matches!(delivery.event, Event::SessionReplayComplete(_)) {
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

    /// Flushes retained transcript and permanently resumes protocol order.
    fn finish_staging(&mut self, delivery: RendererDelivery) -> Vec<RendererDelivery> {
        let mut ready = self.finish();
        ready.push(delivery);
        ready
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
    })
}
