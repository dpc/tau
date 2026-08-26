//! Interactive chat as a socket client of the harness daemon: input
//! loop, draft debouncer, and the threading glue that joins them.

use std::sync::atomic as path_std_sync_atomic;
use std::{cell as path_std_cell, sync as path_std_sync};

use tau_config::settings as path_tau_config_settings;

use crate::{list_agents as path_crate_list_agents, theme as path_crate_theme};

#[cfg(test)]
mod agent_picker_tests;
pub(crate) mod cold_attach_stager;
#[cfg(test)]
mod event_message_tests;
#[cfg(test)]
mod recorded_line_routing_tests;
mod renderer_scheduler;

#[cfg(test)]
mod ui_io_tests;
use std::borrow::Cow;
use std::collections::HashMap;
#[cfg(test)]
use std::io::BufReader;
use std::io::{self, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use cold_attach_stager::{ColdAttachStager, renderer_event_from_delivery};
use renderer_scheduler::{LocalRendererSender, RendererCommandScheduler};
use tau_config::settings::CliBindingAction;
use tau_harness::SessionLaunchStatus;
use tau_proto::{
    CborValue, Disconnect, Event, HarnessInputMessage, HarnessOutputMessage, PeerInputReader,
    PeerOutputWriter, SessionId, UiFocusChanged, UiPromptDraft, UiPromptSubmitted, UnixMicros,
};

use crate::action_commands::ActionCommandState;
use crate::agent_navigation::AgentNavigation;
use crate::daemon::{
    DaemonCliOverrides, DaemonHandle, daemon_output_for_chat_session, resolve_daemon,
    storage_mode_from_ephemeral,
};
use crate::event_renderer::{EventRenderer, ToolTimerNotifier, ToolTimerState, UiIoStats};
use crate::prompt_history::{PromptHistoryAdmission, PromptHistoryStore};
use crate::tool_render::ui_dir_block;
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, PromptCommandHandling,
    create_user_agent_prompt,
};
use crate::{CliError, MUTEX_POISONED, build_banner, locked, ui_logging};

/// Cumulative protocol-I/O accounting shared by UI socket writers and readers.
pub(crate) type UiIoMeter = tau_client::ProtocolIoMeter;
type UiIoCumulativeStats = tau_client::ProtocolIoCumulativeStats;

struct UiIoTracker {
    inner: tau_client::ProtocolIoTracker,
}

impl UiIoTracker {
    fn new(meter: UiIoMeter) -> Self {
        Self {
            inner: tau_client::ProtocolIoTracker::new(meter),
        }
    }

    fn recv_timeout(&self) -> Duration {
        self.inner.recv_timeout()
    }

    fn sample_if_due(&mut self, renderer: &mut EventRenderer) {
        if let Some(stats) = self.inner.sample_if_due() {
            handle_ui_io_sample(stats, renderer);
        }
    }

    fn sample_now(&mut self, renderer: &mut EventRenderer) {
        let stats = self.inner.sample_now();
        handle_ui_io_sample(stats, renderer);
    }
}

fn handle_ui_io_sample(stats: tau_client::ProtocolIoRollingStats, renderer: &mut EventRenderer) {
    log_ui_io_sample_if_yellow(&stats.sample);
    renderer.handle_ui_io_sample(UiIoStats {
        uplink_max_bytes_per_sec: stats.uplink_max_bytes_per_sec,
        downlink_max_bytes_per_sec: stats.downlink_max_bytes_per_sec,
    });
}

fn log_ui_io_sample_if_yellow(sample: &tau_client::ProtocolIoSample) {
    let uplink_yellow = sample.uplink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC;
    let downlink_yellow =
        sample.downlink_bytes >= crate::event_renderer::UI_IO_MEDIUM_BYTES_PER_SEC;
    if !uplink_yellow && !downlink_yellow {
        return;
    }
    let direction = match (uplink_yellow, downlink_yellow) {
        (true, true) => "both",
        (true, false) => "uplink",
        (false, true) => "downlink",
        (false, false) => "none",
    };
    tracing::info!(
        target: "tau_cli::ui_io",
        direction,
        uplink_bytes = sample.uplink_bytes,
        downlink_bytes = sample.downlink_bytes,
        uplink_breakdown = %tau_client::format_protocol_io_breakdown(&sample.uplink_breakdown),
        downlink_breakdown = %tau_client::format_protocol_io_breakdown(&sample.downlink_breakdown),
        "ui io exceeded yellow threshold"
    );
}

/// Serializes and accounts for one interactive UI connection's outbound frames.
pub(crate) struct UiWriter {
    writer: PeerOutputWriter<BufWriter<Box<dyn Write + Send>>>,
    meter: UiIoMeter,
}

impl UiWriter {
    /// Wraps an outbound stream with framed-message encoding and I/O
    /// accounting.
    pub(crate) fn new<W>(writer: W, meter: UiIoMeter) -> Self
    where
        W: Write + Send + 'static,
    {
        Self {
            writer: PeerOutputWriter::new(BufWriter::new(Box::new(writer))),
            meter,
        }
    }

    fn send_frame(
        &mut self,
        message: &HarnessInputMessage,
        diagnostic_seq: Option<u64>,
    ) -> io::Result<()> {
        let hold_started = Instant::now();
        let frame_bytes = self
            .writer
            .write_message_with_size(message)
            .map_err(io::Error::other)?;
        let flush_started = Instant::now();
        self.writer.flush()?;
        let flushed_at = Instant::now();
        let metering_started = Instant::now();
        self.meter.record_uplink_frame_bytes(message, frame_bytes);
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            diagnostic_seq,
            message_kind = tau_client::harness_input_message_name(message),
            encoded_bytes = frame_bytes.get(),
            write_us = flush_started.duration_since(hold_started).as_micros(),
            flush_us = flushed_at.duration_since(flush_started).as_micros(),
            metering_us = metering_started.elapsed().as_micros(),
            total_hold_us = hold_started.elapsed().as_micros(),
            "terminal UI uplink written and flushed"
        );
        Ok(())
    }
}

/// Shared writer handle: the input loop and prompt-draft debounce thread both
/// send events on one socket. The application protocol is a stream of
/// self-delimiting CBOR items, so the mutex serializes each complete item and
/// prevents concurrent producers from interleaving its bytes. This is
/// application-level serialization, not reliance on atomic stream writes or
/// `PIPE_BUF`.
type WriterHandle = Arc<Mutex<UiWriter>>;

/// Wrapping process-local identity for content-free submission diagnostics.
static NEXT_PROMPT_SUBMISSION_DIAGNOSTIC_SEQ: AtomicU64 = AtomicU64::new(1);

const RENDERER_QUEUE_MAX_ITEMS: usize = 1_024;
const RENDERER_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;
static LAST_QUEUE_STALL_WARNING: std::sync::OnceLock<Mutex<Option<Instant>>> =
    path_std_sync::OnceLock::new();

fn admit_queue_stall_warning(now: Instant) -> bool {
    let mut last = LAST_QUEUE_STALL_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect(MUTEX_POISONED);
    if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(5)) {
        return false;
    }
    *last = Some(now);
    true
}

/// Byte permits for the bounded socket-to-renderer queue.
struct RendererByteBudget {
    /// Encoded bytes currently admitted but not dequeued.
    used: Mutex<usize>,
    /// Wakes socket admission after the renderer releases bytes.
    available: Condvar,
}

