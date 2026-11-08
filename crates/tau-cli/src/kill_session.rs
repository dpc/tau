//! Headless graceful shutdown for exact running sessions.

use tau_proto::{HarnessInputMessage, UiShutdownRequest};

use crate::CliError;

/// Attempts every requested session in order and returns all failures together.
pub(crate) fn run(session_ids: &[tau_proto::SessionId]) -> Result<(), CliError> {
    if let [session_id] = session_ids {
        return run_one(session_id);
    }

    let mut failures = Vec::new();
    for session_id in session_ids {
        if let Err(error) = run_one(session_id) {
            failures.push(format!("session `{session_id}`: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliError::Participant(failures.join("\n")))
    }
}

/// Requests canonical shutdown and reports success only after the exact socket
/// peer's process exit is confirmed.
fn run_one(session_id: &tau_proto::SessionId) -> Result<(), CliError> {
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
    let Some(peer_exit) = peer_exit else {
        return Err(CliError::Participant(format!(
            "shutdown requested for session `{session_id}`, but this platform cannot confirm the daemon's exit"
        )));
    };
    // Keep admission alive until the daemon exits. Closing immediately after
    // flushing can make a concurrent downlink write fail and disconnect this
    // client before the harness handles its queued shutdown request.
    let exited = peer_exit.wait(crate::daemon::REQUESTED_DAEMON_EXIT_WAIT);
    drop((reader, writer));
    match exited {
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
