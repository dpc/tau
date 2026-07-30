//! One-shot stdin prompt client.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use tau_harness::SessionLaunchStatus;
use tau_proto::{
    AgentPromptTerminated, ContentPart, ContextItem, ContextRole, Event, EventName, EventSelector,
    HarnessInputMessage, HarnessOutputMessage, ProviderResponseFinished, ProviderResponseTextDelta,
    ProviderResponseUpdated, UiCreateAgentOutcome,
};

use crate::daemon::{DaemonCliOverrides, DaemonHandle, daemon_output_for_session, resolve_daemon};
use crate::ui_prompt::{
    CreateUserAgentPromptOptions, DEFAULT_AGENT_ROLE, create_user_agent_prompt,
};
use crate::{CliError, PromptStdinError};

const CREATE_AGENT_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Read a single user prompt from stdin, submit it to a daemon, print the final
/// reasoning snapshots and answer, then exit.
pub(crate) fn run_prompt_stdin(
    session_id: &tau_proto::SessionId,
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
    print_prompt_stdin_headers(session_id.as_str(), startup_role);

    let daemon_output = if attach {
        None
    } else {
        Some(daemon_output_for_session(session_id.as_str(), ephemeral)?)
    };
    let mut daemon = resolve_daemon(
        attach,
        session_id.as_str(),
        session_status,
        daemon_output,
        startup_role,
        cli_overrides,
        ephemeral,
    )?;
    let (reader, mut writer) = connect_prompt_stdin_client(&mut daemon)?;
    let messages = spawn_prompt_stdin_reader(reader);
    let result = (|| {
        let role = prompt_stdin_role(startup_role);
        let submitted = submit_prompt(&mut writer, session_id, role, prompt)?;
        let admission =
            wait_for_create_agent_admission(&messages, &submitted.request_id, &submitted.ctx_id)?;

        let mut output = OneShotOutput {
            request_id: Some(submitted.request_id),
            agent_id: Some(admission.agent_id),
            ctx_id: Some(submitted.ctx_id),
            initial_prompt_index: admission.initial_prompt_index,
            ..OneShotOutput::default()
        };
        read_one_shot_result(&messages, &mut output)?;
        output.write()?;
        Ok(())
    })();

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
            EventSelector::Exact(EventName::AGENT_PROMPT_FAILED),
            EventSelector::Exact(EventName::AGENT_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CREATE_AGENT_RESULT),
        ],
    )
}
fn submit_prompt(
    writer: &mut OneShotWriter,
    session_id: &tau_proto::SessionId,
    role: &str,
    prompt: String,
) -> io::Result<SubmittedPrompt> {
    let request = create_user_agent_prompt(
        session_id,
        role,
        prompt,
        CreateUserAgentPromptOptions::default(),
    );
    let request_id = request.request_id.clone();
    let ctx_id = request
        .ctx_id
        .clone()
        .expect("initial user prompt has a correlation id");
    crate::ui_client::send_message(
        writer,
        &HarnessInputMessage::emit(Event::UiCreateAgent(request)),
    )?;
    Ok(SubmittedPrompt { request_id, ctx_id })
}

/// Correlation identities stamped on one submitted stdin prompt.
struct SubmittedPrompt {
    /// Admission-only create request identity.
    request_id: String,
    /// Materialized prompt/provider-chain identity.
    ctx_id: String,
}

type PromptStdinMessages =
    mpsc::Receiver<Result<Option<HarnessOutputMessage>, tau_proto::DecodeError>>;

fn spawn_prompt_stdin_reader(mut reader: OneShotReader) -> PromptStdinMessages {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let message = reader.read_message();
            let done = !matches!(message, Ok(Some(_)));
            if tx.send(message).is_err() || done {
                break;
            }
        }
    });
    rx
}

fn wait_for_create_agent_admission(
    messages: &PromptStdinMessages,
    request_id: &str,
    ctx_id: &str,
) -> Result<CreateAgentAdmission, CliError> {
    wait_for_create_agent_admission_until(
        messages,
        request_id,
        ctx_id,
        std::time::Instant::now() + CREATE_AGENT_ADMISSION_TIMEOUT,
    )
}

