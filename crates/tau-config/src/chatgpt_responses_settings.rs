//! Shared private ChatGPT wire selection, independent of credentials.

use serde::{Deserialize, Serialize};

/// Startup-only wire controls shared by profile loading and replay admission.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct ChatgptResponsesSettings {
    /// Explicit opt-in to the Lite surface on models implementing that surface.
    #[serde(default, skip_serializing_if = "is_false")]
    pub responses_lite_compatibility: bool,
}

impl ChatgptResponsesSettings {
    /// Read only the wire controls of an already validated ChatGPT profile.
    ///
    /// This projection is not profile validation or component authentication.
    /// Callers must establish the configured built-in publisher and its
    /// accepted model declaration before using it for compatibility.
    pub fn from_private_profile(contents: &[u8]) -> Option<Self> {
        let profile: PrivateProfile = serde_json::from_slice(contents).ok()?;
        let PrivateProfile::Chatgpt(settings) = profile;
        Some(settings)
    }

    /// Whether the exact upstream model uses the explicitly selected Lite mode.
    pub fn uses_lite(self, model: &str) -> bool {
        self.responses_lite_compatibility
            && matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna")
    }
}

/// Closed private profile discriminator; other adapters cannot opt in by tags.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PrivateProfile {
    /// Wire controls of the private ChatGPT adapter.
    Chatgpt(ChatgptResponsesSettings),
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests;
