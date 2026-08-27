//! Opt-in, process-local decoded-delivery ownership measurements.

use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use tau_delivery_memory::DecodedMemoryEstimate;

/// Independently observable CLI ownership cuts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DeliveryMemoryCut {
    /// Socket reader's current typed message.
    DecodeCurrent,
    /// Cold-attach reconstruction state.
    ColdStaging,
    /// Bounded socket-to-renderer FIFO.
    RendererFifo,
    /// Scheduler lookahead or folded command.
    Scheduler,
    /// Current renderer handler input.
    Handler,
}

impl DeliveryMemoryCut {
    /// Number of finite independently observable cuts.
    const COUNT: usize = 5;
    /// Every finite independently observable cut.
    const ALL: [Self; Self::COUNT] = [
        Self::DecodeCurrent,
        Self::ColdStaging,
        Self::RendererFifo,
        Self::Scheduler,
        Self::Handler,
    ];

    /// Stable, content-free diagnostic label.
    const fn label(self) -> &'static str {
        match self {
            Self::DecodeCurrent => "decode_current",
            Self::ColdStaging => "cold_staging",
            Self::RendererFifo => "renderer_fifo",
            Self::Scheduler => "scheduler",
            Self::Handler => "handler",
        }
    }

    /// Stable index into bounded per-cut high-water arrays.
    const fn index(self) -> usize {
        self as usize
    }
}

/// One active allocation estimate and its current independent owner.
#[derive(Clone, Copy)]
struct ActiveEstimate {
    /// Current observable owner.
    cut: DeliveryMemoryCut,
    /// Recursive content-free estimate.
    estimate: DecodedMemoryEstimate,
}

/// Guarded process-local estimates keyed by a CLI-local delivery sequence.
///
/// The map is absent while the trace target is disabled. Entries live no longer
/// than decoded deliveries and do not leave this process or appear in output.
pub(super) struct DeliveryMemoryTracker {
    /// Lazily allocated active estimates.
    state: Mutex<Option<Box<TrackerState>>>,
    /// Focused-test override local to this tracker.
    #[cfg(test)]
    force_enabled: AtomicBool,
}

/// Enabled-only active estimates and bounded per-cut high-water values.
struct TrackerState {
    /// Active delivery estimates.
    active: HashMap<u64, ActiveEstimate>,
    /// Largest aggregate estimate observed at each cut.
    high_water: [DecodedMemoryEstimate; DeliveryMemoryCut::COUNT],
    /// Largest active item count observed at each cut.
    high_water_items: [u64; DeliveryMemoryCut::COUNT],
}

impl DeliveryMemoryTracker {
    /// Creates a disabled-by-default tracker with no allocated accounting
    /// state.
    pub(super) const fn new() -> Self {
        Self {
            state: Mutex::new(None),
            #[cfg(test)]
            force_enabled: AtomicBool::new(false),
        }
    }

    /// Recursively measures one current decoded message behind the trace guard.
    pub(super) fn observe_decode(
        &self,
        delivery_id: u64,
        message: &impl Serialize,
        encoded_bytes: tau_proto::ProtocolMessageBytes,
    ) {
        if !self.enabled() {
            return;
        }
        let Some(estimate) = DecodedMemoryEstimate::from_serializable(message, encoded_bytes)
        else {
            return;
        };
        let mut state = self.state.lock().expect("delivery-memory mutex poisoned");
        let state = state.get_or_insert_with(|| {
            Box::new(TrackerState {
                active: HashMap::new(),
                high_water: [DecodedMemoryEstimate::default(); DeliveryMemoryCut::COUNT],
                high_water_items: [0; DeliveryMemoryCut::COUNT],
            })
        });
        state.active.insert(
            delivery_id,
            ActiveEstimate {
                cut: DeliveryMemoryCut::DecodeCurrent,
                estimate,
            },
        );
        state.emit_snapshot();
    }

