//! Protocol frame byte/count accounting shared by Tau clients.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tau_proto::{HarnessInputMessage, HarnessOutputMessage};

/// Number of one-second samples retained for rolling protocol-I/O status.
pub const PROTOCOL_IO_SAMPLE_WINDOW_SECS: usize = 30;

/// Maximum retained keys per direction for one meter, including the overflow
/// bucket.
pub const PROTOCOL_IO_MAX_KEYS_PER_DIRECTION: usize = 128;

/// Bucket used after one direction reaches
/// [`PROTOCOL_IO_MAX_KEYS_PER_DIRECTION`].
pub const PROTOCOL_IO_OVERFLOW_KEY: &str = "other";

/// Direction of one protocol frame relative to the harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolIoDirection {
    /// A peer-to-harness frame.
    Uplink,
    /// A harness-to-peer frame.
    Downlink,
}

/// Cumulative protocol payload totals grouped by direction and message/event
/// key.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProtocolIoCumulativeStats {
    /// Bytes and counts for peer-to-harness frames.
    pub uplink: BTreeMap<String, ProtocolIoFrameStats>,
    /// Bytes and counts for harness-to-peer frames.
    pub downlink: BTreeMap<String, ProtocolIoFrameStats>,
}

/// Cumulative payload accounting for one protocol message or delivered event
/// key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProtocolIoFrameStats {
    /// Number of frames observed for this key.
    pub count: u64,
    /// Encoded bytes observed for this key.
    pub bytes: u64,
}

impl ProtocolIoFrameStats {
    /// Add one observed frame of `bytes` encoded bytes.
    pub fn record_bytes(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

#[derive(Clone, Default)]
struct ProtocolIoBuckets {
    uplink: BTreeMap<String, u64>,
    downlink: BTreeMap<String, u64>,
}

#[derive(Default)]
struct ProtocolIoState {
    buckets: ProtocolIoBuckets,
    cumulative: ProtocolIoCumulativeStats,
}

/// Shared protocol frame meter.
///
/// The meter records frames that already cross an existing transport. It does
/// not subscribe to events, does not inspect replay policy, and does not affect
/// dispatch; callers choose where in their read/write path a successful frame
/// is counted.
#[derive(Clone, Default)]
pub struct ProtocolIoMeter {
    state: Arc<Mutex<ProtocolIoState>>,
}

impl ProtocolIoMeter {
    /// Record a peer-to-harness input frame, grouped by event name for `Emit`
    /// messages and by protocol message variant for all other frames.
    pub fn record_uplink_frame(&self, message: &HarnessInputMessage) {
        self.record_bytes(
            ProtocolIoDirection::Uplink,
            input_message_key(message),
            message_len(message),
        );
    }

    /// Record a harness-to-peer output frame, grouped by event name for
    /// delivered events and by protocol message variant for all other frames.
    pub fn record_downlink_frame(&self, message: &HarnessOutputMessage) {
        self.record_bytes(
            ProtocolIoDirection::Downlink,
            output_message_key(message),
            message_len(message),
        );
    }

    /// Record an already-classified frame.
    ///
    /// This is useful for tests and for callers that have a custom grouping key
    /// but still want the same cumulative/sample accounting.
    pub fn record_bytes(&self, direction: ProtocolIoDirection, key: String, bytes: Option<u64>) {
        let Some(bytes) = bytes else {
            return;
        };
        let mut state = self.state.lock().expect("protocol io meter mutex");
        let (bucket_entries, cumulative_entries): (
            &mut BTreeMap<String, u64>,
            &mut BTreeMap<String, ProtocolIoFrameStats>,
        ) = match direction {
            ProtocolIoDirection::Uplink => {
                let ProtocolIoState {
                    buckets,
                    cumulative,
                } = &mut *state;
                (&mut buckets.uplink, &mut cumulative.uplink)
            }
            ProtocolIoDirection::Downlink => {
                let ProtocolIoState {
                    buckets,
                    cumulative,
                } = &mut *state;
                (&mut buckets.downlink, &mut cumulative.downlink)
            }
        };
        record_bytes_bounded(bucket_entries, &key, bytes);
        record_frame_bounded(cumulative_entries, &key, bytes);
    }

    /// Drain the current one-second sample buckets without clearing cumulative
    /// counters.
    pub fn take_sample(&self) -> ProtocolIoSample {
        let buckets =
            std::mem::take(&mut self.state.lock().expect("protocol io meter mutex").buckets);
        ProtocolIoSample::from_buckets(buckets)
    }

    /// Return lifetime counters for this meter.
    pub fn cumulative_stats(&self) -> ProtocolIoCumulativeStats {
        self.state
            .lock()
            .expect("protocol io meter mutex")
            .cumulative
            .clone()
    }
}

fn bounded_key<'a, T>(stats: &BTreeMap<String, T>, key: &'a str) -> &'a str {
    if stats.contains_key(key)
        || key == PROTOCOL_IO_OVERFLOW_KEY
        || stats.len() < PROTOCOL_IO_MAX_KEYS_PER_DIRECTION.saturating_sub(1)
    {
        key
    } else {
        PROTOCOL_IO_OVERFLOW_KEY
    }
}

