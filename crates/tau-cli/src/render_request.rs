use std::io as path_std_io;

use tau_proto::{EventName, EventSelector, HarnessInputMessage, HarnessOutputMessage};

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
    handle_result: impl FnMut(HarnessOutputMessage, &str) -> RenderResponse<T>,
) -> Result<T, CliError> {
    request_rendered_value_with_selectors(
        daemon,
        client_name,
        request_id_prefix,
        Vec::new(),
        build_request,
        handle_result,
    )
}

/// Runs one rendered preview while subscribing to additional snapshot events.
pub(crate) fn request_rendered_value_with_selectors<T>(
    daemon: &mut DaemonHandle,
    client_name: &'static str,
    request_id_prefix: &str,
    additional_selectors: Vec<EventSelector>,
    build_request: impl FnOnce(String) -> HarnessInputMessage,
    mut handle_result: impl FnMut(HarnessOutputMessage, &str) -> RenderResponse<T>,
) -> Result<T, CliError> {
    let (mut reader, mut writer) =
        connect_render_client(daemon, client_name, additional_selectors)?;
    let result = (|| {
        crate::ui_client::wait_for_subscription(&mut reader, None)?;
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
                    RenderResponse::Matched(result) => return result,
                },
            }
        }
    })();
    // Render diagnostics own a private one-shot daemon. Terminate that daemon
    // explicitly; ordinary UI disconnect no longer controls session lifetime.
    let _ = crate::ui_client::send_message(
        &mut writer,
        &HarnessInputMessage::UiShutdownRequest(tau_proto::UiShutdownRequest {}),
    );
    disconnect_render_client(&mut writer);
    result
}

fn connect_render_client(
    daemon: &mut DaemonHandle,
    client_name: &'static str,
    mut additional_selectors: Vec<EventSelector>,
) -> Result<(UiInputReader, UiOutputWriter), CliError> {
    let (reader, mut writer) =
        crate::ui_client::connect_daemon_ui_client(daemon, client_name, None)?;
    additional_selectors.push(EventSelector::Exact(EventName::SESSION_REPLAY_COMPLETE));
    crate::ui_client::subscribe(&mut writer, additional_selectors)?;
    Ok((reader, writer))
}

fn disconnect_render_client(writer: &mut UiOutputWriter) {
    let _ = crate::ui_client::send_message(
        writer,
        &HarnessInputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    );
}
