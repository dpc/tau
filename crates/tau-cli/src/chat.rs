//! Interactive chat as a socket client of the harness daemon: input
//! loop, draft debouncer, and the threading glue that joins them.

#[cfg(test)]
mod recorded_line_routing_tests;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_config::settings::CliBindingAction;
use tau_harness::SessionLaunchStatus;
use tau_proto::{
    CborValue, Disconnect, Event, HarnessInputMessage, HarnessOutputMessage, PeerInputReader,
    PeerOutputWriter, UiFocusChanged, UiPromptDraft, UiPromptSubmitted, UnixMicros,
};

use crate::action_commands::ActionCommandState;
use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::event_renderer::{EventRenderer, ToolTimerNotifier, ToolTimerState, UiIoStats};
use crate::prompt_history::PromptHistoryStore;
use crate::tool_render::ui_dir_block;
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, create_user_agent_prompt,
};
use crate::{CliError, MUTEX_POISONED, build_banner, locked, ui_logging};

const UI_IO_SAMPLE_WINDOW_SECS: usize = 30;

#[derive(Clone, Default)]
struct UiIoMeter {
    state: Arc<Mutex<UiIoState>>,
}

impl UiIoMeter {
    fn record_uplink_frame(&self, message: &HarnessInputMessage) {
        self.record_bytes(
            UiIoDirection::Uplink,
            ui_io_input_message_key(message),
            ui_io_message_len(message),
        );
    }

    fn record_downlink_frame(&self, message: &HarnessOutputMessage) {
        self.record_bytes(
            UiIoDirection::Downlink,
            ui_io_output_message_key(message),
            ui_io_message_len(message),
        );
    }

    fn record_bytes(&self, direction: UiIoDirection, key: String, bytes: Option<u64>) {
        let Some(bytes) = bytes else {
            return;
        };
        let mut state = locked(&self.state);
        let (bucket_entries, cumulative_entries): (
            &mut BTreeMap<String, u64>,
            &mut BTreeMap<String, UiIoEventStats>,
        ) = match direction {
            UiIoDirection::Uplink => {
                let UiIoState {
                    buckets,
                    cumulative,
                } = &mut *state;
                (&mut buckets.uplink, &mut cumulative.uplink)
            }
            UiIoDirection::Downlink => {
                let UiIoState {
                    buckets,
                    cumulative,
                } = &mut *state;
                (&mut buckets.downlink, &mut cumulative.downlink)
            }
        };
        *bucket_entries.entry(key.clone()).or_insert(0) += bytes;
        cumulative_entries
            .entry(key)
            .or_default()
            .record_bytes(bytes);
    }

    fn take_sample(&self) -> UiIoSample {
        let buckets = std::mem::take(&mut locked(&self.state).buckets);
        UiIoSample::from_buckets(buckets)
    }

    fn cumulative_stats(&self) -> UiIoCumulativeStats {
        locked(&self.state).cumulative.clone()
    }
}

#[derive(Default)]
struct UiIoState {
    /// Resettable buckets drained once per second for `/set show-ui-io` status
    /// bar rates and threshold logging.
    buckets: UiIoBuckets,
    /// Lifetime counters for this UI client, exposed by
    /// `/debug-show-ui-event-stats` and reset when the client exits.
    cumulative: UiIoCumulativeStats,
}

#[derive(Clone, Default)]
struct UiIoBuckets {
    uplink: BTreeMap<String, u64>,
    downlink: BTreeMap<String, u64>,
}

/// Cumulative UI transport payload totals grouped by direction and event type.
#[derive(Clone, Default)]
struct UiIoCumulativeStats {
    /// Bytes and counts for messages sent from this UI to the harness.
    uplink: BTreeMap<String, UiIoEventStats>,
    /// Bytes and counts for messages received by this UI from the harness.
    downlink: BTreeMap<String, UiIoEventStats>,
}

/// Cumulative payload accounting for one UI message or delivered event type.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct UiIoEventStats {
    /// Number of UI frames observed for this type.
    count: u64,
    /// Encoded bytes observed for this type.
    bytes: u64,
}

impl UiIoEventStats {
    fn record_bytes(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }
}

struct UiIoSample {
    uplink_bytes: u64,
    downlink_bytes: u64,
    uplink_breakdown: BTreeMap<String, u64>,
    downlink_breakdown: BTreeMap<String, u64>,
}

impl UiIoSample {
    fn from_buckets(buckets: UiIoBuckets) -> Self {
        Self {
            uplink_bytes: buckets.uplink.values().sum(),
            downlink_bytes: buckets.downlink.values().sum(),
            uplink_breakdown: buckets.uplink,
            downlink_breakdown: buckets.downlink,
        }
    }

    fn status_pair(&self) -> (u64, u64) {
        (self.uplink_bytes, self.downlink_bytes)
    }

    fn exceeds_yellow(&self) -> bool {
        self.uplink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC
            || self.downlink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC
    }

    fn exceeded_direction(&self) -> &'static str {
        match (
            self.uplink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC,
            self.downlink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC,
        ) {
            (true, true) => "both",
            (true, false) => "uplink",
            (false, true) => "downlink",
            (false, false) => "none",
        }
    }

    fn log_if_yellow(&self) {
        if !self.exceeds_yellow() {
            return;
        }
        tracing::info!(
            target: "tau_cli::ui_io",
            direction = self.exceeded_direction(),
            uplink_bytes = self.uplink_bytes,
            downlink_bytes = self.downlink_bytes,
            uplink_breakdown = %format_ui_io_breakdown(&self.uplink_breakdown),
            downlink_breakdown = %format_ui_io_breakdown(&self.downlink_breakdown),
            "ui io exceeded yellow threshold"
        );
    }
}

#[derive(Clone, Copy)]
enum UiIoDirection {
    Uplink,
    Downlink,
}

struct UiIoTracker {
    meter: UiIoMeter,
    samples: VecDeque<(u64, u64)>,
    next_sample_at: Instant,
}

impl UiIoTracker {
    fn new(meter: UiIoMeter) -> Self {
        Self {
            meter,
            samples: VecDeque::with_capacity(UI_IO_SAMPLE_WINDOW_SECS),
            next_sample_at: Instant::now() + Duration::from_secs(1),
        }
    }

    fn recv_timeout(&self) -> Duration {
        self.next_sample_at
            .saturating_duration_since(Instant::now())
    }

    fn sample_if_due(&mut self, renderer: &mut EventRenderer) {
        let now = Instant::now();
        if now < self.next_sample_at {
            return;
        }
        self.sample_at(renderer, now);
    }

    fn sample_now(&mut self, renderer: &mut EventRenderer) {
        self.sample_at(renderer, Instant::now());
    }

    fn sample_at(&mut self, renderer: &mut EventRenderer, now: Instant) {
        let sample = self.meter.take_sample();
        let status_pair = sample.status_pair();
        sample.log_if_yellow();

        if self.samples.len() == UI_IO_SAMPLE_WINDOW_SECS {
            self.samples.pop_front();
        }
        self.samples.push_back(status_pair);
        self.next_sample_at = now + Duration::from_secs(1);

        renderer.handle_ui_io_sample(self.rolling_max());
    }

    fn rolling_max(&self) -> UiIoStats {
        let mut stats = UiIoStats::default();
        for &(uplink, downlink) in &self.samples {
            stats.uplink_max_bytes_per_sec = stats.uplink_max_bytes_per_sec.max(uplink);
            stats.downlink_max_bytes_per_sec = stats.downlink_max_bytes_per_sec.max(downlink);
        }
        stats
    }
}

struct UiWriter {
    writer: PeerOutputWriter<BufWriter<Box<dyn Write + Send>>>,
    meter: UiIoMeter,
}

impl UiWriter {
    fn new<W>(writer: W, meter: UiIoMeter) -> Self
    where
        W: Write + Send + 'static,
    {
        Self {
            writer: PeerOutputWriter::new(BufWriter::new(Box::new(writer))),
            meter,
        }
    }

    fn send_frame(&mut self, message: &HarnessInputMessage) -> io::Result<()> {
        self.writer
            .write_message(message)
            .map_err(io::Error::other)?;
        self.writer.flush()?;
        self.meter.record_uplink_frame(message);
        Ok(())
    }
}

/// Shared writer handle: the input loop and the prompt-draft debounce
/// thread both need to send events on the same socket. Stream
/// `write()` calls are atomic only up to `PIPE_BUF` (~4 KB on
/// AF_UNIX) so we serialize whole-event writes through a `Mutex`
/// instead of risking a long draft burst interleaving with a
/// `UiPromptSubmitted` mid-byte. Contention is essentially zero —
/// debounce fires at most once per second per typing burst.
type WriterHandle = Arc<Mutex<UiWriter>>;

struct UiConnection {
    read_stream: Box<dyn Read + Send>,
    writer: WriterHandle,
    socket_shutdown_stream: Option<UnixStream>,
}

fn connect_ui_transport(
    daemon: &mut DaemonHandle,
    ui_io_meter: &UiIoMeter,
    startup_started_at: Instant,
) -> Result<UiConnection, CliError> {
    if let Some(initial_ui) = daemon.take_initial_ui_stdio() {
        tracing::debug!(target: "tau_cli::startup", "using harness stdio for initial UI connection");
        return Ok(UiConnection {
            read_stream: Box::new(initial_ui.stdout),
            writer: Arc::new(Mutex::new(UiWriter::new(
                initial_ui.stdin,
                ui_io_meter.clone(),
            ))),
            socket_shutdown_stream: None,
        });
    }

    let socket_path = daemon.socket_path();
    tracing::debug!(target: "tau_cli::startup", socket_path = %socket_path.display(), "connecting to harness daemon socket");
    let stream = UnixStream::connect(&socket_path)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "connected to harness daemon socket");
    let read_stream = stream.try_clone()?;
    let socket_shutdown_stream = read_stream.try_clone()?;
    Ok(UiConnection {
        read_stream: Box::new(read_stream),
        writer: Arc::new(Mutex::new(UiWriter::new(stream, ui_io_meter.clone()))),
        socket_shutdown_stream: Some(socket_shutdown_stream),
    })
}

/// Lock the writer, write one frame and flush. Returns the underlying
/// `io::Error` on failure so callers can use `?` or discard with
/// `let _ = …`.
fn send_frame(writer: &WriterHandle, message: &HarnessInputMessage) -> io::Result<()> {
    locked(writer).send_frame(message)
}

fn send_handshake_frame(
    writer: &WriterHandle,
    read_stream: &mut Box<dyn Read + Send>,
    message: &HarnessInputMessage,
) -> Result<(), CliError> {
    match send_frame(writer, message) {
        Ok(()) => Ok(()),
        Err(error) => Err(startup_disconnect_or_io_error(read_stream, error)),
    }
}

fn startup_disconnect_or_io_error(
    read_stream: &mut Box<dyn Read + Send>,
    error: io::Error,
) -> CliError {
    let mut reader = PeerInputReader::new(BufReader::new(read_stream));
    match reader.read_message() {
        Ok(Some(HarnessOutputMessage::Disconnect(disconnect))) => CliError::DaemonExited(
            disconnect
                .reason
                .unwrap_or_else(|| "harness disconnected during startup handshake".to_owned()),
        ),
        _ => CliError::Io(error),
    }
}

/// Convenience wrapper around [`send_frame`] for [`Event`] payloads.
fn send_event(writer: &WriterHandle, event: &Event) -> io::Result<()> {
    send_frame(writer, &HarnessInputMessage::emit(event.clone()))
}

fn ui_io_message_len<M: serde::Serialize>(message: &M) -> Option<u64> {
    tau_proto::encode_message_to_vec(message)
        .ok()
        .map(|bytes| bytes.len() as u64)
}

fn ui_io_input_message_key(message: &HarnessInputMessage) -> String {
    if let HarnessInputMessage::Emit(emit) = message {
        return emit.event.name().to_string();
    }
    format!("message.{}", ui_io_harness_input_message_name(message))
}

fn ui_io_output_message_key(message: &HarnessOutputMessage) -> String {
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
    }
}

fn ui_io_harness_input_message_name(message: &HarnessInputMessage) -> &'static str {
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
    }
}