fn record_bytes_bounded(stats: &mut BTreeMap<String, u64>, key: &str, bytes: u64) {
    let key = bounded_key(stats, key).to_owned();
    *stats.entry(key).or_insert(0) += bytes;
}

fn record_frame_bounded(stats: &mut BTreeMap<String, ProtocolIoFrameStats>, key: &str, bytes: u64) {
    let key = bounded_key(stats, key).to_owned();
    stats.entry(key).or_default().record_bytes(bytes);
}

/// One drained protocol-I/O sample bucket.
pub struct ProtocolIoSample {
    /// Total peer-to-harness bytes in this sample.
    pub uplink_bytes: u64,
    /// Total harness-to-peer bytes in this sample.
    pub downlink_bytes: u64,
    /// Peer-to-harness byte breakdown by message/event key.
    pub uplink_breakdown: BTreeMap<String, u64>,
    /// Harness-to-peer byte breakdown by message/event key.
    pub downlink_breakdown: BTreeMap<String, u64>,
}

impl ProtocolIoSample {
    fn from_buckets(buckets: ProtocolIoBuckets) -> Self {
        Self {
            uplink_bytes: buckets.uplink.values().sum(),
            downlink_bytes: buckets.downlink.values().sum(),
            uplink_breakdown: buckets.uplink,
            downlink_breakdown: buckets.downlink,
        }
    }

    /// Total `(uplink, downlink)` byte pair for rolling-rate displays.
    #[must_use]
    pub fn status_pair(&self) -> (u64, u64) {
        (self.uplink_bytes, self.downlink_bytes)
    }
}

/// Rolling one-second samples for protocol-I/O status displays.
pub struct ProtocolIoTracker {
    meter: ProtocolIoMeter,
    samples: VecDeque<(u64, u64)>,
    next_sample_at: Instant,
}

impl ProtocolIoTracker {
    /// Create a tracker over `meter`.
    #[must_use]
    pub fn new(meter: ProtocolIoMeter) -> Self {
        Self {
            meter,
            samples: VecDeque::with_capacity(PROTOCOL_IO_SAMPLE_WINDOW_SECS),
            next_sample_at: Instant::now() + Duration::from_secs(1),
        }
    }

    /// Duration until the next sample is due.
    #[must_use]
    pub fn recv_timeout(&self) -> Duration {
        self.next_sample_at
            .saturating_duration_since(Instant::now())
    }

    /// Take a sample if one is due.
    pub fn sample_if_due(&mut self) -> Option<ProtocolIoRollingStats> {
        let now = Instant::now();
        (self.next_sample_at <= now).then(|| self.sample_at(now))
    }

    /// Take a sample immediately.
    pub fn sample_now(&mut self) -> ProtocolIoRollingStats {
        self.sample_at(Instant::now())
    }

