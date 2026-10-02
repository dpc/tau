//! Public ChatGPT plan profile, deliberately separate from legacy Codex auth.

pub(crate) mod runtime;
mod setup;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
pub(crate) use setup::{add, login, logout};
use tau_provider_chatgpt::credential::Credential;

use crate::responses::{ResponsesModel, ResponsesProvider};

/// Account-specific public Responses profile from Sign in with ChatGPT.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptPlanProfile {
    /// Explicit account-catalog snapshot selected during provider setup.
    pub models: Vec<ResponsesModel>,
    /// Hydrated only from this profile's separate Secret credential slot.
    #[serde(skip)]
    pub(crate) credential: Option<Credential>,
    /// Worker-side access to the selected instance's credential coordinator.
    #[serde(skip)]
    pub(crate) session: Option<std::sync::Arc<runtime::Session>>,
}

impl ChatGptPlanProfile {
    /// Reject generic knobs that this restricted route cannot honor.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.models.iter().any(|model| {
            model.compat.is_some()
                || model.cache_contract.is_some()
                || model
                    .local_summary_compaction
                    .is_some_and(|config| config.max_output_tokens.is_some())
        }) {
            return Err(
                "ChatGPT plan profiles do not support generic cache or output-limit controls",
            );
        }
        self.responses().validate_reasoning_effort()
    }

    /// Reuse public Responses metadata and local-summary behavior with no API
    /// key, custom endpoint, output cap or WebSocket escape hatch.
    pub(crate) fn responses(&self) -> ResponsesProvider {
        ResponsesProvider {
            base_url: tau_provider_chatgpt::RESOURCE.into(),
            api_key: self
                .credential
                .as_ref()
                .and_then(|credential| credential.access_token().ok())
                .unwrap_or_default()
                .into(),
            models: self.models.clone(),
            max_output_tokens: 0,
            ..Default::default()
        }
    }
}

/// Resolve only a selected account model; renewal runs in its retained worker.
pub(crate) fn resolve_backend(
    model: &tau_proto::ModelId,
    profile: &ChatGptPlanProfile,
) -> Option<crate::PromptBackend> {
    profile.credential.as_ref()?;
    let model_index = profile
        .models
        .iter()
        .position(|candidate| candidate.id == model.model)?;
    Some(crate::PromptBackend::ChatGptPlan {
        profile: Arc::new(profile.clone()),
        model_index,
    })
}
