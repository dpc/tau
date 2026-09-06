//! Headless graceful shutdown for one exact running session.

use tau_proto::{HarnessInputMessage, UiShutdownRequest};

use crate::CliError;

/// Requests canonical shutdown and reports success only after the exact socket
/// peer's process exit is confirmed.
pub(crate) fn run(session_id: &tau_proto::SessionId) -> Result<(), CliError> {
    let harness_path = tau_harness::runtime_dir::find_harness_for_session(session_id.as_str())
        .map_err(|error| CliError::Participant(error.to_string()))?
        .ok_or_else(|| {
            CliError::Participant(format!("session `{session_id}` is not currently running"))
        })?;
    let socket_path = tau_harness::runtime_dir::socket_path(&harness_path);
    let (reader, mut writer, peer_exit) = crate::ui_client::connect_ui_client_with_peer_exit(
        &socket_path,
        "tau-session-kill",
        session_id,
    )
    .map_err(|error| {
        CliError::Participant(format!(
            "cannot connect to running session `{session_id}`: {error}"
        ))
    })?;
    crate::ui_client::send_message(
        &mut writer,
        &HarnessInputMessage::UiShutdownRequest(UiShutdownRequest {}),
    )
    .map_err(|error| {
        CliError::Participant(format!(
            "failed to request shutdown for session `{session_id}`: {error}"
        ))
    })?;
    drop(writer);
    drop(reader);

    let Some(peer_exit) = peer_exit else {
        return Err(CliError::Participant(format!(
            "shutdown requested for session `{session_id}`, but this platform cannot confirm the daemon's exit"
        )));
    };
    match peer_exit.wait(crate::daemon::REQUESTED_DAEMON_EXIT_WAIT) {
        Ok(true) => {
            crate::line_output::write_stdout(&format!("Session `{session_id}` terminated\n"))
        }
        Ok(false) => Err(CliError::Participant(format!(
            "shutdown requested for session `{session_id}`, but the daemon did not confirm termination before the deadline"
        ))),
        Err(error) => Err(CliError::Participant(format!(
            "shutdown requested for session `{session_id}`, but daemon exit confirmation failed: {error}"
        ))),
    }
}
