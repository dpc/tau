//! OpenAI-compatible chat completions client.
//!
//! Works with any endpoint speaking the OpenAI chat completions API:
//! llama.cpp, vLLM, Ollama, OpenAI, etc.

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentPromptRequest, AgentPromptResponse, AgentToolCall, CborValue, ContentBlock,
    ConversationMessage, ConversationRole, ToolDefinition,
};

/// Configuration for the OpenAI-compatible backend.
#[derive(Clone, Debug)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
}

/// Error from the OpenAI client.
#[derive(Debug)]
pub enum OpenAiError {
    Http(Box<ureq::Error>),
    Io(std::io::Error),
    Json(serde_json::Error),
    NoChoices,
}

impl std::fmt::Display for OpenAiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::NoChoices => f.write_str("API returned no choices"),
        }
    }
}

impl std::error::Error for OpenAiError {}

/// Calls the chat completions endpoint and returns the response.
pub fn chat_completion(
    config: &OpenAiConfig,
    request: &AgentPromptRequest,
) -> Result<AgentPromptResponse, OpenAiError> {
    let url = format!(
        "{}/chat/completions",
        config.base_url.trim_end_matches('/')
    );

    let body = build_request(config, request);
    let body_str = serde_json::to_string(&body).map_err(OpenAiError::Json)?;

    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", config.api_key))
        .send_string(&body_str)
        .map_err(|e| OpenAiError::Http(Box::new(e)))?;

    let response_str = response.into_string().map_err(OpenAiError::Io)?;
    let parsed: CompletionResponse =
        serde_json::from_str(&response_str).map_err(OpenAiError::Json)?;

    parse_response(parsed, request)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CompletionRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct ApiToolCall {
    id: String,
    r#type: String,
    function: ApiFunction,
}

#[derive(Serialize)]
struct ApiFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ApiTool {
    r#type: String,
    function: ApiToolFunction,
}

#[derive(Serialize)]
struct ApiToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

fn build_request(config: &OpenAiConfig, request: &AgentPromptRequest) -> CompletionRequest {
    let mut messages = Vec::new();

    // System prompt.
    if !request.system_prompt.is_empty() {
        messages.push(ApiMessage {
            role: "system".to_owned(),
            content: Some(request.system_prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    // Conversation messages.
    for msg in &request.messages {
        convert_message(msg, &mut messages);
    }

    // Tool definitions.
    let tools: Vec<ApiTool> = request
        .tools
        .iter()
        .map(|t| convert_tool_definition(t))
        .collect();

    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_owned())
    };

    CompletionRequest {
        model: config.model_id.clone(),
        messages,
        tools,
        tool_choice,
    }
}

fn convert_message(msg: &ConversationMessage, out: &mut Vec<ApiMessage>) {
    match msg.role {
        ConversationRole::User => {
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push(ApiMessage {
                            role: "user".to_owned(),
                            content: Some(text.clone()),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(ApiMessage {
                            role: "tool".to_owned(),
                            content: Some(content.clone()),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                            name: None,
                        });
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
        }
        ConversationRole::Assistant => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        let args_json = cbor_to_json(input);
                        tool_calls.push(ApiToolCall {
                            id: id.clone(),
                            r#type: "function".to_owned(),
                            function: ApiFunction {
                                name: name.clone(),
                                arguments: Some(
                                    serde_json::to_string(&args_json).unwrap_or_default(),
                                ),
                                description: None,
                                parameters: None,
                            },
                        });
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }

            let content = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            };

            out.push(ApiMessage {
                role: "assistant".to_owned(),
                content,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                name: None,
            });
        }
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> ApiTool {
    ApiTool {
        r#type: "function".to_owned(),
        function: ApiToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CompletionResponse {
    choices: Vec<CompletionChoice>,
}

#[derive(Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
}

#[derive(Deserialize)]
struct CompletionMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunction,
}

#[derive(Deserialize)]
struct ResponseFunction {
    name: String,
    arguments: String,
}

fn parse_response(
    response: CompletionResponse,
    request: &AgentPromptRequest,
) -> Result<AgentPromptResponse, OpenAiError> {
    let choice = response.choices.into_iter().next().ok_or(OpenAiError::NoChoices)?;

    let tool_calls: Vec<AgentToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| {
            let args: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
            AgentToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments: json_to_cbor(&args),
            }
        })
        .collect();

    Ok(AgentPromptResponse {
        turn_id: request.turn_id.clone(),
        session_id: request.session_id.clone(),
        text: choice.message.content,
        tool_calls,
    })
}

// ---------------------------------------------------------------------------
// CBOR ↔ JSON value conversion
// ---------------------------------------------------------------------------

fn cbor_to_json(v: &CborValue) -> serde_json::Value {
    match v {
        CborValue::Null => serde_json::Value::Null,
        CborValue::Bool(b) => serde_json::Value::Bool(*b),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            serde_json::json!(n)
        }
        CborValue::Float(f) => serde_json::json!(f),
        CborValue::Text(s) => serde_json::Value::String(s.clone()),
        CborValue::Bytes(_) => serde_json::Value::Null,
        CborValue::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(cbor_to_json).collect())
        }
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                map.insert(key, cbor_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => serde_json::Value::Null,
    }
}

fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => {
            CborValue::Array(arr.iter().map(json_to_cbor).collect())
        }
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}
