//! OpenRouter profile and bounded discovery/cache behavior.

use std::time as path_std_time;

use tokio::runtime as path_tokio_runtime;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tau_proto::ModelName;

use super::{ChatCompletionsCompat, ChatCompletionsModel, ChatCompletionsProvider};

const OPENROUTER_DISCOVERY_TIMEOUT: std::time::Duration = path_std_time::Duration::from_secs(30);
const MAX_OPENROUTER_MODELS_BODY_BYTES: usize = 4 * 1024 * 1024;
const OPENROUTER_MODELS_CACHE_VERSION: u8 = 1;

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

/// Versioned local cache containing capability-complete discovered models.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedOpenRouterModels {
    /// Exact cache schema version.
    version: u8,
    /// Models normalized from one successful discovery response.
    models: Vec<ChatCompletionsModel>,
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
        self.clone().into_chat_completions()
    }

    /// Move an owned OpenRouter profile into a standard Chat Completions route.
    pub fn into_chat_completions(mut self) -> ChatCompletionsProvider {
        for model in &mut self.models {
            // OpenRouter is a known remote route; the explicit cache contract
            // has no authority on this profile because the selected upstream
            // may vary. A model-level compatibility override retains unrelated
            // controls but cannot alter the telemetry-only cache route.
            model.cache_contract = None;
            model.input_modalities.clear();
            model.tool_result_modalities.clear();
            if let Some(compat) = model.compat.as_mut() {
                compat.stream_options = true;
                compat.openai_prompt_cache = None;
                compat.cache_usage = super::CacheUsageCompat::OpenAi;
            }
        }
        ChatCompletionsProvider {
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            api_key: self.api_key,
            models: self.models,
            tags: Vec::new(),
            max_output_tokens: tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS,
            extra_body: BTreeMap::new(),
            compat: ChatCompletionsCompat {
                stream_options: true,
                parallel_tool_calls: false,
                tool_choice: true,
                openai_prompt_cache: None,
                reasoning_effort: ChatCompletionsCompat::openai_defaults().reasoning_effort,
                reasoning_replay: super::ChatCompletionsReasoningReplay::ReasoningContent,
                single_initial_system_message: false,
                max_completion_tokens: true,
                // OpenRouter supplies this documented OpenAI-compatible usage
                // shape. It remains response-local telemetry: selected upstream
                // routing does not establish cache mechanism or residency.
                cache_usage: super::CacheUsageCompat::OpenAi,
            },
        }
    }

    /// Reject models that violate shared Chat Completions identity, modality,
    /// Function-only, parallel-call, or compatibility invariants.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        for model in &self.models {
            if model
                .input_modalities
                .contains(&tau_proto::InputModality::Image)
                || model
                    .tool_result_modalities
                    .contains(&tau_proto::InputModality::Image)
            {
                return Err("OpenRouter does not support image modality declarations");
            }
            if let Some(compat) = model.compat {
                compat.validate()?;
            }
        }
        self.to_chat_completions().validate()
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
    let cache_path = cache_file_path();
    fetch_openrouter_models_from(api_key, network, url, cache_path.as_deref())
}

/// Runs discovery against one explicit endpoint and cache path.
///
/// The production wrapper supplies OpenRouter's fixed endpoint and standard
/// cache path; the seam keeps deterministic acceptance off the public network.
fn fetch_openrouter_models_from(
    api_key: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    url: &str,
    cache_path: Option<&Path>,
) -> Result<Vec<ChatCompletionsModel>, Box<dyn std::error::Error>> {
    let runtime = path_tokio_runtime::Builder::new_current_thread()
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
            match runtime.block_on(read_openrouter_models(response, network, url)) {
                Ok(models) if !models.is_empty() => {
                    cache_openrouter_models(&models, cache_path);
                    Ok(models)
                }
                Ok(_) => Err(OpenRouterDiscoveryError::InvalidResponse.into()),
                Err(error @ OpenRouterDiscoveryError::Outbound(_)) => {
                    cached_or_error(cache_path, error)
                }
                Err(error) => Err(error.into()),
            }
        }
        err => {
            let error = match err {
                Ok(resp) => {
                    if let Some(error) = network.proxy_response_error(url, resp.status().as_u16()) {
                        OpenRouterDiscoveryError::Outbound(error)
                    } else {
                        OpenRouterDiscoveryError::Status(resp.status().as_u16())
                    }
                }
                Err(error) => error,
            };
            cached_or_error(cache_path, error)
        }
    }
}

