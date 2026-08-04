//! Version-zero API-key credential record.

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Version-zero API-key credential record stored in Secret scope.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiKeyCredential {
    /// Persisted record schema version.
    version: u8,
    /// Exact credential record discriminator.
    kind: ApiKeyKind,
    /// Provider API key.
    value: String,
}

/// Wire-only API-key fields validated before constructing a domain record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiKeyCredentialWire {
    /// Persisted record schema version.
    version: u8,
    /// Exact credential record discriminator.
    kind: ApiKeyKind,
    /// Provider API key.
    value: String,
}

impl<'de> Deserialize<'de> for ApiKeyCredential {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = ApiKeyCredentialWire::deserialize(deserializer)?;
        if wire.version != 0 {
            return Err(Deserializer::Error::custom(
                "unsupported API-key credential record version",
            ));
        }
        Ok(Self {
            version: wire.version,
            kind: wire.kind,
            value: wire.value,
        })
    }
}

/// Exact kind marker for version-zero API-key records.
#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum ApiKeyKind {
    /// API-key credentials.
    #[serde(rename = "api_key")]
    ApiKey,
}

impl ApiKeyCredential {
    /// Creates one version-zero API-key record.
    pub(crate) fn new(value: String) -> Self {
        Self {
            version: 0,
            kind: ApiKeyKind::ApiKey,
            value,
        }
    }

    /// Returns whether setup status should report a configured API key.
    pub(crate) fn has_value(&self) -> bool {
        !self.value.is_empty()
    }

    /// Consumes the already validated record and returns its key.
    pub(crate) fn into_value(self) -> String {
        self.value
    }
}