impl RendererByteBudget {
    fn new() -> Self {
        Self {
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(&self, bytes: usize) {
        self.acquire_inner(bytes, None);
    }

    /// Acquires bytes while notifying a test after observing a full budget.
    #[cfg(test)]
    fn acquire_after_wait_observed(&self, bytes: usize, hook: &mut dyn FnMut()) {
        self.acquire_inner(bytes, Some(hook));
    }

    /// Implements byte admission with an optional one-shot full-budget hook.
    fn acquire_inner(&self, bytes: usize, mut wait_observed: Option<&mut dyn FnMut()>) {
        let mut used = self.used.lock().expect(MUTEX_POISONED);
        while RENDERER_QUEUE_MAX_BYTES.saturating_sub(*used) < bytes {
            if let Some(hook) = wait_observed.take() {
                hook();
            }
            used = self.available.wait(used).expect(MUTEX_POISONED);
        }
        *used += bytes;
    }

    fn release(&self, bytes: usize) -> usize {
        let mut used = self.used.lock().expect(MUTEX_POISONED);
        *used = used.saturating_sub(bytes);
        let remaining = *used;
        self.available.notify_all();
        remaining
    }
}

struct UiConnection {
    /// Harness-to-UI protocol stream.
    read_stream: Box<dyn Read + Send>,
    /// Shared UI-to-harness protocol writer.
    writer: WriterHandle,
    /// Transport-specific active read cancellation used before joining workers.
    shutdown: UiTransportShutdown,
}

/// Active cancellation available for a UI transport's blocking read.
enum UiTransportShutdown {
    /// Parent endpoint paired with the harness's initial standard output.
    InitialStdio(UnixStream),
    /// Clone of an attached harness socket.
    Socket(UnixStream),
}

impl UiTransportShutdown {
    /// Returns the stream used to bound the admission handshake.
    fn stream(&self) -> &UnixStream {
        match self {
            Self::InitialStdio(stream) | Self::Socket(stream) => stream,
        }
    }

    /// Interrupts any blocking transport read before worker joins.
    fn cancel(self) {
        let stream = match self {
            Self::InitialStdio(stream) | Self::Socket(stream) => stream,
        };
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn connect_ui_transport(
    daemon: &mut DaemonHandle,
    ui_io_meter: &UiIoMeter,
    startup_started_at: Instant,
) -> Result<UiConnection, CliError> {
    if let Some(initial_ui) = daemon.take_initial_ui_stdio() {
        tracing::debug!(target: "tau_cli::startup", "using harness stdio for initial UI connection");
        return Ok(UiConnection {
            read_stream: initial_ui.stdout,
            writer: Arc::new(Mutex::new(UiWriter::new(
                initial_ui.stdin,
                ui_io_meter.clone(),
            ))),
            shutdown: UiTransportShutdown::InitialStdio(initial_ui.shutdown_stream.ok_or_else(
                || {
                    CliError::Participant(
                        "owned harness initial UI transport cannot cancel reads".to_owned(),
                    )
                },
            )?),
        });
    }

    let socket_path = daemon.socket_path();
    tracing::debug!(target: "tau_cli::startup", socket_path = %socket_path.display(), "connecting to harness daemon socket");
    let stream = UnixStream::connect(&socket_path)?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "connected to harness daemon socket");
    let read_stream = stream.try_clone()?;
    let shutdown_stream = read_stream.try_clone()?;
    Ok(UiConnection {
        read_stream: Box::new(read_stream),
        writer: Arc::new(Mutex::new(UiWriter::new(stream, ui_io_meter.clone()))),
        shutdown: UiTransportShutdown::Socket(shutdown_stream),
    })
}

/// Lock the writer, write one frame and flush. Returns the underlying
/// `io::Error` on failure so callers can use `?` or discard with
/// `let _ = …`.
fn send_frame(writer: &WriterHandle, message: &HarnessInputMessage) -> io::Result<()> {
    send_frame_with_diagnostic(writer, message, None)
}

/// Sends one frame with optional process-local diagnostic correlation.
fn send_frame_with_diagnostic(
    writer: &WriterHandle,
    message: &HarnessInputMessage,
    diagnostic_seq: Option<u64>,
) -> io::Result<()> {
    let started = Instant::now();
    let mut writer = locked(writer);
    let acquired = Instant::now();
    let result = writer.send_frame(message, diagnostic_seq);
    let writer_total_hold_us = acquired.elapsed().as_micros();
    drop(writer);
    tracing::trace!(
        target: "tau_cli::frontend_progress",
        diagnostic_seq,
        message_kind = tau_client::harness_input_message_name(message),
        writer_wait_us = acquired.duration_since(started).as_micros(),
        writer_total_hold_us,
        writer_total_us = started.elapsed().as_micros(),
        "terminal UI writer lock lifecycle"
    );
    result
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
    let mut reader = PeerInputReader::new(read_stream);
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
pub(crate) fn send_event(writer: &WriterHandle, event: &Event) -> io::Result<()> {
    send_frame(writer, &durable_emit_message(event))
}

/// Sends the production cancellation event through the direct uplink.
fn send_cancel_prompt_frame(
    writer: &WriterHandle,
    session_id: &tau_proto::SessionId,
    target_agent_id: Option<tau_proto::AgentId>,
) -> io::Result<()> {
    send_event(
        writer,
        &crate::ui_events::cancel_prompt(session_id, target_agent_id),
    )
}

/// Send the point-to-point connection-control request used by `:detach`.
fn send_ui_detach_request(writer: &WriterHandle) -> io::Result<()> {
    send_frame(
        writer,
        &HarnessInputMessage::UiDetachRequest(tau_proto::UiDetachRequest {}),
    )
}

/// Consume `:detach`, send its connection-control request, and select the
/// daemon-preserving exit path.
fn handle_ui_detach_command_text(text: &str, writer: &WriterHandle) -> Option<InputLoopExit> {
    if text != ":detach" {
        return None;
    }
    // If the write fails we still exit — the daemon will notice the disconnect
    // and fall back to its default behavior.
    let _ = send_ui_detach_request(writer);
    Some(InputLoopExit::Detach)
}

/// Wrap an event in the interactive UI's durable-by-default Emit message.
fn durable_emit_message(event: &Event) -> HarnessInputMessage {
    HarnessInputMessage::emit(event.clone())
}

/// Moves a one-shot event into the interactive UI's durable Emit message.
fn durable_emit_message_owned(event: Event) -> HarnessInputMessage {
    HarnessInputMessage::emit(event)
}

fn format_ui_io_cumulative_stats(stats: &UiIoCumulativeStats) -> String {
    tau_client::format_protocol_io_cumulative_stats(
        "UI event I/O cumulative stats",
        "uplink",
        "downlink",
        "no UI frames recorded yet",
        stats,
    )
}

fn format_ui_io_stats(meter: &UiIoMeter) -> String {
    let cumulative = format_ui_io_cumulative_stats(&meter.cumulative_stats());
    let diagnostics = meter.format_diagnostics();
    if diagnostics.is_empty() {
        cumulative
    } else {
        format!("{cumulative}\n\n{diagnostics}")
    }
}

fn handle_debug_show_ui_event_stats_command_text(
    text: &str,
    meter: &UiIoMeter,
    mut command_feedback: impl FnMut(&str),
) -> bool {
    if text == ":debug-show-ui-event-stats" {
        command_feedback(&format_ui_io_stats(meter));
        return true;
    }
    if text.starts_with(":debug-show-ui-event-stats ") {
        command_feedback(":debug-show-ui-event-stats takes no arguments");
        return true;
    }
    false
}

const DEBUG_SHOW_EVENT_STATS_USAGE: &str = ":debug-show-event-stats <extension>";

fn parse_debug_show_event_stats_command(
    text: &str,
) -> Result<Option<HarnessInputMessage>, &'static str> {
    let mut parts = text.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(None);
    };
    if command != ":debug-show-event-stats" {
        return Ok(None);
    }
    let Some(extension_name) = parts.next() else {
        return Err(DEBUG_SHOW_EVENT_STATS_USAGE);
    };
    if parts.next().is_some() {
        return Err(DEBUG_SHOW_EVENT_STATS_USAGE);
    }
    Ok(Some(HarnessInputMessage::UiDebugEventStatsRequest(
        tau_proto::UiDebugEventStatsRequest {
            extension_name: tau_proto::ExtensionName::parse(extension_name)
                .map_err(|_| DEBUG_SHOW_EVENT_STATS_USAGE)?,
        },
    )))
}

fn handle_debug_show_event_stats_command_text(
    text: &str,
    writer: &WriterHandle,
    mut show_usage: impl FnMut(&str),
) -> bool {
    match parse_debug_show_event_stats_command(text) {
        Ok(Some(message)) => {
            let _ = send_frame(writer, &message);
            true
        }
        Ok(None) => false,
        Err(usage) => {
            show_usage(usage);
            true
        }
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
) -> Option<String> {
    if groups.is_empty() {
        print_local("cycle-role: no agent roles are available yet");
        return None;
    }
    let current = current_role_state.lock().ok().and_then(|role| role.clone());
    let mut memory = role_group_memory
        .lock()
        .map(|memory| memory.clone())
        .unwrap_or_default();
    remember_group_role(&mut memory, groups, current.as_deref());
    let Some(next) = next_role_in_groups(current.as_deref(), groups, alternate, &memory) else {
        print_local("cycle-role: no agent roles are available yet");
        return None;
    };
    remember_group_role(&mut memory, groups, Some(&next));
    if let Ok(mut shared_memory) = role_group_memory.lock() {
        *shared_memory = memory;
    }
    let selected = next.clone();
    let _ = send_event(
        writer,
        &Event::UiRoleSelect(tau_proto::UiRoleSelect { role: next }),
    );
    Some(selected)
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
) -> Option<String> {
    let roles = match roles_available.lock() {
        Ok(roles) => roles.clone(),
        Err(_) => Vec::new(),
    };
    if roles.is_empty() {
        print_local("cycle-role: no agent roles are available yet");
        return None;
    }
    let current = current_role_state.lock().ok().and_then(|role| role.clone());
    let next = match current
        .as_deref()
        .and_then(|current| roles.iter().position(|role| role == current))
    {
        Some(index) => roles[(index + 1) % roles.len()].clone(),
        None => roles[0].clone(),
    };
    let selected = next.clone();
    let _ = send_event(
        writer,
        &Event::UiRoleSelect(tau_proto::UiRoleSelect { role: next }),
    );
    Some(selected)
}

/// Debounce period for `UiPromptDraft` emission while the user is
/// typing. Kept generous on purpose: the only consumer today
/// (std-notifications) only cares about second-or-better resolution
/// to bump its idle deadline.
const DRAFT_DEBOUNCE: Duration = Duration::from_secs(1);
const EOF_DURING_AGENT_NOTICE: &str =
    "An agent is still running; use :quit to terminate the session in progress.";
const TREE_NAVIGATION_USAGE: &str = ":tree: use a prompt anchor, `root`, or explicit `node <id>`";

fn parse_agent_picker_command(
    text: &str,
) -> Option<Result<crate::list_agents::AgentPickerFilter, &'static str>> {
    let mut args = text.split_whitespace();
    let filter = match args.next()? {
        ":pick-agent" => path_crate_list_agents::AgentPickerFilter::Active,
        ":pick-agent-all" => path_crate_list_agents::AgentPickerFilter::All,
        _ => return None,
    };
    Some(if args.next().is_none() {
        Ok(filter)
    } else {
        Err("agent picker commands take no arguments")
    })
}

const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    (":quit", "Exit the chat session"),
    (":cancel", "Cancel the current in-flight prompt"),
    (
        ":retry",
        "Run the selected agent's delayed provider retry now",
    ),
    (
        ":detach",
        "Leave the UI but keep the harness running for later reattach",
    ),
    (
        ":pick-agent",
        "Pick a currently active agent with optional fzf",
    ),
    (
        ":pick-agent-all",
        "Pick any current live agent with optional fzf",
    ),
    (
        ":model",
        "Switch selected agent model (e.g. :model openai/gpt-5)",
    ),
    (":agent", "Manage agent transcript navigation"),
    (
        ":new",
        "Start a new agent, optionally with a role (`:new reviewer`)",
    ),
    (":name", "Alias for :agent name on the selected agent"),
    (
        ":ephemeral",
        "Stage the next :new agent as memory-only (:ephemeral on|off)",
    ),
    (":suspend", "Alias for :agent suspend on the selected agent"),
    (":resume", "Alias for :agent resume on the selected agent"),
    (":role", "Switch, create, edit, or delete an agent role"),
    (
        ":prompt",
        "Replace the editor with a configured custom prompt template",
    ),
    (
        ":skill",
        "Invoke a user-invocable skill (e.g. :skill jujutsu optional args)",
    ),
    (
        ":session",
        "Manage chat sessions (e.g. :session new starts a fresh session)",
    ),
    (
        ":session-stats",
        "Print flat token totals for the current session",
    ),
    (
        ":tree",
        "Print prompt rewind anchors (`:tree <anchor>` rewinds before that prompt)",
    ),
    (
        ":compact",
        "Force a provider-side compaction pass on the current session",
    ),
    (":fast", "Toggle Fast mode"),
    (
        ":verbose-mode-toggle",
        "Toggle compact conversation and verbose diagnostic display",
    ),
    (
        ":set",
        "Set a UI setting (e.g. :set show-diff true); Tab cycles names + values",
    ),
    (
        ":theme",
        "Switch this UI's theme for this run; Tab cycles available themes",
    ),
    (":version", "Print Tau version and build information"),
    (
        ":provider-auth",
        "Add or replace a provider profile (runs `tau provider add [kind]`)",
    ),
    (
        ":debug-show-ui-event-stats",
        "Print cumulative UI event byte/count counters for this client",
    ),
    (
        ":debug-show-event-stats",
        "Request cumulative protocol byte/count counters for an extension",
    ),
];

/// Single-slot mailbox the input loop pushes the latest prompt
/// snapshot into; the debounce thread drains it. `pending = None` +
/// `done = false` means "nothing to send, keep waiting"; `done =
/// true` is the shutdown signal.
#[derive(Default)]
pub(crate) struct DraftSlot {
    /// Latest snapshot paired with the draft epoch captured at enqueue time.
    ///
    /// A target change or submission advances [`Self::epoch`] and clears this
    /// slot, so the debounce worker rejects a snapshot from the former draft.
    pub(crate) pending: Option<(u64, UiPromptDraft)>,
    /// Whether prompt-draft snapshots include the current prompt buffer.
    pub(crate) send_content: bool,
    /// Generation incremented whenever pending draft data becomes stale.
    pub(crate) epoch: u64,
    /// Shutdown request that wakes and terminates the debounce worker.
    pub(crate) done: bool,
}

/// Shared handle for the debounce mailbox. Wakeups are coordinated
/// via the `Condvar`; the debounce thread waits on it for new drafts
/// or a shutdown signal, the input loop notifies it on every
/// `BufferChanged`.
type DraftHandle = Arc<(Mutex<DraftSlot>, Condvar)>;

/// Send the first queued draft immediately, then coalesce later edits into the
/// latest snapshot sent at most once per `DRAFT_DEBOUNCE`. The sleep is
/// interruptible via the `done` shutdown signal so process exit is prompt.
///
/// Never drops a notification: a draft pushed during the
/// sleep stays in the slot and is sent on the next iteration.
fn debounce_loop(handle: DraftHandle, writer: WriterHandle) {
    debounce_loop_with_period(handle, writer, DRAFT_DEBOUNCE);
}

/// Runs the prompt-draft coalescer with its caller-selected period.
fn debounce_loop_with_period(handle: DraftHandle, writer: WriterHandle, period: Duration) {
    debounce_loop_with_wait(handle, writer, |handle| {
        let (mtx, cv) = handle.as_ref();
        let g = locked(mtx);
        let (g, _timed_out) = cv
            .wait_timeout_while(g, period, |s| !s.done)
            .expect(MUTEX_POISONED);
        !(g.done && g.pending.is_none())
    });
}

/// Runs the prompt-draft coalescer and delegates the post-send boundary wait.
///
/// Production waits one debounce period; tests use an explicit boundary to
/// verify ordering without treating wall-clock scheduling as behavior.
pub(crate) fn debounce_loop_with_wait(
    handle: DraftHandle,
    writer: WriterHandle,
    mut wait_after_send: impl FnMut(&DraftHandle) -> bool,
) {
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
        if let Some((epoch, draft)) = snapshot {
            // Best-effort: a write failure means the socket is gone,
            // and the input loop will notice on its next write.
            let _ = send_draft_snapshot_with_before_writer(
                &writer,
                handle.as_ref(),
                epoch,
                draft,
                || {},
            );
        }
        if !wait_after_send(&handle) {
            return;
        }
    }
}

