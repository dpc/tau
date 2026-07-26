//! Shared UI socket client helpers.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tau_proto::{
    ClientKind, EventName, EventSelector, HarnessInputMessage, Hello, PROTOCOL_VERSION,
    PeerInputReader, PeerOutputWriter, Subscribe,
};

use crate::daemon::DaemonHandle;

pub(crate) type UiInputReader = PeerInputReader<BufReader<Box<dyn Read + Send>>>;
pub(crate) type UiOutputWriter = PeerOutputWriter<BufWriter<Box<dyn Write + Send>>>;

pub(crate) fn connect_ui_client(
    socket_path: &Path,
    client_name: impl Into<tau_proto::ExtensionName>,
) -> io::Result<(UiInputReader, UiOutputWriter)> {
    let stream = UnixStream::connect(socket_path)?;
    let read_stream = stream.try_clone()?;
    connect_ui_streams(read_stream, stream, client_name)
}

pub(crate) fn connect_ui_client_until(
    socket_path: &Path,
    client_name: impl Into<tau_proto::ExtensionName>,
    deadline: std::time::Instant,
) -> io::Result<(UiInputReader, UiOutputWriter)> {
    let timeout = deadline.saturating_duration_since(std::time::Instant::now());
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "UI request deadline elapsed while connecting",
        ));
    }
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(socket_path)?, timeout)?;
    let fd: OwnedFd = socket.into();
    let stream: UnixStream = fd.into();
    stream.set_write_timeout(Some(timeout))?;
    let read_stream = stream.try_clone()?;
    connect_ui_streams(
        DeadlineUnixReader {
            stream: read_stream,
            deadline,
        },
        DeadlineUnixWriter { stream, deadline },
        client_name,
    )
}

/// Unix reader that reapplies one absolute deadline before every underlying
/// read, including reads performed inside CBOR decoding.
struct DeadlineUnixReader {
    /// Connected stream.
    stream: UnixStream,
    /// Absolute end of the one-shot request.
    deadline: std::time::Instant,
}

impl Read for DeadlineUnixReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "UI request deadline elapsed")
                })?;
            self.stream
                .set_read_timeout(Some(remaining.min(Duration::from_millis(100))))?;
            match self.stream.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}

/// Unix writer that applies the same absolute request deadline to every write.
struct DeadlineUnixWriter {
    /// Connected stream.
    stream: UnixStream,
    /// Absolute end of the one-shot request.
    deadline: std::time::Instant,
}

impl Write for DeadlineUnixWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "UI request deadline elapsed")
            })?;
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub(crate) fn connect_ui_streams<R, W>(
    reader: R,
    writer: W,
    client_name: impl Into<tau_proto::ExtensionName>,
) -> io::Result<(UiInputReader, UiOutputWriter)>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut writer =
        PeerOutputWriter::new(BufWriter::new(Box::new(writer) as Box<dyn Write + Send>));
    send_hello(&mut writer, client_name)?;
    let reader = PeerInputReader::new(BufReader::new(Box::new(reader) as Box<dyn Read + Send>));
    Ok((reader, writer))
}

pub(crate) fn connect_daemon_ui_client(
    daemon: &mut DaemonHandle,
    client_name: impl Into<tau_proto::ExtensionName>,
) -> io::Result<(UiInputReader, UiOutputWriter)> {
    if let Some(initial_ui) = daemon.take_initial_ui_stdio() {
        connect_ui_streams(initial_ui.stdout, initial_ui.stdin, client_name)
    } else {
        connect_ui_client(&daemon.socket_path(), client_name)
    }
}

pub(crate) fn connect_ui_writer(
    socket_path: &Path,
    client_name: impl Into<tau_proto::ExtensionName>,
) -> io::Result<UiOutputWriter> {
    let stream = UnixStream::connect(socket_path)?;
    let mut writer =
        PeerOutputWriter::new(BufWriter::new(Box::new(stream) as Box<dyn Write + Send>));
    send_hello(&mut writer, client_name)?;
    Ok(writer)
}

