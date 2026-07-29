//! Opt-in content-free UI protocol diagnostics.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use tau_proto::{
    AgentId, AgentStatsUpdated, Event, EventName, HarnessOutputMessage,
    HarnessProviderQuotaChanged, ProviderName,
};

use super::{
    ProtocolIoFrameStats, record_frame_bounded, sorted_protocol_io_frame_stats,
    total_protocol_io_frame_stats,
};

const SIZE_BUCKETS: usize = 65;

/// Replay classification from the event-delivery envelope.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum DeliveryKind {
    /// Delivery whose protocol replay marker is true.
    Replay,
    /// Direct or committed delivery whose protocol replay marker is false.
    NonReplay,
}

impl DeliveryKind {
    fn label(self) -> &'static str {
        match self {
            Self::Replay => "replay",
            Self::NonReplay => "non-replay",
        }
    }
}

/// Connection phase relative to the initial session catch-up boundary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum AttachPhase {
    /// Initial connection traffic through `session.replay_complete`.
    ColdAttach,
    /// Connection traffic after the initial replay boundary.
    Steady,
}

impl AttachPhase {
    fn label(self) -> &'static str {
        match self {
            Self::ColdAttach => "cold-attach",
            Self::Steady => "steady",
        }
    }
}

/// Orthogonal attach/replay scope for frame and equality accounting.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DeliveryScope {
    /// Connection phase at delivery time.
    attach: AttachPhase,
    /// Replay marker classification for the delivery.
    delivery: DeliveryKind,
}

/// Closed set of selected payload and equality measurements.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MeasurementKind {
    /// Complete encoded harness-to-peer frame.
    Encoded,
    /// Complete frame for a result with no provider content.
    ProviderContentMissing,
    /// Complete frame for a result carrying provider content.
    ProviderContentPresent,
    /// Complete frame for a result carrying display state.
    DisplayPresent,
    /// Complete frame for a result with no display state.
    DisplayMissing,
    /// Complete observed stats or quota frame.
    Observed,
    /// Exact stats repeat within one loaded-agent epoch and traffic scope.
    ExactDuplicateWithinLoadedEpoch,
    /// Changed stats snapshot within one loaded-agent epoch and traffic scope.
    ChangedWithinLoadedEpoch,
    /// First stats snapshot in one loaded-agent epoch and traffic scope.
    InitialLoadedEpoch,
    /// Exact quota snapshot repeat within one traffic scope.
    ExactDuplicate,
    /// Quota snapshot differing only by sequence.
    SequenceOnlyChange,
    /// Quota snapshot with a substantive state change.
    SubstantiveChange,
    /// First quota snapshot for one provider and traffic scope.
    InitialSnapshot,
}

impl MeasurementKind {
    fn label(self) -> &'static str {
        match self {
            Self::Encoded => "encoded-frame",
            Self::ProviderContentMissing => "provider-content-missing-frame",
            Self::ProviderContentPresent => "provider-content-present-frame",
            Self::DisplayPresent => "display-present-frame",
            Self::DisplayMissing => "display-missing-frame",
            Self::Observed => "observed-frame",
            Self::ExactDuplicateWithinLoadedEpoch => "exact-duplicate-within-loaded-epoch-frame",
            Self::ChangedWithinLoadedEpoch => "changed-within-loaded-epoch-frame",
            Self::InitialLoadedEpoch => "initial-loaded-epoch-frame",
            Self::ExactDuplicate => "exact-duplicate-frame",
            Self::SequenceOnlyChange => "sequence-only-change-frame",
            Self::SubstantiveChange => "substantive-change-frame",
            Self::InitialSnapshot => "initial-snapshot-frame",
        }
    }
}

/// Typed identity for one selected event measurement.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MeasurementKey {
    /// Orthogonal attach/replay dimensions.
    scope: DeliveryScope,
    /// Typed event family being measured.
    event_name: EventName,
    /// Selected component or equality classification.
    kind: MeasurementKind,
}

/// Typed measurement storage.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Measurements(
    /// Distribution indexed by typed event/component identity.
    BTreeMap<MeasurementKey, SizeDistribution>,
);