/// Sends an eligible draft after revalidating it inside the serialized writer
/// boundary.
///
/// `before_writer` exposes the validation-to-writer boundary to deterministic
/// concurrency tests. Lock ordering is always writer then draft slot here.
/// Invalidation only holds the draft slot and releases it before prompt
/// submission acquires the writer, so this nesting cannot form a lock cycle.
pub(crate) fn send_draft_snapshot_with_before_writer(
    writer: &WriterHandle,
    handle: &(Mutex<DraftSlot>, Condvar),
    epoch: u64,
    draft: UiPromptDraft,
    before_writer: impl FnOnce(),
) -> io::Result<bool> {
    if !should_send_draft_snapshot(handle, epoch) {
        return Ok(false);
    }
    before_writer();
    let mut writer = locked(writer);
    if !should_send_draft_snapshot(handle, epoch) {
        return Ok(false);
    }
    writer.send_frame(&durable_emit_message(&Event::UiPromptDraft(draft)), None)?;
    Ok(true)
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

/// Queue a debounced prompt-draft snapshot with the currently viewed agent
/// target captured by the caller.
pub(crate) fn queue_prompt_draft_snapshot(
    handle: &(Mutex<DraftSlot>, Condvar),
    session_id: SessionId,
    target_agent_id: Option<tau_proto::AgentId>,
    text: String,
) {
    let (mtx, cv) = handle;
    if let Ok(mut g) = mtx.lock() {
        let text = g.send_content.then(|| {
            let canonical = tau_cli_term::canonical_literal_colon_prompt(text.trim());
            let classification_text = canonical.as_deref().unwrap_or_else(|| text.trim());
            redact_sensitive_action_line(classification_text).unwrap_or(text)
        });
        g.pending = Some((
            g.epoch,
            UiPromptDraft {
                session_id,
                target_agent_id,
                text,
            },
        ));
        cv.notify_one();
    }
}

/// Start a new draft epoch and queue the current buffer under a new viewed
/// agent target.
pub(crate) fn retarget_prompt_draft_snapshot(
    handle: &(Mutex<DraftSlot>, Condvar),
    session_id: SessionId,
    target_agent_id: Option<tau_proto::AgentId>,
    text: String,
) {
    invalidate_pending_draft(handle);
    queue_prompt_draft_snapshot(handle, session_id, target_agent_id, text);
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

/// Enqueues one decoded delivery while preserving its decode correlation and
/// original decoded event box.
fn enqueue_remote_delivery(
    delivery: cold_attach_stager::RendererDelivery,
    remote_tx: &mpsc::SyncSender<RendererCmd>,
    renderer_byte_budget: &RendererByteBudget,
    queued_remote_items: &path_std_sync_atomic::AtomicUsize,
    renderer_arbiter: &Mutex<()>,
    remote_admitted: &path_std_sync_atomic::AtomicU64,
) -> bool {
    let delivery_id = delivery.delivery_id;
    let queue_bytes = delivery.queue_bytes;
    let presentation = delivery.presentation;
    let abandoned_shell_starts = delivery.abandoned_shell_starts;
    let cmd = RendererCmd::Remote {
        event: delivery.event,
        presentation,
        abandoned_shell_starts,
        recorded_at: delivery.recorded_at,
        delivery_id,
        queue_bytes,
        enqueued_at: Instant::now(),
    };
    let enqueue_started = Instant::now();
    tracing::trace!(
        target: "tau_cli::frontend_progress",
        delivery_id,
        frame_bytes = queue_bytes,
        "renderer event admission started"
    );
    renderer_byte_budget.acquire(queue_bytes);
    let queued_items = queued_remote_items.fetch_add(1, Ordering::AcqRel) + 1;
    {
        let _guard = renderer_arbiter.lock().expect(MUTEX_POISONED);
        remote_admitted.fetch_add(1, Ordering::AcqRel);
    }
    if remote_tx.send(cmd).is_err() {
        renderer_byte_budget.release(queue_bytes);
        queued_remote_items.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    tracing::trace!(
        target: "tau_cli::frontend_progress",
        delivery_id,
        enqueue_wait_us = enqueue_started.elapsed().as_micros(),
        queue_items = queued_items,
        queue_bytes = *renderer_byte_budget.used.lock().expect(MUTEX_POISONED),
        "renderer event enqueued"
    );
    true
}

pub(crate) fn run_chat(
    session_id: &tau_proto::SessionId,
    attach: bool,
    session_status: SessionLaunchStatus,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<(), CliError> {
    use tau_cli_term::{CommandCompletion, HighTerm};

    let (startup_profile, ui_logging, mut daemon, startup_started_at) = start_chat_daemon(
        session_id,
        attach,
        session_status,
        startup_role,
        cli_overrides,
        ephemeral,
    )?;
    let harness_socket_path = daemon.socket_path();
    let ui_io_meter = UiIoMeter::with_diagnostics();
    let UiConnection {
        mut read_stream,
        writer,
        shutdown,
    } = connect_ui_transport(&mut daemon, &ui_io_meter, startup_started_at)?;

    // UI connection handshake.
    send_handshake_frame(
        &writer,
        &mut read_stream,
        &crate::ui_client::hello_message(
            tau_proto::ExtensionName::parse("tau-chat")
                .expect("chat UI name must satisfy the extension identifier grammar"),
            Some(session_id),
        ),
    )?;
    let socket_reader_input = await_ui_session_admission(
        read_stream,
        session_id.clone(),
        Some(shutdown.stream()),
        UI_SESSION_ADMISSION_TIMEOUT,
    )?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "verified UI session admission");
    send_frame(&writer, &crate::ui_client::chat_subscribe_message())?;
    tracing::debug!(target: "tau_cli::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "sent subscribe");

    // The socket reader feeds a bounded remote FIFO. Local renderer commands
    // use a separate channel, while direct input/cancel uplink bypasses both.
    // Each local command captures a finite remote admission watermark. The
    // renderer drains that prefix before the local command, then services later
    // remote arrivals; socket disconnect stays at the FIFO tail.
    let remote_admitted = Arc::new(path_std_sync_atomic::AtomicU64::new(0));
    let renderer_arbiter = Arc::new(Mutex::new(()));
    let (event_tx, event_rx) =
        LocalRendererSender::channel(remote_admitted.clone(), renderer_arbiter.clone());
    let (remote_tx, remote_rx) = mpsc::sync_channel::<RendererCmd>(RENDERER_QUEUE_MAX_ITEMS);
    let renderer_byte_budget = Arc::new(RendererByteBudget::new());
    let socket_renderer_byte_budget = renderer_byte_budget.clone();
    let queued_remote_items = Arc::new(path_std_sync_atomic::AtomicUsize::new(0));
    let socket_queued_remote_items = queued_remote_items.clone();
    let socket_remote_admitted = remote_admitted.clone();
    let socket_renderer_arbiter = renderer_arbiter.clone();
    let input_shutdown_handle: Arc<Mutex<Option<tau_cli_term::TermHandle>>> =
        Arc::new(Mutex::new(None));
    let socket_input_shutdown = input_shutdown_handle.clone();
    let remote_disconnected = Arc::new(AtomicBool::new(false));
    let socket_remote_disconnected = remote_disconnected.clone();
    let socket_ui_io_meter = ui_io_meter.clone();
    let local_disconnect_started = Arc::new(AtomicBool::new(false));
    let socket_local_disconnect_started = local_disconnect_started.clone();
    let socket_reader = std::thread::spawn(move || {
        let mut cold_attach_stager = if attach {
            ColdAttachStager::staging()
        } else {
            ColdAttachStager::pass_through()
        };
        let next_delivery_id = path_std_cell::Cell::new(1_u64);
        let allocate_delivery_id = || {
            let delivery_id = next_delivery_id.get();
            next_delivery_id.set(delivery_id.saturating_add(1));
            delivery_id
        };
        let notify_disconnect = |reason: Option<String>, delivery_id: u64, queue_bytes: usize| {
            socket_remote_disconnected.store(true, Ordering::Release);
            if let Some(handle) = socket_input_shutdown
                .lock()
                .expect(MUTEX_POISONED)
                .as_ref()
                .cloned()
            {
                handle.request_input_shutdown();
            }
            let enqueue_started = Instant::now();
            tracing::trace!(
                target: "tau_cli::frontend_progress",
                delivery_id,
                frame_bytes = queue_bytes,
                "remote disconnect admission started"
            );
            socket_renderer_byte_budget.acquire(queue_bytes);
            let queued_items = socket_queued_remote_items.fetch_add(1, Ordering::AcqRel) + 1;
            {
                let _guard = socket_renderer_arbiter.lock().expect(MUTEX_POISONED);
                socket_remote_admitted.fetch_add(1, Ordering::AcqRel);
            }
            let cmd = RendererCmd::RemoteDisconnect {
                reason,
                delivery_id,
                queue_bytes,
                enqueued_at: Instant::now(),
            };
            if remote_tx.send(cmd).is_err() {
                socket_renderer_byte_budget.release(queue_bytes);
                socket_queued_remote_items.fetch_sub(1, Ordering::AcqRel);
            } else {
                tracing::trace!(
                    target: "tau_cli::frontend_progress",
                    delivery_id,
                    enqueue_wait_us = enqueue_started.elapsed().as_micros(),
                    frame_bytes = queue_bytes,
                    queue_items = queued_items,
                    "remote disconnect enqueued"
                );
            }
        };
        let drain_staging = |stager: &mut ColdAttachStager| {
            stager
                .finish_before_disconnect()
                .into_iter()
                .all(|delivery| {
                    enqueue_remote_delivery(
                        delivery,
                        &remote_tx,
                        &socket_renderer_byte_budget,
                        &socket_queued_remote_items,
                        &socket_renderer_arbiter,
                        &socket_remote_admitted,
                    )
                })
        };
        let mut reader = socket_reader_input;
        loop {
            let delivery_id = allocate_delivery_id();
            let read_started = Instant::now();
            tracing::trace!(
                target: "tau_cli::frontend_progress",
                delivery_id,
                "socket read and decode started"
            );
            match reader.read_message_with_size() {
                Ok(Some(decoded)) => {
                    let message = decoded.message;
                    let frame_bytes = decoded.encoded_bytes;
                    let read_elapsed = read_started.elapsed();
                    let queue_bytes = usize::try_from(frame_bytes.get()).unwrap_or(usize::MAX);
                    tracing::trace!(
                        target: "tau_cli::frontend_progress",
                        delivery_id,
                        read_decode_us = read_elapsed.as_micros(),
                        frame_bytes = queue_bytes,
                        "socket frame read and decoded"
                    );
                    socket_ui_io_meter.record_downlink_frame_bytes(&message, frame_bytes);
                    let deliveries = match message {
                        HarnessOutputMessage::Deliver(delivery) => {
                            let Some(delivery) =
                                renderer_event_from_delivery(delivery, queue_bytes, delivery_id)
                            else {
                                continue;
                            };
                            cold_attach_stager.admit(delivery)
                        }
                        HarnessOutputMessage::Disconnect(d) => {
                            if socket_local_disconnect_started.load(Ordering::Acquire) {
                                return;
                            }
                            if !drain_staging(&mut cold_attach_stager) {
                                return;
                            }
                            notify_disconnect(d.reason, delivery_id, queue_bytes);
                            return;
                        }
                        _ => continue,
                    };
                    for delivery in deliveries {
                        if !enqueue_remote_delivery(
                            delivery,
                            &remote_tx,
                            &socket_renderer_byte_budget,
                            &socket_queued_remote_items,
                            &socket_renderer_arbiter,
                            &socket_remote_admitted,
                        ) {
                            return;
                        }
                    }
                }
                Ok(None) => {
                    if !socket_local_disconnect_started.load(Ordering::Acquire) {
                        if !drain_staging(&mut cold_attach_stager) {
                            return;
                        }
                        notify_disconnect(
                            Some("harness connection closed".to_owned()),
                            delivery_id,
                            0,
                        );
                    }
                    return;
                }
                Err(error) => {
                    if !socket_local_disconnect_started.load(Ordering::Acquire) {
                        tracing::warn!(target: "tau_cli::ui", %error, "socket reader exiting");
                        if !drain_staging(&mut cold_attach_stager) {
                            return;
                        }
                        notify_disconnect(
                            Some(format!("harness connection error: {error}")),
                            delivery_id,
                            0,
                        );
                    }
                    return;
                }
            }
        }
    });

    // Terminal setup.
    let commands: Vec<CommandCompletion> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, description)| CommandCompletion::new(*name, *description))
        .collect();
    let action_state = ActionCommandState::new(BUILTIN_COMMANDS.iter().map(|(name, _)| *name));
    // Fail fast on a malformed `cli.yaml`. The fields here drive
    // keybindings, prompt symbol, cursor shape, and theme — silently
    // falling back to defaults would leave the user with broken
    // keybindings or unreadable colors and no clue why. Refuse to
    // start the TUI instead.
    let dirs = path_tau_config_settings::TauDirs::default();
    let settings = tau_config::settings::load_cli_settings_in(&dirs)
        .map_err(|error| CliError::Participant(format!("cli.yaml failed to parse:\n{error}")))?;
    let theme = crate::theme::select_theme(&dirs, settings.theme.clone())
        .map_err(|error| CliError::Participant(format!("cli theme failed to load:\n{error}")))?;
    let prompt = crate::theme::active_prompt_marker(&theme, &settings.prompt_symbol, None);
    let cwd = std::env::current_dir()?;
    let home_dir = dirs::home_dir();
    let right_prompt =
        crate::theme::right_prompt_context(&theme, &cwd, home_dir.as_deref(), session_id.as_str());
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
        path_tau_config_settings::CliState::load_with_default(&dirs, settings.default_state());
    let prompt_history = PromptHistoryStore::new(&dirs);
    let input_history = match prompt_history.load() {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(target: "tau_cli::ui", %error, "failed to load persistent prompt history");
            Vec::new()
        }
    };
    let terminal_options = terminal_options_from_settings(&settings);
    let (mut term, handle, completion_data) = HighTerm::new_with_completion_rules(
        prompt,
        commands,
        theme.clone(),
        bindings,
        input_history,
        completion_rules,
        terminal_options,
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
    let draft_handle: DraftHandle = Arc::new((
        Mutex::new(DraftSlot {
            send_content: settings.send_prompt_draft_content,
            ..DraftSlot::default()
        }),
        Condvar::new(),
    ));
    let active_session_state = Arc::new(Mutex::new(session_id.to_owned()));

    // Event renderer thread — drains the channel and renders via
    // the thread-safe TermHandle.
    let renderer_handle = handle.clone();
    let renderer_rx = event_rx;
    // Pre-build the renderer so we can grab its shared state handles
    // for the input loop. CLI config provides the default UI toggle values;
    // persisted `cli.json` state overrides them so `:set show-*` changes
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
    renderer.set_startup_profile_selection(startup_profile);
    renderer.set_osc8_links(settings.osc8_links);
    renderer.set_draft_retargeter(draft_handle.clone(), active_session_state.clone());
    renderer.set_right_prompt_paths(cwd.clone(), home_dir.clone());
    renderer.set_action_state(action_state.clone());
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new(":skill"),
        renderer.skill_arg_completer(),
    );
    let tool_timer = ToolTimerNotifier::new();
    renderer.set_tool_timer(tool_timer.clone());
    let timer_tx = event_tx.clone();
    let timer_state = tool_timer.inner();
    let timer_thread = std::thread::spawn(move || tool_timer_loop(timer_state, timer_tx));
    // Register `:set`'s context-aware arg completer. The first-arg
    // menu shows each setting's *current* value (read through the
    // renderer's shared mirror), and the second-arg menu shows
    // value-with-meaning for the selected setting.
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new(":set"),
        build_set_arg_completer(renderer.cli_state_mirror()),
    );
    let agent_in_progress = renderer.agent_in_progress_state();
    let fast_service_tier_state = renderer.fast_service_tier_state();
    let current_role_state = renderer.current_role_state();
    let current_agent_state = renderer.current_agent_state();
    let known_agents = renderer.known_agents();
    let agent_display_names = renderer.agent_display_names();
    let agent_navigation = renderer.agent_navigation();
    let ephemeral_agents = renderer.ephemeral_agents();
    let agent_estimated_api_costs = renderer.agent_estimated_api_costs();
    let input_routing = InputRoutingState::new(
        current_agent_state.clone(),
        known_agents.clone(),
        agent_navigation,
        ephemeral_agents.clone(),
    );
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new(":agent"),
        build_agent_arg_completer(input_routing.clone(), agent_display_names.clone()),
    );
    completion_data
        .set_agent_mention_completer(build_agent_mention_completer(input_routing.clone()));
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new(":session"),
        build_session_arg_completer(),
    );
    completion_data.set_arg_completer(
        tau_cli_term::CommandName::new(":theme"),
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
        let mut scheduler = RendererCommandScheduler::new(remote_rx, renderer_rx, renderer_arbiter);
        loop {
            let cmd =
                scheduler.recv_timeout(ui_io_tracker.recv_timeout().min(Duration::from_millis(10)));
            match cmd {
                Ok(cmd) => {
                    match cmd {
                        RendererCmd::Remote {
                            event,
                            presentation,
                            abandoned_shell_starts,
                            recorded_at,
                            delivery_id,
                            queue_bytes,
                            enqueued_at,
                        } => {
                            let queue_items =
                                queued_remote_items.fetch_sub(1, Ordering::AcqRel) - 1;
                            let remaining_bytes = renderer_byte_budget.release(queue_bytes);
                            let queue_age = enqueued_at.elapsed();
                            tracing::trace!(
                                target: "tau_cli::frontend_progress",
                                delivery_id,
                                queue_age_ms = queue_age.as_millis(),
                                queue_items,
                                queue_bytes = remaining_bytes,
                                "renderer event dequeued"
                            );
                            if Duration::from_millis(500) <= queue_age
                                && admit_queue_stall_warning(Instant::now())
                            {
                                tracing::warn!(
                                    target: "tau_cli::frontend_progress",
                                    delivery_id,
                                    queue_age_ms = queue_age.as_millis(),
                                    queue_items,
                                    queue_bytes = remaining_bytes,
                                    "renderer queue stalled"
                                );
                            }
                            renderer.abandon_shell_starts(&abandoned_shell_starts);
                            match presentation {
                                cold_attach_stager::RendererPresentation::Ordinary => {
                                    renderer.handle_socket_delivery(
                                        &event,
                                        recorded_at,
                                        delivery_id,
                                    );
                                }
                                cold_attach_stager::RendererPresentation::StandaloneShellTerminal => {
                                    let Event::ShellCommandFinished(finished) = &*event else {
                                        unreachable!(
                                            "standalone shell presentation requires a shell terminal"
                                        );
                                    };
                                    renderer.handle_standalone_socket_shell_finished(
                                        finished,
                                        recorded_at,
                                        delivery_id,
                                    );
                                }
                                cold_attach_stager::RendererPresentation::ReconstructedToolStart {
                                    owner,
                                } => {
                                    assert!(
                                        matches!(&*event, Event::ToolStarted(started) if started.agent_id == owner),
                                        "reconstructed start presentation requires its validated owner"
                                    );
                                    renderer.handle_reconstructed_tool_start_socket_delivery(
                                        &event,
                                        &owner,
                                        recorded_at,
                                        delivery_id,
                                    );
                                }
                            }
                        }
                        RendererCmd::RemoteDisconnect {
                            reason,
                            delivery_id,
                            queue_bytes,
                            enqueued_at,
                        } => {
                            let queue_items =
                                queued_remote_items.fetch_sub(1, Ordering::AcqRel) - 1;
                            let remaining_bytes = renderer_byte_budget.release(queue_bytes);
                            tracing::trace!(
                                target: "tau_cli::frontend_progress",
                                delivery_id,
                                queue_age_ms = enqueued_at.elapsed().as_millis(),
                                queue_items,
                                queue_bytes = remaining_bytes,
                                "remote disconnect dequeued"
                            );
                            renderer.handle_disconnect(reason);
                        }
                        RendererCmd::Set { name, value } => renderer.apply_setting(&name, &value),
                        RendererCmd::ToggleVerboseMode => renderer.toggle_verbose_mode(),
                        RendererCmd::SwitchAgent { agent_id } => renderer.switch_agent(agent_id),
                        RendererCmd::ClearSelectedAgent => renderer.clear_selected_agent(),
                        RendererCmd::SetTheme { theme } => renderer.apply_theme(theme),
                        RendererCmd::ShowSessionStats => renderer.show_session_token_stats(),
                        RendererCmd::ActionInvoked {
                            invocation_id,
                            owner_agent_id,
                        } => renderer.record_action_invocation(invocation_id, owner_agent_id),
                        RendererCmd::ToolTimerTick => renderer.handle_tool_timer_tick(),
                    }
                    ui_io_tracker.sample_if_due(&mut renderer);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => ui_io_tracker.sample_now(&mut renderer),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if scheduler.remote_closed() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    });

    // Spawn the prompt-draft debounce thread. The input loop signals
    // it on every `BufferChanged` event with the latest buffer
    // contents; the thread coalesces a typing burst into one
    // `UiPromptDraft` per `DRAFT_DEBOUNCE` window and sends it on the
    // shared writer.
    let debounce_thread = {
        let handle = draft_handle.clone();
        let writer = writer.clone();
        std::thread::spawn(move || debounce_loop(handle, writer))
    };

    // Terminal input loop — shares the writer with the debounce
    // thread via `WriterHandle`. Theme clone is for printing local
    // validation errors (e.g. `:role engineer effort foo`) through the same
    // TermHandle as remote events, so they don't garble the TUI like
    // `eprintln!` would.
    let mut active_session_id = session_id.to_owned();
    tracing::info!(target: "tau_cli::ui", "terminal UI input ready");
    let input_result = terminal_input_loop(
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
            active_session_state,
            editor_context,
            action_state,
            draft_handle: draft_handle.clone(),
            prompt_history,
            custom_prompts,
            ui_io_meter: ui_io_meter.clone(),
            harness_socket_path,
            agent_estimated_api_costs,
        },
    );
    let (exit, foreground_ownership_error) = match input_result {
        Ok(exit) => (exit, None),
        Err(CliError::ForegroundOwnershipUnconfirmed(message)) => {
            (InputLoopExit::ForegroundOwnershipUnconfirmed, Some(message))
        }
        Err(error) => return Err(error),
    };

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
        shutdown,
        socket_reader,
        renderer_thread,
        exit,
        local_disconnect_started,
    );
    finish_daemon_for_exit(exit, daemon);

    tracing::info!(target: "tau_cli::ui", reason, "terminal UI exiting");

    match foreground_ownership_error {
        Some(message) => Err(CliError::ForegroundOwnershipUnconfirmed(message)),
        None => Ok(()),
    }
}

