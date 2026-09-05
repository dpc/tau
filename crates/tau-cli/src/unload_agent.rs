//! One-shot saved-agent unload command.

use std::io;
use std::time::{Duration, Instant};

use tau_proto::{
    HarnessInputMessage, HarnessOutputMessage, UnloadSessionAgent, UnloadSessionAgentOutcome,
};

use crate::CliError;

const AGENT_UNLOAD_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `tau agent unload`.
pub(crate) fn run(args: &crate::cli::AgentUnloadArgs) -> Result<(), CliError> {
    let harness_path = tau_harness::runtime_dir::find_harness_for_session(args.session_id.as_str())
        .map_err(|error| CliError::Participant(error.to_string()))?
        .ok_or_else(|| {
            CliError::Participant(format!(
                "no running harness for session `{}`",
                args.session_id
            ))
        })?;
    request_at_socket(
        &tau_harness::runtime_dir::socket_path(&harness_path),
        &args.session_id,
        &args.agent_id,
        AGENT_UNLOAD_RPC_TIMEOUT,
    )
}

/// Sends one unload RPC and waits for its directed terminal result.
fn request_at_socket(
    socket_path: &std::path::Path,
    session_id: &tau_proto::SessionId,
    agent_id: &tau_proto::AgentId,
    timeout: Duration,
) -> Result<(), CliError> {
    let deadline = Instant::now() + timeout;
    let (mut reader, mut writer) = crate::ui_client::connect_ui_client_until(
        socket_path,
        "tau-unload-agent",
        session_id,
        deadline,
    )?;
    let request_id = crate::ui_client::next_request_id("agent-unload");
    send_request(
        &mut writer,
        HarnessInputMessage::UnloadSessionAgent(UnloadSessionAgent {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        }),
    )?;
    loop {
        if Instant::now() >= deadline {
            return Err(indeterminate_error("agent unload request timed out"));
        }
        let message = match reader.read_message() {
            Ok(Some(message)) => message,
            Ok(None) => return Err(indeterminate_error("daemon disconnected")),
            Err(tau_proto::DecodeError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(indeterminate_error(&format!(
                    "response decode failed: {error}"
                )));
            }
        };
        match message {
            HarnessOutputMessage::UnloadSessionAgentResult(result)
                if result.request_id == request_id =>
            {
                if &result.session_id != session_id || &result.agent_id != agent_id {
                    return Err(indeterminate_error(
                        "agent unload response targeted a different session or agent",
                    ));
                }
                return classify_outcome(result.outcome);
            }
            HarnessOutputMessage::Disconnect(disconnect) => {
                return Err(indeterminate_error(
                    disconnect
                        .reason
                        .as_deref()
                        .unwrap_or("daemon disconnected"),
                ));
            }
            _ => {}
        }
    }
}

/// Writes and flushes the lifecycle request; every failure after writing starts
/// is indeterminate.
fn send_request<W: io::Write>(
    writer: &mut tau_proto::HarnessInputWriter<W>,
    request: HarnessInputMessage,
) -> Result<(), CliError> {
    writer.write_message(&request).map_err(|error| {
        indeterminate_error(&format!("agent unload request write failed: {error}"))
    })?;
    writer.flush().map_err(|error| {
        indeterminate_error(&format!("agent unload request flush failed: {error}"))
    })
}

/// Maps a typed harness outcome to the command's success or failure status.
fn classify_outcome(outcome: UnloadSessionAgentOutcome) -> Result<(), CliError> {
    match outcome {
        UnloadSessionAgentOutcome::Unloaded | UnloadSessionAgentOutcome::AlreadyUnloaded => Ok(()),
        other => Err(CliError::Participant(format!(
            "agent unload rejected: {}",
            other.as_str()
        ))),
    }
}

/// Marks failures after request transmission as indeterminate and safely
/// retryable.
fn indeterminate_error(detail: &str) -> CliError {
    CliError::Participant(format!("{detail}; outcome unknown; retry safely"))
}

#[cfg(test)]
mod tests;