impl Measurements {
    fn entry(
        &mut self,
        key: MeasurementKey,
    ) -> std::collections::btree_map::Entry<'_, MeasurementKey, SizeDistribution> {
        self.0.entry(key)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&MeasurementKey, &SizeDistribution)> {
        self.0.iter()
    }
}

impl MeasurementKey {
    fn label(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            self.scope.attach.label(),
            self.scope.delivery.label(),
            self.event_name,
            self.kind.label()
        )
    }
}

/// One pre-encoded measurement collected outside the meter mutex.
pub(super) struct MeasurementSample {
    /// Closed measurement identity.
    kind: MeasurementKind,
    /// Already-encoded byte size.
    bytes: u64,
}

/// Fixed logarithmic histogram.
///
/// Bucket zero contains exact zeroes. Bucket `n > 0` contains values whose
/// inclusive upper bound is `2^n - 1`. Count, sum, and maximum remain exact.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SizeDistribution {
    /// Number of samples recorded.
    count: u64,
    /// Exact sum of encoded sample bytes.
    bytes: u64,
    /// Exact largest encoded sample.
    max_bytes: u64,
    /// Fixed logarithmic buckets; index semantics are documented above.
    buckets: [u64; SIZE_BUCKETS],
}

impl Default for SizeDistribution {
    fn default() -> Self {
        Self {
            count: 0,
            bytes: 0,
            max_bytes: 0,
            buckets: [0; SIZE_BUCKETS],
        }
    }
}

impl SizeDistribution {
    /// Add one exact size to the fixed histogram.
    fn record_bytes(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        self.max_bytes = self.max_bytes.max(bytes);
        let bucket = if bytes == 0 {
            0
        } else {
            bytes.ilog2() as usize + 1
        };
        self.buckets[bucket] += 1;
    }

    /// Return the inclusive logarithmic bucket bound for one percentile.
    fn percentile_upper_bound(&self, percent: u8) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let rank = self
            .count
            .saturating_mul(u64::from(percent.min(100)))
            .div_ceil(100)
            .max(1);
        let mut seen = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if rank <= seen {
                return size_bucket_upper_bound(index);
            }
        }
        self.max_bytes
    }
}

/// Cumulative diagnostic aggregates owned and mutated by [`State`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Stats {
    /// Exact frame totals by attach phase, replay kind, and event name.
    downlink: BTreeMap<AttachPhase, BTreeMap<DeliveryKind, BTreeMap<String, ProtocolIoFrameStats>>>,
    /// Selected component and equality distributions.
    measurements: Measurements,
}

impl Stats {
    /// Return true when the opt-in collector has not observed a delivery.
    fn is_empty(&self) -> bool {
        self.downlink.is_empty() && self.measurements.is_empty()
    }
}

/// Equality key scoped to one loaded agent and both traffic dimensions.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AgentComparisonKey {
    /// Attach/replay scope isolated from other comparison streams.
    scope: DeliveryScope,
    /// Loaded agent whose latest snapshot is cached.
    agent_id: AgentId,
}

/// Equality key scoped to one provider and both traffic dimensions.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QuotaComparisonKey {
    /// Attach/replay scope isolated from other comparison streams.
    scope: DeliveryScope,
    /// Provider whose latest snapshot is cached.
    provider: ProviderName,
}

/// Mutable opt-in diagnostics owner.
///
/// `attach_phase` advances only after recording `session.replay_complete`, so
/// the boundary itself remains cold-attach/non-replay. Equality caches include
/// both traffic dimensions and reset on their observed lifecycle boundaries.
pub(super) struct State {
    /// Cumulative content-free measurements exposed to the formatter.
    stats: Stats,
    /// Current connection phase, advanced after the initial replay boundary.
    attach_phase: AttachPhase,
    /// Last stats snapshot within each traffic scope and loaded-agent epoch.
    last_agent_stats: BTreeMap<AgentComparisonKey, AgentStatsUpdated>,
    /// Last quota snapshot within each traffic scope and provider.
    last_provider_quota: BTreeMap<QuotaComparisonKey, HarnessProviderQuotaChanged>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            stats: Stats::default(),
            attach_phase: AttachPhase::ColdAttach,
            last_agent_stats: BTreeMap::new(),
            last_provider_quota: BTreeMap::new(),
        }
    }
}

