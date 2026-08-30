//! Lightweight tool-call facts shared across one terminal dispatch.

use tau_proto::{ProviderResponseFinished, ProviderStopReason, ToolCallId, ToolName};

/// Retained metadata for one provider-declared tool call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TerminalToolCall {
    /// Stable call identity needed by lifecycle and ownership state.
    pub(super) call_id: ToolCallId,
    /// Tool name needed by placeholder classification.
    pub(super) name: ToolName,
    /// Whether the terminal permits this call to become executable.
    pub(super) admitted: bool,
}

/// Lightweight tool-call metadata projected once from one provider terminal.
#[derive(Clone, Debug)]
pub(super) struct TerminalToolCalls {
    /// Calls in their original provider output order.
    calls: Vec<TerminalToolCall>,
    /// Exact construction work for allocation and traversal regression tests.
    work: TerminalToolCallWork,
}

/// Exact operation-coupled work performed by one terminal projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct TerminalToolCallWork {
    /// Complete output items visited by the projection loop.
    pub(super) output_items_visited: usize,
    /// Metadata buffers allocated by the projection.
    pub(super) metadata_buffers_allocated: usize,
    /// Metadata element slots reserved in the single buffer.
    pub(super) metadata_slots_reserved: usize,
    /// Lightweight id/name fields cloned for retention.
    pub(super) metadata_fields_cloned: usize,
}

impl TerminalToolCalls {
    /// Retains only lightweight fields from one linear terminal-output
    /// traversal.
    pub(super) fn from_finished(finished: &ProviderResponseFinished) -> Self {
        let admitted = finished.stop_reason != ProviderStopReason::Length;
        let mut calls = Vec::new();
        let mut work = TerminalToolCallWork::default();
        for (index, item) in finished.output_items.iter().enumerate() {
            work.output_items_visited += 1;
            let tau_proto::ContextItem::ToolCall(call) = item else {
                continue;
            };
            if calls.is_empty() {
                let slots = finished.output_items.len() - index;
                calls.reserve_exact(slots);
                work.metadata_buffers_allocated = 1;
                work.metadata_slots_reserved = slots;
            }
            calls.push(TerminalToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                admitted,
            });
            work.metadata_fields_cloned += 2;
        }
        observe_terminal_tool_call_work(work);
        Self { calls, work }
    }

    /// Returns whether the terminal declares no tool calls.
    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Returns the number of declared calls.
    pub(super) fn len(&self) -> usize {
        self.calls.len()
    }

    /// Returns the number of executable calls from the canonical
    /// classification.
    pub(super) fn admitted_len(&self) -> usize {
        self.calls.iter().filter(|call| call.admitted).count()
    }

    /// Iterates retained metadata in provider output order.
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &TerminalToolCall> {
        self.calls.iter()
    }

    /// Iterates call ids needed by activity and ownership state.
    pub(super) fn call_ids(&self) -> impl ExactSizeIterator<Item = &ToolCallId> {
        self.calls.iter().map(|call| &call.call_id)
    }

    /// Returns exact construction work for tracing and regression tests.
    pub(super) fn work(&self) -> TerminalToolCallWork {
        self.work
    }
}

#[cfg(test)]
/// Test-only callback for one complete projection measurement.
type TerminalToolCallWorkObserver = Box<dyn FnMut(TerminalToolCallWork)>;

#[cfg(test)]
thread_local! {
    /// Per-thread observer used by complete dispatch regression tests.
    static TERMINAL_TOOL_CALL_WORK_OBSERVER:
        std::cell::RefCell<Option<TerminalToolCallWorkObserver>> =
        const { std::cell::RefCell::new(None) };
}

/// Reports one projection's exact work to the test-only dispatch observer.
fn observe_terminal_tool_call_work(work: TerminalToolCallWork) {
    #[cfg(test)]
    TERMINAL_TOOL_CALL_WORK_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().as_mut() {
            observer(work);
        }
    });
    #[cfg(not(test))]
    let _ = work;
}

/// Installs a scoped projection-work observer for complete dispatch tests.
#[cfg(test)]
pub(super) fn with_terminal_tool_call_work_observer<T>(
    observer: impl FnMut(TerminalToolCallWork) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    /// Clears the observer even when the measured dispatch unwinds.
    struct ObserverReset;

    impl Drop for ObserverReset {
        fn drop(&mut self) {
            TERMINAL_TOOL_CALL_WORK_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    TERMINAL_TOOL_CALL_WORK_OBSERVER.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "projection observer already installed"
        );
        *slot.borrow_mut() = Some(Box::new(observer));
    });
    let _reset = ObserverReset;
    run()
}