    fn sample_at(&mut self, now: Instant) -> ProtocolIoRollingStats {
        let sample = self.meter.take_sample();
        let status_pair = sample.status_pair();

        if self.samples.len() == PROTOCOL_IO_SAMPLE_WINDOW_SECS {
            self.samples.pop_front();
        }
        self.samples.push_back(status_pair);
        self.next_sample_at = now + Duration::from_secs(1);

        ProtocolIoRollingStats {
            uplink_max_bytes_per_sec: self
                .samples
                .iter()
                .map(|(uplink, _)| *uplink)
                .max()
                .unwrap_or_default(),
            downlink_max_bytes_per_sec: self
                .samples
                .iter()
                .map(|(_, downlink)| *downlink)
                .max()
                .unwrap_or_default(),
            sample,
        }
    }
}

/// Rolling status plus the just-drained sample.
pub struct ProtocolIoRollingStats {
    /// Maximum peer-to-harness bytes per second in the rolling window.
    pub uplink_max_bytes_per_sec: u64,
    /// Maximum harness-to-peer bytes per second in the rolling window.
    pub downlink_max_bytes_per_sec: u64,
    /// The one-second bucket that was just drained.
    pub sample: ProtocolIoSample,
}

/// Stable grouping key for one peer-to-harness frame.
#[must_use]
pub fn input_message_key(message: &HarnessInputMessage) -> String {
    if let HarnessInputMessage::Emit(emit) = message {
        return emit.event.name().to_string();
    }
    format!("message.{}", harness_input_message_name(message))
}

/// Stable grouping key for one harness-to-peer frame.
#[must_use]
pub fn output_message_key(message: &HarnessOutputMessage) -> String {
    match message {
        HarnessOutputMessage::Deliver(delivery) => delivery.event().name().to_string(),
        HarnessOutputMessage::Disconnect(_) => "message.disconnect".to_owned(),
        HarnessOutputMessage::Configure(_) => "message.configure".to_owned(),
        HarnessOutputMessage::InterceptRequest(_) => "message.intercept_request".to_owned(),
        HarnessOutputMessage::AgentPromptCreatedResult(_) => {
            "message.agent_prompt_created_result".to_owned()
        }
        HarnessOutputMessage::RenderedSystemPromptResult(_) => {
            "message.rendered_system_prompt_result".to_owned()
        }
        HarnessOutputMessage::RenderedPromptResult(_) => {
            "message.rendered_prompt_result".to_owned()
        }
        HarnessOutputMessage::RenderedToolDefinitionsResult(_) => {
            "message.rendered_tool_definitions_result".to_owned()
        }
        HarnessOutputMessage::ExtensionDataResult(_) => "message.extension_data_result".to_owned(),
        HarnessOutputMessage::ExternalAgentMessageResult(_) => {
            "message.external_agent_message_result".to_owned()
        }
        HarnessOutputMessage::ExternalAgentMessageAuthResult(_) => {
            "message.external_agent_message_auth_result".to_owned()
        }
    }
}

/// Protocol variant name for one peer-to-harness frame.
#[must_use]
pub fn harness_input_message_name(message: &HarnessInputMessage) -> &'static str {
    match message {
        HarnessInputMessage::Hello(_) => "hello",
        HarnessInputMessage::Subscribe(_) => "subscribe",
        HarnessInputMessage::Intercept(_) => "intercept",
        HarnessInputMessage::Ready(_) => "ready",
        HarnessInputMessage::Disconnect(_) => "disconnect",
        HarnessInputMessage::ConfigError(_) => "config_error",
        HarnessInputMessage::Emit(_) => "emit",
        HarnessInputMessage::InterceptReply(_) => "intercept_reply",
        HarnessInputMessage::GetAgentPromptCreated(_) => "get_agent_prompt_created",
        HarnessInputMessage::GetRenderedSystemPrompt(_) => "get_rendered_system_prompt",
        HarnessInputMessage::GetRenderedPrompt(_) => "get_rendered_prompt",
        HarnessInputMessage::GetRenderedToolDefinitions(_) => "get_rendered_tool_definitions",
        HarnessInputMessage::ExtensionDataRequest(_) => "extension_data_request",
        HarnessInputMessage::ExternalAgentMessage(_) => "external_agent_message",
        HarnessInputMessage::ExternalAgentMessageAuth(_) => "external_agent_message_auth",
    }
}

