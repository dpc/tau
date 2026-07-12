//! Shared UI socket client helpers.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

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
    })
}

pub(crate) fn chat_subscription_selectors() -> Vec<EventSelector> {
    use EventName as E;

    // Keep this as an exact allow-list. Read the repository-root
    // `specs/DESIGN-exact-event-subscriptions.md` policy before adding broad
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
        EventSelector::Exact(E::AGENT_MESSAGE_INCOMING),
        EventSelector::Exact(E::AGENT_MESSAGE_OUTGOING),
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
        EventSelector::Exact(E::PROVIDER_TOOL_RESULT),
        EventSelector::Exact(E::PROVIDER_TOOL_ERROR),
        EventSelector::Exact(E::PROVIDER_PROMPT_SUBMITTED),
        EventSelector::Exact(E::PROVIDER_RESPONSE_UPDATED),
        EventSelector::Exact(E::PROVIDER_RESPONSE_FINISHED),
        // Tool and shell progress shown in generic ToolUseState blocks.
        EventSelector::Exact(E::TOOL_REQUEST),
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
        EventSelector::Exact(E::EXTENSION_SKILL_AVAILABLE),
        EventSelector::Exact(E::EXTENSION_AGENTS_MD_AVAILABLE),
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
