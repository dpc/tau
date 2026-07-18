//! OpenRouter provider backend helpers.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tau_proto::ModelName;

use crate::{ChatCompletionsCompat, ChatCompletionsModel, ChatCompletionsProvider};

const OPENROUTER_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_OPENROUTER_MODELS_BODY_BYTES: usize = 4 * 1024 * 1024;

/// OpenRouter's wire representation for one discoverable model.
#[derive(Deserialize)]
struct OpenRouterModelEntry {
    /// Provider model identifier.
    id: String,
    /// Optional user-facing model name.
    name: Option<String>,
    /// Optional advertised context-window size.
    context_length: Option<u64>,
    /// Optional OpenRouter parameter names accepted by the model.
    supported_parameters: Option<Vec<String>>,
}

/// Bounded OpenRouter model-list response envelope.
#[derive(Deserialize)]
struct OpenRouterModelsResponse {
    /// Model entries returned by the discovery endpoint.
    data: Vec<OpenRouterModelEntry>,
}

/// Bounded, credential-safe OpenRouter discovery failure.
#[derive(Debug)]
pub enum OpenRouterDiscoveryError {
    /// Shared outbound route or transport failure.
    Outbound(tau_provider::OutboundError),
    /// The local async runtime could not start.
    Runtime,
    /// The endpoint returned a non-success status.
    Status(u16),
    /// The response exceeded its explicit byte cap.
    BodyTooLarge,
    /// The response or cache was malformed or unavailable.
    InvalidResponse,
}

impl std::fmt::Display for OpenRouterDiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outbound(error) => error.fmt(formatter),
            Self::Runtime => formatter.write_str("OpenRouter discovery runtime failed"),
            Self::Status(status) => {
                write!(formatter, "OpenRouter discovery returned HTTP {status}")
            }
            Self::BodyTooLarge => {
                formatter.write_str("OpenRouter discovery response exceeded size limit")
            }
            Self::InvalidResponse => {
                formatter.write_str("OpenRouter discovery response was invalid")
            }
        }
    }
}

impl std::error::Error for OpenRouterDiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Outbound(error) => Some(error),
            _ => None,
        }
    }
}

/// OpenRouter profile stored by the built-in provider extension.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterProfile {
    /// API key for OpenRouter bearer auth.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Models configured for this profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChatCompletionsModel>,
}

impl OpenRouterProfile {
    /// Convert OpenRouterProfile to a standard ChatCompletionsProvider.
    pub fn to_chat_completions(&self) -> ChatCompletionsProvider {
        ChatCompletionsProvider {
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            api_key: self.api_key.clone(),
            models: self.models.clone(),
            tags: Vec::new(),
            max_output_tokens: crate::DEFAULT_MAX_OUTPUT_TOKENS,
            extra_body: BTreeMap::new(),
            compat: ChatCompletionsCompat {
                stream_options: true,
                parallel_tool_calls: false,
                prompt_cache_key: false,
                reasoning_effort: true,
                max_completion_tokens: true,
            },
        }
    }
}

fn cache_file_path() -> Option<PathBuf> {
    dirs::cache_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("tau").join("openrouter_models.json"))
}

/// Fetch available models from OpenRouter API and cache successful results.
///
/// A transport or non-success-status failure uses a non-empty existing cache.
/// Successful responses with oversized or invalid bodies fail closed instead of
/// silently replacing current discovery with stale data.
///
/// # Errors
///
/// Returns a redacted runtime, outbound, HTTP-status, size, or decoding error
/// when discovery fails and no eligible cache fallback exists.
pub fn fetch_openrouter_models(
    api_key: &str,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<Vec<ChatCompletionsModel>, Box<dyn std::error::Error>> {
    let url = "https://openrouter.ai/api/v1/models";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| OpenRouterDiscoveryError::Runtime)?;
    let result = runtime.block_on(async {
        let client = network
            .client_for(url)
            .map_err(OpenRouterDiscoveryError::Outbound)?;
        let mut request = client.get(url);
        if !api_key.trim().is_empty() {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }
        request
            .timeout(OPENROUTER_DISCOVERY_TIMEOUT)
            .send()
            .await
            .map_err(|error| {
                OpenRouterDiscoveryError::Outbound(network.reqwest_error(
                    url,
                    tau_provider::OutboundPhase::Request,
                    &error,
                ))
            })
    });
    match result {
        Ok(response) if response.status() == 200 => {
            let models = runtime.block_on(read_openrouter_models(response, network, url))?;
            cache_openrouter_models(&models);
            Ok(models)
        }
        err => {
            if let Some(cached) = cached_openrouter_models() {
                eprintln!("Network offline/failed. Loaded cached OpenRouter models.");
                return Ok(cached);
            }

            match err {
                Ok(resp) => {
                    if let Some(error) = network.proxy_response_error(url, resp.status().as_u16()) {
                        Err(OpenRouterDiscoveryError::Outbound(error).into())
                    } else {
                        Err(OpenRouterDiscoveryError::Status(resp.status().as_u16()).into())
                    }
                }
                Err(error) => Err(error.into()),
            }
        }
    }
}

async fn read_openrouter_models(
    mut response: reqwest::Response,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
) -> Result<Vec<ChatCompletionsModel>, OpenRouterDiscoveryError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        OpenRouterDiscoveryError::Outbound(network.reqwest_error(
            url,
            tau_provider::OutboundPhase::Body,
            &error,
        ))
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_OPENROUTER_MODELS_BODY_BYTES {
            return Err(OpenRouterDiscoveryError::BodyTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    let parsed: OpenRouterModelsResponse =
        serde_json::from_slice(&bytes).map_err(|_| OpenRouterDiscoveryError::InvalidResponse)?;
    Ok(parsed
        .data
        .into_iter()
        .filter_map(openrouter_model)
        .collect())
}

fn openrouter_model(entry: OpenRouterModelEntry) -> Option<ChatCompletionsModel> {
    let id = ModelName::try_new(entry.id).ok()?;
    let supports_reasoning = entry
        .supported_parameters
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|parameter| parameter == "reasoning");
    Some(ChatCompletionsModel {
        id,
        display_name: entry.name,
        context_window: entry.context_length.unwrap_or(2_000_000),
        compat: Some(ChatCompletionsCompat {
            stream_options: true,
            parallel_tool_calls: false,
            prompt_cache_key: false,
            reasoning_effort: supports_reasoning,
            max_completion_tokens: true,
        }),
        tags: Vec::new(),
    })
}

fn cache_openrouter_models(models: &[ChatCompletionsModel]) {
    let Some(path) = cache_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(file) = fs::File::create(path) {
        let _ = serde_json::to_writer(file, models);
    }
}

fn cached_openrouter_models() -> Option<Vec<ChatCompletionsModel>> {
    let file = fs::File::open(cache_file_path()?).ok()?;
    let cached: Vec<ChatCompletionsModel> = serde_json::from_reader(file).ok()?;
    (!cached.is_empty()).then_some(cached)
}
