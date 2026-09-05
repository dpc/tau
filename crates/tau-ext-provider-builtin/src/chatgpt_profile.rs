//! ChatGPT profile controls and their startup-only diagnostic projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tau_proto::ProviderName;
use tau_provider::cache_diagnostic::CacheDiagnostics;

use super::{BuiltinProviderProfile, BuiltinProviderProfiles, CodexMode, OpenAiAuth, is_false};

/// ChatGPT/Codex provider profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptProfile {
    /// Startup-frozen scalar cache diagnostics; exact captures are independent.
    #[serde(default, skip_serializing_if = "CacheDiagnostics::is_metadata")]
    pub cache_diagnostics: CacheDiagnostics,
    /// OAuth credentials used for ChatGPT/Codex Responses calls.
    #[serde(default)]
    pub auth: OpenAiAuth,
    /// Select the startup-stable legacy Responses Lite route, not
    /// authentication.
    #[serde(default, skip_serializing_if = "is_false")]
    pub responses_lite_compatibility: bool,
}

impl ChatGptProfile {
    /// Return the route selected by this profile's immutable settings.
    pub(crate) fn responses_mode(&self) -> CodexMode {
        if self.responses_lite_compatibility {
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