fn format_ui_io_breakdown(breakdown: &BTreeMap<String, u64>) -> String {
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
        .map(|(name, bytes)| format!("{name}={}", format_ui_io_bytes(*bytes)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_ui_io_cumulative_stats(stats: &UiIoCumulativeStats) -> String {
    let uplink_total = total_ui_io_event_stats(&stats.uplink);
    let downlink_total = total_ui_io_event_stats(&stats.downlink);
    let mut lines = vec![
        "UI event I/O cumulative stats".to_owned(),
        format!(
            "uplink: {} in {} frame(s)",
            format_ui_io_bytes(uplink_total.bytes),
            uplink_total.count
        ),
        format_ui_io_event_stats_section(&stats.uplink),
        format!(
            "downlink: {} in {} frame(s)",
            format_ui_io_bytes(downlink_total.bytes),
            downlink_total.count
        ),
        format_ui_io_event_stats_section(&stats.downlink),
    ];
    if uplink_total.count == 0 && downlink_total.count == 0 {
        lines.push("no UI frames recorded yet".to_owned());
    }
    lines.join("\n")
}

fn total_ui_io_event_stats(stats: &BTreeMap<String, UiIoEventStats>) -> UiIoEventStats {
    stats
        .values()
        .fold(UiIoEventStats::default(), |mut total, stats| {
            total.count += stats.count;
            total.bytes += stats.bytes;
            total
        })
}

fn format_ui_io_event_stats_section(stats: &BTreeMap<String, UiIoEventStats>) -> String {
    if stats.is_empty() {
        return "  (none)".to_owned();
    }
    sorted_ui_io_event_stats(stats)
        .into_iter()
        .map(|(name, stats)| {
            format!(
                "  {name}: {} bytes={} count={}",
                format_ui_io_bytes(stats.bytes),
                stats.bytes,
                stats.count
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sorted_ui_io_event_stats(
    stats: &BTreeMap<String, UiIoEventStats>,
) -> Vec<(&str, UiIoEventStats)> {
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

fn handle_debug_show_ui_event_stats_command_text(
    text: &str,
    meter: &UiIoMeter,
    mut system_info: impl FnMut(&str),
) -> bool {
    if text == "/debug-show-ui-event-stats" {
        system_info(&format_ui_io_cumulative_stats(&meter.cumulative_stats()));
        return true;
    }
    if text.starts_with("/debug-show-ui-event-stats ") {
        system_info("/debug-show-ui-event-stats takes no arguments");
        return true;
    }
    false
}

fn format_ui_io_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    if bytes < 1024 * 1024 {
        return format_ui_io_scaled_bytes(bytes, 1024, "K");
    }
    format_ui_io_scaled_bytes(bytes, 1024 * 1024, "M")
}

fn format_ui_io_scaled_bytes(bytes: u64, divisor: u64, suffix: &str) -> String {
    let whole = bytes / divisor;
    let tenth = bytes % divisor * 10 / divisor;
    if whole < 10 && tenth != 0 {
        format!("{whole}.{tenth}{suffix}")
    } else {
        format!("{whole}{suffix}")
    }
}

#[cfg(test)]
mod ui_io_tests {
    use std::os::unix::net::UnixStream;

    use tau_proto::HarnessOutputWriter;

    use super::*;

    /// Downlink deliveries should attribute bytes to the inner event name so
    /// the breakdown points at the event family worth optimizing rather
    /// than the transport envelope.
    #[test]
    fn ui_io_output_message_key_uses_delivered_event_name() {
        let message = HarnessOutputMessage::deliver(Event::TermBell(tau_proto::TermBell {}));

        assert_eq!(ui_io_output_message_key(&message), "term.bell");
    }

    /// Uplink event emissions should be grouped by the inner event name, not
    /// the generic transport envelope, so the debug dump can identify noisy
    /// UI-originated event types such as prompt drafts.
    #[test]
    fn ui_io_input_message_key_uses_emitted_event_name() {
        let message = HarnessInputMessage::emit(Event::TermBell(tau_proto::TermBell {}));

        assert_eq!(ui_io_input_message_key(&message), "term.bell");
    }

    /// Breakdown logging should put the largest contributors first so a noisy
    /// one-second sample immediately shows the best optimization target.
    #[test]
    fn ui_io_breakdown_formats_largest_first() {
        let mut breakdown = BTreeMap::new();
        breakdown.insert("small.event".to_owned(), 512);
        breakdown.insert("large.event".to_owned(), 12 * 1024);

        assert_eq!(
            format_ui_io_breakdown(&breakdown),
            "large.event=12K, small.event=512B"
        );
    }

    /// The debug stats command uses cumulative counters, so sampling the
    /// one-second status buckets must not clear per-event totals or counts.
    #[test]
    fn ui_io_meter_keeps_cumulative_event_stats_after_sampling() {
        let meter = UiIoMeter::default();
        meter.record_bytes(UiIoDirection::Downlink, "small.event".to_owned(), Some(10));
        meter.record_bytes(UiIoDirection::Downlink, "small.event".to_owned(), Some(15));

        let sample = meter.take_sample();
        assert_eq!(sample.downlink_bytes, 25);
        assert_eq!(
            meter
                .cumulative_stats()
                .downlink
                .get("small.event")
                .copied(),
            Some(UiIoEventStats {
                count: 2,
                bytes: 25
            })
        );
    }

    /// The human-readable debug dump should sort event types by descending
    /// bytes and include both exact byte totals and frame counts for
    /// diagnosis.
    #[test]
    fn ui_io_cumulative_stats_format_lists_largest_events_first() {
        let mut stats = UiIoCumulativeStats::default();
        stats.uplink.insert(
            "message.hello".to_owned(),
            UiIoEventStats {
                count: 1,
                bytes: 50,
            },
        );
        stats.downlink.insert(
            "small.event".to_owned(),
            UiIoEventStats {
                count: 3,
                bytes: 512,
            },
        );
        stats.downlink.insert(
            "large.event".to_owned(),
            UiIoEventStats {
                count: 2,
                bytes: 12 * 1024,
            },
        );

        let formatted = format_ui_io_cumulative_stats(&stats);

        assert!(formatted.contains("uplink: 50B in 1 frame(s)"));
        assert!(formatted.contains("  message.hello: 50B bytes=50 count=1"));
        assert!(formatted.contains("downlink: 12K in 5 frame(s)"));
        assert!(
            formatted.find("large.event").expect("large event line")
                < formatted.find("small.event").expect("small event line")
        );
    }

    /// The slash-command handler should print the same cumulative dump users
    /// get interactively and consume the command locally instead of letting it
    /// fall through to prompt submission.
    #[test]
    fn debug_show_ui_event_stats_command_prints_current_counters() {
        let meter = UiIoMeter::default();
        meter.record_bytes(
            UiIoDirection::Uplink,
            "ui.prompt_draft".to_owned(),
            Some(42),
        );
        let mut output = Vec::new();

        let handled = handle_debug_show_ui_event_stats_command_text(
            "/debug-show-ui-event-stats",
            &meter,
            |message| output.push(message.to_owned()),
        );

        assert!(handled);
        assert_eq!(output.len(), 1);
        assert!(output[0].contains("UI event I/O cumulative stats"));
        assert!(output[0].contains("ui.prompt_draft: 42B bytes=42 count=1"));
    }

    /// A mistyped debug stats invocation with arguments should be consumed with
    /// a local usage notice rather than becoming an unknown slash action or
    /// prompt text.
    #[test]
    fn debug_show_ui_event_stats_command_rejects_arguments() {
        let meter = UiIoMeter::default();
        let mut output = Vec::new();

        let handled = handle_debug_show_ui_event_stats_command_text(
            "/debug-show-ui-event-stats now",
            &meter,
            |message| output.push(message.to_owned()),
        );

        assert!(handled);
        assert_eq!(
            output,
            vec!["/debug-show-ui-event-stats takes no arguments".to_owned()]
        );
    }

    #[test]
    fn handshake_write_error_reads_pending_startup_disconnect() {
        let (mut harness_stdout, ui_stdout) = UnixStream::pair().expect("stdout pair");
        let (ui_stdin, harness_stdin) = UnixStream::pair().expect("stdin pair");
        drop(harness_stdin);

        let mut harness_writer = HarnessOutputWriter::new(&mut harness_stdout);
        harness_writer
            .write_message(&HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some("harness startup failed: missing secret".to_owned()),
            }))
            .expect("write disconnect");
        harness_writer.flush().expect("flush disconnect");

        let writer = Arc::new(Mutex::new(UiWriter::new(ui_stdin, UiIoMeter::default())));
        let mut read_stream: Box<dyn Read + Send> = Box::new(ui_stdout);
        let error = send_handshake_frame(
            &writer,
            &mut read_stream,
            &crate::ui_client::hello_message("tau-chat"),
        )
        .expect_err("handshake should fail");

        assert!(error.to_string().contains("harness startup failed"));
        assert!(error.to_string().contains("missing secret"));
    }
}

fn current_role_name(
    current_role_state: &Arc<Mutex<Option<String>>>,
    print_local: &impl Fn(&str),
) -> Option<String> {
    match current_role_state.lock().ok().and_then(|role| role.clone()) {
        Some(role) => Some(role),
        None => {
            print_local("no selected role yet");
            None
        }
    }
}

fn send_current_role_update(
    writer: &WriterHandle,
    current_role_state: &Arc<Mutex<Option<String>>>,
    action: tau_proto::UiRoleUpdateAction,
    print_local: &impl Fn(&str),
) {
    let Some(role) = current_role_name(current_role_state, print_local) else {
        return;
    };
    let _ = send_event(
        writer,
        &Event::UiRoleUpdate(tau_proto::UiRoleUpdate { role, action }),
    );
}

fn cycle_role_in_groups(
    writer: &WriterHandle,
    current_role_state: &Arc<Mutex<Option<String>>>,
    role_group_memory: &Arc<Mutex<HashMap<String, String>>>,
    groups: &[tau_proto::HarnessRoleGroup],
    alternate: bool,
    print_local: &impl Fn(&str),
) {
    if groups.is_empty() {
        print_local("cycle-role: no agent roles are available yet");
        return;
    }
    let current = current_role_state.lock().ok().and_then(|role| role.clone());
    let mut memory = role_group_memory
        .lock()
        .map(|memory| memory.clone())
        .unwrap_or_default();
    remember_group_role(&mut memory, groups, current.as_deref());
    let Some(next) = next_role_in_groups(current.as_deref(), groups, alternate, &memory) else {
        print_local("cycle-role: no agent roles are available yet");
        return;
    };
    remember_group_role(&mut memory, groups, Some(&next));
    if let Ok(mut shared_memory) = role_group_memory.lock() {
        *shared_memory = memory;
    }
    let _ = send_event(
        writer,
        &Event::UiRoleSelect(tau_proto::UiRoleSelect { role: next }),
    );
}

fn remember_group_role(
    memory: &mut HashMap<String, String>,
    groups: &[tau_proto::HarnessRoleGroup],
    role: Option<&str>,
) {
    let Some(role) = role else {
        return;
    };
    if let Some(group) = groups
        .iter()
        .find(|group| group.roles.iter().any(|candidate| candidate == role))
    {
        memory.insert(group.name.clone(), role.to_owned());
    }
}

fn next_role_in_groups(
    current: Option<&str>,
    groups: &[tau_proto::HarnessRoleGroup],
    alternate: bool,
    memory: &HashMap<String, String>,
) -> Option<String> {
    let current_pos = current.and_then(|current| {
        groups.iter().enumerate().find_map(|(group_index, group)| {
            group
                .roles
                .iter()
                .position(|role| role == current)
                .map(|role_index| (group_index, role_index))
        })
    });
    if alternate {
        let (group_index, role_index) = current_pos.unwrap_or((0, 0));
        let roles = groups.get(group_index)?.roles.as_slice();
        return roles.get((role_index + 1) % roles.len()).cloned();
    }
    let next_group = current_pos.map_or(0, |(group_index, _)| (group_index + 1) % groups.len());
    let group = groups.get(next_group)?;
    memory
        .get(&group.name)
        .filter(|role| group.roles.iter().any(|candidate| candidate == *role))
        .cloned()
        .or_else(|| group.roles.first().cloned())
}

fn cycle_role(
    writer: &WriterHandle,
    current_role_state: &Arc<Mutex<Option<String>>>,
    roles_available: &Arc<Mutex<Vec<String>>>,
    print_local: &impl Fn(&str),
) {
    let roles = match roles_available.lock() {
        Ok(roles) => roles.clone(),
        Err(_) => Vec::new(),
    };
    if roles.is_empty() {
        print_local("cycle-role: no agent roles are available yet");
        return;
    }
    let current = current_role_state.lock().ok().and_then(|role| role.clone());
    let next = match current
        .as_deref()
        .and_then(|current| roles.iter().position(|role| role == current))
    {
        Some(index) => roles[(index + 1) % roles.len()].clone(),
        None => roles[0].clone(),
    };
    let _ = send_event(
        writer,
        &Event::UiRoleSelect(tau_proto::UiRoleSelect { role: next }),
    );
}

/// Debounce period for `UiPromptDraft` emission while the user is
/// typing. Kept generous on purpose: the only consumer today
/// (std-notifications) only cares about second-or-better resolution
/// to bump its idle deadline.
const DRAFT_DEBOUNCE: Duration = Duration::from_secs(1);
const EOF_DURING_AGENT_NOTICE: &str =
    "An agent is still running; use /quit to terminate the session in progress.";
pub(crate) const SUSPENDED_AGENT_PROMPT: &str =
    "This agent is suspended. Use `/resume` to resume it before sending messages.";
const TREE_NAVIGATION_USAGE: &str = "/tree: use a prompt anchor, `root`, or explicit `node <id>`";
const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/quit", "Exit the chat session"),
    ("/cancel", "Cancel the current in-flight prompt"),
    (
        "/detach",
        "Leave the UI but keep the harness running for later reattach",
    ),
    (
        "/model",
        "Switch selected agent model (e.g. /model openai/gpt-5)",
    ),
    ("/agent", "Manage visible/suspended agent transcripts"),
    ("/new", "Alias for /agent new"),
    ("/name", "Alias for /agent name on the selected agent"),
    (
        "/ephemeral",
        "Stage the next /new agent as memory-only (/ephemeral on|off)",
    ),
    ("/suspend", "Alias for /agent suspend on the selected agent"),
    ("/resume", "Alias for /agent resume on the selected agent"),
    ("/role", "Switch, create, edit, or delete an agent role"),
    (
        "/prompt",
        "Replace the editor with a configured custom prompt template",
    ),
    (
        "/skill",
        "Invoke a user-invocable skill (e.g. /skill jujutsu optional args)",
    ),
    (
        "/session",
        "Manage chat sessions (e.g. /session new starts a fresh session)",
    ),
    (
        "/tree",
        "Print prompt rewind anchors (`/tree <anchor>` rewinds before that prompt)",
    ),
    (
        "/compact",
        "Force a provider-side compaction pass on the current session",
    ),
    ("/fast", "Toggle Fast mode"),
    (
        "/set",
        "Set a UI setting (e.g. /set show-diff true); Tab cycles names + values",
    ),
    (
        "/theme",
        "Switch this UI's theme for this run; Tab cycles available themes",
    ),
    ("/version", "Print Tau version and build information"),
    (
        "/provider-auth",
        "Add or replace a provider profile (runs `tau provider add [kind]`)",
    ),
    (
        "/debug-show-ui-event-stats",
        "Print cumulative UI event byte/count counters for this client",
    ),
];

/// Single-slot mailbox the input loop pushes the latest prompt
/// snapshot into; the debounce thread drains it. `pending = None` +
/// `done = false` means "nothing to send, keep waiting"; `done =
/// true` is the shutdown signal.
#[derive(Default)]
pub(crate) struct DraftSlot {
    pub(crate) pending: Option<(u64, UiPromptDraft)>,
    pub(crate) epoch: u64,
    pub(crate) done: bool,
}

/// Shared handle for the debounce mailbox. Wakeups are coordinated
/// via the `Condvar`; the debounce thread waits on it for new drafts
/// or a shutdown signal, the input loop notifies it on every
/// `BufferChanged`.
type DraftHandle = Arc<(Mutex<DraftSlot>, Condvar)>;

/// Trailing-edge debounce: wait for at least one draft to appear,
/// send the *latest* one (any older draft was overwritten by a more
/// recent typing burst), then sleep `DRAFT_DEBOUNCE` before looking
/// at the slot again. The sleep is interruptible via the `done`
/// shutdown signal so process exit is prompt.
///
/// Never drops a notification: a draft pushed during the
/// sleep stays in the slot and is sent on the next iteration.
fn debounce_loop(handle: DraftHandle, writer: WriterHandle) {
    let (mtx, cv) = &*handle;
    loop {
        // Wait for a draft to send, or shutdown.
        let snapshot = {
            let mut g = locked(mtx);
            while g.pending.is_none() && !g.done {
                g = cv.wait(g).expect(MUTEX_POISONED);
            }
            if g.done && g.pending.is_none() {
                return;
            }
            g.pending.take()
        };
        if let Some((epoch, draft)) = snapshot
            && should_send_draft_snapshot(handle.as_ref(), epoch)
        {
            // Best-effort: a write failure means the socket is gone,
            // and the input loop will notice on its next write.
            let _ = send_event(&writer, &Event::UiPromptDraft(draft));
        }
        // Coalesce subsequent typing into one event per window. Wake
        // early on shutdown so we don't spend a second sleeping after
        // the user already typed `/quit`.
        let g = locked(mtx);
        let (g, _timed_out) = cv
            .wait_timeout_while(g, DRAFT_DEBOUNCE, |s| !s.done)
            .expect(MUTEX_POISONED);
        if g.done && g.pending.is_none() {
            return;
        }
    }
}

pub(crate) fn should_send_draft_snapshot(handle: &(Mutex<DraftSlot>, Condvar), epoch: u64) -> bool {
    let (mtx, _cv) = handle;
    let g = locked(mtx);
    !g.done && g.epoch == epoch
}

pub(crate) fn invalidate_pending_draft(handle: &(Mutex<DraftSlot>, Condvar)) {
    let (mtx, cv) = handle;
    if let Ok(mut g) = mtx.lock() {
        g.epoch = g.epoch.wrapping_add(1);
        g.pending = None;
        cv.notify_one();
    }
}

fn encode_binding_action(action: &CliBindingAction) -> String {
    let Some(command) = action.command.as_deref().filter(|c| !c.is_empty()) else {
        return action.action.clone();
    };
    format!(
        "{}:{}:{}",
        action.action,
        if action.trim { "trim" } else { "raw" },
        command,
    )
}

pub(crate) fn run_chat(
    session_id: &str,
    attach: bool,
    session_status: SessionLaunchStatus,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<(), CliError> {
    use tau_cli_term::{HighTerm, SlashCommand};

    let state_dir = tau_session_inspect::default_state_dir();
    let ui_logging = if ephemeral {
        ui_logging::init_ephemeral()
    } else {
        ui_logging::init(&state_dir)?
    };
    tracing::info!(
        target: "tau_cli::ui",
        ui_id = ui_logging.ui_id(),
        ui_dir = %ui_logging.dir().display(),
        log_path = %ui_logging.log_path().display(),
        session_id,
        attach,
        "terminal UI starting"
    );

    let startup_started_at = Instant::now();
    let daemon_output = if attach {
        None
    } else {
        Some(daemon_output_for_session(session_id, ephemeral)?)
    };
    let mut daemon = resolve_daemon(
        attach,
        session_id,
        session_status,
        daemon_output,
        startup_role,
        cli_overrides,
        ephemeral,
    )?;
    let ui_io_meter = UiIoMeter::default();
    let UiConnection {
        mut read_stream,
        writer,
        socket_shutdown_stream,
    } = connect_ui_transport(&mut daemon, &ui_io_meter, startup_started_at)?;

    // Handshake.
    send_handshake_frame(
        &writer,
        &mut read_stream,
        &crate::ui_client::hello_message("tau-chat"),
    )?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "sent hello");
    send_handshake_frame(
        &writer,
        &mut read_stream,
        &crate::ui_client::subscribe_message(crate::ui_client::chat_subscription_selectors()),
    )?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "sent subscribe");

    // Background socket reader — decodes events and sends them to
    // a channel as `RendererCmd::Remote`. The input thread pushes
    // `RendererCmd::Set` variants (e.g. `/set show-diff true`) into the
    // same channel so the renderer thread sees a single ordered
    // stream and never needs to share state with the input thread.
    let (event_tx, event_rx) = mpsc::channel::<RendererCmd>();
    let socket_event_tx = event_tx.clone();
    let input_shutdown_handle: Arc<Mutex<Option<tau_cli_term::TermHandle>>> =
        Arc::new(Mutex::new(None));
    let socket_input_shutdown = input_shutdown_handle.clone();
    let remote_disconnected = Arc::new(AtomicBool::new(false));
    let socket_remote_disconnected = remote_disconnected.clone();
    let socket_ui_io_meter = ui_io_meter.clone();
    let local_disconnect_started = Arc::new(AtomicBool::new(false));
    let socket_local_disconnect_started = local_disconnect_started.clone();
    let socket_reader = std::thread::spawn(move || {
        let notify_disconnect = |reason: Option<String>| {
            socket_remote_disconnected.store(true, Ordering::Release);
            if let Some(handle) = socket_input_shutdown
                .lock()
                .expect(MUTEX_POISONED)
                .as_ref()
                .cloned()
            {
                handle.request_input_shutdown();
            }
            let _ = socket_event_tx.send(RendererCmd::RemoteDisconnect(reason));
        };
        let mut reader = PeerInputReader::new(BufReader::new(read_stream));
        loop {
            match reader.read_message() {
                Ok(Some(message)) => {
                    socket_ui_io_meter.record_downlink_frame(&message);
                    let cmd = match message {
                        HarnessOutputMessage::Deliver(delivery) => {
                            // The renderer folds replay frames like live ones:
                            // the UI renders state, so history is welcome.
                            let (event, _replay, recorded_at) = delivery.into_parts();
                            RendererCmd::Remote {
                                event: Box::new(event),
                                recorded_at: recorded_at.unwrap_or_else(UnixMicros::now),
                            }
                        }
                        HarnessOutputMessage::Disconnect(d) => {
                            if socket_local_disconnect_started.load(Ordering::Acquire) {
                                return;
                            }
                            notify_disconnect(d.reason);
                            return;
                        }
                        _ => continue,
                    };
                    if socket_event_tx.send(cmd).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    if !socket_local_disconnect_started.load(Ordering::Acquire) {
                        notify_disconnect(Some("harness connection closed".to_owned()));
                    }
                    return;
                }
                Err(error) => {
                    if !socket_local_disconnect_started.load(Ordering::Acquire) {
                        tracing::warn!(target: "tau_cli::ui", %error, "socket reader exiting");
                        notify_disconnect(Some(format!("harness connection error: {error}")));
                    }
                    return;
                }
            }
        }
    });

    // Terminal setup.
    let commands: Vec<SlashCommand> = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(name, description)| SlashCommand::new(*name, *description))
        .collect();
    let action_state =
        ActionCommandState::new(BUILTIN_SLASH_COMMANDS.iter().map(|(name, _)| *name));
    // Fail fast on a malformed `cli.yaml`. The fields here drive
    // keybindings, prompt symbol, cursor shape, and theme — silently
    // falling back to defaults would leave the user with broken
    // keybindings or unreadable colors and no clue why. Refuse to
    // start the TUI instead.
    let dirs = tau_config::settings::TauDirs::default();
    let settings = tau_config::settings::load_cli_settings_in(&dirs)
        .map_err(|error| CliError::Participant(format!("cli.yaml failed to parse:\n{error}")))?;
    let theme = crate::theme::select_theme(&dirs, settings.theme.clone())
        .map_err(|error| CliError::Participant(format!("cli theme failed to load:\n{error}")))?;
    let prompt = crate::theme::active_prompt_marker(&theme, &settings.prompt_symbol, None);
    let cwd = std::env::current_dir()?;
    let home_dir = dirs::home_dir();
    let right_prompt = crate::theme::cwd_right_prompt(&theme, &cwd, home_dir.as_deref());
    let cursor_shape = if settings.bar_cursor {
        tau_cli_term::CursorShape::Bar
    } else {
        tau_cli_term::CursorShape::Block
    };
    let completions = settings
        .completions
        .iter()
        .map(|(prefix, spec)| {
            tau_cli_term::CompletionRule::parse(prefix.clone(), spec).ok_or_else(|| {
                CliError::Participant(format!("invalid completion rule `{prefix}: {spec}`"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let completion_rules = tau_cli_term::CompletionRules::new(completions);
    let bindings = settings
        .bind
        .iter()
        .map(|(key, action)| (key.clone(), encode_binding_action(action)));
    let cli_state =
        tau_config::settings::CliState::load_with_default(&dirs, settings.default_state());
    let prompt_history = PromptHistoryStore::new(&dirs);
    let input_history = match prompt_history.load() {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(target: "tau_cli::ui", %error, "failed to load persistent prompt history");
            Vec::new()
        }
    };
    let (mut term, handle, completion_data) = HighTerm::new_with_completion_rules(
        prompt,
        commands,
        theme.clone(),
        cursor_shape,
        bindings,
        input_history,
        completion_rules,
    )?;
    *input_shutdown_handle.lock().expect(MUTEX_POISONED) = Some(handle.clone());
    if remote_disconnected.load(Ordering::Acquire) {
        handle.request_input_shutdown();
    }
    handle.set_right_prompt(right_prompt);
    handle.set_prompt_scroll_indicator(cli_state.show_prompt_scroll_indicator);
    handle.set_redraw_history_size(cli_state.redraw_history_size);
    // Show logo if enabled.
    if settings.show_logo {
        handle.print_output(
            "banner",
            tau_cli_term::StyledBlock::new(build_banner(&theme)),
        );
    }
    if tau_proto::NoticeLevel::Info.visible_at(cli_state.notice_level) {
        handle.print_output("ui-dir", ui_dir_block(&theme, ui_logging.dir()));
    }

    handle.redraw();

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    // Pre-build the renderer so we can grab its shared state handles
    // for the input loop. CLI config provides the default UI toggle values;
    // persisted `cli.json` state overrides them so `/set show-*` changes
    // survive restarts.
    let mut renderer = EventRenderer::new_with_state(
        renderer_handle,
        completion_data.clone(),
        theme.clone(),
        cli_state,
        dirs.clone(),
        settings.prompt_symbol.clone(),
        settings.submitted_prompt_symbol,
    );
    renderer.set_action_state(action_state.clone());
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/skill"),
        renderer.skill_arg_completer(),
    );
    let tool_timer = ToolTimerNotifier::new();
    renderer.set_tool_timer(tool_timer.clone());
    let timer_tx = event_tx.clone();
    let timer_state = tool_timer.inner();
    let timer_thread = std::thread::spawn(move || tool_timer_loop(timer_state, timer_tx));
    // Register `/set`'s context-aware arg completer. The first-arg
    // menu shows each setting's *current* value (read through the
    // renderer's shared mirror), and the second-arg menu shows
    // value-with-meaning for the selected setting.
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/set"),
        build_set_arg_completer(renderer.cli_state_mirror()),
    );
    let agent_in_progress = renderer.agent_in_progress_state();
    let fast_service_tier_state = renderer.fast_service_tier_state();
    let current_role_state = renderer.current_role_state();
    let current_agent_state = renderer.current_agent_state();
    let known_agents = renderer.known_agents();
    let agent_display_names = renderer.agent_display_names();
    let live_agents = renderer.live_agents();
    let ephemeral_agents = renderer.ephemeral_agents();
    let suspended_agents = renderer.suspended_agents();
    let input_routing = InputRoutingState::new(
        current_agent_state.clone(),
        known_agents.clone(),
        live_agents.clone(),
        ephemeral_agents.clone(),
        suspended_agents.clone(),
    );
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/agent"),
        build_agent_arg_completer(input_routing.clone(), agent_display_names.clone()),
    );
    completion_data
        .set_agent_mention_completer(build_agent_mention_completer(input_routing.clone()));
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/session"),
        build_session_arg_completer(),
    );
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new("/theme"),
        build_theme_arg_completer(dirs.clone()),
    );
    let roles_available = renderer.roles_available();
    let custom_prompts = renderer.custom_prompts();
    let role_groups_available = renderer.role_groups_available();
    let role_group_memory = renderer.role_group_memory();
    let editor_context = renderer.editor_context();
    term.set_editor_context_handle(editor_context.clone());
    let renderer_ui_io_meter = ui_io_meter.clone();
    let renderer_thread = std::thread::spawn(move || {
        let mut renderer = renderer;
        let mut ui_io_tracker = UiIoTracker::new(renderer_ui_io_meter);
        loop {
            match renderer_rx.recv_timeout(ui_io_tracker.recv_timeout()) {
                Ok(cmd) => {
                    match cmd {
                        RendererCmd::Remote { event, recorded_at } => {
                            renderer.handle_recorded_at(&event, recorded_at);
                        }
                        RendererCmd::RemoteDisconnect(reason) => renderer.handle_disconnect(reason),
                        RendererCmd::Set { name, value } => renderer.apply_setting(&name, &value),
                        RendererCmd::SwitchAgent { agent_id } => renderer.switch_agent(agent_id),
                        RendererCmd::SuspendAgent { agent_id } => renderer.suspend_agent(&agent_id),
                        RendererCmd::ResumeAgent { agent_id } => renderer.resume_agent(agent_id),
                        RendererCmd::ClearSelectedAgent => renderer.clear_selected_agent(),
                        RendererCmd::SetTheme { theme } => renderer.apply_theme(theme),
                        RendererCmd::ToolTimerTick => renderer.handle_tool_timer_tick(),
                    }
                    ui_io_tracker.sample_if_due(&mut renderer);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => ui_io_tracker.sample_now(&mut renderer),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    // Spawn the prompt-draft debounce thread. The input loop signals
    // it on every `BufferChanged` event with the latest buffer
    // contents; the thread coalesces a typing burst into one
    // `UiPromptDraft` per `DRAFT_DEBOUNCE` window and sends it on the
    // shared writer.
    let draft_handle: DraftHandle = Arc::new((Mutex::new(DraftSlot::default()), Condvar::new()));
    let debounce_thread = {
        let handle = draft_handle.clone();
        let writer = writer.clone();
        std::thread::spawn(move || debounce_loop(handle, writer))
    };

    // Terminal input loop — shares the writer with the debounce
    // thread via `WriterHandle`. Theme clone is for printing local
    // validation errors (e.g. `/role engineer effort foo`) through the same
    // TermHandle as remote events, so they don't garble the TUI like
    // `eprintln!` would.
    let mut active_session_id = session_id.to_owned();
    let exit = terminal_input_loop(
        &mut term,
        &writer,
        &mut active_session_id,
        TerminalInputLoopCtx {
            fast_service_tier_state,
            current_role_state,
            routing: input_routing,
            roles_available,
            role_groups_available,
            role_group_memory,
            theme,
            dirs: dirs.clone(),
            cwd,
            home_dir,
            prompt_symbol: settings.prompt_symbol,
            agent_in_progress,
            remote_disconnected,
            renderer_tx: event_tx,
            editor_context,
            action_state,
            draft_handle: draft_handle.clone(),
            prompt_history,
            custom_prompts,
            ui_io_meter: ui_io_meter.clone(),
        },
    )?;

    tool_timer.stop();
    let _ = timer_thread.join();

    // Tell the debounce thread to exit and wait for it so we don't
    // race with the disconnect below (the thread might otherwise
    // emit one final draft on the closing socket and trip an `EPIPE`).
    {
        let (mtx, cv) = &*draft_handle;
        let mut g = locked(mtx);
        g.done = true;
        cv.notify_all();
    }
    let _ = debounce_thread.join();

    let reason = shutdown_ui_connection(
        writer,
        socket_shutdown_stream,
        socket_reader,
        renderer_thread,
        exit,
        local_disconnect_started,
    );
    finish_daemon_for_exit(exit, daemon);

    tracing::info!(target: "tau_cli::ui", reason, "terminal UI exiting");

    Ok(())
}

/// How the input loop ended. Controls daemon disposition on exit.
#[derive(Clone, Copy)]
enum InputLoopExit {
    /// User typed `/quit`, hit Ctrl-D, or the socket dropped. The
    /// daemon should be killed (if we own it) or just disconnected
    /// from (if we were attached).
    Quit,
    /// User typed `/detach`. We leave the daemon running whether we
    /// spawned it or attached to it.
    Detach,
}

impl InputLoopExit {
    fn reason(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Detach => "detach",
        }
    }
}

fn shutdown_ui_connection(
    writer: WriterHandle,
    socket_shutdown_stream: Option<UnixStream>,
    socket_reader: std::thread::JoinHandle<()>,
    renderer_thread: std::thread::JoinHandle<()>,
    exit: InputLoopExit,
    local_disconnect_started: Arc<AtomicBool>,
) -> &'static str {
    let reason = exit.reason();
    local_disconnect_started.store(true, Ordering::Release);
    let _ = send_frame(
        &writer,
        &HarnessInputMessage::Disconnect(Disconnect {
            reason: Some(reason.to_owned()),
        }),
    );

    // Drop the writer, then explicitly shut down the extra stream clone kept
    // by the main thread so the socket reader unblocks even if the daemon does
    // not close promptly after the best-effort disconnect.
    drop(writer);
    if let Some(stream) = socket_shutdown_stream {
        let _ = stream.shutdown(Shutdown::Both);
    }

    join_ui_thread(socket_reader, "socket reader");
    join_ui_thread(renderer_thread, "renderer");
    reason
}

fn join_ui_thread(handle: std::thread::JoinHandle<()>, name: &str) {
    if handle.join().is_err() {
        tracing::warn!(target: "tau_cli::ui", name, "UI worker thread panicked during shutdown");
    }
}

fn finish_daemon_for_exit(exit: InputLoopExit, daemon: DaemonHandle) {
    // On detach, we explicitly leak the daemon child (if we own one) so it
    // outlives this process. `DaemonHandle::Drop` would otherwise kill the
    // child we spawned; `/detach` is exactly the case where we want it to keep
    // running.
    match exit {
        InputLoopExit::Quit => drop(daemon),
        InputLoopExit::Detach => daemon.leak(),
    }
}

fn tool_timer_loop(
    state: Arc<(Mutex<ToolTimerState>, Condvar)>,
    renderer_tx: mpsc::Sender<RendererCmd>,
) {
    let (mutex, cv) = &*state;
    let mut guard = locked(mutex);
    loop {
        while guard.active_tool_ids.is_empty() && !guard.done {
            guard = cv.wait(guard).expect(MUTEX_POISONED);
        }
        if guard.done {
            return;
        }
        let (next_guard, timeout) = cv
            .wait_timeout(guard, Duration::from_secs(1))
            .expect(MUTEX_POISONED);
        guard = next_guard;
        if guard.done {
            return;
        }
        if !guard.active_tool_ids.is_empty()
            && timeout.timed_out()
            && renderer_tx.send(RendererCmd::ToolTimerTick).is_err()
        {
            return;
        }
    }
}

/// Commands the renderer thread drains from a single ordered channel.
/// The socket reader pushes `Remote(event)`; the input loop pushes
/// local UI commands like `Set`. Keeping it one channel
/// removes the need for shared state between the two threads.
enum RendererCmd {
    /// `/set <name> <value>` — validated by the input loop before send.
    Set {
        name: String,
        value: String,
    },
    /// `/agent switch <agent_id>` — switch visible known agent transcript.
    SwitchAgent {
        agent_id: String,
    },
    /// Suspend a loaded agent from active UI choices. Emitted by `/agent
    /// suspend` and the selected-agent `/suspend` alias.
    SuspendAgent {
        agent_id: String,
    },
    /// Resume a suspended agent into active UI choices. Emitted by `/agent
    /// resume` and the selected-agent `/resume` alias.
    ResumeAgent {
        agent_id: String,
    },
    /// Return to the start-new-agent prompt state.
    ClearSelectedAgent,
    /// `/theme <name>` — apply a theme to this UI process only.
    SetTheme {
        theme: tau_themes::Theme,
    },
    Remote {
        event: Box<Event>,
        recorded_at: UnixMicros,
    },
    ToolTimerTick,
    /// The harness sent a `Disconnect` message over the wire.
    RemoteDisconnect(Option<String>),
}

struct TerminalInputLoopCtx {
    fast_service_tier_state: Arc<std::sync::atomic::AtomicBool>,
    current_role_state: Arc<Mutex<Option<String>>>,
    routing: InputRoutingState,
    roles_available: Arc<Mutex<Vec<String>>>,
    role_groups_available: Arc<Mutex<Vec<tau_proto::HarnessRoleGroup>>>,
    role_group_memory: Arc<Mutex<HashMap<String, String>>>,
    theme: tau_themes::Theme,
    dirs: tau_config::settings::TauDirs,
    cwd: std::path::PathBuf,
    home_dir: Option<std::path::PathBuf>,
    prompt_symbol: String,
    agent_in_progress: Arc<std::sync::atomic::AtomicBool>,
    remote_disconnected: Arc<AtomicBool>,
    renderer_tx: mpsc::Sender<RendererCmd>,
    editor_context: Arc<Mutex<tau_cli_term::EditorContext>>,
    action_state: ActionCommandState,
    draft_handle: DraftHandle,
    prompt_history: PromptHistoryStore,
    custom_prompts: Arc<Mutex<Vec<tau_proto::HarnessCustomPrompt>>>,
    ui_io_meter: UiIoMeter,
}

#[derive(Clone)]
struct InputRoutingState {
    current_agent_state: Arc<Mutex<Option<String>>>,
    known_agents: Arc<Mutex<Vec<String>>>,
    live_agents: Arc<Mutex<std::collections::HashSet<String>>>,
    ephemeral_agents: Arc<Mutex<std::collections::HashSet<String>>>,
    suspended_agents: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl InputRoutingState {
    fn new(
        current_agent_state: Arc<Mutex<Option<String>>>,
        known_agents: Arc<Mutex<Vec<String>>>,
        live_agents: Arc<Mutex<std::collections::HashSet<String>>>,
        ephemeral_agents: Arc<Mutex<std::collections::HashSet<String>>>,
        suspended_agents: Arc<Mutex<std::collections::HashSet<String>>>,
    ) -> Self {
        Self {
            current_agent_state,
            known_agents,
            live_agents,
            ephemeral_agents,
            suspended_agents,
        }
    }

    fn selected_agent_id(&self) -> Option<String> {
        self.current_agent_state
            .lock()
            .ok()
            .and_then(|agent| agent.clone())
    }

    fn selected_side_agent_id(&self) -> Option<tau_proto::AgentId> {
        self.selected_agent_id().map(|agent_id| {
            tau_proto::AgentId::parse(&agent_id).expect("UI stores valid agent ids")
        })
    }

    fn set_selected_agent(&self, agent_id: Option<String>) {
        if let Ok(mut current) = self.current_agent_state.lock() {
            *current = agent_id;
        }
    }

    fn known_agents(&self) -> Vec<String> {
        self.known_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default()
    }

    fn active_agents(&self) -> std::collections::HashSet<String> {
        let live = self
            .live_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        let suspended = self
            .suspended_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        live.into_iter()
            .filter(|agent| !suspended.contains(agent))
            .collect()
    }

    fn active_count(&self) -> usize {
        active_side_agent_count_from_handles(&self.live_agents, &self.suspended_agents)
    }

    fn agent_is_known(&self, agent_id: &str) -> bool {
        self.known_agents
            .lock()
            .map(|agents| agents.iter().any(|known| known == agent_id))
            .unwrap_or(false)
    }

    fn agent_is_active(&self, agent_id: &str) -> bool {
        let live = self
            .live_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        let suspended = self
            .suspended_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        agent_is_active_in_sets(&live, &suspended, agent_id)
    }

    fn agent_is_ephemeral(&self, agent_id: &str) -> bool {
        self.ephemeral_agents
            .lock()
            .map(|agents| agents.contains(agent_id))
            .unwrap_or(false)
    }

    fn agent_switch_target(&self, target: Option<&str>) -> Result<Option<String>, String> {
        let Some(arg) = target.map(str::trim).filter(|arg| !arg.is_empty()) else {
            return Err("/agent switch <agent_id|none>".to_owned());
        };
        if arg == "none" {
            return Ok(None);
        }
        if !self.agent_is_known(arg) {
            return Err(format!("unknown agent: {arg}"));
        }
        Ok(Some(arg.to_owned()))
    }

    fn mark_suspended(&self, agent_id: &str) {
        mark_agent_suspended(&self.suspended_agents, agent_id);
    }

    fn mark_resumed(&self, agent_id: &str) {
        mark_agent_resumed(&self.live_agents, &self.suspended_agents, agent_id);
    }

    fn next_active_agent(&self, delta: isize) -> Option<String> {
        let current = self.selected_agent_id();
        let known = self.known_agents();
        let live = self
            .live_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        let suspended = self
            .suspended_agents
            .lock()
            .map(|agents| agents.clone())
            .unwrap_or_default();
        next_active_agent(current.as_deref(), &known, &live, &suspended, delta)
    }

    fn role_cycling_enabled(&self) -> bool {
        role_cycling_enabled(&self.current_agent_state)
    }
}

fn handle_agent_suspend_command(
    routing: &InputRoutingState,
    renderer_tx: &mpsc::Sender<RendererCmd>,
    target: Option<&str>,
    print_local: &impl Fn(&str),
) {
    let target = target
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| routing.selected_agent_id());
    let Some(agent_id) = target else {
        print_local("/agent suspend <agent_id>");
        return;
    };
    if !routing.agent_is_known(&agent_id) {
        print_local(&format!("unknown agent: {agent_id}"));
        return;
    }
    routing.mark_suspended(&agent_id);
    let _ = renderer_tx.send(RendererCmd::SuspendAgent { agent_id });
}

fn handle_agent_resume_command(
    routing: &InputRoutingState,
    renderer_tx: &mpsc::Sender<RendererCmd>,
    target: Option<&str>,
    print_local: &impl Fn(&str),
) {
    let target = target
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            routing
                .selected_agent_id()
                .filter(|agent_id| !routing.agent_is_active(agent_id))
        });
    let Some(agent_id) = target else {
        print_local("/agent resume <agent_id>");
        return;
    };
    if !routing.agent_is_known(&agent_id) {
        print_local(&format!("unknown agent: {agent_id}"));
        return;
    }
    routing.mark_resumed(&agent_id);
    let _ = renderer_tx.send(RendererCmd::ResumeAgent { agent_id });
}

enum AgentSwitchCommandAction {
    ClearedSelection,
    SwitchedAgent,
}

fn handle_agent_switch_command(
    routing: &InputRoutingState,
    renderer_tx: &mpsc::Sender<RendererCmd>,
    target: Option<&str>,
    print_local: &impl Fn(&str),
) -> Option<AgentSwitchCommandAction> {
    match routing.agent_switch_target(target) {
        Ok(None) => {
            routing.set_selected_agent(None);
            let _ = renderer_tx.send(RendererCmd::ClearSelectedAgent);
            Some(AgentSwitchCommandAction::ClearedSelection)
        }
        Ok(Some(agent_id)) => {
            routing.set_selected_agent(Some(agent_id.clone()));
            let _ = renderer_tx.send(RendererCmd::SwitchAgent { agent_id });
            Some(AgentSwitchCommandAction::SwitchedAgent)
        }
        Err(message) => {
            print_local(&message);
            None
        }
    }
}

/// Local UI output used by the input thread while it holds `&mut HighTerm`.
///
/// The input loop cannot borrow `HighTerm` for rendering while it is also
/// waiting on `get_next_event`, so this helper owns a cloned `TermHandle` and
/// keeps the local status/echo styling in one place.
struct LocalTerminalOutput {
    handle: tau_cli_term::TermHandle,
    theme: tau_themes::Theme,
}

impl LocalTerminalOutput {
    fn new(handle: tau_cli_term::TermHandle, theme: tau_themes::Theme) -> Self {
        Self { handle, theme }
    }

    fn set_theme(&mut self, theme: tau_themes::Theme) {
        self.theme = theme;
    }

    fn system_info(&self, message: &str) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        self.handle.print_output(
            "system-info",
            themed_block(&self.theme, names::SYSTEM_INFO, message.to_owned()),
        );
    }

    fn command_echo(&self, text: &str) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        self.handle.print_output(
            "user-command",
            themed_block(&self.theme, names::USER_PROMPT, text.to_owned()),
        );
    }
}

/// Result of trying to consume a submitted line as a local command.
///
/// `NotHandled` means the line should become a normal user prompt. `Continue`
/// means a command consumed the line and the loop should wait for more input.
/// `Exit` carries the daemon-disposition decision for `/quit` and `/detach`.
enum CommandOutcome {
    NotHandled,
    Continue,
    Exit(InputLoopExit),
}

trait RecordedLineHandlers {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError>;
    fn handle_dynamic_action(&mut self, text: &str) -> CommandOutcome;
    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit>;
    fn system_info(&mut self, message: &str);
}

/// Mutable state for one terminal input loop invocation.
///
/// Keeping the borrows and owned context together lets each command-family
/// helper stay small while still sharing the same writer, session id, draft
/// mailbox, and local output path as the old monolithic loop.
struct TerminalInputSession<'a> {
    term: &'a mut tau_cli_term::HighTerm,
    writer: &'a WriterHandle,
    session_id: &'a mut String,
    ctx: TerminalInputLoopCtx,
    output: LocalTerminalOutput,
    pending_new_agent_options: PendingNewAgentOptions,
}

