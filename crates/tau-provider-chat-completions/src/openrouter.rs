//! OpenRouter provider backend helpers.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tau_proto::ModelName;

use crate::{ChatCompletionsCompat, ChatCompletionsModel, ChatCompletionsProvider};

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

/// Fetch available models from OpenRouter API, caching them on success and
/// reading from cache on failure/offline.
pub fn fetch_openrouter_models(
    api_key: &str,
) -> Result<Vec<ChatCompletionsModel>, Box<dyn std::error::Error>> {
    let url = "https://openrouter.ai/api/v1/models";
    let mut request = tau_provider::oauth::proxy_agent().get(url);
    if !api_key.trim().is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }

    match request.call() {
        Ok(response) if response.status() == 200 => {
            #[derive(Deserialize)]
            struct OpenRouterModelEntry {
                id: String,
                name: Option<String>,
                context_length: Option<u64>,
                supported_parameters: Option<Vec<String>>,
            }

            #[derive(Deserialize)]
            struct OpenRouterModelsResponse {
                data: Vec<OpenRouterModelEntry>,
            }

            let parsed: OpenRouterModelsResponse =
                serde_json::from_reader(response.into_body().into_reader())?;
            let mut models = Vec::new();
            for entry in parsed.data {
                if let Ok(model_name) = ModelName::try_new(entry.id.clone()) {
                    let supports_reasoning = entry
                        .supported_parameters
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|parameter| parameter == "reasoning");
                    let compat = ChatCompletionsCompat {
                        stream_options: true,
                        parallel_tool_calls: false,
                        prompt_cache_key: false,
                        reasoning_effort: supports_reasoning,
                        max_completion_tokens: true,
                    };
                    models.push(ChatCompletionsModel {
                        id: model_name,
                        display_name: entry.name,
                        context_window: entry.context_length.unwrap_or(2_000_000),
                        compat: Some(compat),
                        tags: Vec::new(),
                    });
                }
            }

            // Cache successfully fetched models to ~/.cache/tau/openrouter_models.json
            if let Some(path) = cache_file_path() {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Ok(file) = fs::File::create(path) {
                    let _ = serde_json::to_writer(file, &models);
                }
            }

            Ok(models)
        }
        err => {
            // Try to load cached models on error or offline
            if let Some(path) = cache_file_path()
                && path.exists()
                && let Ok(file) = fs::File::open(path)
                && let Ok(cached) = serde_json::from_reader::<_, Vec<ChatCompletionsModel>>(file)
                && !cached.is_empty()
            {
                eprintln!("Network offline/failed. Loaded cached OpenRouter models.");
                return Ok(cached);
            }

            match err {
                Ok(resp) => Err(format!("OpenRouter models status {}", resp.status()).into()),
                Err(e) => Err(e.into()),
            }
        }
    }
}
