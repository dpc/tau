//! Headless command submission client.

use std::io as path_std_io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tau_proto::{Event, HarnessInputMessage, HarnessOutputMessage};

use crate::CliError;
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, PromptCommandHandling,
    create_user_agent_prompt,
};

const TREE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

enum SendLineDisposition {
    Message(Box<HarnessInputMessage>),
    Noop,
}

pub(crate) fn run_send(session_id: &str, line: &str) -> Result<(), CliError> {
    let session_id = tau_proto::SessionId::parse(session_id).map_err(|error| {
        CliError::Participant(format!("invalid session id `{session_id}`: {error}"))
    })?;
    let disposition = classify_send_line(&session_id, line)?;
    let SendLineDisposition::Message(message) = disposition else {
        return Ok(());
    };
    send_message(&session_id, *message)
}

fn classify_send_line(
    session_id: &tau_proto::SessionId,
    line: &str,
) -> Result<SendLineDisposition, CliError> {
    let canonical_line = tau_cli_term::canonical_literal_colon_prompt(line);
    let text = canonical_line.as_deref().unwrap_or(line).trim();
    if text.is_empty() {
        return Ok(SendLineDisposition::Noop);
    }

    if canonical_line.is_some() {
        return Ok(SendLineDisposition::Message(Box::new(
            HarnessInputMessage::emit(Event::UiCreateAgent(create_user_agent_prompt(
                session_id,
                DEFAULT_AGENT_ROLE,
                text,
                CreateUserAgentPromptOptions {
                    command_handling: PromptCommandHandling::LiteralEscape,
                    ..CreateUserAgentPromptOptions::default()
                },
            ))),
        )));
    }
    if text == ":tree" {
        return Ok(SendLineDisposition::Message(Box::new(
            crate::ui_events::tree_request_message(session_id, None),
        )));
    }
    if let Some(event) = event_for_line(session_id, text) {
        return Ok(SendLineDisposition::Message(Box::new(
            HarnessInputMessage::emit(event),
        )));
    }
    if valid_headless_noop(text) {
        return Ok(SendLineDisposition::Noop);
    }
    Err(CliError::Participant(format!(
        "unknown or unsupported command `{}`",
        text.split_whitespace().next().unwrap_or(text)
    )))
}

fn send_message(
    session_id: &tau_proto::SessionId,
    message: HarnessInputMessage,
) -> Result<(), CliError> {
    let harness_path = find_daemon_for_session(session_id.as_str()).ok_or_else(|| {
        CliError::Participant(format!("no running daemon for session `{session_id}`"))
    })?;
    let socket_path = tau_harness::runtime_dir::socket_path(&harness_path);
    if matches!(message, HarnessInputMessage::UiTreeRequest(_)) {
        let deadline = Instant::now() + TREE_REQUEST_TIMEOUT;
        let (mut reader, mut writer) =
            crate::ui_client::connect_ui_client_until(&socket_path, "tau-dev-send", deadline)?;
        crate::ui_client::send_message(&mut writer, &message)?;
        print!("{}", tree_stdout_text(&read_tree_result(&mut reader)?));
    } else {
        let mut writer = crate::ui_client::connect_ui_writer(&socket_path, "tau-dev-send")?;
        crate::ui_client::send_message(&mut writer, &message)?;
    }
    Ok(())
}

/// Formats one requester-directed tree result for unconditional headless
/// output.
fn tree_stdout_text(result: &str) -> String {
    format!("{result}\n")
}

#[cfg(test)]
fn message_for_line(session_id: &str, line: &str) -> Option<HarnessInputMessage> {
    let session_id = tau_proto::SessionId::parse(session_id).ok()?;
    match classify_send_line(&session_id, line).ok()? {
        SendLineDisposition::Message(message) => Some(*message),
        SendLineDisposition::Noop => None,
    }
}

#[cfg(test)]
fn event_for_test_line(session_id: &str, line: &str) -> Option<Event> {
    let HarnessInputMessage::Emit(emit) = message_for_line(session_id, line)? else {
        return None;
    };
    Some(*emit.event)
}

fn event_for_line(session_id: &tau_proto::SessionId, text: &str) -> Option<Event> {
    if text == ":quit" || text == ":quit-session" || text == ":detach" {
        return None;
    }
    if text == ":cancel" {
        return Some(crate::ui_events::cancel_prompt(session_id, None));
    }
    if text == ":retry" {
        return Some(crate::ui_events::retry_prompt(session_id, None));
    }
    if let Some(arg) = text.strip_prefix(":tree ")
        && let Ok(target) = crate::ui_commands::parse_tree_navigation_target(arg)
    {
        return Some(crate::ui_events::navigate_tree(session_id, None, target));
    }
    if text == ":compact" {
        return Some(crate::ui_events::compact_request(session_id, None));
    }
    if let Some(rest) = text.strip_prefix(":role ") {
        return role_event_for_command(rest.trim());
    }
    if let Some(model) = text.strip_prefix(":model ") {
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
    if text == ":skill" || text.starts_with(":skill ") || text.starts_with(":skill:") {
        return Some(Event::UiCreateAgent(create_user_agent_prompt(
            session_id,
            DEFAULT_AGENT_ROLE,
            text,
            CreateUserAgentPromptOptions::default(),
        )));
    }
    if !text.starts_with(':') {
        return Some(Event::UiCreateAgent(create_user_agent_prompt(
            session_id,
            DEFAULT_AGENT_ROLE,
            text,
            CreateUserAgentPromptOptions::default(),
        )));
    }
    None
}

fn valid_headless_noop(text: &str) -> bool {
    let args = text.split_whitespace().collect::<Vec<_>>();
    matches!(
        args.as_slice(),
        [":quit"
            | ":quit-session"
            | ":detach"
            | ":fast"
            | ":suspend"
            | ":resume"
            | ":version"
            | ":role"
            | ":pick-agent"
            | ":pick-agent-all"]
            | [":session-stats"]
            | [":new"]
            | [":new", _]
            | [":name", _, ..]
            | [":ephemeral"]
            | [":ephemeral", "on" | "off"]
            | [":set", _, _]
            | [":theme"]
            | [":theme", _]
            | [":prompt", _]
            | [":session", "new"]
            | [":provider-auth"]
            | [":provider-auth", _]
            | [":debug-show-ui-event-stats"]
            | [":debug-show-event-stats", _]
            | [":agent"]
            | [":agent", "new"]
            | [":agent", "switch" | "suspend" | "resume" | "auto"]
            | [":agent", "switch" | "suspend" | "resume" | "auto", _]
            | [":agent", "name", _, _, ..]
    )
}

fn read_tree_result(reader: &mut crate::ui_client::UiInputReader) -> Result<String, CliError> {
    loop {
        let Some(message) = reader.read_message().map_err(path_std_io::Error::other)? else {
            return Err(CliError::Participant(
                "daemon disconnected before returning the tree".to_owned(),
            ));
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => {
                if let Event::HarnessNotice(notice) = delivery.into_event()
                    && notice.kind == tau_proto::notice_kind::HARNESS_NOTICE
                    && notice.purpose == tau_proto::NoticePurpose::Response
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