/// One-shot options staged while the UI is in new-agent mode.
#[derive(Default)]
struct PendingNewAgentOptions {
    /// Optional model override for the next created agent.
    model: Option<tau_proto::ModelId>,
    /// Whether the next created agent should be memory-only.
    ephemeral: bool,
}

impl PendingNewAgentOptions {
    fn apply_model_selection(
        &mut self,
        session_id: &str,
        selected_agent_id: Option<tau_proto::AgentId>,
        model: tau_proto::ModelId,
    ) -> Option<Event> {
        if let Some(target_agent_id) = selected_agent_id {
            Some(crate::ui_events::agent_model_select(
                session_id,
                Some(target_agent_id),
                model,
            ))
        } else {
            self.stage(model);
            None
        }
    }

    fn stage(&mut self, model: tau_proto::ModelId) {
        self.model = Some(model);
    }

    fn take_model(&mut self) -> Option<tau_proto::ModelId> {
        self.model.take()
    }

    fn take_ephemeral(&mut self) -> bool {
        std::mem::take(&mut self.ephemeral)
    }

    fn set_ephemeral(&mut self, ephemeral: bool) {
        self.ephemeral = ephemeral;
    }

    fn ephemeral(&self) -> bool {
        self.ephemeral
    }

    fn clear(&mut self) {
        self.model = None;
        self.ephemeral = false;
    }
}

