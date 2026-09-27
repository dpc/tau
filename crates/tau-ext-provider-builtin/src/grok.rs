//! Native Grok profile ownership; credentials never serialize with settings.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tau_proto::{
    EstimatedUsdPerMillion, InputModality, ModelName, NativeReasoningEffort, ProviderModelInfo,
    ProviderName, TokenCount,
};
use tau_provider_grok::catalog::{Catalog, LanguageModel};
use tau_provider_grok::credential::Credential;
use tau_provider_grok::oauth::Client;
use tokio::runtime::Builder;

use crate::ReasoningEffortMapping;
use crate::responses::{ResponsesModel, ResponsesProvider};
use crate::setup_store::SetupStore;

/// Immutable account/registration pin, deliberately independent of bearer
/// rotation and excluded from diagnostics.
pub(crate) struct RetryIdentity {
    /// Validated login subject, not an access-token claim.
    subject: String,
    /// Frozen public OAuth registration.
    client_id: String,
}

impl RetryIdentity {
    /// Capture the selected native account when the logical request begins.
    pub(crate) fn from_backend(backend: &crate::PromptBackend) -> Option<Self> {
        let crate::PromptBackend::Grok { profile, .. } = backend else {
            return None;
        };
        Some(Self {
            subject: profile.credential.as_ref()?.subject().to_owned(),
            client_id: profile.client_id.clone(),
        })
    }

    /// Missing credentials may report remediation; another account must never
    /// inherit an automatic retry. Explicit manual readmission remains
    /// separate.
    pub(crate) fn matches(&self, backend: &crate::PromptBackend) -> bool {
        match backend {
            crate::PromptBackend::Grok { profile, .. } => {
                self.client_id == profile.client_id
                    && profile
                        .credential
                        .as_ref()
                        .is_some_and(|credential| credential.subject() == self.subject)
            }
            crate::PromptBackend::Unavailable { .. } => true,
            _ => false,
        }
    }
}

/// Shared public CLI registration selected by the approved native defaults.
pub const DEFAULT_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

/// Local Grok logout never revokes remotely or modifies profile settings.
pub(crate) fn logout(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn std::error::Error>> {
    let [name] = args else {
        return Err("tau provider logout requires exactly one NAME".into());
    };
    let name = ProviderName::try_new(name.clone())?;
    let store = SetupStore::open_default()?;
    let matching = store
        .snapshot(extension_instance)?
        .profiles
        .into_iter()
        .filter(|profile| profile.provider == name)
        .collect::<Vec<_>>();
    let [profile] = matching.as_slice() else {
        return Err("provider profile is missing or duplicated".into());
    };
    let (parsed, credential) = crate::parse_settings_profile(&name, &profile.contents)
        .map_err(|reason| format!("invalid provider profile: {reason}"))?;
    if !matches!(parsed, crate::BuiltinProviderProfile::Grok(_)) {
        return Err("local logout currently supports native Grok profiles only".into());
    }
    let crate::ProviderCredential::Stored(reference) = credential else {
        return Err("Grok profile requires its native OAuth credential".into());
    };
    store.logout_profile(
        extension_instance,
        &name,
        profile.source,
        &profile.contents,
        &reference,
    )?;
    eprintln!(
        "Removed local Grok credentials for '{name}'; settings are unchanged. No remote token revocation was attempted."
    );
    Ok(())
}

/// Authenticate interactively with the fixed issuer; callers publish only after
/// validating the existing profile source and bytes.
pub(crate) fn login(
    client_id: &str,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<Credential, Box<dyn std::error::Error>> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let client = Client::new(client_id, network)?;
        let device = client.authorize_device().await?;
        eprintln!(
            "Open {} and enter code {}.",
            device.verification_uri, device.user_code
        );
        let tokens = client.finish_device(device, std::future::pending()).await?;
        let subject = client.subject(&tokens.access_token).await?;
        Ok(Credential::from_login(tokens, subject, crate::now_ms())?)
    })
}

