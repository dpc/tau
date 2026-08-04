//! Closed credential-reference schema shared by provider setup and startup.

use std::fmt;

use serde::Serialize;
use tau_proto::{ExtensionDataPath, ProviderName};

mod instance_lock;

pub use instance_lock::{ProviderSettingsInstanceLock, ProviderSettingsLockAttempt};

/// The only credential slots owned by a built-in provider profile.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProviderCredentialSlot {
    /// ChatGPT OAuth credential record.
    OAuth,
    /// API-key credential record.
    ApiKey,
}

impl ProviderCredentialSlot {
    /// Return every credential slot owned by the built-in provider schema.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::OAuth, Self::ApiKey]
    }

    /// Returns the canonical Secret-scope path for this provider and slot.
    #[must_use]
    pub fn path(self, provider: &ProviderName) -> ExtensionDataPath {
        let file = match self {
            Self::OAuth => "oauth.json",
            Self::ApiKey => "api-key.json",
        };
        ExtensionDataPath::new(format!("providers/{provider}/{file}"))
    }

    fn kind(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
        }
    }
}

/// A validated credential destination and optional named source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialReference {
    /// Closed credential slot selected by the profile kind.
    slot: ProviderCredentialSlot,
    /// Exact canonical Secret-scope record path.
    path: ExtensionDataPath,
    /// Declared named secret that setup/startup materializes, if any.
    named_source: Option<String>,
}

/// Error returned for an invalid credential-free provider settings binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialReferenceError {
    /// Redacted explanation of the schema violation.
    message: String,
}

impl fmt::Display for ProviderCredentialReferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ProviderCredentialReferenceError {}

fn invalid(message: impl Into<String>) -> ProviderCredentialReferenceError {
    ProviderCredentialReferenceError {
        message: message.into(),
    }
}

impl ProviderCredentialReference {
    /// Construct a reference whose destination and source combination satisfy
    /// the closed provider credential schema.
    pub fn new(
        provider: &ProviderName,
        slot: ProviderCredentialSlot,
        named_source: Option<&str>,
    ) -> Result<Self, ProviderCredentialReferenceError> {
        if slot == ProviderCredentialSlot::OAuth && named_source.is_some() {
            return Err(invalid("OAuth credentials cannot bind a named source"));
        }
        if let Some(name) = named_source {
            crate::secret_sources::validate_secret_name(name)
                .map_err(|_| invalid("provider credential source is invalid"))?;
        }
        Ok(Self {
            slot,
            path: slot.path(provider),
            named_source: named_source.map(str::to_owned),
        })
    }

    /// Return the closed credential slot.
    #[must_use]
    pub fn slot(&self) -> ProviderCredentialSlot {
        self.slot
    }

    /// Return the canonical Secret-scope destination.
    #[must_use]
    pub fn path(&self) -> &ExtensionDataPath {
        &self.path
    }

    /// Return the validated named source, when configured.
    #[must_use]
    pub fn named_source(&self) -> Option<&str> {
        self.named_source.as_deref()
    }

    /// Serialize this validated reference into provider settings JSON.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(SerializedReference {
            kind: self.slot.kind(),
            secret_path: self.path.as_str(),
            source: self.named_source().map(|name| SerializedSource {
                kind: "named_secret",
                name,
            }),
        })
        .expect("validated credential reference must serialize")
    }
}

/// Borrowed named-source representation used only after validation.
#[derive(Serialize)]
struct SerializedSource<'a> {
    /// Closed source discriminator.
    kind: &'static str,
    /// Validated configured declaration name.
    name: &'a str,
}

/// Borrowed credential-reference representation used only after validation.
#[derive(Serialize)]
struct SerializedReference<'a> {
    /// Closed credential-slot discriminator.
    kind: &'static str,
    /// Canonical Secret-scope destination.
    secret_path: &'a str,
    /// Optional validated named source.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<SerializedSource<'a>>,
}

/// Parse the only credential reference form accepted in one provider settings
/// object. The caller retains ownership of all non-credential settings fields.
pub fn parse_provider_credential_reference(
    provider: &ProviderName,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<ProviderCredentialReference, ProviderCredentialReferenceError> {
    if settings.contains_key("auth")
        || settings.contains_key("api_key")
        || settings.contains_key("api_key_secret")
    {
        return Err(invalid(
            "provider settings must not contain credential fields".to_owned(),
        ));
    }
    let credential = settings.get("credential").ok_or_else(|| {
        invalid("provider settings are missing a credential reference".to_owned())
    })?;
    let credential = credential
        .as_object()
        .ok_or_else(|| invalid("provider credential reference must be an object".to_owned()))?;
    let kind = credential
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("provider credential reference is missing kind".to_owned()))?;
    let slot = match kind {
        "oauth" => ProviderCredentialSlot::OAuth,
        "api_key" => ProviderCredentialSlot::ApiKey,
        _ => {
            return Err(invalid(
                "unknown provider credential reference kind".to_owned(),
            ));
        }
    };
    let path = credential
        .get("secret_path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            invalid("provider credential reference is missing secret_path".to_owned())
        })?;
    let named_source = match credential.get("source") {
        None => None,
        Some(value) if slot == ProviderCredentialSlot::ApiKey => {
            let source = value.as_object().ok_or_else(|| {
                invalid("provider credential source must be an object".to_owned())
            })?;
            if source.len() != 2
                || source.get("kind").and_then(serde_json::Value::as_str) != Some("named_secret")
            {
                return Err(invalid("provider credential source is invalid".to_owned()));
            }
            let name = source
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| crate::secret_sources::validate_secret_name(name).is_ok())
                .ok_or_else(|| invalid("provider credential source is invalid".to_owned()))?;
            Some(name.to_owned())
        }
        Some(_) => {
            return Err(invalid(
                "OAuth credentials cannot bind a named source".to_owned(),
            ));
        }
    };
    let expected = slot.path(provider);
    if path != expected.as_str() {
        return Err(invalid(
            "provider credential reference does not match its provider and kind".to_owned(),
        ));
    }
    if credential.len() != usize::from(named_source.is_some()) + 2 {
        return Err(invalid(
            "provider credential reference has unknown fields".to_owned(),
        ));
    }
    ProviderCredentialReference::new(provider, slot, named_source.as_deref())
}

#[cfg(test)]
mod tests;