fn tree_command_event(
    session_id: &str,
    target_agent_id: Option<tau_proto::AgentId>,
    text: &str,
) -> Result<Option<Event>, &'static str> {
    if text == "/tree" {
        return Ok(Some(crate::ui_events::tree_request(
            session_id,
            target_agent_id,
        )));
    }
    if let Some(arg) = text.strip_prefix("/tree ") {
        let target = crate::ui_commands::parse_tree_navigation_target(arg)
            .map_err(|()| TREE_NAVIGATION_USAGE)?;
        return Ok(Some(crate::ui_events::navigate_tree(
            session_id,
            target_agent_id,
            target,
        )));
    }
    Ok(None)
}

#[derive(Debug, Eq, PartialEq)]
struct AgentDisplayNameRequest {
    agent_id: tau_proto::AgentId,
    display_name: String,
}

impl AgentDisplayNameRequest {
    fn event(&self, session_id: &str) -> Event {
        crate::ui_events::set_agent_display_name(
            session_id,
            self.agent_id.clone(),
            self.display_name.clone(),
        )
    }
}

fn name_alias_request(
    text: &str,
    selected_agent_id: Option<String>,
    agent_is_known: impl FnOnce(&str) -> bool,
) -> Result<AgentDisplayNameRequest, String> {
    let display_name = text.strip_prefix("/name").unwrap_or("").trim();
    if display_name.is_empty() {
        return Err("/name <display_name>".to_owned());
    }
    let Some(agent_id) = selected_agent_id else {
        return Err(
            "/name requires a selected agent; use /agent switch <agent_id> or /agent name <agent_id> <display_name>"
                .to_owned(),
        );
    };
    if !agent_is_known(&agent_id) {
        return Err(format!("unknown agent: {agent_id}"));
    }
    Ok(AgentDisplayNameRequest {
        agent_id: tau_proto::AgentId::parse(&agent_id).expect("known agent id is valid"),
        display_name: display_name.to_owned(),
    })
}

