//! Content-free renderer progress and presentation facts.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tau_cli_term::RendererDeliveryId;
use tau_proto::Event;

use super::LAST_HANDLER_STALL_WARNING;
use crate::MUTEX_POISONED;

/// Rate-limits renderer stall warnings to one shared five-second window.
pub(super) fn admit_handler_stall_warning(now: Instant) -> bool {
    let mut last = LAST_HANDLER_STALL_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect(MUTEX_POISONED);
    if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(5)) {
        return false;
    }
    *last = Some(now);
    true
}

/// Content-free renderer stage timing emitted on every handler exit.
pub(super) struct HandlerProgress {
    /// Process-local delivery correlation when the socket supplied this event.
    pub(super) delivery_id: Option<RendererDeliveryId>,
    /// Stable content-free protocol event name.
    pub(super) event_name: tau_proto::EventName,
    /// Monotonic handler start time.
    pub(super) started_at: Instant,
}

impl Drop for HandlerProgress {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            delivery_id = self.delivery_id.map(RendererDeliveryId::get),
            event_name = %self.event_name,
            handler_us = elapsed.as_micros(),
            "renderer handler finished"
        );
        if Duration::from_millis(500) <= elapsed && admit_handler_stall_warning(Instant::now()) {
            tracing::warn!(
                target: "tau_cli::frontend_progress",
                delivery_id = self.delivery_id.map(RendererDeliveryId::get),
                event_name = %self.event_name,
                handler_ms = elapsed.as_millis(),
                "renderer handler stalled"
            );
        }
    }
}

/// Selects canonical facts whose visible presentation can be flush-correlated.
pub(super) fn presentation_fact(event: &Event) -> Option<PresentationFactClass> {
    presentation_fact_name(&event.name())
}

/// CLI-owned canonical selected-presentation fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum PresentationFactClass {
    /// A prompt entered the visible queued state.
    PromptQueued,
    /// A queued prompt reached its visible submitted state.
    PromptSubmitted,
    /// A visible prompt accepted steering content.
    PromptSteered,
    /// A streaming response visibly advanced.
    ResponseUpdated,
    /// A response visibly reached its canonical terminal presentation.
    ResponseFinished,
    /// A prompt visibly ended through cancellation or supersession.
    PromptTerminated,
}

impl PresentationFactClass {
    /// Returns the one invariant event/class label written to operational
    /// traces.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::PromptQueued => "agent.prompt_queued/prompt_queued",
            Self::PromptSubmitted => "agent.prompt_submitted/prompt_submitted",
            Self::PromptSteered => "agent.prompt_steered/prompt_steered",
            Self::ResponseUpdated => "provider.response_updated/response_updated",
            Self::ResponseFinished => "provider.response_finished/response_finished",
            Self::PromptTerminated => "agent.prompt_terminated/prompt_terminated",
        }
    }

    /// Returns this fact's opaque terminal-layer invalidation key.
    fn key(self) -> tau_cli_term::PresentationObservationKey {
        tau_cli_term::PresentationObservationKey::new(self as u8)
            .expect("finite CLI presentation class must fit the raw invalidation mask")
    }

    /// Returns the opaque predecessor-key mask superseded by this fact.
    fn invalidates(self) -> tau_cli_term::PresentationInvalidation {
        let none = tau_cli_term::PresentationInvalidation::none();
        match self {
            Self::PromptSubmitted => none.with(Self::PromptQueued.key()),
            Self::ResponseFinished => none.with(Self::ResponseUpdated.key()),
            Self::PromptTerminated => none
                .with(Self::PromptQueued.key())
                .with(Self::PromptSubmitted.key())
                .with(Self::ResponseUpdated.key()),
            _ => none,
        }
    }

    /// Builds the application-agnostic typed fact accepted by the raw layer.
    pub(crate) fn opaque_fact(self) -> tau_cli_term::OpaquePresentationFact {
        tau_cli_term::OpaquePresentationFact::new(self.label(), self.key(), self.invalidates())
    }

    /// Returns whether mutation and registration require atomic capture
    /// suppression.
    pub(super) const fn invalidates_pending(self) -> bool {
        matches!(
            self,
            Self::PromptSubmitted | Self::ResponseFinished | Self::PromptTerminated
        )
    }
}

/// Maps stable canonical event names to content-free presentation classes.
pub(super) fn presentation_fact_name(
    event_name: &tau_proto::EventName,
) -> Option<PresentationFactClass> {
    use PresentationFactClass as Class;
    match event_name {
        name if name == &tau_proto::EventName::AGENT_PROMPT_QUEUED => Some(Class::PromptQueued),
        name if name == &tau_proto::EventName::AGENT_PROMPT_SUBMITTED => {
            Some(Class::PromptSubmitted)
        }
        name if name == &tau_proto::EventName::AGENT_PROMPT_STEERED => Some(Class::PromptSteered),
        name if name == &tau_proto::EventName::PROVIDER_RESPONSE_UPDATED => {
            Some(Class::ResponseUpdated)
        }
        name if name == &tau_proto::EventName::PROVIDER_RESPONSE_FINISHED => {
            Some(Class::ResponseFinished)
        }
        name if name == &tau_proto::EventName::AGENT_PROMPT_TERMINATED => {
            Some(Class::PromptTerminated)
        }
        _ => None,
    }
}
