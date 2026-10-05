//! Tracks initial subscription admission without retaining replay payloads.

use tau_proto::{Event, HarnessOutputMessage, SessionId};

/// One attachment's history-to-live boundary, independent of renderer
/// selection.
pub(crate) struct SubscriptionHandoff {
    /// Exact admitted session when the caller already knows it.
    expected_session_id: Option<SessionId>,
    /// A failure remains terminal even if a later success marker arrives.
    state: HandoffState,
}

/// Initial replay admission; this state never queues or retries user work.
enum HandoffState {
    /// Historical delivery and live eligibility are not yet complete.
    Pending,
    /// The successful, non-replayed boundary was observed.
    Ready,
    /// Replay or transport failed before handoff.
    Failed(String),
}

impl SubscriptionHandoff {
    /// Starts closed until the exact successful session boundary arrives.
    pub(crate) fn new(expected_session_id: Option<SessionId>) -> Self {
        Self {
            expected_session_id,
            state: HandoffState::Pending,
        }
    }

    /// Observes protocol admission without consuming any renderer-owned
    /// payload.
    pub(crate) fn observe(&mut self, message: &HarnessOutputMessage) {
        if !matches!(self.state, HandoffState::Pending) {
            return;
        }
        match message {
            HarnessOutputMessage::Deliver(delivery) => match delivery.event.as_ref() {
                Event::AgentReplayComplete(complete) => {
                    if let Some(error) = &complete.error {
                        self.state = HandoffState::Failed(format!("Agent replay failed: {error}"));
                    }
                }
                Event::SessionReplayComplete(complete) if !delivery.replay => {
                    self.state = if self
                        .expected_session_id
                        .as_ref()
                        .is_some_and(|expected| expected != &complete.session_id)
                    {
                        HandoffState::Failed(
                            "Subscription completed for a different session.".to_owned(),
                        )
                    } else if let Some(error) = &complete.error {
                        HandoffState::Failed(format!("Session replay failed: {error}"))
                    } else {
                        HandoffState::Ready
                    };
                }
                _ => {}
            },
            HarnessOutputMessage::Disconnect(disconnect) => {
                self.state = HandoffState::Failed(disconnect.reason.clone().unwrap_or_else(|| {
                    "Daemon disconnected before subscription handoff.".to_owned()
                }));
            }
            _ => {}
        }
    }

    /// Returns an explicit input diagnostic while submission remains closed.
    pub(crate) fn submission_blocked(&self) -> Option<&str> {
        match &self.state {
            HandoffState::Pending => {
                Some("Session history is still loading; press Enter again when ready.")
            }
            HandoffState::Failed(error) => Some(error),
            HandoffState::Ready => None,
        }
    }

    /// Reports terminal failure to blocking one-shot callers.
    pub(crate) fn failure(&self) -> Option<&str> {
        match &self.state {
            HandoffState::Failed(error) => Some(error),
            HandoffState::Pending | HandoffState::Ready => None,
        }
    }
}

#[cfg(test)]
mod tests;
