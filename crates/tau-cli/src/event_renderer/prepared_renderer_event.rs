//! Provenance-coupled renderer events and deferred ownership.

use tau_proto::{Event, ProviderResponseFinished, UnixMicros};

use super::terminal_tool_calls::TerminalToolCalls;

/// One event coupled to its exact terminal metadata projection when applicable.
pub(super) struct PreparedRendererEvent<'a> {
    /// Canonical event used by generic routing.
    event: &'a Event,
    /// Lightweight metadata present exactly for provider terminals.
    tool_calls: Option<TerminalToolCalls>,
}

impl<'a> PreparedRendererEvent<'a> {
    /// Couples one incoming event to exactly one terminal projection.
    pub(super) fn new(event: &'a Event) -> Self {
        let tool_calls = match event {
            Event::ProviderResponseFinished(finished) => {
                Some(TerminalToolCalls::from_finished(finished))
            }
            _ => None,
        };
        Self { event, tool_calls }
    }

    /// Returns the canonical generic event.
    pub(super) fn event(&self) -> &'a Event {
        self.event
    }

    /// Returns the provenance-coupled provider terminal and metadata.
    pub(super) fn finished(&self) -> Option<(&'a ProviderResponseFinished, &TerminalToolCalls)> {
        match (self.event, self.tool_calls.as_ref()) {
            (Event::ProviderResponseFinished(finished), Some(tool_calls)) => {
                Some((finished, tool_calls))
            }
            (Event::ProviderResponseFinished(_), None) | (_, Some(_)) => {
                unreachable!("private constructor preserves terminal provenance")
            }
            (_, None) => None,
        }
    }

    /// Moves lightweight metadata into deferred storage.
    pub(super) fn deferred(self, recorded_at: UnixMicros) -> DeferredRendererEvent {
        match (self.event, self.tool_calls) {
            (Event::ProviderResponseFinished(finished), Some(tool_calls)) => {
                DeferredRendererEvent {
                    inner: DeferredRendererEventKind::ProviderFinished {
                        finished: finished.clone(),
                        tool_calls,
                        recorded_at,
                    },
                }
            }
            (Event::ProviderResponseFinished(_), None) | (_, Some(_)) => {
                unreachable!("private constructor preserves terminal provenance")
            }
            (event, None) => DeferredRendererEvent {
                inner: DeferredRendererEventKind::Ordinary {
                    event: event.clone(),
                    recorded_at,
                },
            },
        }
    }
}

/// One deferred initial-discovery event with terminal metadata retained once.
pub(super) struct DeferredRendererEvent {
    /// Private representation constructed only from validated prepared events.
    inner: DeferredRendererEventKind,
}

/// Owned deferred representation hidden from renderer callers.
enum DeferredRendererEventKind {
    /// Ordinary deferred event.
    Ordinary {
        /// Canonical event payload.
        event: Event,
        /// Original durable recording time.
        recorded_at: UnixMicros,
    },
    /// Provider terminal with its already-built lightweight projection.
    ProviderFinished {
        /// Canonical terminal payload retained by existing discovery replay.
        finished: ProviderResponseFinished,
        /// Single metadata projection reused during publication.
        tool_calls: TerminalToolCalls,
        /// Original durable recording time.
        recorded_at: UnixMicros,
    },
}

impl DeferredRendererEvent {
    /// Reborrows owned deferred state through the same validated prepared
    /// shape.
    pub(super) fn with_prepared<T>(
        self,
        run: impl for<'a> FnOnce(PreparedRendererEvent<'a>, UnixMicros) -> T,
    ) -> T {
        match self.inner {
            DeferredRendererEventKind::Ordinary { event, recorded_at } => {
                run(PreparedRendererEvent::new(&event), recorded_at)
            }
            DeferredRendererEventKind::ProviderFinished {
                finished,
                tool_calls,
                recorded_at,
            } => {
                let event = Event::ProviderResponseFinished(finished);
                run(
                    PreparedRendererEvent {
                        event: &event,
                        tool_calls: Some(tool_calls),
                    },
                    recorded_at,
                )
            }
        }
    }

    /// Returns `Some` for an ordinary event and `None` for a provider terminal.
    pub(super) fn ordinary_event(&self) -> Option<&Event> {
        match &self.inner {
            DeferredRendererEventKind::Ordinary { event, .. } => Some(event),
            DeferredRendererEventKind::ProviderFinished { .. } => None,
        }
    }
}
