//! First-party agent process.
//!
//! Receives `SessionPromptCreated` from the harness and emits
//! `AgentResponseUpdated` / `AgentResponseFinished` events.

mod openai;

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_config::settings::{self, HarnessSettings, ModelRegistry};
use tau_proto::{
    AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, AgentToolCall, CborValue,
    ClientKind, ContentBlock, ConversationRole, Event, EventName, EventReader, EventSelector,
    EventWriter, LifecycleHello, LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION,
};

use crate::openai::OpenAiConfig;

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    let backend = resolve_backend();

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SessionPromptCreated),
            EventSelector::Exact(EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some(match &backend {
            Backend::Llm(config) => {
                format!("agent ready ({}:{})", config.base_url, config.model_id)
            }
            Backend::Deterministic => "agent ready (deterministic)".to_owned(),
        }),
    }))?;
    writer.flush()?;

    let mut next_call_number = 1_u64;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::SessionPromptCreated(prompt) => {
                let session_prompt_id = prompt.session_prompt_id.clone();

                // Announce we accepted the prompt.
                writer.write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                    session_prompt_id: session_prompt_id.clone(),
                }))?;
                writer.flush()?;

                match &backend {
                    Backend::Llm(config) => {
                        handle_llm(&session_prompt_id, config, &prompt, &mut writer)?;
                    }
                    Backend::Deterministic => {
                        let (text, tool_calls) =
                            handle_deterministic(&prompt, &mut next_call_number);
                        if let Some(ref t) = text {
                            writer.write_event(&Event::AgentResponseUpdated(
                                AgentResponseUpdated {
                                    session_prompt_id: session_prompt_id.clone(),
                                    text: t.clone(),
                                },
                            ))?;
                        }
                        writer.write_event(&Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text,
                                tool_calls,
                            },
                        ))?;
                        writer.flush()?;
                    }
                }
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Backend resolution
// ---------------------------------------------------------------------------

enum Backend {
    Llm(OpenAiConfig),
    Deterministic,
}

fn resolve_backend() -> Backend {
    let settings = settings::load_harness_settings().unwrap_or_default();
    let models = settings::load_models().unwrap_or_default();
    if let Some(config) = resolve_llm_config(&settings, &models) {
        Backend::Llm(config)
    } else {
        Backend::Deterministic
    }
}

fn resolve_llm_config(settings: &HarnessSettings, models: &ModelRegistry) -> Option<OpenAiConfig> {
    let default_model = settings.default_model.as_ref()?;
    let (provider_name, model_id) = default_model.split_once('/')?;
    let provider = models.providers.get(provider_name)?;
    let base_url = provider.base_url.as_ref()?;
    Some(OpenAiConfig {
        base_url: base_url.clone(),
        api_key: provider.api_key.clone().unwrap_or_default(),
        model_id: model_id.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// LLM backend
// ---------------------------------------------------------------------------

fn handle_llm<W: Write>(
    session_prompt_id: &str,
    config: &OpenAiConfig,
    prompt: &tau_proto::SessionPromptCreated,
    writer: &mut EventWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    // Build a minimal AgentPromptRequest-like struct for the openai
    // client (it needs system_prompt, messages, tools).
    let request = openai::PromptPayload {
        system_prompt: &prompt.system_prompt,
        messages: &prompt.messages,
        tools: &prompt.tools,
    };

    match openai::chat_completion_stream(config, &request, |text_so_far| {
        let _ = writer.write_event(&Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: session_prompt_id.into(),
            text: text_so_far.to_owned(),
        }));
        let _ = writer.flush();
    }) {
        Ok(state) => {
            let text = if state.text.is_empty() {
                None
            } else {
                Some(state.text.clone())
            };
            writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: session_prompt_id.into(),
                text,
                tool_calls: state.into_tool_calls(),
            }))?;
            writer.flush()?;
        }
        Err(error) => {
            writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: session_prompt_id.into(),
                text: Some(format!("LLM error: {error}")),
                tool_calls: Vec::new(),
            }))?;
            writer.flush()?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deterministic backend
// ---------------------------------------------------------------------------

fn handle_deterministic(
    prompt: &tau_proto::SessionPromptCreated,
    next_call_number: &mut u64,
) -> (Option<String>, Vec<AgentToolCall>) {
    // If the last message is a tool result, return it as text.
    if let Some(msg) = prompt.messages.last() {
        if msg.role == ConversationRole::User {
            for block in &msg.content {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    let text = if *is_error {
                        format!("Tool error: {content}")
                    } else {
                        content.clone()
                    };
                    return (Some(text), Vec::new());
                }
            }
        }
    }

    // Find the last user text and decide which tool to call.
    let user_text = prompt
        .messages
        .iter()
        .rev()
        .find(|m| m.role == ConversationRole::User)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default();

    let call_id = format!("call-{next_call_number}");
    *next_call_number += 1;
    (None, vec![tool_call_for_text(&call_id, &user_text)])
}

fn tool_call_for_text(call_id: &str, text: &str) -> AgentToolCall {
    if let Some(path) = text.strip_prefix("read ") {
        return AgentToolCall {
            id: call_id.to_owned(),
            name: "fs.read".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.trim().to_owned()),
            )]),
        };
    }
    if let Some(command) = text.strip_prefix("shell ") {
        return AgentToolCall {
            id: call_id.to_owned(),
            name: "shell.exec".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text(command.trim().to_owned()),
            )]),
        };
    }
    AgentToolCall {
        id: call_id.to_owned(),
        name: "demo.echo".to_owned(),
        arguments: CborValue::Text(text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_routes_to_fs_read() {
        let call = tool_call_for_text("call-1", "read Cargo.toml");
        assert_eq!(call.name, "fs.read");
    }

    #[test]
    fn user_message_routes_to_shell_exec() {
        let call = tool_call_for_text("call-1", "shell printf hi");
        assert_eq!(call.name, "shell.exec");
    }

    #[test]
    fn no_config_means_deterministic() {
        let settings = HarnessSettings::default();
        let models = ModelRegistry::default();
        assert!(resolve_llm_config(&settings, &models).is_none());
    }
}
