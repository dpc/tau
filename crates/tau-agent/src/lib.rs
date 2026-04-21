//! First-party agent process.
//!
//! Receives `SessionPromptCreated` from the harness and emits
//! `AgentResponseUpdated` / `AgentResponseFinished` events.

mod openai;

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_config::settings::{self, ModelRegistry, ProviderConfig};
use tau_proto::{
    AgentPromptSubmitted, AgentResponseFinished, AgentResponseUpdated, ClientKind, Event,
    EventName, EventReader, EventSelector, EventWriter, LifecycleHello, LifecycleReady,
    LifecycleSubscribe, PROTOCOL_VERSION,
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

    let model_registry = settings::load_models().unwrap_or_default();
    let auth_store = tau_provider::storage::load().unwrap_or_default();

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
        message: Some("agent ready".to_owned()),
    }))?;
    writer.flush()?;

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

                // Resolve config from the model specified in the prompt.
                let config = prompt
                    .model
                    .as_deref()
                    .and_then(|m| resolve_model_config(m, &model_registry, &auth_store));

                match config {
                    Some(cfg) => {
                        handle_llm(&session_prompt_id, &cfg, &prompt, &mut writer)?;
                    }
                    None => {
                        let msg = match &prompt.model {
                            Some(m) => format!("cannot resolve model config for: {m}"),
                            None => "no model specified".to_owned(),
                        };
                        writer.write_event(&Event::AgentResponseFinished(
                            AgentResponseFinished {
                                session_prompt_id,
                                text: Some(msg),
                                tool_calls: Vec::new(),
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
// Model config resolution
// ---------------------------------------------------------------------------

/// Resolve a `"provider/model_id"` string into an `OpenAiConfig`.
///
/// Checks models.json5 for base_url/api_key first, then falls back to
/// auth.json for OAuth credentials with default base URLs per auth type.
fn resolve_model_config(
    model: &str,
    models: &ModelRegistry,
    auth_store: &tau_provider::storage::AuthStore,
) -> Option<OpenAiConfig> {
    let (provider_name, model_id) = model.split_once('/')?;
    let provider = models.providers.get(provider_name)?;

    // Try direct config (base_url + api_key in models.json5).
    if let Some(config) = resolve_from_provider_config(provider, model_id) {
        return Some(config);
    }

    // Try OAuth credentials from auth.json.
    resolve_from_auth(provider_name, provider, model_id, auth_store)
}

/// Resolve from models.json5 provider config (has base_url + api_key).
fn resolve_from_provider_config(provider: &ProviderConfig, model_id: &str) -> Option<OpenAiConfig> {
    let base_url = provider.base_url.as_ref()?;
    Some(OpenAiConfig {
        base_url: base_url.clone(),
        api_key: provider.api_key.clone().unwrap_or_default(),
        model_id: model_id.to_owned(),
    })
}

/// Resolve from auth.json OAuth credentials.
fn resolve_from_auth(
    provider_name: &str,
    provider: &ProviderConfig,
    model_id: &str,
    auth_store: &tau_provider::storage::AuthStore,
) -> Option<OpenAiConfig> {
    use tau_provider::storage::Credentials;

    let creds = auth_store.providers.get(provider_name)?;
    match creds {
        Credentials::Oauth { access_token, .. } => {
            let base_url = match provider.auth.as_deref() {
                Some("openai-codex") => "https://api.openai.com/v1".to_owned(),
                Some("github-copilot") => {
                    // Extract proxy-ep from token if possible.
                    extract_copilot_base_url(access_token)
                        .unwrap_or_else(|| {
                            "https://api.individual.githubcopilot.com".to_owned()
                        })
                }
                _ => return None,
            };
            Some(OpenAiConfig {
                base_url,
                api_key: access_token.clone(),
                model_id: model_id.to_owned(),
            })
        }
        Credentials::ApiKey { api_key, .. } => {
            // api_key in auth.json but no base_url in models.json5 — use OpenAI default.
            Some(OpenAiConfig {
                base_url: "https://api.openai.com/v1".to_owned(),
                api_key: api_key.clone(),
                model_id: model_id.to_owned(),
            })
        }
        Credentials::None { .. } => None,
    }
}

/// Parse `proxy-ep` from a Copilot token string.
fn extract_copilot_base_url(token: &str) -> Option<String> {
    for part in token.split(';') {
        if let Some(ep) = part.strip_prefix("proxy-ep=") {
            return Some(format!("https://{ep}"));
        }
    }
    None
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
// Echo agent (for tests)
// ---------------------------------------------------------------------------

/// A simple echo agent for integration tests. Echoes user text back
/// as a `demo.echo` tool call, or returns tool results as text.
pub fn run_echo<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    use tau_proto::{
        AgentToolCall, CborValue, ContentBlock, ConversationRole,
    };

    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent-echo".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SessionPromptCreated),
            EventSelector::Exact(EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("echo agent ready".to_owned()),
    }))?;
    writer.flush()?;

    let mut next_call = 1_u64;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::SessionPromptCreated(prompt) => {
                let spid = prompt.session_prompt_id.clone();
                writer.write_event(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
                    session_prompt_id: spid.clone(),
                }))?;

                // If last message is a tool result, return it as text.
                let is_tool_result = prompt.messages.last().is_some_and(|m| {
                    m.role == ConversationRole::User
                        && m.content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                });
                if is_tool_result {
                    let text = prompt
                        .messages
                        .last()
                        .and_then(|m| {
                            m.content.iter().find_map(|b| match b {
                                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                                _ => None,
                            })
                        })
                        .unwrap_or_default();
                    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                        session_prompt_id: spid,
                        text: Some(text),
                        tool_calls: Vec::new(),
                    }))?;
                } else {
                    // Find user text and make a tool call.
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

                    let call_id = format!("call-{next_call}");
                    next_call += 1;

                    let tool_call = if let Some(path) = user_text.strip_prefix("read ") {
                        AgentToolCall {
                            id: call_id,
                            name: "fs.read".to_owned(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("path".to_owned()),
                                CborValue::Text(path.trim().to_owned()),
                            )]),
                        }
                    } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                        AgentToolCall {
                            id: call_id,
                            name: "shell.exec".to_owned(),
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("command".to_owned()),
                                CborValue::Text(cmd.trim().to_owned()),
                            )]),
                        }
                    } else {
                        AgentToolCall {
                            id: call_id,
                            name: "demo.echo".to_owned(),
                            arguments: CborValue::Text(user_text),
                        }
                    };

                    writer.write_event(&Event::AgentResponseFinished(AgentResponseFinished {
                        session_prompt_id: spid,
                        text: None,
                        tool_calls: vec![tool_call],
                    }))?;
                }
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_resolves_none() {
        let models = ModelRegistry::default();
        let auth = tau_provider::storage::AuthStore::default();
        assert!(resolve_model_config("fake/model", &models, &auth).is_none());
    }
}
