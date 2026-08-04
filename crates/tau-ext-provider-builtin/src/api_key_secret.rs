//! Resolution of API-key secret references in provider profiles.

use std::collections::BTreeMap;
use std::fmt;

use tau_proto::{ProviderName, SecretValue};

/// A credential-safe error while resolving one provider profile's API-key
/// source.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum ApiKeySecretError {
    /// The profile supplied both credential source forms.
    BothSources {
        /// Provider namespace owning the invalid profile.
        provider: ProviderName,
    },
    /// The profile supplied an invalid logical secret name.
    InvalidReference {
        /// Provider namespace owning the invalid profile.
        provider: ProviderName,
    },
    /// The configured extension snapshot does not contain the reference.
    UnavailableReference {
        /// Provider namespace owning the profile.
        provider: ProviderName,
        /// Logical secret name; this is configuration metadata, never its
        /// value.
        reference: String,
    },
}

impl fmt::Display for ApiKeySecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BothSources { provider } => write!(
                formatter,
                "provider profile '{provider}' must not set both api_key and api_key_secret"
            ),
            Self::InvalidReference { provider } => write!(
                formatter,
                "provider profile '{provider}' has invalid api_key_secret; use a nonempty ASCII name containing only letters, digits, '.', '_' or '-' (not '.' or '..')"
            ),
            Self::UnavailableReference {
                provider,
                reference,
            } => write!(
                formatter,
                "provider profile '{provider}' requires unavailable declared secret '{reference}'; declare it under extensions.provider-builtin.secrets and set TAU_SECRET_{}, then restart Tau",
                reference.to_ascii_uppercase()
            ),
        }
    }
}

impl std::error::Error for ApiKeySecretError {}

/// Resolves one optional API-key reference against the extension's startup
/// snapshot.
///
/// The profile reference must exactly match a configured logical secret name.
/// This keeps profile authorization scoped to the harness-provided map and
/// avoids accepting spelling variants that the extension did not receive.
pub(super) fn resolve(
    provider: &ProviderName,
    api_key: &mut String,
    api_key_secret: &Option<String>,
    secrets: &BTreeMap<String, SecretValue>,
) -> Result<(), ApiKeySecretError> {
    let Some(reference) = api_key_secret else {
        return Ok(());
    };
    if !api_key.is_empty() {
        return Err(ApiKeySecretError::BothSources {
            provider: provider.clone(),
        });
    }
    if !is_valid_secret_name(reference) {
        return Err(ApiKeySecretError::InvalidReference {
            provider: provider.clone(),
        });
    }
    let Some(secret) = secrets.get(reference) else {
        return Err(ApiKeySecretError::UnavailableReference {
            provider: provider.clone(),
            reference: reference.clone(),
        });
    };
    let value = secret.expose_secret();
    if value.is_empty() {
        return Err(ApiKeySecretError::UnavailableReference {
            provider: provider.clone(),
            reference: reference.clone(),
        });
    }
    *api_key = value.to_owned();
    Ok(())
}

/// Returns whether a string uses the existing extension-secret logical-name
/// grammar.
pub(super) fn is_valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