fn prompt_line_targets_ephemeral_agent_state(
    text: &str,
    selected_agent_is_ephemeral: bool,
    has_selected_agent: bool,
    pending_new_agent_ephemeral: bool,
    local_or_action: bool,
) -> bool {
    if text.starts_with('!') && selected_agent_is_ephemeral {
        return true;
    }
    if local_or_action {
        return false;
    }
    if has_selected_agent {
        return selected_agent_is_ephemeral;
    }
    pending_new_agent_ephemeral
}

fn apply_ephemeral_staging_command(
    text: &str,
    has_selected_agent: bool,
    pending: &mut PendingNewAgentOptions,
    mut system_info: impl FnMut(&str),
) -> bool {
    if text != "/ephemeral" && !text.starts_with("/ephemeral ") {
        return false;
    }
    let Some(rest) = text.strip_prefix("/ephemeral") else {
        return false;
    };
    let rest = rest.trim();
    if !rest.is_empty() && !matches!(rest, "on" | "off") || text.split_whitespace().count() > 2 {
        system_info("/ephemeral [on|off]");
        return true;
    }
    if has_selected_agent {
        system_info("Use /new first; /ephemeral controls only the next new agent.");
        return true;
    }
    match rest {
        "on" => pending.set_ephemeral(true),
        "off" => pending.set_ephemeral(false),
        "" => {
            let next = !pending.ephemeral();
            pending.set_ephemeral(next);
        }
        _ => unreachable!("validated above"),
    }
    if pending.ephemeral() {
        system_info("next agent will be ephemeral (forgotten when this daemon exits)");
    } else {
        system_info("next agent will be persistent");
    }
    true
}

impl<'a> TerminalInputSession<'a> {
    fn run(&mut self) -> Result<InputLoopExit, CliError> {
        loop {
            let event = self.term.get_next_event()?;
            if let Some(exit) = self.handle_event(event)? {
                return Ok(exit);
            }
        }
    }

    fn handle_event(
        &mut self,
        event: tau_cli_term::Event,
    ) -> Result<Option<InputLoopExit>, CliError> {
        use tau_cli_term::Event as TermEvent;

        match event {
            TermEvent::Line(line) => self.handle_line(&line),
            TermEvent::Eof => Ok(self.handle_eof()),
            TermEvent::CancelPrompt => {
                self.send_cancel_prompt();
                Ok(None)
            }
            other => {
                self.handle_non_exit_event(other);
                Ok(None)
            }
        }
    }

    fn handle_non_exit_event(&mut self, event: tau_cli_term::Event) {
        use tau_cli_term::Event as TermEvent;

        // These events update local UI/session state only; none of them can
        // terminate the input loop, unlike submitted lines and EOF.

        match event {
            TermEvent::Resize { .. } => {
                tracing::debug!(target: "tau_cli::ui", "terminal resized");
            }
            TermEvent::FocusChanged { focused } => self.send_focus_changed(focused),
            TermEvent::BufferChanged => self.update_draft(),
            TermEvent::Action(action) => self.handle_binding_action(&action),
            TermEvent::BackTab => self.cycle_role_group(),
            TermEvent::Escape => self.recall_queued_prompt(),
            TermEvent::Line(_) | TermEvent::Eof | TermEvent::CancelPrompt => {}
        }
    }

    fn handle_binding_action(&mut self, action: &str) {
        match action {
            "fast-toggle" => self.toggle_fast_service_tier(),
            "cycle-role" => self.cycle_role_inner(),
            "cycle-role-group" => self.cycle_role_group(),
            "agent-previous" => self.switch_agent_by_delta(-1),
            "agent-next" => self.switch_agent_by_delta(1),
            _ => self
                .output
                .system_info(&format!("binding: unknown application action `{action}`")),
        }
    }

    fn send_focus_changed(&self, focused: bool) {
        let _ = send_event(
            self.writer,
            &Event::UiFocusChanged(UiFocusChanged {
                session_id: self.session_id.as_str().into(),
                focused,
            }),
        );
    }

    fn recall_queued_prompt(&self) {
        let _ = send_event(
            self.writer,
            &Event::UiRecallQueuedPrompt(tau_proto::UiRecallQueuedPrompt {
                session_id: self.session_id.as_str().into(),
                target_agent_id: self.selected_side_agent_id(),
            }),
        );
    }

    fn handle_line(&mut self, line: &str) -> Result<Option<InputLoopExit>, CliError> {
        let text = line.trim();
        if text.is_empty() {
            return Ok(None);
        }

        // Preserve the original side-effect order: every non-empty line is
        // recorded before command handling, and local slash commands are echoed
        // before they produce validation errors or exit the loop.
        self.record_prompt_line_if_persistent(line, text);
        if is_local_slash_command(text) || self.ctx.action_state.is_known_action_line(text) {
            self.output.command_echo(&redacted_command_echo_line(text));
        }
        self.handle_recorded_line(text)
    }

