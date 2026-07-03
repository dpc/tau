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

    vec![
        EventSelector::Prefix("ui.".to_owned()),
        EventSelector::Prefix("action.".to_owned()),
        EventSelector::Exact(E::AGENT_START_REQUEST),
        EventSelector::Exact(E::AGENT_START_ACCEPTED),
        EventSelector::Exact(E::AGENT_START_RESULT),
        EventSelector::Exact(E::AGENT_MESSAGE_SENT),
        EventSelector::Exact(E::AGENT_MESSAGE_RECEIVED),
        EventSelector::Exact(E::AGENT_STATE),
        EventSelector::Exact(E::AGENT_PROMPT_SUBMITTED),
        EventSelector::Exact(E::AGENT_PROMPT_QUEUED),
        EventSelector::Exact(E::AGENT_PROMPT_RECALLED),
        EventSelector::Exact(E::AGENT_PROMPT_STEERED),
        EventSelector::Exact(E::AGENT_COMPACTION_TRIGGERED),
        EventSelector::Exact(E::AGENT_PROMPT_STARTED),
        EventSelector::Exact(E::AGENT_PROMPT_TERMINATED),
        EventSelector::Exact(E::AGENT_USER_MESSAGE_INJECTED),
        EventSelector::Exact(E::AGENT_HEAD_MOVED),
        EventSelector::Exact(E::AGENT_STARTED),
        EventSelector::Exact(E::AGENT_DISPLAY_NAME_SET),
        EventSelector::Prefix("session.".to_owned()),
        EventSelector::Prefix("provider.".to_owned()),
        EventSelector::Prefix("tool.".to_owned()),
        EventSelector::Prefix("extension.".to_owned()),
        EventSelector::Prefix("harness.".to_owned()),
        EventSelector::Prefix("shell.".to_owned()),
        EventSelector::Prefix("term.".to_owned()),
    ]
}

pub(crate) fn subscribe_message(selectors: Vec<EventSelector>) -> HarnessInputMessage {
    HarnessInputMessage::Subscribe(Subscribe { selectors })
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