impl State {
    /// Record one already-encoded delivery and advance/reset diagnostic state.
    pub(super) fn record_delivery(
        &mut self,
        delivery_kind: DeliveryKind,
        event: &Event,
        event_key: &str,
        frame_bytes: u64,
        measurements: Vec<MeasurementSample>,
    ) {
        let scope = DeliveryScope {
            attach: self.attach_phase,
            delivery: delivery_kind,
        };
        record_frame_bounded(
            self.stats
                .downlink
                .entry(scope.attach)
                .or_default()
                .entry(scope.delivery)
                .or_default(),
            event_key,
            frame_bytes,
        );
        for sample in measurements {
            self.record_measurement(scope, event.name(), sample.kind, sample.bytes);
        }
        self.classify_equality(scope, event, frame_bytes);
        self.reset_lifecycle_caches(event);
        if matches!(event, Event::SessionReplayComplete(_)) {
            self.attach_phase = AttachPhase::Steady;
        }
    }

    /// Format the current content-free diagnostic snapshot.
    pub(super) fn format(&self) -> String {
        format_stats(&self.stats)
    }

    fn classify_equality(&mut self, scope: DeliveryScope, event: &Event, frame_bytes: u64) {
        match event {
            Event::AgentStatsUpdated(stats) => {
                let key = AgentComparisonKey {
                    scope,
                    agent_id: stats.agent_id.clone(),
                };
                let kind = match self.last_agent_stats.get(&key) {
                    Some(previous) if previous == stats => {
                        MeasurementKind::ExactDuplicateWithinLoadedEpoch
                    }
                    Some(_) => MeasurementKind::ChangedWithinLoadedEpoch,
                    None => MeasurementKind::InitialLoadedEpoch,
                };
                self.record_measurement(scope, event.name(), kind, frame_bytes);
                self.last_agent_stats.insert(key, stats.clone());
            }
            Event::HarnessProviderQuotaChanged(quota) => {
                let key = QuotaComparisonKey {
                    scope,
                    provider: quota.provider.clone(),
                };
                let kind = match self.last_provider_quota.get(&key) {
                    Some(previous) if previous == quota => MeasurementKind::ExactDuplicate,
                    Some(previous) if same_quota_except_sequence(previous, quota) => {
                        MeasurementKind::SequenceOnlyChange
                    }
                    Some(_) => MeasurementKind::SubstantiveChange,
                    None => MeasurementKind::InitialSnapshot,
                };
                self.record_measurement(scope, event.name(), kind, frame_bytes);
                self.last_provider_quota.insert(key, quota.clone());
            }
            _ => {}
        }
    }

    fn reset_lifecycle_caches(&mut self, event: &Event) {
        match event {
            Event::SessionAgentLoaded(loaded) => self
                .last_agent_stats
                .retain(|key, _| key.agent_id != loaded.agent_id),
            Event::SessionAgentUnloaded(unloaded) => self
                .last_agent_stats
                .retain(|key, _| key.agent_id != unloaded.agent_id),
            Event::SessionStarted(_) => {
                self.last_agent_stats.clear();
                self.last_provider_quota.clear();
            }
            _ => {}
        }
    }

    fn record_measurement(
        &mut self,
        scope: DeliveryScope,
        event_name: EventName,
        kind: MeasurementKind,
        bytes: u64,
    ) {
        self.stats
            .measurements
            .entry(MeasurementKey {
                scope,
                event_name,
                kind,
            })
            .or_default()
            .record_bytes(bytes);
    }
}