/// Translates static CLI settings into the immutable raw-terminal policy.
pub(crate) fn terminal_options_from_settings(
    settings: &path_tau_config_settings::CliSettings,
) -> tau_cli_term::TerminalOptions {
    tau_cli_term::TerminalOptions {
        cursor_shape: if settings.bar_cursor {
            tau_cli_term::CursorShape::Bar
        } else {
            tau_cli_term::CursorShape::Block
        },
        mouse: settings.mouse,
    }
}

/// Starts or attaches the daemon and establishes process-local UI logging.
fn start_chat_daemon(
    session_id: &tau_proto::SessionId,
    attach: bool,
    session_status: SessionLaunchStatus,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<
    (
        Option<tau_config::settings::ProfileSelection>,
        ui_logging::UiLogging,
        DaemonHandle,
        Instant,
    ),
    CliError,
> {
    let startup_profile = (!attach).then(|| cli_overrides.profile.cloned()).flatten();
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
        session_id = %session_id,
        attach,
        "terminal UI starting"
    );
    let startup_started_at = Instant::now();
    let storage_mode = storage_mode_from_ephemeral(ephemeral);
    let daemon_output = (!attach)
        .then(|| daemon_output_for_chat_session(session_id.as_str(), storage_mode, session_status))
        .transpose()?;
    let daemon = resolve_daemon(
        attach,
        session_id.as_str(),
        session_status,
        daemon_output,
        startup_role,
        cli_overrides,
        storage_mode,
    )?;
    Ok((startup_profile, ui_logging, daemon, startup_started_at))
}

/// Performs the blocking admission read on an owned thread, retaining its
/// buffered reader for normal delivery and bounding a peer that withholds ACK.
fn await_ui_session_admission(
    read_stream: Box<dyn Read + Send>,
    expected_session_id: tau_proto::SessionId,
    shutdown_stream: Option<&UnixStream>,
    timeout: Duration,
) -> Result<crate::ui_client::UiInputReader, CliError> {
    let reader = PeerInputReader::new(read_stream);
    crate::ui_client::await_ui_session_admission(
        reader,
        expected_session_id,
        shutdown_stream.and_then(|stream| stream.try_clone().ok()),
        timeout,
    )
    .map_err(CliError::Io)
}

/// How the input loop ended. Controls daemon disposition on exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputLoopExit {
    /// User typed `:quit`, hit Ctrl-D, or the socket dropped. The
    /// daemon should be killed (if we own it) or just disconnected
    /// from (if we were attached).
    Quit,
    /// User typed `:detach`. We leave the daemon running whether we
    /// spawned it or attached to it.
    Detach,
    /// Foreground ownership is unconfirmed, so only this attachment may exit.
    ForegroundOwnershipUnconfirmed,
}

impl InputLoopExit {
    fn reason(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Detach => "detach",
            Self::ForegroundOwnershipUnconfirmed => "foreground-ownership-unconfirmed",
        }
    }

    fn harness_disconnect_reason(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Detach | Self::ForegroundOwnershipUnconfirmed => "detach",
        }
    }

    fn daemon_disposition(self) -> DaemonDisposition {
        match self {
            Self::Quit => DaemonDisposition::StopOwned,
            Self::Detach | Self::ForegroundOwnershipUnconfirmed => DaemonDisposition::KeepRunning,
        }
    }
}

fn shutdown_ui_connection(
    writer: WriterHandle,
    shutdown: UiTransportShutdown,
    socket_reader: std::thread::JoinHandle<()>,
    renderer_thread: std::thread::JoinHandle<()>,
    exit: InputLoopExit,
    local_disconnect_started: Arc<AtomicBool>,
) -> &'static str {
    let reason = exit.reason();
    local_disconnect_started.store(true, Ordering::Release);
    send_ui_exit_frames(exit, &writer);

    // Drop the writer, then actively cancel the read transport so the reader
    // cannot wait on a daemon that intentionally survives detach.
    drop(writer);
    shutdown.cancel();

    join_ui_thread(socket_reader, "socket reader");
    join_ui_thread(renderer_thread, "renderer");
    reason
}

fn send_ui_exit_frames(exit: InputLoopExit, writer: &WriterHandle) {
    if exit == InputLoopExit::ForegroundOwnershipUnconfirmed {
        let _ = send_ui_detach_request(writer);
    }
    let _ = send_frame(
        writer,
        &HarnessInputMessage::Disconnect(Disconnect {
            reason: Some(exit.harness_disconnect_reason().to_owned()),
        }),
    );
}

fn join_ui_thread(handle: std::thread::JoinHandle<()>, name: &str) {
    if handle.join().is_err() {
        tracing::warn!(target: "tau_cli::ui", name, "UI worker thread panicked during shutdown");
    }
}

fn finish_daemon_for_exit(exit: InputLoopExit, daemon: DaemonHandle) {
    // On detach, we explicitly leak the daemon child (if we own one) so it
    // outlives this process. `DaemonHandle::Drop` would otherwise kill the
    // child we spawned; `:detach` is exactly the case where we want it to keep
    // running.
    match exit.daemon_disposition() {
        DaemonDisposition::StopOwned => drop(daemon),
        DaemonDisposition::KeepRunning => daemon.leak(),
    }
}

/// Daemon action selected after the terminal input loop ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonDisposition {
    /// Stop a daemon owned by this CLI process.
    StopOwned,
    /// Leave the daemon available after this attachment disconnects.
    KeepRunning,
}

fn tool_timer_loop(state: Arc<(Mutex<ToolTimerState>, Condvar)>, renderer_tx: LocalRendererSender) {
    let (mutex, cv) = &*state;
    let mut guard = locked(mutex);
    loop {
        while guard.active_tool_ids.is_empty() && !guard.quota_active && !guard.done {
            guard = cv.wait(guard).expect(MUTEX_POISONED);
        }
        if guard.done {
            return;
        }
        let interval = if guard.active_tool_ids.is_empty() {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(1)
        };
        let (next_guard, timeout) = cv.wait_timeout(guard, interval).expect(MUTEX_POISONED);
        guard = next_guard;
        if guard.done {
            return;
        }
        if (!guard.active_tool_ids.is_empty() || guard.quota_active)
            && timeout.timed_out()
            && renderer_tx.send(RendererCmd::ToolTimerTick).is_err()
        {
            return;
        }
    }
}

/// Commands drained from separate remote and local renderer channels.
///
/// The bounded remote FIFO has precedence over an already-admitted prefix.
/// Local commands remain independent from socket admission, while prompt and
/// cancel uplink bypass renderer work entirely.
enum RendererCmd {
    /// Toggle the process-local top-level transcript presentation mode.
    ToggleVerboseMode,
    /// `:set <name> <value>` — validated by the input loop before send.
    Set {
        /// Registered CLI setting name.
        name: String,
        /// Validated serialized setting value.
        value: String,
    },
    /// `:agent switch <agent_id>` — switch visible known agent transcript.
    SwitchAgent {
        /// Stable local agent identity to display.
        agent_id: String,
    },
    /// Return to the start-new-agent prompt state.
    ClearSelectedAgent,
    /// `:theme <name>` — apply a theme to this UI process only.
    SetTheme {
        /// Fully resolved process-local theme.
        theme: tau_themes::Theme,
    },
    /// `:session-stats` — print flat totals after admitted remote events.
    ShowSessionStats,
    /// Dynamic extension action was invoked from the current viewed transcript.
    ActionInvoked {
        /// Correlation identity for the later action result.
        invocation_id: tau_proto::ActionInvocationId,
        /// Transcript that owns the action result.
        owner_agent_id: Option<String>,
    },
    /// One decoded harness delivery admitted to the bounded remote FIFO.
    Remote {
        /// Original decoded event allocation moved intact from
        /// `RendererDelivery` for renderer interpretation.
        event: Box<Event>,
        /// Presentation-only interpretation derived during cold-attach staging.
        presentation: cold_attach_stager::RendererPresentation,
        /// Presentation-only shell starts disproved by the attach snapshot.
        abandoned_shell_starts: Vec<cold_attach_stager::ShellStartPresentation>,
        /// Harness-provided observation time.
        recorded_at: UnixMicros,
        /// Process-local content-free stage correlation.
        delivery_id: u64,
        /// Encoded frame bytes charged to the queue budget.
        queue_bytes: usize,
        /// Monotonic queue admission time.
        enqueued_at: Instant,
    },
    ToolTimerTick,
    /// The harness disconnected after every earlier remote FIFO item.
    RemoteDisconnect {
        /// Optional harness-provided disconnect reason.
        reason: Option<String>,
        /// Process-local content-free stage correlation.
        delivery_id: u64,
        /// Encoded frame bytes charged to the queue budget.
        queue_bytes: usize,
        /// Monotonic queue admission time.
        enqueued_at: Instant,
    },
}

struct TerminalInputLoopCtx {
    fast_service_tier_state: Arc<path_std_sync::atomic::AtomicBool>,
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
    agent_in_progress: Arc<path_std_sync::atomic::AtomicBool>,
    remote_disconnected: Arc<AtomicBool>,
    renderer_tx: LocalRendererSender,
    active_session_state: Arc<Mutex<tau_proto::SessionId>>,
    editor_context: Arc<Mutex<tau_cli_term::EditorContext>>,
    action_state: ActionCommandState,
    draft_handle: DraftHandle,
    prompt_history: PromptHistoryStore,
    custom_prompts: Arc<Mutex<Vec<tau_proto::HarnessCustomPrompt>>>,
    ui_io_meter: UiIoMeter,
    /// Socket used for requester-directed roster snapshots.
    harness_socket_path: std::path::PathBuf,
    /// Canonical cumulative costs projected by the event renderer.
    agent_estimated_api_costs: crate::estimated_cost::AgentCostProjection,
}

