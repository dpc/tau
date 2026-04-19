//! First-party agent process.
//!
//! Receives `AgentPromptRequest` from the harness with a fully
//! assembled prompt and returns `AgentPromptResponse`.
//!
//! If a model is configured in `~/.config/tau/settings.json` and
//! `~/.config/tau/models.json`, the agent calls the LLM API.
//! Otherwise it falls back to deterministic behavior for testing.

mod openai;

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_config::settings::{self, ModelRegistry, Settings};
use tau_proto::{
    AgentPromptRequest, AgentPromptResponse, AgentToolCall, CborValue, ClientKind, ContentBlock,
    ConversationRole, Event, EventName, EventReader, EventSelector, EventWriter, LifecycleHello,
    LifecycleReady, LifecycleSubscribe, PROTOCOL_VERSION,
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

    // Try to load LLM config.
    let backend = resolve_backend();

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::AgentPromptRequest),
            EventSelector::Exact(EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some(match &backend {
            Backend::Llm(config) => format!("agent ready ({}:{})", config.base_url, config.model_id),
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
            Event::AgentPromptRequest(request) => {
                let response = match &backend {
                    Backend::Llm(config) => handle_llm_request(config, &request),
                    Backend::Deterministic => {
                        handle_deterministic_request(&request, &mut next_call_number)
                    }
                };
                writer.write_event(&Event::AgentPromptResponse(response))?;
                writer.flush()?;
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
    let settings = settings::load_settings().unwrap_or_default();
    let models = settings::load_models().unwrap_or_default();

    if let Some(config) = resolve_llm_config(&settings, &models) {
        Backend::Llm(config)
    } else {
        Backend::Deterministic
    }
}

fn resolve_llm_config(settings: &Settings, models: &ModelRegistry) -> Option<OpenAiConfig> {
    let default_model = settings.default_model.as_ref()?;

    // Parse "provider/model-id" format.
    let (provider_name, model_id) = default_model.split_once('/')?;

    let provider = models.providers.get(provider_name)?;
    let base_url = provider.base_url.as_ref()?;
    let api_key = provider.api_key.clone().unwrap_or_default();

    Some(OpenAiConfig {
        base_url: base_url.clone(),
        api_key,
        model_id: model_id.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// LLM backend
// ---------------------------------------------------------------------------

fn handle_llm_request(config: &OpenAiConfig, request: &AgentPromptRequest) -> AgentPromptResponse {
    match openai::chat_completion(config, request) {
        Ok(response) => response,
        Err(error) => {
            // Return the error as text so the user sees it.
            AgentPromptResponse {
                turn_id: request.turn_id.clone(),
                session_id: request.session_id.clone(),
                text: Some(format!("LLM error: {error}")),
                tool_calls: Vec::new(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic backend (testing/fallback)
// ---------------------------------------------------------------------------

fn handle_deterministic_request(
    request: &AgentPromptRequest,
    next_call_number: &mut u64,
) -> AgentPromptResponse {
    // If the last message is a tool result, format it as text.
    if let Some(msg) = request.messages.last() {
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
                    return AgentPromptResponse {
                        turn_id: request.turn_id.clone(),
                        session_id: request.session_id.clone(),
                        text: Some(text),
                        tool_calls: Vec::new(),
                    };
                }
            }
        }
    }

    // Find the last user text message.
    let user_text = request
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

    // Determine which tool to call.
    let call_id = format!("call-{next_call_number}");
    *next_call_number += 1;
    let tool_call = tool_call_for_text(&call_id, &user_text);

    AgentPromptResponse {
        turn_id: request.turn_id.clone(),
        session_id: request.session_id.clone(),
        text: None,
        tool_calls: vec![tool_call],
    }
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
    fn user_message_routes_to_fs_read_when_requested() {
        let call = tool_call_for_text("call-1", "read Cargo.toml");
        assert_eq!(call.name, "fs.read");
        assert_eq!(
            call.arguments,
            CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("Cargo.toml".to_owned()),
            )])
        );
    }

    #[test]
    fn user_message_routes_to_shell_exec_when_requested() {
        let call = tool_call_for_text("call-1", "shell printf hi");
        assert_eq!(call.name, "shell.exec");
    }

    #[test]
    fn resolve_config_parses_provider_model() {
        let settings = Settings {
            greeting: true,
            default_model: Some("local/llama-3".to_owned()),
        };
        let mut models = ModelRegistry::default();
        models.providers.insert(
            "local".to_owned(),
            settings::ProviderConfig {
                base_url: Some("http://localhost:8080/v1".to_owned()),
                api_key: Some("test".to_owned()),
                ..Default::default()
            },
        );

        let config = resolve_llm_config(&settings, &models).expect("should resolve");
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(config.model_id, "llama-3");
        assert_eq!(config.api_key, "test");
    }

    #[test]
    fn no_config_means_deterministic() {
        let settings = Settings::default();
        let models = ModelRegistry::default();
        assert!(resolve_llm_config(&settings, &models).is_none());
    }
}
