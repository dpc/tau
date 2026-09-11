//! ChatGPT profile controls and their startup-only diagnostic projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tau_proto::ProviderName;
use tau_provider::cache_diagnostic::CacheDiagnostics;

use super::{BuiltinProviderProfile, BuiltinProviderProfiles, CodexMode, OpenAiAuth};

/// ChatGPT/Codex provider profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptProfile {
    /// Startup-frozen scalar cache diagnostics; exact captures are independent.
    #[serde(default, skip_serializing_if = "CacheDiagnostics::is_metadata")]
    pub cache_diagnostics: CacheDiagnostics,
    /// OAuth credentials used for ChatGPT/Codex Responses calls.
    #[serde(default)]
    pub auth: OpenAiAuth,
    /// Startup-stable wire selection, shared with harness replay admission.
    #[serde(flatten)]
    pub responses: tau_config::chatgpt_responses_settings::ChatgptResponsesSettings,
    /// Enables the provider-owned, prompt-only image tool for this account.
    /// Startup declaration alone never dispatches a generation request.
    #[serde(default = "default_image_generation", skip_serializing_if = "is_true")]
    pub image_generation: bool,
}

impl Default for ChatGptProfile {
    fn default() -> Self {
        Self {
            cache_diagnostics: CacheDiagnostics::default(),
            auth: OpenAiAuth::default(),
            responses: Default::default(),
            image_generation: default_image_generation(),
        }
    }
}

impl ChatGptProfile {
    /// Return the route selected by this profile's immutable settings.
    pub(crate) fn responses_mode(&self) -> CodexMode {
        if self.responses.responses_lite_compatibility {
            CodexMode::LiteCompatibility
        } else {
            CodexMode::Standard
        }
    }

    /// Replace test credentials without altering startup profile controls.
    #[cfg(test)]
    pub(crate) fn replace_auth(&mut self, refreshed: OpenAiAuth) {
        self.auth = refreshed;
    }
}

const fn default_image_generation() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl BuiltinProviderProfiles {
    /// Project only supported adapters' immutable startup metadata settings.
    pub(crate) fn startup_cache_diagnostics(&self) -> BTreeMap<ProviderName, CacheDiagnostics> {
        self.providers
            .iter()
            .filter_map(|(name, profile)| match profile {
                BuiltinProviderProfile::Chatgpt(profile) => {
                    Some((name.clone(), profile.cache_diagnostics))
                }
                _ => None,
            })
            .collect()
    }
}