/// Returns a non-empty valid cache for an eligible failure, or the failure.
fn cached_or_error(
    cache_path: Option<&Path>,
    error: OpenRouterDiscoveryError,
) -> Result<Vec<ChatCompletionsModel>, Box<dyn std::error::Error>> {
    let eligible = match &error {
        OpenRouterDiscoveryError::Outbound(error) => {
            error.kind() != tau_provider::OutboundErrorKind::InvalidConfiguration
        }
        OpenRouterDiscoveryError::Status(_) => true,
        OpenRouterDiscoveryError::Runtime
        | OpenRouterDiscoveryError::BodyTooLarge
        | OpenRouterDiscoveryError::InvalidResponse => false,
    };
    if eligible && let Some(cached) = cached_openrouter_models(cache_path) {
        eprintln!("Network offline/failed. Loaded cached OpenRouter models.");
        Ok(cached)
    } else {
        Err(error.into())
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
    let models = parsed
        .data
        .into_iter()
        .filter_map(openrouter_model)
        .collect::<Vec<_>>();
    let profile = OpenRouterProfile {
        api_key: String::new(),
        models,
    };
    profile
        .validate()
        .map_err(|_| OpenRouterDiscoveryError::InvalidResponse)?;
    Ok(profile.models)
}

fn openrouter_model(entry: OpenRouterModelEntry) -> Option<ChatCompletionsModel> {
    let id = ModelName::try_new(entry.id).ok()?;
    let context_window = entry.context_length?;
    let supported_parameters = entry.supported_parameters.as_deref().unwrap_or_default();
    let supports = |expected| {
        supported_parameters
            .iter()
            .any(|parameter| parameter == expected)
    };
    let supports_tools = supports("tools");
    let supports_parallel_tool_calls = supports_tools && supports("parallel_tool_calls");
    Some(ChatCompletionsModel {
        id,
        display_name: entry.name,
        context_window,
        compat: Some(ChatCompletionsCompat {
            stream_options: true,
            parallel_tool_calls: supports_parallel_tool_calls,
            tool_choice: supports("tool_choice"),
            openai_prompt_cache: None,
            reasoning_effort: supports("reasoning").then(|| {
                ChatCompletionsCompat::openai_defaults()
                    .reasoning_effort
                    .expect("OpenAI defaults publish reasoning effort")
            }),
            reasoning_replay: super::ChatCompletionsReasoningReplay::ReasoningContent,
            single_initial_system_message: false,
            max_completion_tokens: true,
            cache_usage: super::CacheUsageCompat::OpenAi,
        }),
        tags: Vec::new(),
        hosted_tool_capabilities: Vec::new(),
        supported_tool_types: supports_tools
            .then_some(tau_proto::ToolType::Function)
            .into_iter()
            .collect(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls,
        local_summary_compaction: None,
        cache_contract: None,
        est_uncached_input_cost_1m_usd: None,
        est_cached_input_cost_1m_usd: None,
        est_cache_write_input_cost_1m_usd: None,
        est_output_cost_1m_usd: None,
        est_cache_storage_cost_1m_token_hour_usd: None,
    })
}

fn cache_openrouter_models(models: &[ChatCompletionsModel], path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cached = CachedOpenRouterModels {
        version: OPENROUTER_MODELS_CACHE_VERSION,
        models: models.to_vec(),
    };
    if let Ok(bytes) = serde_json::to_vec(&cached) {
        let _ = tau_config::atomic::atomic_write_following_symlink(path, &bytes, None);
    }
}

fn cached_openrouter_models(path: Option<&Path>) -> Option<Vec<ChatCompletionsModel>> {
    let file = fs::File::open(path?).ok()?;
    let cached: CachedOpenRouterModels = serde_json::from_reader(file).ok()?;
    if cached.version != OPENROUTER_MODELS_CACHE_VERSION || cached.models.is_empty() {
        return None;
    }
    let profile = OpenRouterProfile {
        api_key: String::new(),
        models: cached.models,
    };
    profile.validate().ok()?;
    Some(profile.models)
}
