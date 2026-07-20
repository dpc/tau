//! Opt-in content-free UI protocol diagnostics.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use tau_proto::{
    AgentId, AgentStatsUpdated, Event, EventName, HarnessOutputMessage,
    HarnessProviderQuotaChanged, ProviderName,
};

use super::{
    ProtocolIoFrameStats, message_len, record_frame_bounded, sorted_protocol_io_frame_stats,
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
    EncodedFrame,
    /// Bare event without its delivery envelope.
    FullEvent,
    /// Raw successful tool result field.
    RawResult,
    /// Non-empty provider-visible tool content.
    ProviderContent,
    /// Complete frame for a result with no provider content.
    ProviderContentMissingFrame,
    /// Complete frame for a result carrying provider content.
    ProviderContentPresentFrame,
    /// Present tool display state.
    Display,
    /// Complete frame for a result with no display state.
    DisplayMissingFrame,
    /// Authoritative typed final-response output projection.
    SemanticOutputProjection,
    /// Sum of independently encoded provider replay sidecars.
    ProviderReplaySidecarsSummed,
    /// Final-response event with output items removed.
    MetadataOnlyEvent,
    /// Complete observed stats or quota frame.
    ObservedFrame,
    /// Exact stats repeat within one loaded-agent epoch and traffic scope.
    ExactDuplicateWithinLoadedEpochFrame,
    /// Changed stats snapshot within one loaded-agent epoch and traffic scope.
    ChangedWithinLoadedEpochFrame,
    /// First stats snapshot in one loaded-agent epoch and traffic scope.
    InitialLoadedEpochFrame,
    /// Exact quota snapshot repeat within one traffic scope.
    ExactDuplicateFrame,
    /// Quota snapshot differing only by sequence.
    SequenceOnlyChangeFrame,
    /// Quota snapshot with a substantive state change.
    SubstantiveChangeFrame,
    /// First quota snapshot for one provider and traffic scope.
    InitialSnapshotFrame,
}

impl MeasurementKind {
    fn label(self) -> &'static str {
        match self {
            Self::EncodedFrame => "encoded-frame",
            Self::FullEvent => "full-event",
            Self::RawResult => "raw-result",
            Self::ProviderContent => "provider-content",
            Self::ProviderContentMissingFrame => "provider-content-missing-frame",
            Self::ProviderContentPresentFrame => "provider-content-present-frame",
            Self::Display => "display",
            Self::DisplayMissingFrame => "display-missing-frame",
            Self::SemanticOutputProjection => "semantic-output-projection",
            Self::ProviderReplaySidecarsSummed => "provider-replay-sidecars-summed",
            Self::MetadataOnlyEvent => "metadata-only-event",
            Self::ObservedFrame => "observed-frame",
            Self::ExactDuplicateWithinLoadedEpochFrame => {
                "exact-duplicate-within-loaded-epoch-frame"
            }
            Self::ChangedWithinLoadedEpochFrame => "changed-within-loaded-epoch-frame",
            Self::InitialLoadedEpochFrame => "initial-loaded-epoch-frame",
            Self::ExactDuplicateFrame => "exact-duplicate-frame",
            Self::SequenceOnlyChangeFrame => "sequence-only-change-frame",
            Self::SubstantiveChangeFrame => "substantive-change-frame",
            Self::InitialSnapshotFrame => "initial-snapshot-frame",
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
            if seen >= rank {
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
                        MeasurementKind::ExactDuplicateWithinLoadedEpochFrame
                    }
                    Some(_) => MeasurementKind::ChangedWithinLoadedEpochFrame,
                    None => MeasurementKind::InitialLoadedEpochFrame,
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
                    Some(previous) if previous == quota => MeasurementKind::ExactDuplicateFrame,
                    Some(previous) if same_quota_except_sequence(previous, quota) => {
                        MeasurementKind::SequenceOnlyChangeFrame
                    }
                    Some(_) => MeasurementKind::SubstantiveChangeFrame,
                    None => MeasurementKind::InitialSnapshotFrame,
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

/// Encode selected event components outside the meter mutex.
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
        Event::ToolResult(result) | Event::ProviderToolResult(result) => {
            measurements.push(sample(MeasurementKind::EncodedFrame, frame_bytes));
            push_value(&mut measurements, MeasurementKind::FullEvent, event);
            push_value(
                &mut measurements,
                MeasurementKind::RawResult,
                &result.result,
            );
            if result.provider_content.is_empty() {
                measurements.push(sample(
                    MeasurementKind::ProviderContentMissingFrame,
                    frame_bytes,
                ));
            } else {
                push_value(
                    &mut measurements,
                    MeasurementKind::ProviderContent,
                    &result.provider_content,
                );
                measurements.push(sample(
                    MeasurementKind::ProviderContentPresentFrame,
                    frame_bytes,
                ));
            }
            if let Some(display) = &result.display {
                push_value(&mut measurements, MeasurementKind::Display, display);
            } else {
                measurements.push(sample(MeasurementKind::DisplayMissingFrame, frame_bytes));
            }
        }
        Event::ToolBackgroundResult(result) => {
            measurements.push(sample(MeasurementKind::EncodedFrame, frame_bytes));
            push_value(&mut measurements, MeasurementKind::FullEvent, event);
            push_value(
                &mut measurements,
                MeasurementKind::RawResult,
                &result.result,
            );
            if let Some(display) = &result.display {
                push_value(&mut measurements, MeasurementKind::Display, display);
            } else {
                measurements.push(sample(MeasurementKind::DisplayMissingFrame, frame_bytes));
            }
        }
        Event::ProviderResponseFinished(response) => {
            push_value(&mut measurements, MeasurementKind::FullEvent, event);
            push_value(
                &mut measurements,
                MeasurementKind::SemanticOutputProjection,
                &semantic_output_projection(&response.output_items),
            );
            measurements.push(sample(
                MeasurementKind::ProviderReplaySidecarsSummed,
                provider_replay_sidecar_bytes(&response.output_items),
            ));
            let mut metadata_only = response.clone();
            metadata_only.output_items.clear();
            push_value(
                &mut measurements,
                MeasurementKind::MetadataOnlyEvent,
                &Event::ProviderResponseFinished(metadata_only),
            );
        }
        Event::AgentStatsUpdated(_) | Event::HarnessProviderQuotaChanged(_) => {
            measurements.push(sample(MeasurementKind::ObservedFrame, frame_bytes));
        }
        _ => {}
    }
    measurements
}

