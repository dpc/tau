//! Version-zero ChatGPT OAuth credential record.

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::OpenAiAuth;

/// Version-zero ChatGPT OAuth credential record stored in Secret scope.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatGptOAuthCredential {
    /// Persisted record schema version.
    version: u8,
    /// Exact credential record discriminator.
    kind: ChatGptOAuthKind,
    /// ChatGPT access token.
    access_token: String,
    /// Rotating OAuth refresh token.
    refresh_token: String,
    /// Access-token expiry in Unix milliseconds.
    expires_at_ms: u64,
    /// Optional ChatGPT account identifier.
    account_id: Option<String>,
}

/// Wire-only OAuth fields validated before constructing a domain record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatGptOAuthCredentialWire {
    /// Persisted record schema version.
    version: u8,
    /// Exact credential record discriminator.
    kind: ChatGptOAuthKind,
    /// ChatGPT access token.
    access_token: String,
    /// Rotating OAuth refresh token.
    refresh_token: String,
    /// Access-token expiry in Unix milliseconds.
    expires_at_ms: u64,
    /// Optional ChatGPT account identifier.
    account_id: Option<String>,
}

impl<'de> Deserialize<'de> for ChatGptOAuthCredential {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        let wire = ChatGptOAuthCredentialWire::deserialize(deserializer)?;
        if wire.version != 0 {
            return Err(Deserializer::Error::custom(
                "unsupported ChatGPT OAuth credential record version",
            ));
        }
        Ok(Self {
            version: wire.version,
            kind: wire.kind,
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_at_ms: wire.expires_at_ms,
            account_id: wire.account_id,
        })
    }
}

/// Exact kind marker for version-zero ChatGPT OAuth records.
#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum ChatGptOAuthKind {
    /// ChatGPT OAuth credentials.
    #[serde(rename = "chatgpt_oauth")]
    ChatGptOAuth,
}

impl From<OpenAiAuth> for ChatGptOAuthCredential {
    fn from(auth: OpenAiAuth) -> Self {
        Self {
            version: 0,
            kind: ChatGptOAuthKind::ChatGptOAuth,
            access_token: auth.access_token,
            refresh_token: auth.refresh_token,
            expires_at_ms: auth.expires_at_ms,
            account_id: auth.account_id,
        }
    }
}

impl From<ChatGptOAuthCredential> for OpenAiAuth {
    fn from(record: ChatGptOAuthCredential) -> Self {
        Self {
            access_token: record.access_token,
            refresh_token: record.refresh_token,
            expires_at_ms: record.expires_at_ms,
            account_id: record.account_id,
        }
    }
}

impl ChatGptOAuthCredential {
    /// Returns whether setup status should report a live OAuth access token.
    pub(crate) fn is_unexpired(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at_ms
    }

    /// Converts the already validated record into provider authentication.
    pub(crate) fn into_auth(self) -> OpenAiAuth {
        self.into()
    }
}