/// Setup-only discovery persists an explicit snapshot, never a runtime service.
pub(crate) fn add(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    target: crate::setup_store::ProfileTarget,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = crate::prompt_provider_name("grok")?;
    let client_id: String = dialoguer::Input::new()
        .with_prompt("Public OAuth client ID")
        .default(default_client_id())
        .interact_text()?;
    let credential = login(&client_id, network)?;
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let routes = runtime
        .block_on(Catalog::new(network)?.fetch_language_models(credential.access_token()))?;
    let mut models = Vec::new();
    for route in routes {
        let aliases = route.metadata.aliases.clone();
        let Some(model) = GrokModel::discovered(route) else {
            eprintln!("Skipped a language model without a known positive context limit.");
            continue;
        };
        models.push(model.clone());
        for alias in aliases {
            if alias == model.id.as_str() {
                continue;
            }
            let mut alias_model = model.clone();
            alias_model.id = ModelName::new(alias);
            models.push(alias_model);
        }
    }
    if models.is_empty() {
        return Err("Grok discovery returned no language models with known contexts".into());
    }
    let profile = GrokProfile {
        client_id,
        models,
        max_output_tokens: 0,
        cache_diagnostics: Default::default(),
        credential: Some(credential),
    };
    profile.validate()?;
    crate::save_profile(
        extension_instance,
        &name,
        &crate::BuiltinProviderProfile::Grok(profile),
        crate::ProviderSetupInput::ProfileOAuth,
        target,
    )?;
    eprintln!(
        "Saved a Grok model snapshot. Catalog visibility and API prices do not establish subscription entitlement or billing. Model changes require updating settings and restarting Tau."
    );
    Ok(())
}

/// Credential-free startup profile for the fixed public xAI Responses route.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrokProfile {
    /// Public OAuth registration; never a client secret.
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Setup-time discovered or explicitly configured language routes.
    pub models: Vec<GrokModel>,
    /// Requested output-token limit, zero to omit.
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Startup-frozen scalar diagnostics independent of exact captures.
    #[serde(default)]
    pub cache_diagnostics: tau_provider::cache_diagnostic::CacheDiagnostics,
    /// Secret-only runtime generation; omitted in both serde directions.
    #[serde(skip)]
    pub(crate) credential: Option<Credential>,
}

impl fmt::Debug for GrokProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrokProfile")
            .field("client_id", &self.client_id)
            .field("models", &self.models)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("cache_diagnostics", &self.cache_diagnostics)
            .finish_non_exhaustive()
    }
}

/// Explicit language-route metadata; missing context is never defaulted.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrokModel {
    /// Exact request identifier, including an alias when explicitly selected.
    pub id: ModelName,
    /// Known context limit required for safe model publication.
    pub context_window: TokenCount,
    /// Exact advertised selectors; unknown future values remain in settings.
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    /// Whether this route accepts ordinary input images.
    #[serde(default)]
    pub image_input: bool,
    /// Explicit route support for native images inside function output.
    #[serde(default)]
    pub native_tool_images: bool,
    /// Explicit route support for function tools.
    #[serde(default)]
    pub function_tools: bool,
    /// Catalog API input price, not evidence of subscription billing.
    #[serde(default)]
    pub est_uncached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    /// Catalog API cached-input price, not evidence of subscription billing.
    #[serde(default)]
    pub est_cached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    /// Catalog API output price, not evidence of subscription billing.
    #[serde(default)]
    pub est_output_cost_1m_usd: Option<EstimatedUsdPerMillion>,
}

/// Default public registration shared with the endorsed CLI integration.
fn default_client_id() -> String {
    DEFAULT_CLIENT_ID.to_owned()
}

/// Resolve only a hydrated unexpired generation; refresh belongs to admission.
pub(crate) fn resolve_backend(
    model: &tau_proto::ModelId,
    profile: &GrokProfile,
) -> Option<crate::PromptBackend> {
    let credential = profile.credential.as_ref()?;
    if credential.is_expired(crate::now_ms()) {
        return None;
    }
    let model_index = profile
        .models
        .iter()
        .position(|configured| configured.id == model.model)?;
    Some(crate::PromptBackend::Grok {
        profile: Arc::new(profile.clone()),
        model_index,
    })
}

