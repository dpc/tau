//! Test-only ownership probes for raw-to-canonical Provider reports.

use std::cell::Cell;
use std::num::NonZeroUsize;

use tau_proto::{ContextItem, Event, ProviderResponseTextDelta};

/// Non-null process-local allocation address used only for equality oracles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AllocationIdentity {
    /// Nonzero integer representation of the process-local allocation address.
    address: NonZeroUsize,
}

impl AllocationIdentity {
    /// Capture the allocation identity behind one non-empty slice.
    pub(super) fn of_slice<T>(value: &[T]) -> Option<Self> {
        if value.is_empty() {
            return None;
        }
        NonZeroUsize::new(value.as_ptr() as usize).map(|address| Self { address })
    }

    /// Capture the allocation identity behind one non-empty string.
    fn of_str(value: &str) -> Option<Self> {
        if value.is_empty() {
            return None;
        }
        NonZeroUsize::new(value.as_ptr() as usize).map(|address| Self { address })
    }
}

/// Allocation identities observed before raw projection and after the owned
/// canonical handoff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ProviderReportOwnershipSnapshot {
    /// Update delta-vector allocation before raw observer projection.
    pub(super) update_raw_projection: Option<AllocationIdentity>,
    /// Update delta-vector allocation received by canonical derivation.
    pub(super) update_canonical: Option<AllocationIdentity>,
    /// First update text allocation before raw observer projection.
    pub(super) update_text_raw_projection: Option<AllocationIdentity>,
    /// First update text allocation received by canonical derivation.
    pub(super) update_text_canonical: Option<AllocationIdentity>,
    /// Terminal output-vector allocation before raw observer projection.
    pub(super) finished_raw_projection: Option<AllocationIdentity>,
    /// Terminal output-vector allocation received by canonical derivation.
    pub(super) finished_canonical: Option<AllocationIdentity>,
    /// First terminal text allocation before raw observer projection.
    pub(super) finished_text_raw_projection: Option<AllocationIdentity>,
    /// First terminal text allocation received by canonical derivation.
    pub(super) finished_text_canonical: Option<AllocationIdentity>,
    /// Number of update payloads consumed by canonical derivation.
    pub(super) update_handoffs: usize,
    /// Number of terminal payloads consumed by canonical derivation.
    pub(super) finished_handoffs: usize,
}

thread_local! {
    /// Per-test-thread probe avoids cross-test interference in the parallel suite.
    static SNAPSHOT: Cell<ProviderReportOwnershipSnapshot> =
        Cell::new(ProviderReportOwnershipSnapshot::default());
}

/// Record allocation identities immediately before the raw observer projection.
pub(super) fn observe_before_raw_projection(event: &Event) {
    SNAPSHOT.with(|snapshot| {
        let mut value = snapshot.get();
        match event {
            Event::ProviderResponseUpdatedReported(updated) => {
                value.update_raw_projection = AllocationIdentity::of_slice(&updated.deltas);
                value.update_text_raw_projection = update_text_ptr(updated);
            }
            Event::ProviderResponseFinishedReported(finished) => {
                value.finished_raw_projection =
                    AllocationIdentity::of_slice(&finished.output_items);
                value.finished_text_raw_projection = output_text_ptr(&finished.output_items);
            }
            _ => return,
        }
        snapshot.set(value);
    });
}

/// Record the owned update payload at the canonical derivation boundary.
pub(super) fn observe_owned_update(updated: &tau_proto::ProviderResponseUpdated) {
    SNAPSHOT.with(|snapshot| {
        let mut value = snapshot.get();
        value.update_canonical = AllocationIdentity::of_slice(&updated.deltas);
        value.update_text_canonical = update_text_ptr(updated);
        value.update_handoffs = value.update_handoffs.saturating_add(1);
        snapshot.set(value);
    });
}

/// Record the owned terminal payload at the canonical derivation boundary.
pub(super) fn observe_owned_finished(finished: &tau_proto::ProviderResponseFinished) {
    SNAPSHOT.with(|snapshot| {
        let mut value = snapshot.get();
        value.finished_canonical = AllocationIdentity::of_slice(&finished.output_items);
        value.finished_text_canonical = output_text_ptr(&finished.output_items);
        value.finished_handoffs = value.finished_handoffs.saturating_add(1);
        snapshot.set(value);
    });
}

/// Reset and return the current test-thread ownership observations.
pub(super) fn take_snapshot() -> ProviderReportOwnershipSnapshot {
    SNAPSHOT.with(|snapshot| snapshot.replace(ProviderReportOwnershipSnapshot::default()))
}

fn update_text_ptr(updated: &tau_proto::ProviderResponseUpdated) -> Option<AllocationIdentity> {
    updated.deltas.first().and_then(|delta| match delta {
        ProviderResponseTextDelta::Message { text, .. }
        | ProviderResponseTextDelta::ReasoningText { text, .. } => AllocationIdentity::of_str(text),
    })
}

fn output_text_ptr(items: &[ContextItem]) -> Option<AllocationIdentity> {
    items
        .iter()
        .find_map(|item| {
            let ContextItem::Message(message) = item else {
                return None;
            };
            message.content.first().map(|part| match part {
                tau_proto::ContentPart::Text { text }
                | tau_proto::ContentPart::SyntheticCompactionSummary { text }
                | tau_proto::ContentPart::HarnessInternalText { text } => text.as_str(),
                tau_proto::ContentPart::UrlCitation { .. }
                | tau_proto::ContentPart::CitationMetadataInvalid => "",
            })
        })
        .and_then(AllocationIdentity::of_str)
}