fn sample(kind: MeasurementKind, bytes: u64) -> MeasurementSample {
    MeasurementSample { kind, bytes }
}

fn push_value(
    measurements: &mut Vec<MeasurementSample>,
    kind: MeasurementKind,
    value: &impl serde::Serialize,
) {
    if let Some(bytes) = message_len(value) {
        measurements.push(sample(kind, bytes));
    }
}

fn semantic_output_projection(items: &[tau_proto::ContextItem]) -> Vec<tau_proto::ContextItem> {
    items
        .iter()
        .filter_map(|item| match item {
            tau_proto::ContextItem::Message(message) => {
                let mut message = message.clone();
                message.responses_raw_json = None;
                Some(tau_proto::ContextItem::Message(message))
            }
            tau_proto::ContextItem::ToolCall(call) => {
                let mut call = call.clone();
                call.raw_arguments_json = None;
                call.responses_envelope = None;
                Some(tau_proto::ContextItem::ToolCall(call))
            }
            tau_proto::ContextItem::ToolResult(result) => {
                Some(tau_proto::ContextItem::ToolResult(result.clone()))
            }
            tau_proto::ContextItem::ReasoningText(reasoning) => {
                Some(tau_proto::ContextItem::ReasoningText(reasoning.clone()))
            }
            tau_proto::ContextItem::CompactionTrigger | tau_proto::ContextItem::Compaction(_) => {
                Some(tau_proto::ContextItem::CompactionTrigger)
            }
            tau_proto::ContextItem::Reasoning(_)
            | tau_proto::ContextItem::UnknownProviderItem(_) => None,
        })
        .collect()
}

fn provider_replay_sidecar_bytes(items: &[tau_proto::ContextItem]) -> u64 {
    items
        .iter()
        .map(|item| match item {
            tau_proto::ContextItem::Message(message) => message
                .responses_raw_json
                .as_ref()
                .and_then(message_len)
                .unwrap_or_default(),
            tau_proto::ContextItem::ToolCall(call) => {
                call.raw_arguments_json
                    .as_ref()
                    .and_then(message_len)
                    .unwrap_or_default()
                    + call
                        .responses_envelope
                        .as_ref()
                        .and_then(message_len)
                        .unwrap_or_default()
            }
            tau_proto::ContextItem::Reasoning(opaque)
            | tau_proto::ContextItem::Compaction(opaque)
            | tau_proto::ContextItem::UnknownProviderItem(opaque) => {
                message_len(opaque).unwrap_or_default()
            }
            tau_proto::ContextItem::ToolResult(_)
            | tau_proto::ContextItem::ReasoningText(_)
            | tau_proto::ContextItem::CompactionTrigger => 0,
        })
        .sum()
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