impl GrokProfile {
    /// Reject incomplete or ambiguous route snapshots before model publication.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.client_id.trim().is_empty()
            || self.client_id.len() > 512
            || self.client_id.chars().any(char::is_control)
        {
            return Err("invalid Grok public client id");
        }
        let mut ids = BTreeSet::new();
        for model in &self.models {
            if model.context_window == TokenCount::new(0)
                || !ids.insert(&model.id)
                || model.id.as_str().is_empty()
                || model.id.as_str().len() > 512
                || model
                    .id
                    .as_str()
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '/')
                || model.native_tool_images && (!model.image_input || !model.function_tools)
                || model.reasoning_efforts.iter().any(|effort| {
                    effort.is_empty() || effort.len() > 128 || effort.chars().any(char::is_control)
                })
            {
                return Err("invalid Grok model metadata");
            }
        }
        Ok(())
    }

    /// Reuse extension-owned Responses sampling and local summary policy, not
    /// generic wire lowering or OpenAI cache controls.
    pub(crate) fn responses(&self) -> ResponsesProvider {
        ResponsesProvider {
            base_url: "https://api.x.ai/v1".into(),
            api_key: self
                .credential
                .as_ref()
                .map_or_else(String::new, |value| value.access_token().to_owned()),
            models: self.models.iter().map(GrokModel::responses).collect(),
            max_output_tokens: self.max_output_tokens,
            cache_diagnostics: self.cache_diagnostics,
            transport: tau_provider_responses::Transport::Sse,
            tags: Vec::new(),
            compat: Default::default(),
        }
    }

    /// Pure declaration projection; no discovery, refresh or credential access.
    pub(crate) fn models_for_provider(&self, name: &ProviderName) -> Vec<ProviderModelInfo> {
        let mut models = crate::responses::models_for_provider(name, &self.responses());
        for (published, configured) in models.iter_mut().zip(&self.models) {
            published.supports_standalone_compaction = true;
            published.standalone_compaction_prefix_budget = None;
            if configured.image_input {
                published.input_modalities = vec![InputModality::Text, InputModality::Image];
            }
            if configured.native_tool_images {
                published.tool_result_modalities = vec![InputModality::Text, InputModality::Image];
            }
            if !configured.function_tools {
                published.supported_tool_types.clear();
            }
        }
        models
    }
}

impl GrokModel {
    /// Project only exact recognized effort values into Tau's portable
    /// selector.
    fn responses(&self) -> ResponsesModel {
        ResponsesModel {
            id: self.id.clone(),
            reasoning_effort: Some(ReasoningEffortMapping::standard(
                NativeReasoningEffort::ALL.into_iter().filter(|effort| {
                    self.reasoning_efforts
                        .iter()
                        .any(|value| value == effort.as_str())
                }),
            )),
            compat: None,
            display_name: None,
            context_window: self.context_window,
            max_input_tokens: None,
            max_output_tokens: None,
            tags: Vec::new(),
            supports_parallel_tool_calls: self.function_tools,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: self.est_uncached_input_cost_1m_usd,
            est_cached_input_cost_1m_usd: self.est_cached_input_cost_1m_usd,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: self.est_output_cost_1m_usd,
            est_cache_storage_cost_1m_token_hour_usd: None,
        }
    }

    /// Convert one positively identified language route without inventing a
    /// context, price, alias default, or tool-image capability.
    pub(crate) fn discovered(route: LanguageModel) -> Option<Self> {
        let metadata = route.metadata;
        // Exact audited public model capability, not a slug-family heuristic.
        let function_tools = metadata.id == "grok-4.7";
        let context = metadata.context_length.filter(|value| *value != 0)?;
        let tiered = metadata
            .long_context_threshold
            .is_some_and(|value| value != 0);
        let price = |value: Option<u64>| {
            (!tiered)
                .then_some(value)
                .flatten()
                .and_then(|value| value.checked_mul(100))
                .map(EstimatedUsdPerMillion::from_micro_usd)
        };
        Some(Self {
            id: ModelName::new(metadata.id),
            context_window: TokenCount::new(context),
            reasoning_efforts: metadata
                .capabilities
                .map_or_else(Vec::new, |value| value.reasoning_effort),
            image_input: route.image_input,
            native_tool_images: false,
            function_tools,
            est_uncached_input_cost_1m_usd: price(metadata.prompt_text_token_price),
            est_cached_input_cost_1m_usd: price(metadata.cached_prompt_text_token_price),
            est_output_cost_1m_usd: price(metadata.completion_text_token_price),
        })
    }
}