pub(crate) fn hello_message(
    client_name: impl Into<tau_proto::ExtensionName>,
) -> HarnessInputMessage {
    HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: client_name.into(),
        client_kind: ClientKind::Ui,
        capabilities: Default::default(),
    })
}

pub(crate) fn chat_subscription_selectors() -> Vec<EventSelector> {
    use EventName as E;

    // Keep this as an exact allow-list. Read the repository-root
    // `specs/DECISION-exact-event-subscriptions.md` policy before adding broad
    // prefix selectors.
    vec![
        // Locally-originated UI echoes rendered by the transcript and activity
        // state.
        EventSelector::Exact(E::UI_PROMPT_SUBMITTED),
        EventSelector::Exact(E::UI_SHELL_COMMAND),
        EventSelector::Exact(E::UI_CANCEL_PROMPT),
        // Dynamic action menus and command results.
        EventSelector::Exact(E::ACTION_SCHEMA_PUBLISHED),
        EventSelector::Exact(E::ACTION_RESULT),
        EventSelector::Exact(E::ACTION_ERROR),
        // Agent and sub-agent lifecycle/rendering. The chat UI intentionally
        // consumes `agent.prompt_started`, not the heavier `agent.prompt_created`.
        EventSelector::Exact(E::AGENT_START_REQUEST),
        EventSelector::Exact(E::AGENT_START_ACCEPTED),
        EventSelector::Exact(E::AGENT_START_RESULT),
        EventSelector::Exact(E::AGENT_MESSAGE_SENT),
        EventSelector::Exact(E::AGENT_MESSAGE_RECEIVED),
        EventSelector::Exact(E::MESSAGE_DELIVERED),
        EventSelector::Exact(E::MESSAGE_EDITED),
        EventSelector::Exact(E::MESSAGE_DELETED),
        EventSelector::Exact(E::MESSAGE_REACTION_ADDED),
        EventSelector::Exact(E::MESSAGE_REACTION_REMOVED),
        EventSelector::Exact(E::MESSAGE_SENT),
        EventSelector::Exact(E::AGENT_PROMPT_SUBMITTED),
        EventSelector::Exact(E::AGENT_PROMPT_QUEUED),
        EventSelector::Exact(E::AGENT_PROMPT_RECALLED),
        EventSelector::Exact(E::AGENT_PROMPT_STEERED),
        EventSelector::Exact(E::AGENT_COMPACTION_TRIGGERED),
        EventSelector::Exact(E::AGENT_PROMPT_STARTED),
        EventSelector::Exact(E::AGENT_PROMPT_TERMINATED),
        EventSelector::Exact(E::AGENT_WATCHES_UPDATED),
        EventSelector::Exact(E::AGENT_STATS_UPDATED),
        EventSelector::Exact(E::AGENT_STARTED),
        EventSelector::Exact(E::AGENT_DISPLAY_NAME_SET),
        // Session and provider state rendered by the UI. Provider prompt
        // submitted/updated/finished events drive streamed assistant output.
        EventSelector::Exact(E::SESSION_STARTED),
        EventSelector::Exact(E::SESSION_SHUTDOWN),
        EventSelector::Exact(E::SESSION_AGENT_UNLOADED),
        EventSelector::Exact(E::PROVIDER_TOOL_ERROR),
        EventSelector::Exact(E::PROVIDER_PROMPT_SUBMITTED),
        EventSelector::Exact(E::PROVIDER_RESPONSE_UPDATED),
        EventSelector::Exact(E::PROVIDER_RESPONSE_FINISHED),
        // Tool and shell progress shown in generic ToolUseState blocks. The
        // interactive renderer starts pending tool state from `tool.started`;
        // it does not consume `tool.request`.
        EventSelector::Exact(E::TOOL_STARTED),
        EventSelector::Exact(E::TOOL_REJECTED),
        EventSelector::Exact(E::TOOL_RESULT),
        EventSelector::Exact(E::TOOL_ERROR),
        EventSelector::Exact(E::TOOL_BACKGROUND_RESULT),
        EventSelector::Exact(E::TOOL_BACKGROUND_ERROR),
        EventSelector::Exact(E::TOOL_PROGRESS),
        EventSelector::Exact(E::TOOL_CANCELLED),
        EventSelector::Exact(E::SHELL_COMMAND_PROGRESS),
        EventSelector::Exact(E::SHELL_COMMAND_FINISHED),
        // Extension/context/status events rendered in the transcript or used to
        // update available actions/skills/instructions.
        EventSelector::Exact(E::EXTENSION_STARTING),
        EventSelector::Exact(E::EXTENSION_READY),
        EventSelector::Exact(E::EXTENSION_EXITED),
        EventSelector::Exact(E::HARNESS_SESSION_SKILLS_AVAILABLE),
        EventSelector::Exact(E::HARNESS_AGENT_CONTEXT_INITIALIZED),
        EventSelector::Exact(E::EXTENSION_CONTEXT_READY),
        // Harness UI state, status, prompt/context accounting, and terminal
        // side-effect events.
        EventSelector::Exact(E::HARNESS_NOTICE),
        EventSelector::Exact(E::HARNESS_SESSION_DIR),
        EventSelector::Exact(E::HARNESS_UI_DIR),
        EventSelector::Exact(E::HARNESS_MODELS_AVAILABLE),
        EventSelector::Exact(E::HARNESS_ROLES_AVAILABLE),
        EventSelector::Exact(E::HARNESS_ROLE_SELECTED),
        EventSelector::Exact(E::HARNESS_CONTEXT_USAGE_CHANGED),
        EventSelector::Exact(E::HARNESS_AGENT_CONTEXT_USAGE_CHANGED),
        EventSelector::Exact(E::HARNESS_PROVIDER_QUOTA_CHANGED),
        EventSelector::Exact(E::HARNESS_EFFORTS_AVAILABLE),
        EventSelector::Exact(E::HARNESS_VERBOSITIES_AVAILABLE),
        EventSelector::Exact(E::HARNESS_THINKING_SUMMARIES_AVAILABLE),
        EventSelector::Exact(E::TERM_OSC1337_SET_USER_VAR),
        EventSelector::Exact(E::TERM_BELL),
    ]
}