fn message_len<M: serde::Serialize>(message: &M) -> Option<u64> {
    tau_proto::encode_message_to_vec(message)
        .ok()
        .map(|bytes| bytes.len() as u64)
}

/// Format a per-event byte/count breakdown for logs.
#[must_use]
pub fn format_protocol_io_breakdown(breakdown: &BTreeMap<String, u64>) -> String {
    if breakdown.is_empty() {
        return "none".to_owned();
    }

    let mut entries = breakdown.iter().collect::<Vec<_>>();
    entries.sort_by(|(left_name, left_bytes), (right_name, right_bytes)| {
        right_bytes
            .cmp(left_bytes)
            .then_with(|| left_name.cmp(right_name))
    });
    entries
        .into_iter()
        .map(|(name, bytes)| format!("{name}={}", format_protocol_io_bytes(*bytes)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Format cumulative protocol-I/O stats for human-facing debug output.
#[must_use]
pub fn format_protocol_io_cumulative_stats(
    title: &str,
    uplink_label: &str,
    downlink_label: &str,
    empty_message: &str,
    stats: &ProtocolIoCumulativeStats,
) -> String {
    let uplink_total = total_protocol_io_frame_stats(&stats.uplink);
    let downlink_total = total_protocol_io_frame_stats(&stats.downlink);
    let mut lines = vec![
        title.to_owned(),
        format!(
            "{uplink_label}: {} in {} frame(s)",
            format_protocol_io_bytes(uplink_total.bytes),
            uplink_total.count
        ),
        format_protocol_io_stats_section(&stats.uplink),
        format!(
            "{downlink_label}: {} in {} frame(s)",
            format_protocol_io_bytes(downlink_total.bytes),
            downlink_total.count
        ),
        format_protocol_io_stats_section(&stats.downlink),
    ];
    if uplink_total.count == 0 && downlink_total.count == 0 {
        lines.push(empty_message.to_owned());
    }
    lines.join("\n")
}

/// Total all stats in one direction.
#[must_use]
pub fn total_protocol_io_frame_stats(
    stats: &BTreeMap<String, ProtocolIoFrameStats>,
) -> ProtocolIoFrameStats {
    stats
        .values()
        .fold(ProtocolIoFrameStats::default(), |mut total, stats| {
            total.count += stats.count;
            total.bytes += stats.bytes;
            total
        })
}

fn format_protocol_io_stats_section(stats: &BTreeMap<String, ProtocolIoFrameStats>) -> String {
    if stats.is_empty() {
        return "  (none)".to_owned();
    }
    sorted_protocol_io_frame_stats(stats)
        .into_iter()
        .map(|(name, stats)| {
            format!(
                "  {name}: {} count={}",
                format_protocol_io_bytes(stats.bytes),
                stats.count
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sort protocol-I/O stats by descending bytes, descending count, then name.
#[must_use]
pub fn sorted_protocol_io_frame_stats(
    stats: &BTreeMap<String, ProtocolIoFrameStats>,
) -> Vec<(&str, ProtocolIoFrameStats)> {
    let mut entries = stats
        .iter()
        .map(|(name, stats)| (name.as_str(), *stats))
        .collect::<Vec<_>>();
    entries.sort_by(|(left_name, left_stats), (right_name, right_stats)| {
        right_stats
            .bytes
            .cmp(&left_stats.bytes)
            .then_with(|| right_stats.count.cmp(&left_stats.count))
            .then_with(|| left_name.cmp(right_name))
    });
    entries
}

/// Format a byte count using the compact UI protocol-I/O style.
#[must_use]
pub fn format_protocol_io_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    if bytes < 1024 * 1024 {
        return format_protocol_io_scaled_bytes(bytes, 1024, "K");
    }
    format_protocol_io_scaled_bytes(bytes, 1024 * 1024, "M")
}

fn format_protocol_io_scaled_bytes(bytes: u64, divisor: u64, suffix: &str) -> String {
    let whole = bytes / divisor;
    let tenth = bytes % divisor * 10 / divisor;
    if whole < 10 && tenth != 0 {
        format!("{whole}.{tenth}{suffix}")
    } else {
        format!("{whole}{suffix}")
    }
}