    fn handle_recorded_line(&mut self, text: &str) -> Result<Option<InputLoopExit>, CliError> {
        handle_recorded_line_with_handlers(text, self)
    }

    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        // Keep session-lifecycle commands first: `/quit` and `/detach` exit
        // immediately, while `/session new` mutates `session_id` for later
        // commands and prompt submission.
        let outcome = self.handle_session_command(text)?;
        if !matches!(outcome, CommandOutcome::NotHandled) {
            return Ok(outcome);
        }
        if self.handle_non_session_command(text) {
            return Ok(CommandOutcome::Continue);
        }
        Ok(CommandOutcome::NotHandled)
    }

    fn handle_non_session_command(&mut self, text: &str) -> bool {
        // The grouping mirrors the old dispatch order while keeping each
        // command-family helper below the cargo-crap hotspot range.
        self.handle_custom_prompt_command(text)
            || self.handle_navigation_or_role_shortcut(text)
            || self.handle_utility_or_shell_shortcut(text)
    }

    fn handle_custom_prompt_command(&mut self, text: &str) -> bool {
        let prompts = self
            .ctx
            .custom_prompts
            .lock()
            .map(|prompts| prompts.clone())
            .unwrap_or_default();
        let Some(result) = custom_prompt_replacement(text, &prompts) else {
            return false;
        };
        match result {
            Ok(prompt) => {
                let cursor = prompt.len();
                self.term.handle().set_buffer(prompt, cursor);
                self.term.handle().redraw();
                self.update_draft();
            }
            Err(message) => self.output.system_info(&message),
        }
        true
    }

    fn handle_navigation_or_role_shortcut(&mut self, text: &str) -> bool {
        self.handle_tree_or_compact_command(text) || self.handle_role_setting_shortcut(text)
    }

    fn handle_utility_or_shell_shortcut(&mut self, text: &str) -> bool {
        self.handle_ephemeral_command(text)
            || self.handle_utility_command(text)
            || self.handle_role_selection_command(text)
            || self.handle_shell_shortcut(text)
    }

    fn handle_ephemeral_command(&mut self, text: &str) -> bool {
        apply_ephemeral_staging_command(
            text,
            self.selected_agent_id().is_some(),
            &mut self.pending_new_agent_options,
            |message| self.output.system_info(message),
        )
    }

    fn record_prompt_line_if_persistent(&self, line: &str, text: &str) {
        if self.prompt_line_targets_ephemeral_agent(text) {
            if let Ok(mut context) = self.ctx.editor_context.lock() {
                context.previous_prompt = Some(text.to_owned());
            }
            return;
        }
        let history_line = redacted_prompt_history_line(line, text);
        if let Err(error) = self.ctx.prompt_history.append(&history_line) {
            tracing::warn!(target: "tau_cli::ui", %error, "failed to append persistent prompt history");
        }
        if let Ok(mut context) = self.ctx.editor_context.lock() {
            context.previous_prompt = Some(text.to_owned());
        }
    }

    fn prompt_line_targets_ephemeral_agent(&self, text: &str) -> bool {
        let selected_agent_id = self.selected_agent_id();
        let selected_agent_is_ephemeral = selected_agent_id
            .as_deref()
            .is_some_and(|agent_id| self.ctx.routing.agent_is_ephemeral(agent_id));
        prompt_line_targets_ephemeral_agent_state(
            text,
            selected_agent_is_ephemeral,
            selected_agent_id.is_some(),
            self.pending_new_agent_options.ephemeral(),
            is_local_slash_command(text) || self.ctx.action_state.is_known_action_line(text),
        )
    }

    fn handle_session_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        if text == "/quit" {
            return Ok(CommandOutcome::Exit(InputLoopExit::Quit));
        }
        if text == "/cancel" {
            self.send_cancel_prompt();
            return Ok(CommandOutcome::Continue);
        }
        if text == "/detach" {
            // Tell the harness to stay alive after we leave,
            // then exit the UI. If the write fails we still
            // exit — the daemon will notice the disconnect
            // and fall back to its default behavior.
            let _ = send_event(
                self.writer,
                &Event::UiDetachRequest(tau_proto::UiDetachRequest {}),
            );
            return Ok(CommandOutcome::Exit(InputLoopExit::Detach));
        }
        if text == "/session" || text.starts_with("/session ") {
            self.handle_session_namespace(text)?;
            return Ok(CommandOutcome::Continue);
        }

        Ok(CommandOutcome::NotHandled)
    }

    fn handle_session_namespace(&mut self, text: &str) -> Result<(), CliError> {
        let rest = text.strip_prefix("/session").unwrap_or("").trim();
        let mut parts = rest.split_whitespace();
        let subcommand = parts.next();
        let extra = parts.next();
        match (subcommand, extra) {
            (Some("new"), None) => self.start_new_session(),
            (None, None) => {
                self.output.system_info("/session new");
                Ok(())
            }
            _ => {
                self.output.system_info("/session new");
                Ok(())
            }
        }
    }

    fn start_new_session(&mut self) -> Result<(), CliError> {
        let cwd = std::env::current_dir()?;
        let new_id = crate::daemon::mint_session_id(&cwd);
        let _ = send_event(
            self.writer,
            &Event::UiSwitchSession(tau_proto::UiSwitchSession {
                new_session_id: new_id.as_str().into(),
                reason: tau_proto::SessionStartReason::New,
            }),
        );
        *self.session_id = new_id;
        self.pending_new_agent_options.clear();
        self.clear_selected_agent();
        Ok(())
    }

    fn send_cancel_prompt(&self) {
        let _ = send_event(
            self.writer,
            &crate::ui_events::cancel_prompt(self.session_id, self.selected_side_agent_id()),
        );
    }

    fn selected_side_agent_id(&self) -> Option<tau_proto::AgentId> {
        self.ctx.routing.selected_side_agent_id()
    }

    fn handle_tree_or_compact_command(&self, text: &str) -> bool {
        self.handle_tree_command(text) || self.handle_compact_command(text)
    }

    fn handle_tree_command(&self, text: &str) -> bool {
        match tree_command_event(self.session_id, self.selected_side_agent_id(), text) {
            Ok(Some(event)) => {
                let _ = send_event(self.writer, &event);
                true
            }
            Ok(None) => false,
            Err(message) => {
                self.output.system_info(message);
                true
            }
        }
    }

    fn handle_compact_command(&self, text: &str) -> bool {
        if text == "/compact" {
            let _ = send_event(
                self.writer,
                &crate::ui_events::compact_request(self.session_id, self.selected_side_agent_id()),
            );
            return true;
        }
        if text.starts_with("/compact ") {
            self.output
                .system_info("/compact forces a compaction pass and takes no arguments");
            return true;
        }
        false
    }

    fn handle_role_setting_shortcut(&self, text: &str) -> bool {
        self.handle_fast_shortcut(text)
    }

    fn handle_fast_shortcut(&self, text: &str) -> bool {
        if text == "/fast" {
            self.toggle_fast_service_tier();
            return true;
        }
        if text.starts_with("/fast ") {
            self.output.system_info("/fast toggles Fast mode");
            return true;
        }
        false
    }

    fn handle_utility_command(&mut self, text: &str) -> bool {
        if self.handle_debug_show_ui_event_stats_command(text) {
            return true;
        }
        if text == "/version" {
            self.output.system_info(&crate::version_label());
            return true;
        }
        if text.starts_with("/version ") {
            self.output.system_info("/version takes no arguments");
            return true;
        }
        if let Some(provider) = text.strip_prefix("/provider-auth ") {
            let provider = provider.trim();
            if !provider.is_empty() {
                let output = &self.output;
                run_provider_auth(provider, &|message| output.system_info(message));
            }
            return true;
        }
        if text == "/provider-auth" {
            let output = &self.output;
            run_provider_auth("", &|message| output.system_info(message));
            return true;
        }
        if text == "/theme" || text.starts_with("/theme ") {
            self.handle_theme_command(text);
            return true;
        }
        if text == "/agent" || text.starts_with("/agent ") {
            self.handle_agent_command(text);
            return true;
        }
        if text == "/new" || text.starts_with("/new ") {
            self.handle_new_alias(text);
            return true;
        }
        if text == "/name" || text.starts_with("/name ") {
            self.handle_name_alias(text);
            return true;
        }
        if text == "/suspend" || text.starts_with("/suspend ") {
            self.handle_suspend_alias(text);
            return true;
        }
        if text == "/resume" || text.starts_with("/resume ") {
            self.handle_resume_alias(text);
            return true;
        }
        if text == "/set" || text.starts_with("/set ") {
            let output = &self.output;
            handle_set_command(text, &self.ctx.renderer_tx, &|message| {
                output.system_info(message);
            });
            return true;
        }

        false
    }

    fn handle_debug_show_ui_event_stats_command(&self, text: &str) -> bool {
        handle_debug_show_ui_event_stats_command_text(text, &self.ctx.ui_io_meter, |message| {
            self.output.system_info(message);
        })
    }

    fn handle_theme_command(&mut self, text: &str) {
        let name = text.strip_prefix("/theme").unwrap_or("").trim();
        if name.is_empty() {
            let names = crate::theme::available_theme_choices(&self.ctx.dirs)
                .into_iter()
                .map(crate::theme::ThemeChoice::into_listing_text)
                .collect::<Vec<_>>()
                .join(", ");
            self.output
                .system_info(&format!("/theme <name>; available: {names}"));
            return;
        }
        let theme = match crate::theme::select_theme_for_command(&self.ctx.dirs, name) {
            Ok(theme) => theme,
            Err(error) => {
                self.output.system_info(&format!("/theme: {error}"));
                return;
            }
        };
        self.ctx.theme = theme.clone();
        self.term.set_theme(theme.clone());
        self.output.set_theme(theme.clone());
        let current_role = self
            .ctx
            .current_role_state
            .lock()
            .ok()
            .and_then(|role| role.clone());
        self.term
            .handle()
            .set_left_prompt(crate::theme::active_prompt_marker(
                &theme,
                &self.ctx.prompt_symbol,
                current_role.as_deref(),
            ));
        self.term
            .handle()
            .set_right_prompt(crate::theme::cwd_right_prompt(
                &theme,
                &self.ctx.cwd,
                self.ctx.home_dir.as_deref(),
            ));
        let _ = self.ctx.renderer_tx.send(RendererCmd::SetTheme {
            theme: theme.clone(),
        });
        self.output
            .system_info(&format!("theme set to `{name}` for this UI"));
    }

    fn handle_agent_command(&mut self, text: &str) {
        let rest = text.strip_prefix("/agent").unwrap_or("").trim();
        if rest.is_empty() {
            let current = self
                .ctx
                .routing
                .selected_agent_id()
                .unwrap_or_else(|| "none".to_owned());
            let known_agents = self.ctx.routing.known_agents();
            let active_count = self.ctx.routing.active_count();
            self.output.system_info(&format!(
                "/agent <new|switch|suspend|resume|name> [agent_id]; current: {current}; active: {active_count}; known: {}",
                known_agents.join(", ")
            ));
            return;
        }

        let mut parts = rest.split_whitespace();
        let Some(subcommand) = parts.next() else {
            return;
        };
        let target = parts.next();
        if subcommand == "name" {
            self.handle_agent_name(rest);
            return;
        }
        if parts.next().is_some() {
            self.output.system_info(
                "/agent: too many arguments (use /agent <new|switch|suspend|resume|name> [agent_id])",
            );
            return;
        }
        match subcommand {
            "new" => self.handle_agent_new(target),
            "switch" => self.handle_agent_switch(target),
            "suspend" => self.handle_agent_suspend(target),
            "resume" => self.handle_agent_resume(target),
            _ => self.output.system_info(
                "/agent <new|switch|suspend|resume|name> [agent_id]; use /agent switch <agent_id>",
            ),
        }
    }

    fn handle_agent_name(&self, rest: &str) {
        let Some(rest) = rest.strip_prefix("name") else {
            self.output
                .system_info("/agent name <agent_id> <display_name>");
            return;
        };
        let rest = rest.trim();
        let Some((agent_id, display_name)) = rest.split_once(char::is_whitespace) else {
            self.output
                .system_info("/agent name <agent_id> <display_name>");
            return;
        };
        let agent_id = agent_id.trim();
        let display_name = display_name.trim();
        if agent_id.is_empty() || display_name.is_empty() {
            self.output
                .system_info("/agent name <agent_id> <display_name>");
            return;
        }
        if !self.agent_is_known(agent_id) {
            self.output
                .system_info(&format!("unknown agent: {agent_id}"));
            return;
        }
        self.send_agent_display_name_request(AgentDisplayNameRequest {
            agent_id: tau_proto::AgentId::parse(agent_id).expect("known agent id is valid"),
            display_name: display_name.to_owned(),
        });
    }

    fn handle_name_alias(&self, text: &str) {
        match name_alias_request(text, self.selected_agent_id(), |agent_id| {
            self.agent_is_known(agent_id)
        }) {
            Ok(request) => self.send_agent_display_name_request(request),
            Err(message) => self.output.system_info(&message),
        }
    }

    fn send_agent_display_name_request(&self, request: AgentDisplayNameRequest) {
        let event = request.event(self.session_id);
        if send_event(self.writer, &event).is_ok() {
            self.output.system_info(&format!(
                "requested agent {} display name set to: {}",
                request.agent_id.as_str(),
                request.display_name
            ));
        }
    }

    fn handle_new_alias(&mut self, text: &str) {
        if text.trim() != "/new" {
            self.output.system_info("/new");
            return;
        }
        self.handle_agent_new(None);
    }

    fn handle_suspend_alias(&self, text: &str) {
        if text.trim() != "/suspend" {
            self.output.system_info("/suspend");
            return;
        }
        self.handle_agent_suspend(None);
    }

    fn handle_resume_alias(&self, text: &str) {
        if text.trim() != "/resume" {
            self.output.system_info("/resume");
            return;
        }
        self.handle_agent_resume(None);
    }

    fn selected_agent_id(&self) -> Option<String> {
        self.ctx.routing.selected_agent_id()
    }

    fn handle_agent_new(&mut self, target: Option<&str>) {
        if target.is_some() {
            self.output.system_info("/agent new");
            return;
        }
        self.clear_selected_agent();
    }

    fn clear_selected_agent(&mut self) {
        self.ctx.routing.set_selected_agent(None);
        self.dismiss_completion_menu();
        let _ = self.ctx.renderer_tx.send(RendererCmd::ClearSelectedAgent);
    }

    fn handle_agent_switch(&mut self, target: Option<&str>) {
        let action = handle_agent_switch_command(
            &self.ctx.routing,
            &self.ctx.renderer_tx,
            target,
            &|message| self.output.system_info(message),
        );
        match action {
            Some(AgentSwitchCommandAction::ClearedSelection) => {
                self.dismiss_completion_menu();
            }
            Some(AgentSwitchCommandAction::SwitchedAgent) => {
                self.pending_new_agent_options.clear();
                self.dismiss_completion_menu();
            }
            None => {}
        }
    }

    fn handle_agent_suspend(&self, target: Option<&str>) {
        handle_agent_suspend_command(
            &self.ctx.routing,
            &self.ctx.renderer_tx,
            target,
            &|message| {
                self.output.system_info(message);
            },
        );
    }

    fn handle_agent_resume(&self, target: Option<&str>) {
        handle_agent_resume_command(
            &self.ctx.routing,
            &self.ctx.renderer_tx,
            target,
            &|message| {
                self.output.system_info(message);
            },
        );
    }

    fn agent_is_known(&self, agent_id: &str) -> bool {
        self.ctx.routing.agent_is_known(agent_id)
    }

    fn agent_is_active(&self, agent_id: &str) -> bool {
        self.ctx.routing.agent_is_active(agent_id)
    }

    fn handle_role_selection_command(&mut self, text: &str) -> bool {
        if text == "/role" || text.starts_with("/role ") {
            let output = &self.output;
            handle_role_command(text, self.writer, &|message| output.system_info(message));
            return true;
        }
        if let Some(model) = text.strip_prefix("/model ") {
            let model = model.trim();
            if !model.is_empty() {
                match model.parse::<tau_proto::ModelId>() {
                    Ok(model) => {
                        if let Some(event) = self.pending_new_agent_options.apply_model_selection(
                            self.session_id,
                            self.ctx.routing.selected_side_agent_id(),
                            model.clone(),
                        ) {
                            let _ = send_event(self.writer, &event);
                        } else {
                            self.output
                                .system_info(&format!("next agent model set to {model}"));
                        }
                    }
                    Err(error) => self.output.system_info(&error.to_string()),
                }
            }
            return true;
        }
        if text == "/model" {
            // No argument — just a reminder.
            return true;
        }

        false
    }

    fn handle_dynamic_action(&self, text: &str) -> CommandOutcome {
        let Some(dispatch) = self.ctx.action_state.parse_line(text) else {
            return CommandOutcome::NotHandled;
        };
        let dispatch = match dispatch {
            Ok(dispatch) => dispatch,
            Err(error) => {
                self.output.system_info(&error.to_string());
                return CommandOutcome::Continue;
            }
        };
        let parsed = dispatch.parsed;
        self.invalidate_pending_draft();
        let event = Event::ActionInvoke(tau_proto::ActionInvoke {
            invocation_id: crate::mint_short_id("action").into(),
            session_id: self.session_id.as_str().into(),
            extension_name: dispatch.extension_name,
            instance_id: dispatch.instance_id,
            action_id: parsed.action_id.clone(),
            raw_line: text.to_owned(),
            argv: parsed.argv.clone(),
            arguments: parsed_action_arguments(&parsed.named_args),
        });
        if send_event(self.writer, &event).is_err() {
            CommandOutcome::Exit(InputLoopExit::Quit)
        } else {
            CommandOutcome::Continue
        }
    }

    fn handle_shell_shortcut(&self, text: &str) -> bool {
        // `!!<cmd>` / `!<cmd>`: run a shell command locally.
        // `!!` excludes the result from the agent's context;
        // `!` (single bang) includes it.
        if let Some(command) = text.strip_prefix("!!") {
            if let Err(error) = self.send_shell_shortcut(command, false) {
                tracing::warn!(target: "tau_cli::ui", %error, "failed to send !! shell command");
            }
            return true;
        }
        if let Some(command) = text.strip_prefix('!') {
            if let Err(error) = self.send_shell_shortcut(command, true) {
                tracing::warn!(target: "tau_cli::ui", %error, "failed to send ! shell command");
            }
            return true;
        }

        false
    }

    fn send_shell_shortcut(&self, command: &str, include_in_context: bool) -> io::Result<()> {
        let command = command.trim();
        if command.is_empty() {
            return Ok(());
        }
        let target_agent_id = self.ctx.routing.selected_side_agent_id();
        send_shell_command(
            self.writer,
            self.session_id,
            command,
            include_in_context,
            target_agent_id,
        )
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        self.invalidate_pending_draft();

        let selected_agent = self.ctx.routing.selected_agent_id();
        if let Some(selected_agent) = selected_agent.as_deref()
            && !self.agent_is_active(selected_agent)
        {
            self.output.system_info(SUSPENDED_AGENT_PROMPT);
            return None;
        }
        self.ctx
            .agent_in_progress
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let event = if let Some(target_agent_id) = selected_agent.map(|agent_id| {
            tau_proto::AgentId::parse(&agent_id).expect("UI stores valid agent ids")
        }) {
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: self.session_id.as_str().into(),
                text: text.to_owned(),
                agent_id: target_agent_id,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            })
        } else {
            let role = self
                .ctx
                .current_role_state
                .lock()
                .ok()
                .and_then(|role| role.clone())
                .unwrap_or_else(|| DEFAULT_AGENT_ROLE.to_owned());
            let model_override = self.pending_new_agent_options.take_model();
            let ephemeral = self.pending_new_agent_options.take_ephemeral();
            create_user_agent_prompt(
                self.session_id,
                role,
                text,
                CreateUserAgentPromptOptions {
                    model_override,
                    ephemeral,
                },
            )
        };
        if send_event(self.writer, &event).is_err() {
            return Some(InputLoopExit::Quit);
        }

        None
    }

    fn invalidate_pending_draft(&self) {
        // Submission terminates the in-flight draft window — the buffer just
        // got cleared by the user pressing Enter, so any pending draft is now
        // stale. Invalidate before sending the submission/action so a debounce
        // thread that already took an older snapshot can't emit it afterward.
        invalidate_pending_draft(&self.ctx.draft_handle);
    }

    fn handle_eof(&self) -> Option<InputLoopExit> {
        if !self.ctx.remote_disconnected.load(Ordering::Acquire)
            && self
                .ctx
                .agent_in_progress
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.output.system_info(EOF_DURING_AGENT_NOTICE);
            return None;
        }

        Some(InputLoopExit::Quit)
    }

    fn update_draft(&self) {
        // Trailing-edge debounce: stash the latest buffer
        // contents and wake the debounce thread; it will
        // coalesce a typing burst into one `UiPromptDraft`
        // per `DRAFT_DEBOUNCE` window.
        let text = self.term.handle().get_buffer();
        let (mtx, cv) = &*self.ctx.draft_handle;
        if let Ok(mut g) = mtx.lock() {
            g.pending = Some((
                g.epoch,
                UiPromptDraft {
                    session_id: self.session_id.as_str().into(),
                    text,
                },
            ));
            tracing::trace!(target: "tau_cli::ui", "prompt draft updated");
            cv.notify_one();
        }
    }

    fn toggle_fast_service_tier(&self) {
        // `fast_service_tier_state` is kept in sync by renderer events. Toggling
        // from Fast sends `None` to restore the role/model default; toggling from
        // any other state requests explicit Fast service.
        let enabled = self
            .ctx
            .fast_service_tier_state
            .load(std::sync::atomic::Ordering::Relaxed);
        let service_tier = if enabled {
            None
        } else {
            Some(tau_proto::ServiceTier::Fast)
        };
        self.send_current_role_update(tau_proto::UiRoleUpdateAction::SetServiceTier {
            service_tier,
        });
    }

    fn send_current_role_update(&self, action: tau_proto::UiRoleUpdateAction) {
        let output = &self.output;
        send_current_role_update(
            self.writer,
            &self.ctx.current_role_state,
            action,
            &|message| output.system_info(message),
        );
    }

    fn switch_agent_by_delta(&mut self, delta: isize) {
        let current = self.selected_agent_id();
        let Some(next) = self.ctx.routing.next_active_agent(delta) else {
            self.output
                .system_info("agent-switch: no active agents available yet");
            return;
        };
        if current.as_deref() == Some(next.as_str()) {
            return;
        }
        self.switch_to_agent(next);
    }

    fn switch_to_agent(&mut self, agent_id: String) {
        self.pending_new_agent_options.clear();
        self.ctx.routing.set_selected_agent(Some(agent_id.clone()));
        self.dismiss_completion_menu();
        let _ = self
            .ctx
            .renderer_tx
            .send(RendererCmd::SwitchAgent { agent_id });
    }

    fn dismiss_completion_menu(&mut self) {
        if self.term.dismiss_completion_menu() {
            self.update_draft();
        }
    }

    fn cycle_role_group(&self) {
        if self.agent_is_selected() {
            return;
        }
        let output = &self.output;
        let groups = self
            .ctx
            .role_groups_available
            .lock()
            .map(|groups| groups.clone())
            .unwrap_or_default();
        if groups.is_empty() {
            cycle_role(
                self.writer,
                &self.ctx.current_role_state,
                &self.ctx.roles_available,
                &|message| output.system_info(message),
            );
        } else {
            cycle_role_in_groups(
                self.writer,
                &self.ctx.current_role_state,
                &self.ctx.role_group_memory,
                &groups,
                false,
                &|message| output.system_info(message),
            );
        }
    }

    fn agent_is_selected(&self) -> bool {
        !self.ctx.routing.role_cycling_enabled()
    }

    fn cycle_role_inner(&self) {
        if self.agent_is_selected() {
            return;
        }
        let output = &self.output;
        let groups = self
            .ctx
            .role_groups_available
            .lock()
            .map(|groups| groups.clone())
            .unwrap_or_default();
        if groups.is_empty() {
            return;
        }
        cycle_role_in_groups(
            self.writer,
            &self.ctx.current_role_state,
            &self.ctx.role_group_memory,
            &groups,
            true,
            &|message| output.system_info(message),
        );
    }
}

