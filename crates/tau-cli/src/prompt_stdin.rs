//! One-shot stdin prompt client.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use tau_harness::SessionLaunchStatus;
use tau_proto::{
    AgentPromptTerminated, ContentPart, ContextItem, ContextRole, Event, EventName, EventSelector,
    HarnessInputMessage, HarnessOutputMessage, ProviderResponseFinished, ProviderResponseTextDelta,
    ProviderResponseUpdated,
};

use crate::CliError;
use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, create_user_agent_prompt,
};

/// Read a single user prompt from stdin, submit it to a daemon, print the final
/// reasoning snapshots and answer, then exit.
pub(crate) fn run_prompt_stdin(
    session_id: &str,
    attach: bool,
    session_status: SessionLaunchStatus,
    startup_role: Option<&str>,
    cli_overrides: DaemonCliOverrides<'_>,
    ephemeral: bool,
) -> Result<(), CliError> {
    let mut prompt = String::new();
    io::stdin().read_to_string(&mut prompt)?;
    if prompt.is_empty() {
        return Ok(());
    }
    print_prompt_stdin_headers(session_id, startup_role);

    let daemon_output = if attach {
        None
    } else {
        Some(daemon_output_for_session(session_id, ephemeral)?)
    };
    let mut daemon = resolve_daemon(
        attach,
        session_id,
        session_status,
        daemon_output,
        startup_role,
        cli_overrides,
        ephemeral,
    )?;
    let (mut reader, mut writer) = connect_prompt_stdin_client(&mut daemon)?;
    let role = prompt_stdin_role(startup_role);
    submit_prompt(&mut writer, session_id, role, prompt)?;

    let mut output = OneShotOutput::default();
    let result = read_one_shot_result(&mut reader, &mut output);

    if result.is_ok() {
        output.write()?;
    }

    disconnect_prompt_stdin_client(&mut writer);
    drop(writer);
    drop(daemon);

    result
}

type OneShotReader = crate::ui_client::UiInputReader;
type OneShotWriter = crate::ui_client::UiOutputWriter;

fn print_prompt_stdin_headers(session_id: &str, startup_role: Option<&str>) {
    eprintln!("session_id: {session_id}");
    eprintln!("role: {}", prompt_stdin_role(startup_role));
}

fn prompt_stdin_role(startup_role: Option<&str>) -> &str {
    startup_role.unwrap_or(DEFAULT_AGENT_ROLE)
}

fn connect_prompt_stdin_client(
    daemon: &mut DaemonHandle,
) -> io::Result<(OneShotReader, OneShotWriter)> {
    let (reader, mut writer) =
        crate::ui_client::connect_daemon_ui_client(daemon, "tau-prompt-stdin")?;
    subscribe_to_prompt_stdin_events(&mut writer)?;
    Ok((reader, writer))
}

fn subscribe_to_prompt_stdin_events(writer: &mut OneShotWriter) -> io::Result<()> {
    crate::ui_client::subscribe(
        writer,
        vec![
            EventSelector::Exact(EventName::PROVIDER_RESPONSE_UPDATED),
            EventSelector::Exact(EventName::PROVIDER_RESPONSE_FINISHED),
            EventSelector::Exact(EventName::AGENT_PROMPT_TERMINATED),
        ],
    )
}
fn submit_prompt(
    writer: &mut OneShotWriter,
    session_id: &str,
    role: &str,
    prompt: String,
) -> io::Result<()> {
    crate::ui_client::send_message(
        writer,
        &HarnessInputMessage::emit(create_user_agent_prompt(
            session_id,
            role,
            prompt,
            CreateUserAgentPromptOptions::default(),
        )),
    )
}
fn read_one_shot_result(
    reader: &mut OneShotReader,
    output: &mut OneShotOutput,
) -> Result<(), CliError> {
    loop {
        let Some(message) = reader.read_message().map_err(io::Error::other)? else {
            return Err(CliError::Participant("daemon disconnected".to_owned()));
        };
        if handle_prompt_stdin_message(message, output)? {
            return Ok(());
        }
    }
}

fn handle_prompt_stdin_message(
    message: HarnessOutputMessage,
    output: &mut OneShotOutput,
) -> Result<bool, CliError> {
    match message {
        HarnessOutputMessage::Deliver(delivery) => match delivery.into_event() {
            Event::ProviderResponseUpdated(update) => {
                output.capture_update(&update);
                Ok(false)
            }
            Event::ProviderResponseFinished(finished) => Ok(output.capture_finished(&finished)),
            Event::AgentPromptTerminated(terminated) => handle_prompt_terminated(&terminated),
            _ => Ok(false),
        },
        HarnessOutputMessage::Disconnect(disconnect) => Err(CliError::Participant(
            disconnect
                .reason
                .unwrap_or_else(|| "daemon disconnected".to_owned()),
        )),
        _ => Ok(false),
    }
}