pub(crate) fn subscribe_message(selectors: Vec<EventSelector>) -> HarnessInputMessage {
    HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors: selectors.clone(),
        live_selectors: selectors,
    })
}

/// Builds the production chat subscription with `tool.started` and terminal
/// side effects restricted to live delivery.
pub(crate) fn chat_subscribe_message() -> HarnessInputMessage {
    let live_selectors = chat_subscription_selectors();
    let historical_selectors = live_selectors
        .iter()
        .filter(|selector| {
            **selector != EventSelector::Exact(EventName::TOOL_STARTED)
                && **selector != EventSelector::Exact(EventName::TERM_OSC1337_SET_USER_VAR)
                && **selector != EventSelector::Exact(EventName::TERM_BELL)
        })
        .cloned()
        .collect();
    HarnessInputMessage::Subscribe(Subscribe {
        historical_selectors,
        live_selectors,
    })
}

pub(crate) fn send_hello(
    writer: &mut UiOutputWriter,
    client_name: impl Into<tau_proto::ExtensionName>,
) -> io::Result<()> {
    send_message(writer, &hello_message(client_name))
}

pub(crate) fn subscribe(
    writer: &mut UiOutputWriter,
    selectors: Vec<EventSelector>,
) -> io::Result<()> {
    send_message(writer, &subscribe_message(selectors))
}

pub(crate) fn send_message(
    writer: &mut UiOutputWriter,
    message: &HarnessInputMessage,
) -> io::Result<()> {
    writer.write_message(message).map_err(io::Error::other)?;
    writer.flush()
}

pub(crate) fn next_request_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        prefix,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests;