    /// Moves one estimate between independently observable owners.
    pub(super) fn transition(&self, delivery_id: u64, cut: DeliveryMemoryCut) {
        if !self.enabled() {
            return;
        }
        let mut state = self.state.lock().expect("delivery-memory mutex poisoned");
        let Some(state) = state.as_mut() else {
            return;
        };
        let Some(estimate) = state.active.get_mut(&delivery_id) else {
            return;
        };
        estimate.cut = cut;
        state.emit_snapshot();
    }

    /// Releases one decoded-delivery estimate after its final observable owner.
    pub(super) fn release(&self, delivery_id: u64) {
        if !self.enabled() {
            return;
        }
        let mut state = self.state.lock().expect("delivery-memory mutex poisoned");
        let Some(current) = state.as_mut() else {
            return;
        };
        current.active.remove(&delivery_id);
        current.emit_snapshot();
    }

    /// Returns whether recursive measurement was explicitly enabled.
    fn enabled(&self) -> bool {
        #[cfg(test)]
        if self.force_enabled.load(Ordering::Relaxed) {
            return true;
        }
        tracing::enabled!(target: "tau_cli::delivery_memory", tracing::Level::TRACE)
    }

    /// Enables this tracker for focused production-seam tests.
    #[cfg(test)]
    pub(super) fn force_enable_for_test(&self) {
        self.force_enabled.store(true, Ordering::Relaxed);
    }

    /// Returns one active cut for focused production-seam tests.
    #[cfg(test)]
    pub(super) fn cut_for_test(&self, delivery_id: u64) -> Option<DeliveryMemoryCut> {
        self.state
            .lock()
            .expect("delivery-memory mutex poisoned")
            .as_ref()
            .and_then(|state| state.active.get(&delivery_id))
            .map(|active| active.cut)
    }

    /// Returns the active item count for focused production-seam tests.
    #[cfg(test)]
    pub(super) fn active_len_for_test(&self) -> usize {
        self.state
            .lock()
            .expect("delivery-memory mutex poisoned")
            .as_ref()
            .map_or(0, |state| state.active.len())
    }
}

impl TrackerState {
    /// Emits bounded content-free aggregates, never active identities or
    /// content.
    fn emit_snapshot(&mut self) {
        for cut in DeliveryMemoryCut::ALL {
            let (items, estimate) = self.active.values().filter(|item| item.cut == cut).fold(
                (0_u64, DecodedMemoryEstimate::default()),
                |(items, total), item| {
                    (items.saturating_add(1), total.saturating_add(item.estimate))
                },
            );
            let index = cut.index();
            self.high_water[index].encoded_bytes = self.high_water[index]
                .encoded_bytes
                .max(estimate.encoded_bytes);
            self.high_water[index].logical_payload_bytes = self.high_water[index]
                .logical_payload_bytes
                .max(estimate.logical_payload_bytes);
            self.high_water[index].requested_capacity_estimate = self.high_water[index]
                .requested_capacity_estimate
                .max(estimate.requested_capacity_estimate);
            self.high_water[index].container_count = self.high_water[index]
                .container_count
                .max(estimate.container_count);
            self.high_water_items[index] = self.high_water_items[index].max(items);
            let high = self.high_water[index];
            tracing::trace!(
                target: "tau_cli::delivery_memory",
                process = "cli",
                cut = cut.label(),
                items,
                owners = u64::from(items != 0),
                encoded_bytes = estimate.encoded_bytes,
                decoded_logical_bytes_estimate = estimate.logical_payload_bytes,
                decoded_requested_capacity_estimate = estimate.requested_capacity_estimate,
                decoded_containers = estimate.container_count,
                expansion_milli = estimate.expansion_milli(),
                shared_allocations = items,
                shared_fanout = 0_u64,
                high_water_items = self.high_water_items[index],
                high_water_encoded_bytes = high.encoded_bytes,
                high_water_decoded_logical_bytes_estimate = high.logical_payload_bytes,
                high_water_decoded_requested_capacity_estimate = high.requested_capacity_estimate,
                kernel_bytes_observable = false,
                retained_projection_bytes_observable = false,
                "decoded delivery memory ownership"
            );
        }
    }
}

#[cfg(test)]
mod tests;
