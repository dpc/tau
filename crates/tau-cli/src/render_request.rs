use std::io as path_std_io;

use tau_proto::{Event, EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage};

use crate::CliError;
use crate::daemon::DaemonHandle;
use crate::ui_client::{UiInputReader, UiOutputWriter};

pub(crate) enum RenderResponse<T> {
    Ignore,
    Matched(Result<T, CliError>),
}

pub(crate) fn request_rendered_value<T>(
    daemon: &mut DaemonHandle,
    client_name: &'static str,
    request_id_prefix: &str,
    build_request: impl FnOnce(String) -> HarnessInputMessage,
    handle_result: impl Fn(HarnessOutputMessage, &str) -> RenderResponse<T>,
) -> Result<T, CliError> {
    let (mut reader, mut writer) = connect_render_client(daemon, client_name)?;
    wait_for_preview_session(&mut reader)?;
    let request_id = crate::ui_client::next_request_id(request_id_prefix);
    crate::ui_client::send_message(&mut writer, &build_request(request_id.clone()))?;

    loop {
        let Some(message) = reader.read_message().map_err(path_std_io::Error::other)? else {
            return Err(CliError::Participant("daemon disconnected".to_owned()));
        };
        match message {
            HarnessOutputMessage::Disconnect(disconnect) => {
                return Err(CliError::Participant(
                    disconnect
                        .reason
                        .unwrap_or_else(|| "daemon disconnected".to_owned()),
                ));
            }
            message => match handle_result(message, &request_id) {
                RenderResponse::Ignore => {}
                RenderResponse::Matched(result) => {
                    disconnect_render_client(&mut writer);
                    return result;
                }
            },
        }
    }
}

fn connect_render_client(
    daemon: &mut DaemonHandle,
    client_name: &'static str,
) -> Result<(UiInputReader, UiOutputWriter), CliError> {
    let (reader, mut writer) = crate::ui_client::connect_daemon_ui_client(daemon, client_name)?;
    crate::ui_client::subscribe(
        &mut writer,
        vec![EventSelector::Exact(EventName::SESSION_REPLAY_COMPLETE)],
    )?;
    Ok((reader, writer))
}

/// Waits for the per-client catch-up boundary emitted after eager session
/// initialization, so previews include the stable extension context surface.
fn wait_for_preview_session(reader: &mut UiInputReader) -> Result<(), CliError> {
    loop {
        let Some(message) = reader.read_message().map_err(path_std_io::Error::other)? else {
            return Err(CliError::Participant("daemon disconnected".to_owned()));
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if let Event::SessionReplayComplete(complete) = *delivery.event {
                    if let Some(error) = complete.error {
                        return Err(CliError::Participant(format!(
                            "preview session `{}` failed to initialize: {error}",
                            complete.session_id
                        )));
                    }
                    return Ok(());
                }
            }
            HarnessOutputMessage::Disconnect(disconnect) => {
                return Err(CliError::Participant(
                    disconnect
                        .reason
                        .unwrap_or_else(|| "daemon disconnected".to_owned()),
                ));
            }
            _ => {}
        }
    }
}

fn disconnect_render_client(writer: &mut UiOutputWriter) {
    let _ = crate::ui_client::send_message(
        writer,
        &HarnessInputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    );
}
