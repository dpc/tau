//! Headless command submission client.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tau_proto::{Event, HarnessInputMessage, HarnessOutputMessage};

use crate::CliError;
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, create_user_agent_prompt,
};

const TREE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn run_send(session_id: &str, line: &str) -> Result<(), CliError> {
    let text = line.trim();
    if text.is_empty() {
        return Ok(());
    }

    let harness_path = find_daemon_for_session(session_id).ok_or_else(|| {
        CliError::Participant(format!("no running daemon for session `{session_id}`"))
    })?;
    let Some(message) = message_for_line(session_id, text) else {
        return Ok(());
    };
    let socket_path = tau_harness::runtime_dir::socket_path(&harness_path);
    if matches!(message, HarnessInputMessage::UiTreeRequest(_)) {
        let deadline = Instant::now() + TREE_REQUEST_TIMEOUT;
        let (mut reader, mut writer) =
            crate::ui_client::connect_ui_client_until(&socket_path, "tau-dev-send", deadline)?;
        crate::ui_client::send_message(&mut writer, &message)?;
        println!("{}", read_tree_result(&mut reader)?);
    } else {
        let mut writer = crate::ui_client::connect_ui_writer(&socket_path, "tau-dev-send")?;
        crate::ui_client::send_message(&mut writer, &message)?;
    }

    Ok(())
}

fn message_for_line(session_id: &str, text: &str) -> Option<HarnessInputMessage> {
    if text == "/tree" {
        return Some(crate::ui_events::tree_request_message(session_id, None));
    }
    event_for_line(session_id, text).map(HarnessInputMessage::emit)
}

fn event_for_line(session_id: &str, text: &str) -> Option<Event> {
    if text == "/quit" || text == "/detach" {
        return None;
    }
    if text == "/cancel" {
        return Some(crate::ui_events::cancel_prompt(session_id, None));
    }
    if text == "/retry" {
        return Some(crate::ui_events::retry_prompt(session_id, None));
    }
    if text
        .strip_prefix("/retry")
        .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_whitespace))
    {
        return None;
    }
    if let Some(arg) = text.strip_prefix("/tree ")
        && let Ok(target) = crate::ui_commands::parse_tree_navigation_target(arg)
    {
        return Some(crate::ui_events::navigate_tree(session_id, None, target));
    }
    if text == "/compact" {
        return Some(crate::ui_events::compact_request(session_id, None));
    }
    if text == "/fast" || text.starts_with("/fast ") {
        return None;
    }
    if text == "/role" {
        return None;
    }
    if let Some(rest) = text.strip_prefix("/role ") {
        return role_event_for_command(rest.trim());
    }
    if let Some(model) = text.strip_prefix("/model ") {
        let model = model.trim();
        if let Ok(model) = model.parse::<tau_proto::ModelId>() {
            return Some(crate::ui_events::agent_model_select(
                session_id, None, model,
            ));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix("!!") {
        let command = command.trim();
        if !command.is_empty() {
            return Some(crate::ui_events::shell_command(
                session_id, command, false, None,
            ));
        }
        return None;
    }
    if let Some(command) = text.strip_prefix('!') {
        let command = command.trim();
        if !command.is_empty() {
            return Some(crate::ui_events::shell_command(
                session_id, command, true, None,
            ));
        }
        return None;
    }

    Some(create_user_agent_prompt(
        session_id,
        DEFAULT_AGENT_ROLE,
        text,
        CreateUserAgentPromptOptions::default(),
    ))
}

fn read_tree_result(reader: &mut crate::ui_client::UiInputReader) -> Result<String, CliError> {
    loop {
        let Some(message) = reader.read_message().map_err(std::io::Error::other)? else {
            return Err(CliError::Participant(
                "daemon disconnected before returning the tree".to_owned(),
            ));
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if let Event::HarnessNotice(notice) = delivery.into_event()
                    && notice.kind == tau_proto::notice_kind::HARNESS_NOTICE
                {
                    return Ok(notice.message);
                }
            }
            HarnessOutputMessage::Disconnect(disconnect) => {
                return Err(CliError::Participant(disconnect.reason.unwrap_or_else(
                    || "daemon disconnected before returning the tree".to_owned(),
                )));
            }
            _ => {}
        }
    }
}

fn role_event_for_command(rest: &str) -> Option<Event> {
    crate::ui_commands::parse_role_command(rest).ok()?
}

fn find_daemon_for_session(session_id: &str) -> Option<PathBuf> {
    tau_harness::runtime_dir::find_harness_for_session(session_id)
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests;