#[derive(Clone)]
struct InputRoutingState {
    current_agent_state: Arc<Mutex<Option<String>>>,
    known_agents: Arc<Mutex<Vec<String>>>,
    agent_navigation: Arc<Mutex<AgentNavigation>>,
    ephemeral_agents: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl InputRoutingState {
    fn new(
        current_agent_state: Arc<Mutex<Option<String>>>,
        known_agents: Arc<Mutex<Vec<String>>>,
        agent_navigation: Arc<Mutex<AgentNavigation>>,
        ephemeral_agents: Arc<Mutex<std::collections::HashSet<String>>>,
    ) -> Self {
        Self {
            current_agent_state,
            known_agents,
            agent_navigation,
            ephemeral_agents,
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
        self.agent_navigation
            .lock()
            .map(|navigation| navigation.active_agents())
            .unwrap_or_default()
    }

    fn live_agents(&self) -> std::collections::HashSet<String> {
        self.agent_navigation
            .lock()
            .map(|navigation| navigation.live_agents())
            .unwrap_or_default()
    }

    fn active_count(&self) -> usize {
        self.agent_navigation
            .lock()
            .map(|navigation| navigation.active_count())
            .unwrap_or_default()
    }

    fn agent_is_known(&self, agent_id: &str) -> bool {
        self.known_agents
            .lock()
            .map(|agents| agents.iter().any(|known| known == agent_id))
            .unwrap_or(false)
    }

    fn agent_is_active(&self, agent_id: &str) -> bool {
        self.agent_navigation
            .lock()
            .map(|navigation| navigation.is_active(agent_id))
            .unwrap_or(false)
    }

    fn agent_is_ephemeral(&self, agent_id: &str) -> bool {
        self.ephemeral_agents
            .lock()
            .map(|agents| agents.contains(agent_id))
            .unwrap_or(false)
    }

    fn known_agent_reference(&self, reference: &str) -> Result<tau_proto::AgentId, String> {
        let agent_id = tau_proto::AgentId::parse_reference(reference)
            .map_err(|error| format!("invalid agent id `{reference}`: {error}"))?;
        if !self.agent_is_known(agent_id.as_str()) {
            return Err(format!("unknown agent: {agent_id}"));
        }
        Ok(agent_id)
    }

    fn resolve_agent_command_target(
        &self,
        target: Option<&str>,
        fallback: Option<String>,
    ) -> Result<Option<tau_proto::AgentId>, String> {
        target
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(|target| self.known_agent_reference(target))
            .transpose()
            .map(|target| {
                target.or_else(|| {
                    fallback.map(|agent_id| {
                        tau_proto::AgentId::parse(agent_id).expect("UI stores valid agent ids")
                    })
                })
            })
    }

    fn agent_switch_target(&self, target: Option<&str>) -> Result<Option<String>, String> {
        let Some(arg) = target.map(str::trim).filter(|arg| !arg.is_empty()) else {
            return Err(":agent switch <agent_id|none>".to_owned());
        };
        if arg == "none" {
            return Ok(None);
        }
        self.known_agent_reference(arg)
            .map(tau_proto::AgentId::into_string)
            .map(Some)
    }

    fn next_agent_cycle_selection(&self, delta: isize) -> Option<String> {
        let current = self.selected_agent_id();
        let known = self.known_agents();
        let active = self
            .agent_navigation
            .lock()
            .map(|navigation| navigation.active_agents())
            .unwrap_or_default();
        next_agent_cycle_selection(current.as_deref(), &known, &active, delta)
    }

    fn role_cycling_enabled(&self) -> bool {
        role_cycling_enabled(&self.current_agent_state)
    }
}

fn handle_agent_suspend_command(
    routing: &InputRoutingState,
    target: Option<&str>,
    print_local: &impl Fn(&str),
) -> Option<tau_proto::AgentId> {
    let target = match routing.resolve_agent_command_target(target, routing.selected_agent_id()) {
        Ok(target) => target,
        Err(message) => {
            print_local(&message);
            return None;
        }
    };
    let Some(agent_id) = target else {
        print_local(":agent suspend <agent_id>");
        return None;
    };
    Some(agent_id)
}

fn handle_agent_resume_command(
    routing: &InputRoutingState,
    target: Option<&str>,
    print_local: &impl Fn(&str),
) -> Option<tau_proto::AgentId> {
    let fallback = routing
        .selected_agent_id()
        .filter(|agent_id| !routing.agent_is_active(agent_id));
    let target = match routing.resolve_agent_command_target(target, fallback) {
        Ok(target) => target,
        Err(message) => {
            print_local(&message);
            return None;
        }
    };
    let Some(agent_id) = target else {
        print_local(":agent resume <agent_id>");
        return None;
    };
    Some(agent_id)
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

    fn command_feedback(&self, message: &str) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        self.handle.print_output(
            "system-info",
            themed_block(
                &self.theme,
                names::SYSTEM_INFO,
                format!("{}{}", crate::transcript_markers::NOTICE, message),
            ),
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
/// `Exit` carries the daemon-disposition decision for `:quit` and `:detach`.
enum CommandOutcome {
    NotHandled,
    Continue,
    Exit(InputLoopExit),
}

/// Prepared dynamic action dispatch and matching renderer owner update.
struct DynamicActionInvocation {
    /// Event sent to the harness/extension action provider.
    event: Event,
    /// Renderer command that records the invocation-time viewed transcript.
    renderer_cmd: RendererCmd,
}

enum NewAliasCommandEffect<'a> {
    StartNewAgent { role: Option<&'a str> },
    Usage(&'static str),
}

trait RecordedLineHandlers {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError>;
    fn handle_dynamic_action(&mut self, text: &str) -> CommandOutcome;
    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit>;
    fn command_feedback(&mut self, message: &str);
}

/// Side effects driven by submitted-line orchestration.
///
/// Implementations may use raw `routing_text` only for command/action ownership
/// and ephemeral-target routing classification. Presentation and persistence
/// owners may retain only `presentation_text`.
trait SubmittedLineHandlers: RecordedLineHandlers {
    fn replace_last_submitted_prompt(&mut self, text: String);
    fn record_prompt_line(&mut self, record: SubmittedLineRecord<'_>);
    fn is_known_command_or_action(&self, text: &str) -> bool;
    fn command_echo(&mut self, text: &str);
    fn submit_literal_prompt(&mut self, text: &str) -> Option<InputLoopExit>;
}

/// Named text views passed to submitted history and editor recording.
struct SubmittedLineRecord<'a> {
    /// Original or literal-canonical history candidate used only when the
    /// presentation is non-sensitive.
    history_fallback: &'a str,
    /// Safe text retained by history and editor presentation owners.
    presentation_text: &'a str,
    /// Raw text limited to ownership and ephemeral-target routing decisions.
    routing_text: &'a str,
}

/// Mutable state for one terminal input loop invocation.
///
/// Keeping the borrows and owned context together lets each command-family
/// helper stay small while still sharing the same writer, session id, draft
/// mailbox, and local output path as the old monolithic loop.
struct TerminalInputSession<'a> {
    term: &'a mut tau_cli_term::HighTerm,
    writer: &'a WriterHandle,
    session_id: &'a mut tau_proto::SessionId,
    ctx: TerminalInputLoopCtx,
    output: LocalTerminalOutput,
    pending_new_agent_options: PendingNewAgentOptions,
    /// Current line's wrapping process-local diagnostic identity.
    prompt_diagnostic_seq: Option<u64>,
}

/// One-shot options staged while the UI is in new-agent mode, as governed by
/// `SPEC-tau-cli-new-agent-staging`.
#[derive(Default)]
struct PendingNewAgentOptions {
    /// Optional role override for the next created agent.
    ///
    /// This is a latency bridge between `:new <role>` and the asynchronous
    /// `harness.role_selected` echo, not a second durable role authority.
    role: Option<String>,
    /// Optional model override for the next created agent.
    model: Option<tau_proto::ModelId>,
    /// Whether the next created agent should be memory-only.
    ephemeral: bool,
}

impl PendingNewAgentOptions {
    fn stage_role(&mut self, role: impl Into<String>) {
        self.role = Some(role.into());
    }

    fn take_role(&mut self) -> Option<String> {
        self.role.take()
    }

    fn clear_role(&mut self) {
        self.role = None;
    }

    fn apply_model_selection(
        &mut self,
        session_id: &tau_proto::SessionId,
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
            self.stage_model(model);
            None
        }
    }

    fn stage_model(&mut self, model: tau_proto::ModelId) {
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
        self.role = None;
        self.model = None;
        self.ephemeral = false;
    }
}

fn new_alias_role(text: &str) -> Result<Option<&str>, &'static str> {
    let rest = text.strip_prefix(":new").unwrap_or("").trim();
    let mut parts = rest.split_whitespace();
    let role = parts.next();
    if parts.next().is_some() {
        return Err(":new [role]");
    }
    Ok(role)
}

fn new_alias_command_effect(text: &str) -> NewAliasCommandEffect<'_> {
    match new_alias_role(text) {
        Ok(role) => NewAliasCommandEffect::StartNewAgent { role },
        Err(usage) => NewAliasCommandEffect::Usage(usage),
    }
}

fn stage_role_selection_for_new_agent(
    pending: &mut PendingNewAgentOptions,
    has_selected_agent: bool,
    event: &Event,
) {
    if has_selected_agent {
        return;
    }
    if let Event::UiRoleSelect(select) = event {
        pending.stage_role(select.role.clone());
    }
}

fn take_new_agent_role(pending: &mut PendingNewAgentOptions, current_role: String) -> String {
    pending.take_role().unwrap_or(current_role)
}

fn tree_command_message(
    session_id: &tau_proto::SessionId,
    target_agent_id: Option<tau_proto::AgentId>,
    text: &str,
) -> Result<Option<HarnessInputMessage>, &'static str> {
    if text == ":tree" {
        return Ok(Some(crate::ui_events::tree_request_message(
            session_id,
            target_agent_id,
        )));
    }
    if let Some(arg) = text.strip_prefix(":tree ") {
        let target = crate::ui_commands::parse_tree_navigation_target(arg)
            .map_err(|()| TREE_NAVIGATION_USAGE)?;
        return Ok(Some(HarnessInputMessage::emit(
            crate::ui_events::navigate_tree(session_id, target_agent_id, target),
        )));
    }
    Ok(None)
}