impl RecordedLineHandlers for TerminalInputSession<'_> {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        TerminalInputSession::handle_known_command(self, text)
    }

    fn handle_dynamic_action(&mut self, text: &str) -> CommandOutcome {
        TerminalInputSession::handle_dynamic_action(self, text)
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        TerminalInputSession::submit_prompt(self, text)
    }

    fn system_info(&mut self, message: &str) {
        self.output.system_info(message);
    }
}

pub(crate) fn role_cycling_enabled(current_agent_state: &Arc<Mutex<Option<String>>>) -> bool {
    current_agent_state
        .lock()
        .ok()
        .and_then(|agent| agent.clone())
        .is_none()
}

pub(crate) fn next_active_agent(
    current: Option<&str>,
    known_agents: &[String],
    live_agents: &std::collections::HashSet<String>,
    suspended_agents: &std::collections::HashSet<String>,
    delta: isize,
) -> Option<String> {
    let active_agents = known_agents
        .iter()
        .filter(|agent| live_agents.contains(*agent) && !suspended_agents.contains(*agent))
        .collect::<Vec<_>>();
    if active_agents.is_empty() {
        return None;
    }
    let len = active_agents.len() as isize;
    let index = current
        .and_then(|current| {
            active_agents
                .iter()
                .position(|agent| agent.as_str() == current)
        })
        .map(|index| (index as isize + delta).rem_euclid(len) as usize)
        .unwrap_or_else(|| {
            if delta < 0 {
                active_agents.len() - 1
            } else {
                0
            }
        });
    Some(active_agents[index].to_string())
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &WriterHandle,
    session_id: &mut String,
    ctx: TerminalInputLoopCtx,
) -> Result<InputLoopExit, CliError> {
    // Cloned `TermHandle` so we can `print_output` for client-side
    // validation errors (`/role engineer effort foo`, `/tree blah`) from this
    // thread without borrowing `term` while the loop also holds
    // `&mut term` for `get_next_event`.
    let output = LocalTerminalOutput::new(term.handle().clone(), ctx.theme.clone());
    TerminalInputSession {
        term,
        writer,
        session_id,
        ctx,
        output,
        pending_new_agent_options: PendingNewAgentOptions::default(),
    }
    .run()
}

pub(crate) fn agent_is_active_in_sets(
    live_agents: &std::collections::HashSet<String>,
    suspended_agents: &std::collections::HashSet<String>,
    agent_id: &str,
) -> bool {
    live_agents.contains(agent_id) && !suspended_agents.contains(agent_id)
}

fn mark_agent_suspended(
    suspended_agents: &Arc<Mutex<std::collections::HashSet<String>>>,
    agent_id: &str,
) {
    if let Ok(mut suspended) = suspended_agents.lock() {
        suspended.insert(agent_id.to_owned());
    }
}

fn mark_agent_resumed(
    live_agents: &Arc<Mutex<std::collections::HashSet<String>>>,
    suspended_agents: &Arc<Mutex<std::collections::HashSet<String>>>,
    agent_id: &str,
) {
    if let Ok(mut live) = live_agents.lock() {
        live.insert(agent_id.to_owned());
    }
    if let Ok(mut suspended) = suspended_agents.lock() {
        suspended.remove(agent_id);
    }
}

fn active_side_agent_count_from_handles(
    live_agents: &Arc<Mutex<std::collections::HashSet<String>>>,
    suspended_agents: &Arc<Mutex<std::collections::HashSet<String>>>,
) -> usize {
    let live = live_agents
        .lock()
        .map(|agents| agents.clone())
        .unwrap_or_default();
    let suspended = suspended_agents
        .lock()
        .map(|agents| agents.clone())
        .unwrap_or_default();
    live.iter()
        .filter(|agent| !suspended.contains(*agent))
        .count()
}

const SESSION_SUBCOMMAND_COMPLETIONS: &[(&str, &str)] = &[("new", "Start a fresh chat session")];

const AGENT_SUBCOMMAND_COMPLETIONS: &[(&str, &str)] = &[
    ("new", "Clear the selected agent"),
    ("switch", "Show a known agent transcript"),
    ("suspend", "Hide an active agent transcript"),
    ("resume", "Show a suspended agent transcript"),
    ("name", "Set an agent display name"),
];

