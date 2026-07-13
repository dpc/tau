//! Headless command submission client.

use std::path::PathBuf;

use tau_proto::{Event, HarnessInputMessage};

use crate::CliError;
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, create_user_agent_prompt,
};

pub(crate) fn run_send(session_id: &str, line: &str) -> Result<(), CliError> {
    let text = line.trim();
    if text.is_empty() {
        return Ok(());
    }

    let harness_path = find_daemon_for_session(session_id).ok_or_else(|| {
        CliError::Participant(format!("no running daemon for session `{session_id}`"))
    })?;
    let mut writer = crate::ui_client::connect_ui_writer(
        &tau_harness::runtime_dir::socket_path(&harness_path),
        "tau-dev-send",
    )?;

    if let Some(event) = event_for_line(session_id, text) {
        crate::ui_client::send_message(&mut writer, &HarnessInputMessage::emit(event))?;
    }

    Ok(())
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
    if text == "/tree" {
        return Some(crate::ui_events::tree_request(session_id, None));
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