fn handle_tree_command_text(
    session_id: &tau_proto::SessionId,
    target_agent_id: Option<tau_proto::AgentId>,
    text: &str,
    writer: &WriterHandle,
    mut show_error: impl FnMut(&str),
) -> bool {
    match tree_command_message(session_id, target_agent_id, text) {
        Ok(Some(message)) => {
            let _ = send_frame(writer, &message);
            true
        }
        Ok(None) => false,
        Err(message) => {
            show_error(message);
            true
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct AgentDisplayNameRequest {
    agent_id: tau_proto::AgentId,
    display_name: String,
}

impl AgentDisplayNameRequest {
    fn from_agent_command(rest: &str, routing: &InputRoutingState) -> Result<Self, String> {
        let usage = ":agent name <agent_id> <display_name>";
        let rest = rest
            .strip_prefix("name")
            .ok_or_else(|| usage.to_owned())?
            .trim();
        let (agent_id, display_name) = rest
            .split_once(char::is_whitespace)
            .ok_or_else(|| usage.to_owned())?;
        let agent_id = agent_id.trim();
        let display_name = display_name.trim();
        if agent_id.is_empty() || display_name.is_empty() {
            return Err(usage.to_owned());
        }
        Ok(Self {
            agent_id: routing.known_agent_reference(agent_id)?,
            display_name: display_name.to_owned(),
        })
    }

    fn event(&self, session_id: &tau_proto::SessionId) -> Event {
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
    let display_name = text.strip_prefix(":name").unwrap_or("").trim();
    if display_name.is_empty() {
        return Err(":name <display_name>".to_owned());
    }
    let Some(agent_id) = selected_agent_id else {
        return Err(
            ":name requires a selected agent; use :agent switch <agent_id> or :agent name <agent_id> <display_name>"
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
    mut command_feedback: impl FnMut(&str),
) -> bool {
    if text != ":ephemeral" && !text.starts_with(":ephemeral ") {
        return false;
    }
    let Some(rest) = text.strip_prefix(":ephemeral") else {
        return false;
    };
    let rest = rest.trim();
    if !rest.is_empty() && !matches!(rest, "on" | "off") || text.split_whitespace().count() > 2 {
        command_feedback(":ephemeral [on|off]");
        return true;
    }
    if has_selected_agent {
        command_feedback("Use :new first; :ephemeral controls only the next new agent.");
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
        command_feedback("next agent will be ephemeral (forgotten when this daemon exits)");
    } else {
        command_feedback("next agent will be persistent");
    }
    true
}

impl<'a> TerminalInputSession<'a> {
    fn run(&mut self) -> Result<InputLoopExit, CliError> {
        loop {
            let event = self.term.get_next_event().map_err(|error| {
                if tau_cli_term::is_foreground_ownership_unconfirmed(&error) {
                    CliError::ForegroundOwnershipUnconfirmed(error.to_string())
                } else {
                    CliError::Io(error)
                }
            })?;
            if let Ok(active_session) = self.ctx.active_session_state.lock()
                && self.session_id.as_str() != active_session.as_str()
            {
                self.session_id.clone_from(&active_session);
            }
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
                self.handle_non_exit_event(other)?;
                Ok(None)
            }
        }
    }

    fn handle_non_exit_event(&mut self, event: tau_cli_term::Event) -> Result<(), CliError> {
        use tau_cli_term::Event as TermEvent;

        // These events update local UI/session state only; none of them can
        // terminate the input loop, unlike submitted lines and EOF.

        match event {
            TermEvent::Resize { .. } => {
                tracing::debug!(target: "tau_cli::ui", "terminal resized");
            }
            TermEvent::FocusChanged { focused } => self.send_focus_changed(focused),
            TermEvent::BufferChanged => self.update_draft(),
            TermEvent::Action(action) => self.handle_binding_action(&action)?,
            TermEvent::BackTab => self.cycle_role_group(),
            TermEvent::Escape => self.recall_queued_prompt(),
            TermEvent::Line(_) | TermEvent::Eof | TermEvent::CancelPrompt => {}
        }
        Ok(())
    }

    fn handle_binding_action(&mut self, action: &str) -> Result<(), CliError> {
        match action {
            "fast-toggle" => self.toggle_fast_service_tier(),
            "verbose-mode-toggle" => {
                let _ = self.ctx.renderer_tx.send(RendererCmd::ToggleVerboseMode);
            }
            "cycle-role" => self.cycle_role_inner(),
            "cycle-role-group" => self.cycle_role_group(),
            "agent-previous" => self.switch_agent_by_delta(-1),
            "agent-next" => self.switch_agent_by_delta(1),
            "agent-pick" => {
                return self.pick_agent(path_crate_list_agents::AgentPickerFilter::Active);
            }
            "agent-pick-all" => {
                return self.pick_agent(path_crate_list_agents::AgentPickerFilter::All);
            }
            _ => self
                .output
                .command_feedback(&format!("binding: unknown application action `{action}`")),
        }
        Ok(())
    }

    fn send_focus_changed(&self, focused: bool) {
        let _ = send_event(
            self.writer,
            &Event::UiFocusChanged(UiFocusChanged {
                session_id: self.session_id.clone(),
                focused,
            }),
        );
    }

    fn recall_queued_prompt(&self) {
        let _ = send_event(
            self.writer,
            &Event::UiRecallQueuedPrompt(tau_proto::UiRecallQueuedPrompt {
                session_id: self.session_id.clone(),
                target_agent_id: self.selected_side_agent_id(),
            }),
        );
    }

    fn handle_line(&mut self, line: &str) -> Result<Option<InputLoopExit>, CliError> {
        let started = Instant::now();
        let diagnostic_seq = NEXT_PROMPT_SUBMISSION_DIAGNOSTIC_SEQ.fetch_add(1, Ordering::Relaxed);
        self.prompt_diagnostic_seq = Some(diagnostic_seq);
        let result = handle_submitted_line_with_handlers(line, self);
        self.prompt_diagnostic_seq = None;
        tracing::trace!(
            target: "tau_cli::prompt_submission",
            diagnostic_seq,
            stage = "chat_history_routing",
            prompt_bytes = line.len(),
            stage_us = started.elapsed().as_micros(),
            "content-free prompt submission stage"
        );
        result
    }

    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        // Keep session-lifecycle commands first: `:quit` and `:detach` exit
        // immediately, while `:session new` mutates `session_id` for later
        // commands and prompt submission.
        let outcome = self.handle_session_command(text)?;
        if !matches!(outcome, CommandOutcome::NotHandled) {
            return Ok(outcome);
        }
        if let Some(command) = parse_agent_picker_command(text) {
            match command {
                Ok(filter) => self.pick_agent(filter)?,
                Err(message) => self.output.command_feedback(message),
            }
            return Ok(CommandOutcome::Continue);
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
        let Some(result) = custom_prompt_replacement_from_snapshot(text, || {
            self.ctx
                .custom_prompts
                .lock()
                .map(|prompts| prompts.clone())
                .unwrap_or_default()
        }) else {
            return false;
        };
        match result {
            Ok(prompt) => {
                let cursor = prompt.len();
                self.term.handle().set_buffer(prompt, cursor);
                self.term.handle().redraw();
                self.update_draft();
            }
            Err(message) => self.output.command_feedback(&message),
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
            |message| self.output.command_feedback(message),
        )
    }

    fn record_prompt_line_if_persistent_with_routing(
        &self,
        line: &str,
        text: &str,
        routing_text: &str,
    ) {
        let started = Instant::now();
        let history_line = redacted_prompt_history_line(line, text);
        if self.prompt_line_targets_ephemeral_agent(routing_text) {
            if let Ok(mut context) = self.ctx.editor_context.lock() {
                context.previous_prompt = Some(history_line.into_owned());
            }
            tracing::trace!(
                target: "tau_cli::prompt_submission",
                diagnostic_seq = self.prompt_diagnostic_seq,
                stage = "chat_history",
                history_admission = "ephemeral",
                stage_us = started.elapsed().as_micros(),
                "content-free prompt-history stage"
            );
            return;
        }
        let admission = self.ctx.prompt_history.append(&history_line);
        match admission {
            PromptHistoryAdmission::Queued | PromptHistoryAdmission::IgnoredEmpty => {}
            PromptHistoryAdmission::DroppedFull => {
                tracing::warn!(
                    target: "tau_cli::ui",
                    "dropped prompt history because the persistence queue is full"
                );
            }
            PromptHistoryAdmission::DroppedUnavailable => {
                tracing::warn!(
                    target: "tau_cli::ui",
                    "dropped prompt history because persistence is unavailable"
                );
            }
        }
        if let Ok(mut context) = self.ctx.editor_context.lock() {
            context.previous_prompt = Some(history_line.into_owned());
        }
        tracing::trace!(
            target: "tau_cli::prompt_submission",
            diagnostic_seq = self.prompt_diagnostic_seq,
            stage = "chat_history",
            history_admission = admission.diagnostic_class(),
            stage_us = started.elapsed().as_micros(),
            "content-free prompt-history stage"
        );
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
            is_known_static_command(text) || self.ctx.action_state.is_known_action_line(text),
        )
    }

    fn handle_session_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        if text == ":quit" {
            return Ok(CommandOutcome::Exit(InputLoopExit::Quit));
        }
        if text == ":cancel" {
            self.send_cancel_prompt();
            return Ok(CommandOutcome::Continue);
        }
        if text == ":retry" {
            let _ = send_event(
                self.writer,
                &crate::ui_events::retry_prompt(self.session_id, self.selected_side_agent_id()),
            );
            return Ok(CommandOutcome::Continue);
        }
        if text
            .strip_prefix(":retry")
            .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_whitespace))
        {
            self.output.command_feedback("usage: :retry");
            return Ok(CommandOutcome::Continue);
        }
        if let Some(exit) = handle_ui_detach_command_text(text, self.writer) {
            // Tell the harness to stay alive after we leave, then exit the UI.
            return Ok(CommandOutcome::Exit(exit));
        }
        if text == ":session" || text.starts_with(":session ") {
            self.handle_session_namespace(text)?;
            return Ok(CommandOutcome::Continue);
        }

        Ok(CommandOutcome::NotHandled)
    }

    fn handle_session_namespace(&mut self, text: &str) -> Result<(), CliError> {
        let rest = text.strip_prefix(":session").unwrap_or("").trim();
        let mut parts = rest.split_whitespace();
        let subcommand = parts.next();
        let extra = parts.next();
        match (subcommand, extra) {
            (Some("new"), None) => self.start_new_session(),
            (None, None) => {
                self.output.command_feedback(":session new");
                Ok(())
            }
            _ => {
                self.output.command_feedback(":session new");
                Ok(())
            }
        }
    }

    fn start_new_session(&mut self) -> Result<(), CliError> {
        let cwd = std::env::current_dir()?;
        let new_id = crate::daemon::mint_session_id(&cwd);
        *self.session_id = new_id;
        if let Ok(mut active_session) = self.ctx.active_session_state.lock() {
            *active_session = self.session_id.clone();
        }
        self.term
            .handle()
            .set_right_prompt(crate::theme::right_prompt_context(
                &self.ctx.theme,
                &self.ctx.cwd,
                self.ctx.home_dir.as_deref(),
                self.session_id,
            ));
        let _ = send_event(
            self.writer,
            &Event::UiSwitchSession(tau_proto::UiSwitchSession {
                new_session_id: self.session_id.clone(),
                reason: tau_proto::SessionStartReason::New,
            }),
        );
        self.pending_new_agent_options.clear();
        self.clear_selected_agent();
        Ok(())
    }

    fn send_cancel_prompt(&self) {
        let target_agent_id = self.selected_side_agent_id();
        tracing::info!(
            target: "tau_cli::frontend_progress",
            target_agent_id = target_agent_id.as_ref().map(ToString::to_string),
            "cancel input received and target resolved"
        );
        let _ = send_cancel_prompt_frame(self.writer, self.session_id, target_agent_id);
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            "cancel uplink finished"
        );
    }

    fn selected_side_agent_id(&self) -> Option<tau_proto::AgentId> {
        self.ctx.routing.selected_side_agent_id()
    }

    fn handle_tree_or_compact_command(&self, text: &str) -> bool {
        self.handle_tree_command(text) || self.handle_compact_command(text)
    }

    fn handle_tree_command(&self, text: &str) -> bool {
        handle_tree_command_text(
            self.session_id,
            self.selected_side_agent_id(),
            text,
            self.writer,
            |message| self.output.command_feedback(message),
        )
    }

    fn handle_compact_command(&self, text: &str) -> bool {
        if text == ":compact" {
            let _ = send_event(
                self.writer,
                &crate::ui_events::compact_request(self.session_id, self.selected_side_agent_id()),
            );
            return true;
        }
        if text.starts_with(":compact ") {
            self.output
                .command_feedback(":compact forces a compaction pass and takes no arguments");
            return true;
        }
        false
    }

    fn handle_role_setting_shortcut(&self, text: &str) -> bool {
        self.handle_fast_shortcut(text)
    }

    fn handle_fast_shortcut(&self, text: &str) -> bool {
        if text == ":fast" {
            self.toggle_fast_service_tier();
            return true;
        }
        if text.starts_with(":fast ") {
            self.output.command_feedback(":fast toggles Fast mode");
            return true;
        }
        false
    }

    fn handle_utility_command(&mut self, text: &str) -> bool {
        if self.handle_verbose_mode_command(text) {
            return true;
        }
        if self.handle_session_stats_command(text) {
            return true;
        }
        if self.handle_debug_utility_command(text) {
            return true;
        }
        if self.handle_version_command(text) {
            return true;
        }
        if self.handle_provider_auth_command(text) {
            return true;
        }
        self.handle_utility_alias_command(text)
    }

    /// Handles the no-argument process-local verbose-mode toggle command.
    fn handle_verbose_mode_command(&self, text: &str) -> bool {
        if text == ":verbose-mode-toggle" {
            let _ = self.ctx.renderer_tx.send(RendererCmd::ToggleVerboseMode);
            return true;
        }
        if text.starts_with(":verbose-mode-toggle ") {
            self.output
                .command_feedback(":verbose-mode-toggle takes no arguments");
            return true;
        }
        false
    }

    /// Handles the local session-wide token totals command.
    fn handle_session_stats_command(&self, text: &str) -> bool {
        if text == ":session-stats" {
            let _ = self.ctx.renderer_tx.send(RendererCmd::ShowSessionStats);
            return true;
        }
        if text.starts_with(":session-stats ") {
            self.output
                .command_feedback(":session-stats takes no arguments");
            return true;
        }
        false
    }

    /// Handles the no-argument local version command and invalid variants.
    fn handle_version_command(&self, text: &str) -> bool {
        if text == ":version" {
            self.output.command_feedback(&crate::version_label());
            return true;
        }
        if text.starts_with(":version ") {
            self.output.command_feedback(":version takes no arguments");
            return true;
        }
        false
    }

    /// Handles provider authentication before the generic command aliases.
    fn handle_provider_auth_command(&self, text: &str) -> bool {
        if let Some(provider) = text.strip_prefix(":provider-auth ") {
            let provider = provider.trim();
            if !provider.is_empty() {
                let output = &self.output;
                run_provider_auth(provider, &|message| output.command_feedback(message));
            }
            return true;
        }
        if text == ":provider-auth" {
            let output = &self.output;
            run_provider_auth("", &|message| output.command_feedback(message));
            return true;
        }
        false
    }

    /// Routes command aliases that mutate this terminal input session.
    fn handle_utility_alias_command(&mut self, text: &str) -> bool {
        if text == ":theme" || text.starts_with(":theme ") {
            self.handle_theme_command(text);
            return true;
        }
        if text == ":agent" || text.starts_with(":agent ") {
            self.handle_agent_command(text);
            return true;
        }
        if text == ":new" || text.starts_with(":new ") {
            self.handle_new_alias(text);
            return true;
        }
        if text == ":name" || text.starts_with(":name ") {
            self.handle_name_alias(text);
            return true;
        }
        if text == ":suspend" || text.starts_with(":suspend ") {
            self.handle_suspend_alias(text);
            return true;
        }
        if text == ":resume" || text.starts_with(":resume ") {
            self.handle_resume_alias(text);
            return true;
        }
        if text == ":set" || text.starts_with(":set ") {
            let output = &self.output;
            handle_set_command(text, &self.ctx.renderer_tx, &|message| {
                output.command_feedback(message);
            });
            return true;
        }

        false
    }

    fn handle_debug_utility_command(&self, text: &str) -> bool {
        self.handle_debug_show_ui_event_stats_command(text)
            || self.handle_debug_show_event_stats_command(text)
    }

    fn handle_debug_show_ui_event_stats_command(&self, text: &str) -> bool {
        handle_debug_show_ui_event_stats_command_text(text, &self.ctx.ui_io_meter, |message| {
            self.output.command_feedback(message);
        })
    }

    fn handle_debug_show_event_stats_command(&self, text: &str) -> bool {
        handle_debug_show_event_stats_command_text(text, self.writer, |usage| {
            self.output.command_feedback(usage);
        })
    }

    fn handle_theme_command(&mut self, text: &str) {
        let name = text.strip_prefix(":theme").unwrap_or("").trim();
        if name.is_empty() {
            let names = crate::theme::available_theme_choices(&self.ctx.dirs)
                .into_iter()
                .map(path_crate_theme::ThemeChoice::into_listing_text)
                .collect::<Vec<_>>()
                .join(", ");
            self.output
                .command_feedback(&format!(":theme <name>; available: {names}"));
            return;
        }
        let theme = match crate::theme::select_theme_for_command(&self.ctx.dirs, name) {
            Ok(theme) => theme,
            Err(error) => {
                self.output.command_feedback(&format!(":theme: {error}"));
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
        let _ = self.ctx.renderer_tx.send(RendererCmd::SetTheme {
            theme: theme.clone(),
        });
        self.output
            .command_feedback(&format!("theme set to `{name}` for this UI"));
    }

    fn handle_agent_command(&mut self, text: &str) {
        match agent_command_effect(text, &self.ctx.routing) {
            Ok(AgentCommandEffect::ShowStatus) => {
                let current = self
                    .ctx
                    .routing
                    .selected_agent_id()
                    .unwrap_or_else(|| "none".to_owned());
                let known_agents = self.ctx.routing.known_agents();
                let active_count = self.ctx.routing.active_count();
                self.output.command_feedback(&format!(
                    ":agent <new|switch|suspend|resume|auto|name> [agent_id]; current: {current}; active: {active_count}; known: {}",
                    known_agents.join(", ")
                ));
            }
            Ok(AgentCommandEffect::New) => self.handle_agent_new(None),
            Ok(AgentCommandEffect::Switch(target)) => self.apply_agent_switch(target),
            Ok(AgentCommandEffect::SetNavigation { agent_id, action }) => {
                self.send_agent_navigation_mode_request(agent_id, action);
            }
            Ok(AgentCommandEffect::SetDisplayName(request)) => {
                self.send_agent_display_name_request(request);
            }
            Err(message) => self.output.command_feedback(&message),
        }
    }

    fn handle_name_alias(&self, text: &str) {
        match name_alias_request(text, self.selected_agent_id(), |agent_id| {
            self.agent_is_known(agent_id)
        }) {
            Ok(request) => self.send_agent_display_name_request(request),
            Err(message) => self.output.command_feedback(&message),
        }
    }

    fn send_agent_display_name_request(&self, request: AgentDisplayNameRequest) {
        let event = request.event(self.session_id);
        if send_event(self.writer, &event).is_ok() {
            self.output.command_feedback(&format!(
                "requested agent {} display name set to: {}",
                request.agent_id.as_str(),
                request.display_name
            ));
        }
    }

    fn handle_new_alias(&mut self, text: &str) {
        match new_alias_command_effect(text) {
            NewAliasCommandEffect::StartNewAgent { role } => self.handle_agent_new(role),
            NewAliasCommandEffect::Usage(usage) => self.output.command_feedback(usage),
        }
    }

    fn handle_suspend_alias(&self, text: &str) {
        if text.trim() != ":suspend" {
            self.output.command_feedback(":suspend");
            return;
        }
        self.handle_agent_suspend(None);
    }

    fn handle_resume_alias(&self, text: &str) {
        if text.trim() != ":resume" {
            self.output.command_feedback(":resume");
            return;
        }
        self.handle_agent_resume(None);
    }

    fn selected_agent_id(&self) -> Option<String> {
        self.ctx.routing.selected_agent_id()
    }

    fn handle_agent_new(&mut self, role: Option<&str>) {
        if let Some(role) = role {
            self.pending_new_agent_options.stage_role(role);
            let _ = send_event(
                self.writer,
                &Event::UiRoleSelect(tau_proto::UiRoleSelect {
                    role: role.to_owned(),
                }),
            );
        } else {
            self.pending_new_agent_options.clear_role();
        }
        self.clear_selected_agent();
    }

    fn clear_selected_agent(&mut self) {
        self.ctx.routing.set_selected_agent(None);
        self.dismiss_completion_menu();
        self.retarget_current_draft();
        let _ = self.ctx.renderer_tx.send(RendererCmd::ClearSelectedAgent);
    }

    fn apply_agent_switch(&mut self, target: Option<String>) {
        match target {
            None => {
                self.ctx.routing.set_selected_agent(None);
                let _ = self.ctx.renderer_tx.send(RendererCmd::ClearSelectedAgent);
                self.dismiss_completion_menu();
                self.retarget_current_draft();
            }
            Some(agent_id) => {
                self.ctx.routing.set_selected_agent(Some(agent_id.clone()));
                let _ = self
                    .ctx
                    .renderer_tx
                    .send(RendererCmd::SwitchAgent { agent_id });
                self.pending_new_agent_options.clear();
                self.dismiss_completion_menu();
                self.retarget_current_draft();
            }
        }
    }

    fn handle_agent_suspend(&self, target: Option<&str>) {
        if let Some(agent_id) =
            handle_agent_suspend_command(&self.ctx.routing, target, &|message| {
                self.output.command_feedback(message);
            })
        {
            self.send_agent_navigation_mode_request(
                agent_id,
                tau_proto::UiAgentNavigationModeAction::SetSuspended,
            );
        }
    }

    fn handle_agent_resume(&self, target: Option<&str>) {
        if let Some(agent_id) = handle_agent_resume_command(&self.ctx.routing, target, &|message| {
            self.output.command_feedback(message);
        }) {
            self.send_agent_navigation_mode_request(
                agent_id,
                tau_proto::UiAgentNavigationModeAction::SetActive,
            );
        }
    }

    fn send_agent_navigation_mode_request(
        &self,
        agent_id: tau_proto::AgentId,
        action: tau_proto::UiAgentNavigationModeAction,
    ) {
        let event = crate::ui_events::set_agent_navigation_mode(self.session_id, agent_id, action);
        let _ = send_event(self.writer, &event);
    }

    fn agent_is_known(&self, agent_id: &str) -> bool {
        self.ctx.routing.agent_is_known(agent_id)
    }

    fn handle_role_selection_command(&mut self, text: &str) -> bool {
        if text == ":role" || text.starts_with(":role ") {
            self.handle_role_command(text);
            return true;
        }
        if let Some(model) = text.strip_prefix(":model ") {
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
                                .command_feedback(&format!("next agent model set to {model}"));
                        }
                    }
                    Err(error) => self.output.command_feedback(&error.to_string()),
                }
            }
            return true;
        }
        if text == ":model" {
            // No argument — just a reminder.
            return true;
        }

        false
    }

    fn handle_role_command(&mut self, text: &str) {
        let rest = text.strip_prefix(":role").unwrap_or("").trim();
        match crate::ui_commands::parse_role_command(rest) {
            Ok(Some(event)) => {
                let has_selected_agent = self.selected_agent_id().is_some();
                stage_role_selection_for_new_agent(
                    &mut self.pending_new_agent_options,
                    has_selected_agent,
                    &event,
                );
                let _ = send_event(self.writer, &event);
            }
            Ok(None) => self.output.command_feedback(
                ":role <role> [delete|model|effort|verbosity|thinking-summary|service-tier|compaction-threshold|tools|enable-tool-groups|disable-tool-groups|enable-tools|disable-tools] [value]",
            ),
            Err(error) => self.output.command_feedback(&error),
        }
    }

    fn handle_dynamic_action(&self, text: &str) -> CommandOutcome {
        let invocation = match prepare_dynamic_action_invocation(
            &self.ctx.action_state,
            &self.ctx.routing,
            self.session_id,
            text,
        ) {
            Ok(Some(invocation)) => invocation,
            Ok(None) => return CommandOutcome::NotHandled,
            Err(error) => {
                self.output.command_feedback(&error);
                return CommandOutcome::Continue;
            }
        };
        self.invalidate_pending_draft();
        // Record the renderer owner before sending `action.invoke`: fast
        // completions carry only `invocation_id`, so result/error routing must
        // already know which transcript was viewed at invocation time.
        let _ = self.ctx.renderer_tx.send(invocation.renderer_cmd);
        if send_event(self.writer, &invocation.event).is_err() {
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
        self.submit_prompt_with_command_handling(text, PromptCommandHandling::Interpret)
    }

    fn submit_literal_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        self.submit_prompt_with_command_handling(text, PromptCommandHandling::LiteralEscape)
    }

    fn submit_prompt_with_command_handling(
        &mut self,
        text: &str,
        command_handling: PromptCommandHandling,
    ) -> Option<InputLoopExit> {
        self.invalidate_pending_draft();

        let selected_agent = self.ctx.routing.selected_agent_id();
        self.ctx
            .agent_in_progress
            .store(true, path_std_sync_atomic::Ordering::Relaxed);
        let event = if let Some(target_agent_id) = selected_agent.map(|agent_id| {
            tau_proto::AgentId::parse(&agent_id).expect("UI stores valid agent ids")
        }) {
            Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: matches!(command_handling, PromptCommandHandling::LiteralEscape),
                session_id: self.session_id.clone(),
                text: text.to_owned(),
                agent_id: target_agent_id,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            })
        } else {
            let current_role = self
                .ctx
                .current_role_state
                .lock()
                .ok()
                .and_then(|role| role.clone())
                .unwrap_or_else(|| DEFAULT_AGENT_ROLE.to_owned());
            let role = take_new_agent_role(&mut self.pending_new_agent_options, current_role);
            let model_override = self.pending_new_agent_options.take_model();
            let ephemeral = self.pending_new_agent_options.take_ephemeral();
            Event::UiCreateAgent(create_user_agent_prompt(
                self.session_id,
                role,
                text,
                CreateUserAgentPromptOptions {
                    model_override,
                    ephemeral,
                    command_handling,
                },
            ))
        };
        let frame_started = Instant::now();
        let event_kind = event.name();
        let message = durable_emit_message_owned(event);
        tracing::trace!(
            target: "tau_cli::prompt_submission",
            diagnostic_seq = self.prompt_diagnostic_seq,
            stage = "frame_construct",
            event_kind = %event_kind,
            stage_us = frame_started.elapsed().as_micros(),
            "content-free prompt frame construction stage"
        );
        if send_frame_with_diagnostic(self.writer, &message, self.prompt_diagnostic_seq).is_err() {
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
                .load(path_std_sync_atomic::Ordering::Relaxed)
        {
            self.output.command_feedback(EOF_DURING_AGENT_NOTICE);
            return None;
        }

        Some(InputLoopExit::Quit)
    }

    fn update_draft(&self) {
        // Queue the first change immediately, then coalesce later changes into
        // one `UiPromptDraft` per `DRAFT_DEBOUNCE` window.
        let text = self.term.handle().get_buffer();
        let target_agent_id = self.selected_side_agent_id();
        queue_prompt_draft_snapshot(
            self.ctx.draft_handle.as_ref(),
            self.session_id.clone(),
            target_agent_id,
            text,
        );
        tracing::trace!(target: "tau_cli::ui", "prompt draft updated");
    }

    fn retarget_current_draft(&self) {
        let text = self.term.handle().get_buffer();
        let target_agent_id = self.selected_side_agent_id();
        retarget_prompt_draft_snapshot(
            self.ctx.draft_handle.as_ref(),
            self.session_id.clone(),
            target_agent_id,
            text,
        );
        tracing::trace!(target: "tau_cli::ui", "prompt draft target changed");
    }

    fn toggle_fast_service_tier(&self) {
        // `fast_service_tier_state` is kept in sync by renderer events. Toggling
        // from Fast sends `None` to restore the role/model default; toggling from
        // any other state requests explicit Fast service.
        let enabled = self
            .ctx
            .fast_service_tier_state
            .load(path_std_sync_atomic::Ordering::Relaxed);
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
            &|message| output.command_feedback(message),
        );
    }

    fn switch_agent_by_delta(&mut self, delta: isize) {
        match dispatch_agent_cycle(&self.ctx.routing, &self.ctx.renderer_tx, delta) {
            AgentCycleAction::KeepSelection => {}
            AgentCycleAction::Select(_) => {
                self.pending_new_agent_options.clear();
                self.dismiss_completion_menu();
                self.retarget_current_draft();
            }
            AgentCycleAction::ClearSelection => {
                self.dismiss_completion_menu();
                self.retarget_current_draft();
            }
        }
    }

    fn pick_agent(
        &mut self,
        filter: crate::list_agents::AgentPickerFilter,
    ) -> Result<(), CliError> {
        let session_id = self.session_id.clone();
        let costs = self.ctx.agent_estimated_api_costs.snapshot();
        let resolution = with_agent_roster(
            || {
                crate::list_agents::request_at_socket(
                    &self.ctx.harness_socket_path,
                    &session_id,
                    tau_proto::SessionAgentListScope::Current,
                )
            },
            |agents| {
                resolve_agent_picker(
                    agents,
                    filter,
                    |agent_id| costs.get(agent_id).copied(),
                    |rows| self.term.pick_agent_row_with_fzf(rows),
                    || {
                        crate::list_agents::request_at_socket(
                            &self.ctx.harness_socket_path,
                            &session_id,
                            tau_proto::SessionAgentListScope::Current,
                        )
                        .ok()
                    },
                    || self.session_id == &session_id,
                    |agent_id| self.ctx.routing.agent_is_known(agent_id),
                )
            },
        );
        match resolution {
            AgentPickerResolution::NoChange => {}
            AgentPickerResolution::Notice(message) => {
                self.output
                    .command_feedback(&format!("agent-picker: {message}"));
            }
            AgentPickerResolution::Fatal(message) => {
                return Err(CliError::ForegroundOwnershipUnconfirmed(message));
            }
            AgentPickerResolution::Select(agent_id) => {
                if self.selected_agent_id().as_deref() != Some(agent_id.as_str()) {
                    self.switch_to_agent(agent_id.to_string());
                }
            }
        }
        Ok(())
    }

    fn switch_to_agent(&mut self, agent_id: String) {
        self.pending_new_agent_options.clear();
        self.ctx.routing.set_selected_agent(Some(agent_id.clone()));
        self.dismiss_completion_menu();
        self.retarget_current_draft();
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

    fn cycle_role_group(&mut self) {
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
        let selected = if groups.is_empty() {
            cycle_role(
                self.writer,
                &self.ctx.current_role_state,
                &self.ctx.roles_available,
                &|message| output.command_feedback(message),
            )
        } else {
            cycle_role_in_groups(
                self.writer,
                &self.ctx.current_role_state,
                &self.ctx.role_group_memory,
                &groups,
                false,
                &|message| output.command_feedback(message),
            )
        };
        if let Some(role) = selected {
            self.pending_new_agent_options.stage_role(role);
        }
    }

    fn agent_is_selected(&self) -> bool {
        !self.ctx.routing.role_cycling_enabled()
    }

    fn cycle_role_inner(&mut self) {
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
        if let Some(role) = cycle_role_in_groups(
            self.writer,
            &self.ctx.current_role_state,
            &self.ctx.role_group_memory,
            &groups,
            true,
            &|message| output.command_feedback(message),
        ) {
            self.pending_new_agent_options.stage_role(role);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum AgentCommandEffect {
    ShowStatus,
    New,
    Switch(Option<String>),
    SetNavigation {
        agent_id: tau_proto::AgentId,
        action: tau_proto::UiAgentNavigationModeAction,
    },
    SetDisplayName(AgentDisplayNameRequest),
}

fn agent_command_effect(
    text: &str,
    routing: &InputRoutingState,
) -> Result<AgentCommandEffect, String> {
    let rest = text.strip_prefix(":agent").unwrap_or("").trim();
    if rest.is_empty() {
        return Ok(AgentCommandEffect::ShowStatus);
    }
    if rest
        .strip_prefix("name")
        .is_some_and(|suffix| suffix.chars().next().is_none_or(char::is_whitespace))
    {
        return AgentDisplayNameRequest::from_agent_command(rest, routing)
            .map(AgentCommandEffect::SetDisplayName);
    }

    let mut parts = rest.split_whitespace();
    let Some(subcommand) = parts.next() else {
        return Ok(AgentCommandEffect::ShowStatus);
    };
    let target = parts.next();
    if parts.next().is_some() {
        return Err(
            ":agent: too many arguments (use :agent <new|switch|suspend|resume|auto|name> [agent_id])"
                .to_owned(),
        );
    }
    match subcommand {
        "new" => target
            .is_none()
            .then_some(AgentCommandEffect::New)
            .ok_or_else(|| ":agent new".to_owned()),
        "switch" => routing
            .agent_switch_target(target)
            .map(AgentCommandEffect::Switch),
        "suspend" => agent_navigation_command_effect(
            routing,
            target,
            routing.selected_agent_id(),
            tau_proto::UiAgentNavigationModeAction::SetSuspended,
            ":agent suspend <agent_id>",
        ),
        "resume" => agent_navigation_command_effect(
            routing,
            target,
            routing
                .selected_agent_id()
                .filter(|agent_id| !routing.agent_is_active(agent_id)),
            tau_proto::UiAgentNavigationModeAction::SetActive,
            ":agent resume <agent_id>",
        ),
        "auto" => agent_navigation_command_effect(
            routing,
            target,
            routing.selected_agent_id(),
            tau_proto::UiAgentNavigationModeAction::SetActiveAuto,
            ":agent auto <agent_id>",
        ),
        _ => Err(
            ":agent <new|switch|suspend|resume|auto|name> [agent_id]; use :agent switch <agent_id>"
                .to_owned(),
        ),
    }
}

fn agent_navigation_command_effect(
    routing: &InputRoutingState,
    target: Option<&str>,
    fallback: Option<String>,
    action: tau_proto::UiAgentNavigationModeAction,
    usage: &str,
) -> Result<AgentCommandEffect, String> {
    let agent_id = routing
        .resolve_agent_command_target(target, fallback)?
        .ok_or_else(|| usage.to_owned())?;
    Ok(AgentCommandEffect::SetNavigation { agent_id, action })
}

/// Terminal-local outcome from one complete agent picker interaction.
#[derive(Debug, Eq, PartialEq)]
enum AgentPickerResolution {
    /// Preserve selection and draft without a notice.
    NoChange,
    /// Preserve selection and draft while showing this notice.
    Notice(String),
    /// Exit the attachment because foreground ownership remains unconfirmed.
    Fatal(String),
    /// Switch to this freshly revalidated agent.
    Select(tau_proto::AgentId),
}

/// Requests the current roster before allowing picker projection or execution.
fn with_agent_roster<E: std::fmt::Display>(
    request: impl FnOnce() -> Result<Vec<tau_proto::SessionAgentListEntry>, E>,
    continue_with: impl FnOnce(Vec<tau_proto::SessionAgentListEntry>) -> AgentPickerResolution,
) -> AgentPickerResolution {
    match request() {
        Ok(agents) => continue_with(agents),
        Err(error) => AgentPickerResolution::Notice(error.to_string()),
    }
}

/// Runs picker projection, selection, and fresh-snapshot revalidation.
fn resolve_agent_picker(
    agents: Vec<tau_proto::SessionAgentListEntry>,
    filter: crate::list_agents::AgentPickerFilter,
    cost_for_agent: impl Fn(&tau_proto::AgentId) -> Option<crate::estimated_cost::AgentCostSnapshot>,
    pick: impl FnOnce(&str) -> Result<Option<String>, tau_cli_term::ExternalProgramError>,
    refresh: impl FnOnce() -> Option<Vec<tau_proto::SessionAgentListEntry>>,
    session_is_current: impl FnOnce() -> bool,
    agent_is_known: impl FnOnce(&str) -> bool,
) -> AgentPickerResolution {
    let visible = crate::list_agents::picker_agents(agents, filter);
    let rows = crate::list_agents::format_picker_rows(&visible, cost_for_agent);
    let selected = match pick(&rows) {
        Ok(Some(row)) => row,
        Ok(None) => return AgentPickerResolution::NoChange,
        Err(error) if error.is_foreground_ownership_unconfirmed() => {
            return AgentPickerResolution::Fatal(error.to_string());
        }
        Err(error) => return AgentPickerResolution::Notice(error.to_string()),
    };
    let agent_id = match crate::list_agents::selected_agent_id(&selected) {
        Ok(agent_id) => agent_id,
        Err(error) => return AgentPickerResolution::Notice(error),
    };
    let revalidated = refresh().is_some_and(|agents| {
        crate::list_agents::picker_selection_is_current(&agents, &agent_id, filter)
    });
    if !session_is_current() || !revalidated || !agent_is_known(agent_id.as_str()) {
        return AgentPickerResolution::Notice("selected agent is no longer available".to_owned());
    }
    AgentPickerResolution::Select(agent_id)
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

    fn command_feedback(&mut self, message: &str) {
        self.output.command_feedback(message);
    }
}

impl SubmittedLineHandlers for TerminalInputSession<'_> {
    fn replace_last_submitted_prompt(&mut self, text: String) {
        self.term.replace_last_submitted_prompt(text);
    }

    fn record_prompt_line(&mut self, record: SubmittedLineRecord<'_>) {
        self.record_prompt_line_if_persistent_with_routing(
            record.history_fallback,
            record.presentation_text,
            record.routing_text,
        );
    }

    fn is_known_command_or_action(&self, text: &str) -> bool {
        is_known_static_command(text) || self.ctx.action_state.is_known_action_line(text)
    }

    fn command_echo(&mut self, text: &str) {
        self.output.command_echo(text);
    }

    fn submit_literal_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        TerminalInputSession::submit_literal_prompt(self, text)
    }
}

pub(crate) fn role_cycling_enabled(current_agent_state: &Arc<Mutex<Option<String>>>) -> bool {
    current_agent_state
        .lock()
        .ok()
        .and_then(|agent| agent.clone())
        .is_none()
}

pub(crate) fn next_agent_cycle_selection(
    current: Option<&str>,
    known_agents: &[String],
    active_agent_ids: &std::collections::HashSet<String>,
    delta: isize,
) -> Option<String> {
    let active_agents = known_agents
        .iter()
        .filter(|agent| active_agent_ids.contains(*agent))
        .collect::<Vec<_>>();
    let cycle_len = active_agents.len() as isize + 1;
    let current_index = current
        .and_then(|current| {
            active_agents
                .iter()
                .position(|agent| agent.as_str() == current)
        })
        .map_or(0, |index| index as isize + 1);
    let next_index = (current_index + delta).rem_euclid(cycle_len) as usize;
    next_index
        .checked_sub(1)
        .map(|index| active_agents[index].to_string())
}

/// Input-loop action required to move from one selection to the next cycle
/// selection.
#[derive(Debug, Eq, PartialEq)]
enum AgentCycleAction {
    /// Keep the current input target unchanged.
    KeepSelection,
    /// Select the named active agent.
    Select(String),
    /// Clear the input target and show the all-agent overview.
    ClearSelection,
}

/// Translate a computed cycle selection into the input-loop operation that
/// publishes the corresponding renderer command.
fn agent_cycle_action(current: Option<&str>, next: Option<String>) -> AgentCycleAction {
    if current == next.as_deref() {
        AgentCycleAction::KeepSelection
    } else if let Some(next) = next {
        AgentCycleAction::Select(next)
    } else {
        AgentCycleAction::ClearSelection
    }
}

/// Update the input target and publish the renderer half of one previous/next
/// navigation action.
fn dispatch_agent_cycle(
    routing: &InputRoutingState,
    renderer_tx: &LocalRendererSender,
    delta: isize,
) -> AgentCycleAction {
    let current = routing.selected_agent_id();
    let next = routing.next_agent_cycle_selection(delta);
    let action = agent_cycle_action(current.as_deref(), next);
    match &action {
        AgentCycleAction::KeepSelection => {}
        AgentCycleAction::Select(agent_id) => {
            routing.set_selected_agent(Some(agent_id.clone()));
            let _ = renderer_tx.send(RendererCmd::SwitchAgent {
                agent_id: agent_id.clone(),
            });
        }
        AgentCycleAction::ClearSelection => {
            routing.set_selected_agent(None);
            let _ = renderer_tx.send(RendererCmd::ClearSelectedAgent);
        }
    }
    action
}

fn prepare_dynamic_action_invocation(
    action_state: &ActionCommandState,
    routing: &InputRoutingState,
    session_id: &tau_proto::SessionId,
    text: &str,
) -> Result<Option<DynamicActionInvocation>, String> {
    let Some(dispatch) = action_state.parse_line(text) else {
        return Ok(None);
    };
    let dispatch = dispatch.map_err(|error| error.to_string())?;
    let parsed = dispatch.parsed;
    let invocation_id = tau_proto::ActionInvocationId::parse(crate::mint_short_id("action"))
        .expect("Tau-generated action invocation id must be valid");
    let owner_agent_id = routing.selected_agent_id();
    let event = Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: invocation_id.clone(),
        session_id: session_id.clone(),
        extension_name: dispatch.extension_name,
        instance_id: dispatch.instance_id,
        action_id: parsed.action_id.clone(),
        raw_line: text.to_owned(),
        argv: parsed.argv.clone(),
        arguments: parsed_action_arguments(&parsed.named_args),
    });
    let renderer_cmd = RendererCmd::ActionInvoked {
        invocation_id,
        owner_agent_id,
    };
    Ok(Some(DynamicActionInvocation {
        event,
        renderer_cmd,
    }))
}

