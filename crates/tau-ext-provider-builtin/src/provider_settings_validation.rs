//! Closed diagnostics for credential-free provider settings validation.

use tau_proto::ProviderName;

/// Closed, redacted reason that one credential-free provider profile is
/// invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProviderSettingsValidationReason {
    /// The settings bytes are not JSON.
    InvalidJson,
    /// The JSON root is not an object.
    NotObject,
    /// The object retains a legacy or inline credential field.
    CredentialFieldsPresent,
    /// The credential reference violates the shared closed schema.
    InvalidCredentialReference,
    /// The remaining provider-specific fields do not match a built-in profile.
    InvalidProfile,
    /// A removed local-summary serialization selector remains configured.
    ObsoleteLocalSummarySerializationProfile,
    /// A removed duplicate local-summary context window remains configured.
    ObsoleteLocalSummaryContextWindow,
    /// A local-summary output token override exceeds the model context window.
    LocalSummaryOutputTokensExceedContextWindow,
    /// A local-summary output byte override exceeds Tau's fixed ceiling.
    LocalSummaryOutputBytesExceedNarrativeLimit,
    /// The credential slot does not match the provider profile kind.
    CredentialKindMismatch,
}

impl std::fmt::Display for ProviderSettingsValidationReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJson => "settings are not valid JSON",
            Self::NotObject => "settings must be an object",
            Self::CredentialFieldsPresent => "credential fields are forbidden",
            Self::InvalidCredentialReference => "credential reference is invalid",
            Self::InvalidProfile => "provider-specific settings are invalid",
            Self::ObsoleteLocalSummarySerializationProfile => {
                "remove obsolete local_summary_compaction.serialization_profile"
            }
            Self::ObsoleteLocalSummaryContextWindow => {
                "remove obsolete local_summary_compaction.context_window_tokens; model context_window is used"
            }
            Self::LocalSummaryOutputTokensExceedContextWindow => {
                "local_summary_compaction.max_output_tokens exceeds model context_window"
            }
            Self::LocalSummaryOutputBytesExceedNarrativeLimit => {
                "local_summary_compaction.max_output_bytes exceeds Tau's 256 KiB limit"
            }
            Self::CredentialKindMismatch => {
                "credential kind does not match the provider profile kind"
            }
        })
    }
}

/// Bounded startup diagnostic carrying only a validated provider name and a
/// closed validation reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProviderSettingsValidationError {
    /// Logical provider profile name from the validated settings filename.
    pub(super) provider: ProviderName,
    /// Closed reason that cannot retain raw settings, paths, or values.
    pub(super) reason: ProviderSettingsValidationReason,
}

impl std::fmt::Display for ProviderSettingsValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider profile '{}' has invalid credential-free settings: {}",
            self.provider, self.reason
        )
    }
}

impl std::error::Error for ProviderSettingsValidationError {}

/// Reject removed local-summary fields before serde collapses them into one
/// generic unknown-field diagnostic.
pub(super) fn reject_obsolete_local_summary_fields(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ProviderSettingsValidationReason> {
    if !matches!(
        object.get("kind").and_then(serde_json::Value::as_str),
        Some("chat_completions" | "openrouter" | "responses")
    ) {
        return Ok(());
    }
    let Some(models) = object.get("models").and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for model in models {
        let Some(summary) = model
            .get("local_summary_compaction")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        if summary.contains_key("serialization_profile") {
            return Err(ProviderSettingsValidationReason::ObsoleteLocalSummarySerializationProfile);
        }
        if summary.contains_key("context_window_tokens") {
            return Err(ProviderSettingsValidationReason::ObsoleteLocalSummaryContextWindow);
        }
    }
    Ok(())
}