fn handle_prompt_terminated(terminated: &AgentPromptTerminated) -> Result<bool, CliError> {
    if terminated.originator.is_user() {
        return Err(CliError::Participant(format!(
            "prompt terminated: {}",
            terminated_reason(terminated)
        )));
    }
    Ok(false)
}

fn disconnect_prompt_stdin_client(writer: &mut OneShotWriter) {
    let _ = crate::ui_client::send_message(
        writer,
        &HarnessInputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("prompt-stdin done".to_owned()),
        }),
    );
}

#[derive(Default)]
struct OneShotOutput {
    thinking_by_prompt: HashMap<String, String>,
    response_by_prompt: HashMap<String, String>,
    thinking_blocks: Vec<String>,
    final_response: Option<String>,
}

impl OneShotOutput {
    fn capture_update(&mut self, update: &ProviderResponseUpdated) {
        if !update.originator.is_user() {
            return;
        }
        let prompt_id = update.agent_prompt_id.to_string();
        if update
            .status
            .as_ref()
            .is_some_and(|status| status.clear_response)
        {
            self.thinking_by_prompt.remove(&prompt_id);
            self.response_by_prompt.remove(&prompt_id);
        }
        let thinking = reasoning_text_from_update(update);
        if let Some(thinking) = thinking.filter(|thinking| !thinking.is_empty()) {
            self.thinking_by_prompt
                .entry(prompt_id.clone())
                .or_default()
                .push_str(&thinking);
        }
        let text = assistant_text_from_update(update).unwrap_or_default();
        if !text.is_empty() {
            self.response_by_prompt
                .entry(prompt_id)
                .or_default()
                .push_str(&text);
        }
    }

    fn capture_finished(&mut self, finished: &ProviderResponseFinished) -> bool {
        if !finished.originator.is_user() {
            return false;
        }
        if let Some(thinking) =
            reasoning_text_from_output_items(&finished.output_items).or_else(|| {
                self.thinking_by_prompt
                    .remove(finished.agent_prompt_id.as_str())
            })
        {
            self.thinking_blocks.push(thinking);
        }
        if finished.stop_reason.requests_tool_calls() {
            return false;
        }
        self.final_response =
            assistant_text_from_output_items(&finished.output_items).or_else(|| {
                self.response_by_prompt
                    .remove(finished.agent_prompt_id.as_str())
            });
        true
    }

    fn write(&self) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        self.write_to(&mut stdout, &mut stderr)
    }

    fn write_to(&self, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<()> {
        let mut wrote_thinking = false;
        for thinking in &self.thinking_blocks {
            write_text_block(stderr, &mut wrote_thinking, thinking)?;
        }
        if wrote_thinking {
            stderr.write_all(b"\n")?;
        }

        let mut wrote_response = false;
        if let Some(response) = self.final_response.as_deref() {
            write_text_block(stdout, &mut wrote_response, response)?;
        }
        if wrote_response {
            stdout.write_all(b"\n")?;
        }
        stderr.flush()?;
        stdout.flush()
    }
}

fn write_text_block(output: &mut impl Write, wrote_block: &mut bool, text: &str) -> io::Result<()> {
    if *wrote_block {
        output.write_all(b"\n\n")?;
    }
    output.write_all(text.as_bytes())?;
    *wrote_block = true;
    Ok(())
}

fn assistant_text_from_update(update: &ProviderResponseUpdated) -> Option<String> {
    let text = update
        .deltas
        .iter()
        .filter_map(|delta| match delta {
            ProviderResponseTextDelta::Message { text, .. } => Some(text.as_str()),
            ProviderResponseTextDelta::ReasoningText { .. } => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn reasoning_text_from_update(update: &ProviderResponseUpdated) -> Option<String> {
    let text = update
        .deltas
        .iter()
        .filter_map(|delta| match delta {
            ProviderResponseTextDelta::ReasoningText { text, .. } => Some(text.as_str()),
            ProviderResponseTextDelta::Message { .. } => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn reasoning_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ReasoningText(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(assistant_text_from_context_item)
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn assistant_text_from_context_item(item: &ContextItem) -> Option<String> {
    match item {
        ContextItem::Message(message) if message.role == ContextRole::Assistant => Some(
            message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.as_str(),
                })
                .collect::<String>(),
        ),
        _ => None,
    }
}

fn terminated_reason(terminated: &AgentPromptTerminated) -> &'static str {
    match terminated.reason {
        tau_proto::AgentPromptTerminationReason::Stale => "stale",
        tau_proto::AgentPromptTerminationReason::Canceled => "canceled",
    }
}

#[cfg(test)]
mod tests;