/// Classify selected events using only the already-observed frame size.
pub(super) fn collect_measurements(
    message: &HarnessOutputMessage,
    frame_bytes: u64,
) -> Vec<MeasurementSample> {
    let HarnessOutputMessage::Deliver(delivery) = message else {
        return Vec::new();
    };
    let event = delivery.event();
    let mut measurements = Vec::new();
    match event {
        Event::ToolResultDisplay(result) => {
            measurements.push(sample(MeasurementKind::Encoded, frame_bytes));
            if result.display.is_some() {
                measurements.push(sample(MeasurementKind::DisplayPresent, frame_bytes));
            } else {
                measurements.push(sample(MeasurementKind::DisplayMissing, frame_bytes));
            }
        }
        Event::ToolResult(result) | Event::ProviderToolResult(result) => {
            measurements.push(sample(MeasurementKind::Encoded, frame_bytes));
            measurements.push(sample(
                if result.provider_content.is_empty() {
                    MeasurementKind::ProviderContentMissing
                } else {
                    MeasurementKind::ProviderContentPresent
                },
                frame_bytes,
            ));
            measurements.push(sample(
                if result.display.is_some() {
                    MeasurementKind::DisplayPresent
                } else {
                    MeasurementKind::DisplayMissing
                },
                frame_bytes,
            ));
        }
        Event::ToolBackgroundResultDisplay(result) => {
            measurements.push(sample(MeasurementKind::Encoded, frame_bytes));
            measurements.push(sample(
                if result.display.is_some() {
                    MeasurementKind::DisplayPresent
                } else {
                    MeasurementKind::DisplayMissing
                },
                frame_bytes,
            ));
        }
        Event::ToolBackgroundResult(result) => {
            measurements.push(sample(MeasurementKind::Encoded, frame_bytes));
            measurements.push(sample(
                if result.display.is_some() {
                    MeasurementKind::DisplayPresent
                } else {
                    MeasurementKind::DisplayMissing
                },
                frame_bytes,
            ));
        }
        Event::AgentStatsUpdated(_) | Event::HarnessProviderQuotaChanged(_) => {
            measurements.push(sample(MeasurementKind::Observed, frame_bytes));
        }
        _ => {}
    }
    measurements
}

fn sample(kind: MeasurementKind, bytes: u64) -> MeasurementSample {
    MeasurementSample { kind, bytes }
}

fn same_quota_except_sequence(
    previous: &HarnessProviderQuotaChanged,
    current: &HarnessProviderQuotaChanged,
) -> bool {
    let HarnessProviderQuotaChanged {
        provider: previous_provider,
        profile_epoch: previous_epoch,
        sequence: _,
        windows: previous_windows,
        route_bindings: previous_bindings,
    } = previous;
    let HarnessProviderQuotaChanged {
        provider: current_provider,
        profile_epoch: current_epoch,
        sequence: _,
        windows: current_windows,
        route_bindings: current_bindings,
    } = current;
    previous_provider == current_provider
        && previous_epoch == current_epoch
        && previous_windows == current_windows
        && previous_bindings == current_bindings
}

fn size_bucket_upper_bound(index: usize) -> u64 {
    match index {
        0 => 0,
        index if index >= u64::BITS as usize => u64::MAX,
        index => (1_u64 << index) - 1,
    }
}

fn format_stats(stats: &Stats) -> String {
    if stats.is_empty() {
        return String::new();
    }
    let mut lines =
        vec!["Downlink attach phase x delivery kind (exact encoded frame bytes)".to_owned()];
    for attach in [AttachPhase::ColdAttach, AttachPhase::Steady] {
        for delivery in [DeliveryKind::Replay, DeliveryKind::NonReplay] {
            let entries = stats
                .downlink
                .get(&attach)
                .and_then(|by_kind| by_kind.get(&delivery));
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: unwrap-or-default
            let total = entries
                .map(total_protocol_io_frame_stats)
                .unwrap_or_default();
            lines.push(format!(
                "{}.{}: bytes={} count={}",
                attach.label(),
                delivery.label(),
                total.bytes,
                total.count
            ));
            if let Some(entries) = entries {
                lines.extend(sorted_protocol_io_frame_stats(entries).into_iter().map(
                    |(name, frame)| {
                        format!("  {name}: bytes={} count={}", frame.bytes, frame.count)
                    },
                ));
            }
        }
    }
    if !stats.measurements.is_empty() {
        lines.push(
            "Selected payload attribution (CBOR sizes only; percentile upper bounds)".to_owned(),
        );
        for (key, distribution) in stats.measurements.iter() {
            lines.push(format!(
                "  {}: bytes={} count={} p50<={} p95<={} p99<={} max={}",
                key.label(),
                distribution.bytes,
                distribution.count,
                distribution.percentile_upper_bound(50),
                distribution.percentile_upper_bound(95),
                distribution.percentile_upper_bound(99),
                distribution.max_bytes,
            ));
        }
    }
    lines.join("\n")
}
