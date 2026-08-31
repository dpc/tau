//! Bounded process-local correlation between opaque presentation facts and
//! redraws.

use std::collections::VecDeque;
use std::time::Instant;

use super::RendererDeliveryId;
use crate::presentation_mutation_generation::PresentationMutationGeneration;

/// Maximum number of exact selected-presentation facts retained for one redraw.
pub(super) const MAX_PENDING_PRESENTATION_OBSERVATIONS: usize = 64;
/// Number of caller-defined opaque invalidation keys represented by one mask.
const PRESENTATION_KIND_BITS: usize = u64::BITS as usize;

/// Validated opaque key used only for caller-owned presentation invalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationObservationKey(u8);

impl PresentationObservationKey {
    /// Creates a key representable by one invalidation mask.
    pub fn new(value: u8) -> Option<Self> {
        (usize::from(value) < PRESENTATION_KIND_BITS).then_some(Self(value))
    }
}

/// Named invalidation mask for opaque predecessor keys.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentationInvalidation(u64);

impl PresentationInvalidation {
    /// Creates an empty invalidation set.
    pub const fn none() -> Self {
        Self(0)
    }

    /// Adds one opaque predecessor key.
    pub const fn with(mut self, key: PresentationObservationKey) -> Self {
        self.0 |= 1_u64 << key.0;
        self
    }

    /// Returns whether this invalidation set contains one opaque key.
    const fn contains(self, key: PresentationObservationKey) -> bool {
        self.0 & (1_u64 << key.0) != 0
    }
}

/// One caller-owned opaque, content-free fact accepted by raw correlation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpaquePresentationFact {
    /// Stable content-free operational trace label.
    label: &'static str,
    /// Opaque key used only for invalidation.
    key: PresentationObservationKey,
    /// Opaque predecessors superseded atomically by this fact.
    invalidates: PresentationInvalidation,
}

impl OpaquePresentationFact {
    /// Creates one validated opaque fact.
    pub const fn new(
        label: &'static str,
        key: PresentationObservationKey,
        invalidates: PresentationInvalidation,
    ) -> Self {
        Self {
            label,
            key,
            invalidates,
        }
    }
}

/// One content-free process-local fact awaiting a successful terminal flush.
pub(super) struct PresentationObservation {
    /// Socket delivery identity scoped to this CLI process.
    pub(super) delivery_id: RendererDeliveryId,
    /// Caller-owned stable content-free label.
    pub(super) fact: &'static str,
    /// Caller-owned validated invalidation key.
    key: PresentationObservationKey,
    /// Monotonic selected-presentation generation assigned at registration.
    pub(super) generation: PresentationMutationGeneration,
    /// Monotonic time at which the selected handler completed its mutation.
    pub(super) observed_at: Instant,
}

/// Observations captured atomically with one prepared redraw.
pub(super) struct CapturedPresentationObservations {
    /// Exact retained observations in registration order.
    pub(super) facts: Vec<PresentationObservation>,
    /// Number of exact observations omitted by the fixed pending bound.
    pub(super) omitted: u64,
    /// Latest selected-presentation generation visible to the prepared frame.
    pub(super) generation: PresentationMutationGeneration,
}

/// Count-only overflow retained for one caller-owned opaque key.
struct OmittedPresentationObservations {
    /// Validated key shared by all omitted observations in this aggregate.
    key: PresentationObservationKey,
    /// Saturating number of omitted observations with this key.
    count: u64,
}

impl OmittedPresentationObservations {
    /// Creates the first omitted observation aggregate for one key.
    const fn new(key: PresentationObservationKey) -> Self {
        Self { key, count: 1 }
    }

    /// Returns whether this aggregate represents one opaque key.
    fn has_key(&self, key: PresentationObservationKey) -> bool {
        self.key == key
    }

    /// Returns whether an invalidation set supersedes this aggregate.
    const fn is_invalidated_by(&self, invalidates: PresentationInvalidation) -> bool {
        invalidates.contains(self.key)
    }

    /// Adds one omitted observation without overflowing the bounded diagnostic.
    fn increment(&mut self) {
        self.count = self.count.saturating_add(1);
    }
}

/// Coherent bounded opaque correlation state protected by `SharedState`.
pub(super) struct PresentationObservationState {
    /// Latest selected-presentation mutation generation.
    generation: PresentationMutationGeneration,
    /// Exact observations awaiting capture by a redraw pass.
    pending: VecDeque<PresentationObservation>,
    /// Count-only overflow retained independently for each opaque key.
    omitted_by_key: Vec<OmittedPresentationObservations>,
    /// Saturating total of all count-only overflow.
    omitted_total: u64,
    /// Successful pass receipts retained only for focused unit-test assertions.
    #[cfg(test)]
    pub(super) successful_test_passes:
        Vec<Vec<(RendererDeliveryId, PresentationMutationGeneration)>>,
}

impl PresentationObservationState {
    /// Creates empty process-local observation state without heap allocation.
    pub(super) fn new() -> Self {
        Self {
            generation: PresentationMutationGeneration::default(),
            pending: VecDeque::new(),
            omitted_by_key: Vec::new(),
            omitted_total: 0,
            #[cfg(test)]
            successful_test_passes: Vec::new(),
        }
    }

    /// Returns whether capture can skip all correlation work.
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.omitted_total == 0
    }

    /// Registers one caller-classified opaque presentation mutation.
    pub(super) fn register(
        &mut self,
        delivery_id: RendererDeliveryId,
        fact: OpaquePresentationFact,
        observed_at: Instant,
    ) {
        self.pending
            .retain(|pending| !fact.invalidates.contains(pending.key));
        self.omitted_by_key.retain(|omitted| {
            if omitted.is_invalidated_by(fact.invalidates) {
                self.omitted_total = self.omitted_total.saturating_sub(omitted.count);
                false
            } else {
                true
            }
        });
        self.generation.advance();
        if self.pending.len() == MAX_PENDING_PRESENTATION_OBSERVATIONS {
            if let Some(omitted) = self
                .omitted_by_key
                .iter_mut()
                .find(|omitted| omitted.has_key(fact.key))
            {
                omitted.increment();
            } else {
                self.omitted_by_key
                    .push(OmittedPresentationObservations::new(fact.key));
            }
            self.omitted_total = self.omitted_total.saturating_add(1);
            return;
        }
        self.pending.push_back(PresentationObservation {
            delivery_id,
            fact: fact.label,
            key: fact.key,
            generation: self.generation,
            observed_at,
        });
    }

    /// Captures and clears all observations represented by a prepared frame.
    pub(super) fn capture(&mut self) -> CapturedPresentationObservations {
        self.omitted_by_key.clear();
        CapturedPresentationObservations {
            facts: self.pending.drain(..).collect(),
            omitted: std::mem::take(&mut self.omitted_total),
            generation: self.generation,
        }
    }

    /// Retains one successful pass receipt for focused unit tests.
    #[cfg(test)]
    pub(super) fn record_success_for_test(
        &mut self,
        observations: &CapturedPresentationObservations,
    ) {
        self.successful_test_passes.push(
            observations
                .facts
                .iter()
                .map(|fact| (fact.delivery_id, fact.generation))
                .collect(),
        );
    }
}

#[cfg(test)]
#[path = "presentation_observation_state_tests.rs"]
mod presentation_observation_state_tests;