fn terminal_input_loop(
    term: &mut tau_cli_term::HighTerm,
    writer: &WriterHandle,
    session_id: &mut tau_proto::SessionId,
    ctx: TerminalInputLoopCtx,
) -> Result<InputLoopExit, CliError> {
    // Cloned `TermHandle` so we can `print_output` for client-side
    // validation errors (`:role engineer effort foo`, `:tree blah`) from this
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
        prompt_diagnostic_seq: None,
    }
    .run()
}

const SESSION_SUBCOMMAND_COMPLETIONS: &[(&str, &str)] = &[("new", "Start a fresh chat session")];

const AGENT_SUBCOMMAND_COMPLETIONS: &[(&str, &str)] = &[
    ("new", "Clear the selected agent"),
    ("switch", "Show a known agent transcript"),
    ("suspend", "Exclude an active agent from navigation"),
    ("resume", "Make a loaded agent always navigation-eligible"),
    ("auto", "Make a loaded agent eligible only while running"),
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
            let live = routing.live_agents();
            let raw_needle = args[1].to_lowercase();
            let (needle, prefixed) = match raw_needle.strip_prefix('@') {
                Some(needle) => (needle, true),
                None => (raw_needle.as_str(), false),
            };
            agent_completion_candidates(args[0], known, live, active)
                .into_iter()
                .filter(|agent| !prefixed || agent != "none")
                .filter(|agent| completion_matches(agent, needle))
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
    live_agents: std::collections::HashSet<String>,
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
            .filter(|agent| live_agents.contains(agent) && !active_agents.contains(agent))
            .collect(),
        "auto" => known_agents
            .into_iter()
            .filter(|agent| live_agents.contains(agent))
            .collect(),
        "name" => known_agents,
        _ => Vec::new(),
    }
}