fn build_session_arg_completer() -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    Arc::new(move |args: &[&str]| match args.len() {
        0 | 1 => {
            let needle = args.first().copied().unwrap_or("").to_lowercase();
            SESSION_SUBCOMMAND_COMPLETIONS
                .iter()
                .filter(|(subcommand, _)| completion_matches(subcommand, &needle))
                .map(|(subcommand, description)| CompletionItem::new(*subcommand, *description))
                .collect()
        }
        _ => Vec::new(),
    })
}

fn build_agent_arg_completer(
    routing: InputRoutingState,
    agent_display_names: Arc<Mutex<HashMap<String, String>>>,
) -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    Arc::new(move |args: &[&str]| match args.len() {
        0 | 1 => {
            let needle = args.first().copied().unwrap_or("").to_lowercase();
            AGENT_SUBCOMMAND_COMPLETIONS
                .iter()
                .filter(|(subcommand, _)| completion_matches(subcommand, &needle))
                .map(|(subcommand, description)| CompletionItem::new(*subcommand, *description))
                .collect()
        }
        2 => {
            let known = routing.known_agents();
            let display_names = agent_display_names
                .lock()
                .map(|names| names.clone())
                .unwrap_or_default();
            let active = routing.active_agents();
            let needle = args[1].to_lowercase();
            agent_completion_candidates(args[0], known, active)
                .into_iter()
                .filter(|agent| completion_matches(agent, &needle))
                .map(|agent| {
                    let description = display_names
                        .get(&agent)
                        .cloned()
                        .unwrap_or_else(|| agent.clone());
                    CompletionItem::new(agent, description)
                })
                .collect()
        }
        _ => Vec::new(),
    })
}

fn build_agent_mention_completer(routing: InputRoutingState) -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    Arc::new(move |args: &[&str]| {
        if args.len() != 1 {
            return Vec::new();
        }
        let known = routing.known_agents();
        let active = routing.active_agents();
        let needle = args[0].to_lowercase();
        known
            .into_iter()
            .filter(|agent| active.contains(agent))
            .filter(|agent| completion_matches(agent, &needle))
            .map(|agent| CompletionItem::new(agent, "agent"))
            .collect()
    })
}

fn completion_matches(candidate: &str, needle: &str) -> bool {
    let lower = candidate.to_lowercase();
    needle.is_empty() || lower.starts_with(needle) || lower.contains(needle)
}

fn agent_completion_candidates(
    subcommand: &str,
    known_agents: Vec<String>,
    active_agents: std::collections::HashSet<String>,
) -> Vec<String> {
    match subcommand {
        "switch" => {
            let mut agents: Vec<String> = known_agents
                .into_iter()
                .filter(|agent| active_agents.contains(agent))
                .collect();
            agents.insert(0, "none".to_owned());
            agents
        }
        "suspend" => known_agents
            .into_iter()
            .filter(|agent| active_agents.contains(agent))
            .collect(),
        "resume" => known_agents
            .into_iter()
            .filter(|agent| !active_agents.contains(agent))
            .collect(),
        "name" => known_agents,
        _ => Vec::new(),
    }
}

/// Build the `/theme` argument completer from built-in and user theme names.
fn build_theme_arg_completer(dirs: tau_config::settings::TauDirs) -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    Arc::new(move |args: &[&str]| {
        if args.len() != 1 {
            return Vec::new();
        }
        let needle = args[0].to_lowercase();
        let mut prefix_matches = Vec::new();
        let mut substr_matches = Vec::new();
        for choice in crate::theme::available_theme_choices(&dirs) {
            let lower = choice.name.to_lowercase();
            let item = CompletionItem::new(choice.name, choice.description);
            if needle.is_empty() || lower.starts_with(&needle) {
                prefix_matches.push(item);
            } else if lower.contains(&needle) {
                substr_matches.push(item);
            }
        }
        prefix_matches.extend(substr_matches);
        prefix_matches
    })
}

/// Build the `/set` argument completer. The first arg is a setting
/// name (description = current value); the second arg is one of that
/// setting's allowed values (description = value meaning). Returns
/// no candidates from the third arg onward.
fn build_set_arg_completer(
    cli_state: Arc<Mutex<tau_config::settings::CliState>>,
) -> tau_cli_term::ArgCompleter {
    use tau_cli_term::CompletionItem;

    use crate::settings_registry;

    Arc::new(move |args: &[&str]| match args.len() {
        1 => {
            // Snapshot the current state once so every name's
            // description sees a consistent view.
            let snapshot = cli_state.lock().ok().map(|g| g.clone());
            let needle = args[0].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for def in settings_registry::SETTINGS {
                let lower = def.name.to_lowercase();
                let current = snapshot
                    .as_ref()
                    .map(|s| (def.get)(s))
                    .unwrap_or_else(|| "?".to_owned());
                let description = format!("[{current}] {}", def.description);
                let item = CompletionItem::new(def.name, description);
                if needle.is_empty() || lower.starts_with(&needle) {
                    prefix_matches.push(item);
                } else if lower.contains(&needle) {
                    substr_matches.push(item);
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        }
        2 => {
            let Some(def) = settings_registry::find(args[0]) else {
                return Vec::new();
            };
            let needle = args[1].to_lowercase();
            let mut prefix_matches = Vec::new();
            let mut substr_matches = Vec::new();
            for v in def.values {
                let lower = v.value.to_lowercase();
                let item = CompletionItem::new(v.value, v.description);
                if needle.is_empty() || lower.starts_with(&needle) {
                    prefix_matches.push(item);
                } else if lower.contains(&needle) {
                    substr_matches.push(item);
                }
            }
            prefix_matches.extend(substr_matches);
            prefix_matches
        }
        _ => Vec::new(),
    })
}

fn parsed_action_arguments(
    args: &std::collections::BTreeMap<String, tau_actions::ParsedArgValue>,
) -> CborValue {
    CborValue::Map(
        args.iter()
            .map(|(name, value)| {
                let value = match value {
                    tau_actions::ParsedArgValue::String(value) => CborValue::Text(value.clone()),
                    tau_actions::ParsedArgValue::Integer(value) => {
                        CborValue::Integer((*value).into())
                    }
                };
                (CborValue::Text(name.clone()), value)
            })
            .collect(),
    )
}

/// Parses `/prompt <id>` and returns the configured prompt replacement or a
/// user-visible validation message.
pub(crate) fn custom_prompt_replacement(
    text: &str,
    prompts: &[tau_proto::HarnessCustomPrompt],
) -> Option<Result<String, String>> {
    let mut parts = text.split_whitespace();
    if parts.next() != Some("/prompt") {
        return None;
    }

    let id = parts.next();
    let extra = parts.next();
    match (id, extra) {
        (Some(id), None) => match prompts.iter().find(|prompt| prompt.id == id) {
            Some(prompt) => Some(Ok(prompt.text.clone())),
            None => Some(Err(format!(
                "unknown custom prompt `{id}`{}",
                custom_prompt_hint(prompts)
            ))),
        },
        (None, None) => Some(Err(if prompts.is_empty() {
            "no custom prompts are configured".to_owned()
        } else {
            format!(
                "usage: /prompt <id>; available: {}",
                custom_prompt_ids(prompts)
            )
        })),
        _ => Some(Err("usage: /prompt <id>".to_owned())),
    }
}

fn custom_prompt_hint(prompts: &[tau_proto::HarnessCustomPrompt]) -> String {
    if prompts.is_empty() {
        String::new()
    } else {
        format!("; available: {}", custom_prompt_ids(prompts))
    }
}

fn custom_prompt_ids(prompts: &[tau_proto::HarnessCustomPrompt]) -> String {
    prompts
        .iter()
        .map(|prompt| prompt.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn leading_slash_action(text: &str) -> Option<&str> {
    let command = text.split_whitespace().next()?;
    command.starts_with('/').then_some(command)
}

pub(crate) fn redacted_command_echo_line(text: &str) -> String {
    redact_sensitive_action_line(text).unwrap_or_else(|| text.to_owned())
}

pub(crate) fn redacted_prompt_history_line(line: &str, text: &str) -> String {
    redact_sensitive_action_line(text).unwrap_or_else(|| line.to_owned())
}

fn redact_sensitive_action_line(text: &str) -> Option<String> {
    let mut parts = text.split_whitespace();
    let root = parts.next()?;
    let auth = parts.next()?;
    let provider = parts.next()?;
    let finish = parts.next()?;
    if root == "/email" && auth == "auth" && provider == "google" && finish == "finish" {
        Some("/email auth google finish <redacted>".to_owned())
    } else {
        None
    }
}

fn is_harness_prompt_slash_action(action: &str) -> bool {
    action == "/skill" || action.starts_with("/skill:")
}

fn handle_recorded_line_with_handlers(
    text: &str,
    handlers: &mut impl RecordedLineHandlers,
) -> Result<Option<InputLoopExit>, CliError> {
    match handlers.handle_known_command(text)? {
        CommandOutcome::NotHandled => match handlers.handle_dynamic_action(text) {
            CommandOutcome::NotHandled => {
                // This is only a candidate leading slash-token detector. It must
                // run after CLI-owned commands, dynamic extension actions, and
                // harness-owned prompt commands such as `/skill` are excluded so
                // each owner keeps its routing contract.
                if let Some(action) = leading_slash_action(text)
                    && !is_harness_prompt_slash_action(action)
                {
                    handlers.system_info(&format!("unknown CLI action `{action}`"));
                    Ok(None)
                } else {
                    Ok(handlers.submit_prompt(text))
                }
            }
            CommandOutcome::Continue => Ok(None),
            CommandOutcome::Exit(exit) => Ok(Some(exit)),
        },
        CommandOutcome::Continue => Ok(None),
        CommandOutcome::Exit(exit) => Ok(Some(exit)),
    }
}

pub(crate) fn is_local_slash_command(text: &str) -> bool {
    let command = text.split_whitespace().next().unwrap_or(text);
    if command == "/skill" || command.starts_with("/skill:") {
        return true;
    }
    matches!(
        command,
        "/quit"
            | "/cancel"
            | "/detach"
            | "/session"
            | "/tree"
            | "/compact"
            | "/fast"
            | "/provider-auth"
            | "/agent"
            | "/new"
            | "/name"
            | "/ephemeral"
            | "/suspend"
            | "/resume"
            | "/set"
            | "/theme"
            | "/role"
            | "/prompt"
            | "/model"
            | "/version"
            | "/debug-show-ui-event-stats"
    )
}

/// Parse and dispatch `/set <name> <value>`. Validation lives here
/// (input-loop thread) so the renderer can trust `RendererCmd::Set`
/// to always be a known name and an allowed value.
fn handle_set_command(
    text: &str,
    renderer_tx: &mpsc::Sender<RendererCmd>,
    print_local: &impl Fn(&str),
) {
    use crate::settings_registry;

    let rest = text.strip_prefix("/set").unwrap_or("").trim();
    let mut parts = rest.split_whitespace();
    let name = parts.next();
    let value = parts.next();
    let extra = parts.next();

    let usage = || {
        let names: Vec<&str> = settings_registry::SETTINGS.iter().map(|s| s.name).collect();
        print_local(&format!("/set <name> <value>; names: {}", names.join(", ")));
    };

    let (Some(name), Some(value)) = (name, value) else {
        usage();
        return;
    };
    if extra.is_some() {
        print_local("/set: too many arguments");
        return;
    }
    let Some(def) = settings_registry::find(name) else {
        print_local(&format!("/set: unknown setting `{name}`"));
        return;
    };
    if !(def.validate)(value) {
        let allowed: Vec<&str> = def.values.iter().map(|v| v.value).collect();
        let hint = if allowed.is_empty() {
            def.value_hint.to_owned()
        } else {
            format!("{}; suggested: {}", def.value_hint, allowed.join(", "))
        };
        print_local(&format!("/set {name}: invalid value `{value}` ({hint})"));
        return;
    }
    let _ = renderer_tx.send(RendererCmd::Set {
        name: name.to_owned(),
        value: value.to_owned(),
    });
}

fn handle_role_command(text: &str, writer: &WriterHandle, print_local: &impl Fn(&str)) {
    let rest = text.strip_prefix("/role").unwrap_or("").trim();
    match crate::ui_commands::parse_role_command(rest) {
        Ok(None) => {
            print_local(
                "/role <role> [delete|model|effort|verbosity|thinking-summary|service-tier|compaction-threshold|tools|enable-tool-groups|disable-tool-groups|enable-tools|disable-tools] [value]",
            );
        }
        Ok(Some(event)) => {
            let _ = send_event(writer, &event);
        }
        Err(error) => print_local(&error),
    }
}

fn run_provider_auth(provider: &str, print_local: &impl Fn(&str)) {
    print_local("starting provider registration; follow prompts in the terminal");
    if !provider.is_empty() {
        print_local(
            "provider arguments are no longer accepted; the add flow will prompt for provider kind and name",
        );
    }
    let args = vec!["add".to_owned()];
    match tau_ext_provider_builtin::run_provider_cli(&args) {
        Ok(()) => print_local("provider profile saved; new prompts will use updated credentials"),
        Err(error) => print_local(&format!("provider registration failed: {error}")),
    }
}

fn send_shell_command(
    writer: &WriterHandle,
    session_id: &str,
    command: &str,
    include_in_context: bool,
    target_agent_id: Option<tau_proto::AgentId>,
) -> io::Result<()> {
    send_event(
        writer,
        &crate::ui_events::shell_command(session_id, command, include_in_context, target_agent_id),
    )
}

#[cfg(test)]
mod role_cycle_tests;