fn wait_for_create_agent_admission_until(
    messages: &PromptStdinMessages,
    request_id: &str,
    ctx_id: &str,
    deadline: std::time::Instant,
) -> Result<CreateAgentAdmission, CliError> {
    let mut initial_prompt = None;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(CliError::PromptStdin(PromptStdinError::AdmissionTimeout {
                timeout: CREATE_AGENT_ADMISSION_TIMEOUT,
            }));
        }
        let message = messages
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    CliError::PromptStdin(PromptStdinError::AdmissionTimeout {
                        timeout: CREATE_AGENT_ADMISSION_TIMEOUT,
                    })
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    CliError::Participant("daemon disconnected".to_owned())
                }
            })?;
        let Some(message) = message.map_err(io::Error::other)? else {
            return Err(CliError::Participant("daemon disconnected".to_owned()));
        };
        match message {
            HarnessOutputMessage::Deliver(delivery) => match delivery.into_event() {
                Event::AgentPromptCreated(prompt) if prompt.ctx_id.as_deref() == Some(ctx_id) => {
                    if initial_prompt.is_none()
                        && let Some(index) = parse_agent_prompt_index(&prompt.agent_prompt_id)
                    {
                        initial_prompt = Some((prompt.agent_id, index));
                    }
                }
                Event::UiCreateAgentResult(result) if result.request_id == request_id => {
                    return match result.outcome {
                        UiCreateAgentOutcome::Created { agent_id, .. } => {
                            let initial_prompt_index = initial_prompt
                                .filter(|(prompt_agent_id, _)| prompt_agent_id == &agent_id)
                                .map(|(_, index)| index);
                            Ok(CreateAgentAdmission {
                                agent_id,
                                initial_prompt_index,
                            })
                        }
                        UiCreateAgentOutcome::Rejected {
                            reason, message, ..
                        } => Err(CliError::PromptStdin(PromptStdinError::Rejected {
                            reason,
                            message,
                        })),
                    };
                }
                _ => {}
            },
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

#[derive(Debug)]
/// Accepted create identity retained across the provider-response phase.
struct CreateAgentAdmission {
    /// Agent that exclusively owns accepted one-shot output.
    agent_id: tau_proto::AgentId,
    /// First correlated prompt counter, when materialized before admission.
    initial_prompt_index: Option<u64>,
}

fn read_one_shot_result(
    messages: &PromptStdinMessages,
    output: &mut OneShotOutput,
) -> Result<(), CliError> {
    loop {
        let message = messages
            .recv()
            .map_err(|_| CliError::Participant("daemon disconnected".to_owned()))?;
        let Some(message) = message.map_err(io::Error::other)? else {
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
            Event::ProviderResponseFinished(finished) => {
                if let Some(message) = output.finished_failure(&finished) {
                    Err(CliError::PromptStdin(PromptStdinError::ExecutionFailed {
                        message,
                    }))
                } else {
                    Ok(output.capture_finished(&finished))
                }
            }
            Event::AgentPromptCreated(prompt)
                if output.initial_prompt_index.is_none()
                    && output.agent_id.as_ref() == Some(&prompt.agent_id)
                    && output.ctx_id.as_deref() == prompt.ctx_id.as_deref() =>
            {
                output.initial_prompt_index = parse_agent_prompt_index(&prompt.agent_prompt_id);
                Ok(false)
            }
            Event::AgentPromptTerminated(terminated)
                if output
                    .agent_id
                    .as_ref()
                    .is_none_or(|agent_id| agent_id == &terminated.agent_id) =>
            {
                if output.prompt_is_in_owned_chain(&terminated.agent_prompt_id) {
                    handle_prompt_terminated(&terminated)
                } else {
                    Ok(false)
                }
            }
            Event::AgentPromptTerminated(_) => Ok(false),
            Event::AgentPromptFailed(failed)
                if output.request_id.as_deref() == Some(failed.request_id.as_str())
                    && output.agent_id.as_ref() == Some(&failed.agent_id)
                    && output.ctx_id.as_deref() == Some(failed.ctx_id.as_str()) =>
            {
                Err(CliError::PromptStdin(PromptStdinError::PromptFailed {
                    stage: failed.stage,
                    message: failed.message,
                }))
            }
            Event::AgentPromptFailed(_) => Ok(false),
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
/// Output and identity state for one admitted stdin invocation.
struct OneShotOutput {
    /// Create request that introduced this initial prompt.
    request_id: Option<String>,
    /// Agent returned by the correlated create result.
    agent_id: Option<tau_proto::AgentId>,
    /// Distinct correlation id copied through prompt materialization.
    ctx_id: Option<String>,
    /// Lowest prompt counter belonging to this invocation's provider chain.
    initial_prompt_index: Option<u64>,
    /// Streaming reasoning accumulated by provider prompt id.
    thinking_by_prompt: HashMap<String, String>,
    /// Streaming assistant text accumulated by provider prompt id.
    response_by_prompt: HashMap<String, String>,
    /// Completed reasoning blocks in provider-chain order.
    thinking_blocks: Vec<String>,
    /// Final assistant answer for stdout.
    final_response: Option<String>,
}

impl OneShotOutput {
    fn capture_update(&mut self, update: &ProviderResponseUpdated) {
        if !update.originator.is_user()
            || self
                .agent_id
                .as_ref()
                .is_some_and(|agent_id| agent_id != &update.agent_id)
            || !self.prompt_is_in_owned_chain(&update.agent_prompt_id)
        {
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

    fn finished_failure(&self, finished: &ProviderResponseFinished) -> Option<String> {
        if !self.owns_finished(finished) {
            return None;
        }
        let failed = matches!(
            finished.stop_reason,
            tau_proto::ProviderStopReason::Error
                | tau_proto::ProviderStopReason::RepetitionDetected
        ) || finished.failure_kind.is_some()
            || finished.error.is_some();
        failed.then(|| {
            finished.error.clone().unwrap_or_else(|| {
                finished.failure_kind.map_or_else(
                    || format!("provider stopped with {:?}", finished.stop_reason),
                    |kind| format!("provider failure: {}", kind.as_str()),
                )
            })
        })
    }

    fn owns_finished(&self, finished: &ProviderResponseFinished) -> bool {
        finished.originator.is_user()
            && self
                .agent_id
                .as_ref()
                .is_none_or(|agent_id| agent_id == &finished.agent_id)
            && self.prompt_is_in_owned_chain(&finished.agent_prompt_id)
    }

    fn capture_finished(&mut self, finished: &ProviderResponseFinished) -> bool {
        if !self.owns_finished(finished) {
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

    fn prompt_is_in_owned_chain(&self, prompt_id: &tau_proto::AgentPromptId) -> bool {
        self.ctx_id.is_none()
            || self.initial_prompt_index.is_some_and(|initial| {
                parse_agent_prompt_index(prompt_id).is_some_and(|current| initial <= current)
            })
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

fn parse_agent_prompt_index(prompt_id: &tau_proto::AgentPromptId) -> Option<u64> {
    prompt_id.as_str().rsplit_once('-')?.1.parse().ok()
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