/// Build the `:theme` argument completer from built-in and user theme names.
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

/// Build the `:set` argument completer. The first arg is a setting
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

/// Parses `:prompt <id>` and returns the configured prompt replacement or a
/// user-visible validation message.
pub(crate) fn custom_prompt_replacement(
    text: &str,
    prompts: &[tau_proto::HarnessCustomPrompt],
) -> Option<Result<String, String>> {
    if !is_custom_prompt_command(text) {
        return None;
    }
    let mut parts = text.split_whitespace();
    let command = parts.next();
    debug_assert_eq!(command, Some(":prompt"));

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
                "usage: :prompt <id>; available: {}",
                custom_prompt_ids(prompts)
            )
        })),
        _ => Some(Err("usage: :prompt <id>".to_owned())),
    }
}

/// Returns whether the first whitespace-delimited input token is `:prompt`.
pub(crate) fn is_custom_prompt_command(text: &str) -> bool {
    text.split_whitespace().next() == Some(":prompt")
}

/// Parses a custom prompt command after taking its payload snapshot only when
/// needed.
pub(crate) fn custom_prompt_replacement_from_snapshot(
    text: &str,
    snapshot: impl FnOnce() -> Vec<tau_proto::HarnessCustomPrompt>,
) -> Option<Result<String, String>> {
    if !is_custom_prompt_command(text) {
        return None;
    }
    let prompts = snapshot();
    custom_prompt_replacement(text, &prompts)
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

pub(crate) fn leading_command_token(text: &str) -> Option<&str> {
    let command = text.split_whitespace().next()?;
    command.starts_with(':').then_some(command)
}

pub(crate) fn redacted_command_echo_line(text: &str) -> Cow<'_, str> {
    redact_sensitive_action_line(text).map_or(Cow::Borrowed(text), Cow::Owned)
}

pub(crate) fn redacted_prompt_history_line<'a>(line: &'a str, text: &str) -> Cow<'a, str> {
    redact_sensitive_action_line(text).map_or(Cow::Borrowed(line), Cow::Owned)
}

fn redact_sensitive_action_line(text: &str) -> Option<String> {
    let mut parts = text.split_whitespace();
    let root = parts.next()?;
    let auth = parts.next()?;
    let provider = parts.next()?;
    let finish = parts.next()?;
    if root == ":email" && auth == "auth" && provider == "google" && finish == "finish" {
        Some(":email auth google finish <redacted>".to_owned())
    } else {
        None
    }
}

fn is_harness_prompt_command(action: &str) -> bool {
    action == ":skill" || action.starts_with(":skill:")
}

fn handle_recorded_line_with_handlers(
    text: &str,
    handlers: &mut impl RecordedLineHandlers,
) -> Result<Option<InputLoopExit>, CliError> {
    match handlers.handle_known_command(text)? {
        CommandOutcome::NotHandled => match handlers.handle_dynamic_action(text) {
            CommandOutcome::NotHandled => {
                // This is only a candidate leading command-token detector. It must
                // run after CLI-owned commands, dynamic extension actions, and
                // harness-owned prompt commands such as `:skill` are excluded so
                // each owner keeps its routing contract.
                if let Some(action) = leading_command_token(text)
                    && !is_harness_prompt_command(action)
                {
                    handlers.command_feedback(&format!("unknown command `{action}`"));
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

fn handle_submitted_line_with_handlers(
    line: &str,
    handlers: &mut impl SubmittedLineHandlers,
) -> Result<Option<InputLoopExit>, CliError> {
    let text = line.trim();
    if text.is_empty() {
        return Ok(None);
    }

    if let Some(redacted) = redact_sensitive_action_line(text) {
        handlers.replace_last_submitted_prompt(redacted);
    }

    if let Some(canonical_line) = tau_cli_term::canonical_literal_colon_prompt(line) {
        let canonical_text = canonical_line.trim();
        let submitted_text = redact_sensitive_action_line(canonical_text)
            .map_or(Cow::Borrowed(canonical_text), Cow::Owned);
        if submitted_text != canonical_text {
            handlers.replace_last_submitted_prompt(submitted_text.clone().into_owned());
        }
        handlers.record_prompt_line(SubmittedLineRecord {
            history_fallback: &canonical_line,
            presentation_text: &submitted_text,
            routing_text: text,
        });
        return Ok(handlers.submit_literal_prompt(&submitted_text));
    }

    // Preserve the original side-effect order: every non-empty line is
    // recorded before command handling, and local commands are echoed before
    // they produce validation errors or exit the loop.
    let presentation_text = redacted_command_echo_line(text);
    handlers.record_prompt_line(SubmittedLineRecord {
        history_fallback: line,
        presentation_text: &presentation_text,
        routing_text: text,
    });
    if handlers.is_known_command_or_action(text) {
        handlers.command_echo(&presentation_text);
    }
    handle_recorded_line_with_handlers(text, handlers)
}

pub(crate) fn is_known_static_command(text: &str) -> bool {
    let command = text.split_whitespace().next().unwrap_or(text);
    if command == ":skill" || command.starts_with(":skill:") {
        return true;
    }
    matches!(
        command,
        ":quit"
            | ":cancel"
            | ":retry"
            | ":detach"
            | ":pick-agent"
            | ":pick-agent-all"
            | ":session"
            | ":session-stats"
            | ":tree"
            | ":compact"
            | ":fast"
            | ":verbose-mode-toggle"
            | ":provider-auth"
            | ":agent"
            | ":new"
            | ":name"
            | ":ephemeral"
            | ":suspend"
            | ":resume"
            | ":set"
            | ":theme"
            | ":role"
            | ":prompt"
            | ":model"
            | ":version"
            | ":debug-show-ui-event-stats"
            | ":debug-show-event-stats"
    )
}

/// Parse and dispatch `:set <name> <value>`. Validation lives here
/// (input-loop thread) so the renderer can trust `RendererCmd::Set`
/// to always be a known name and an allowed value.
fn handle_set_command(text: &str, renderer_tx: &LocalRendererSender, print_local: &impl Fn(&str)) {
    use crate::settings_registry;

    let rest = text.strip_prefix(":set").unwrap_or("").trim();
    let mut parts = rest.split_whitespace();
    let name = parts.next();
    let value = parts.next();
    let extra = parts.next();

    let usage = || {
        let names: Vec<&str> = settings_registry::SETTINGS.iter().map(|s| s.name).collect();
        print_local(&format!(":set <name> <value>; names: {}", names.join(", ")));
    };

    let (Some(name), Some(value)) = (name, value) else {
        usage();
        return;
    };
    if extra.is_some() {
        print_local(":set: too many arguments");
        return;
    }
    let Some(def) = settings_registry::find(name) else {
        print_local(&format!(":set: unknown setting `{name}`"));
        return;
    };
    if !(def.validate)(value) {
        let allowed: Vec<&str> = def.values.iter().map(|v| v.value).collect();
        let hint = if allowed.is_empty() {
            def.value_hint.to_owned()
        } else {
            format!("{}; suggested: {}", def.value_hint, allowed.join(", "))
        };
        print_local(&format!(":set {name}: invalid value `{value}` ({hint})"));
        return;
    }
    let _ = renderer_tx.send(RendererCmd::Set {
        name: name.to_owned(),
        value: value.to_owned(),
    });
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
    session_id: &tau_proto::SessionId,
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
const UI_SESSION_ADMISSION_TIMEOUT: Duration = crate::ui_client::UI_SESSION_ADMISSION_TIMEOUT;
