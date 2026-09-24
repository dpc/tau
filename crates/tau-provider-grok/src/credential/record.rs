//! Closed, version-zero Grok OAuth credential record.

use serde::{Deserialize, Serialize};

use super::Error;
use crate::oauth::TokenResponse;

/// Renewable Grok OAuth credentials, intentionally without `Debug`.
///
/// Subject comes from authenticated userinfo at login, not unsigned JWT claims.
/// Refresh preserves that account binding. Serialization belongs only in the
/// configured extension instance's Secret scope.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct Credential {
    /// Persisted schema version.
    version: u8,
    /// Exact provider-specific discriminator.
    kind: Kind,
    /// Current bearer credential.
    access_token: String,
    /// Renewable credential, potentially single use.
    refresh_token: String,
    /// Server-advertised access expiry; absence remains unknown.
    expires_at_ms: Option<u64>,
    /// Authenticated account subject established at login.
    subject: String,
}

/// Closed wire shape, validated before producing a domain record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCredential {
    /// Persisted schema version.
    version: u8,
    /// Exact provider-specific discriminator.
    kind: Kind,
    /// Current bearer credential.
    access_token: String,
    /// Renewable credential.
    refresh_token: String,
    /// Optional advertised access expiry.
    expires_at_ms: Option<u64>,
    /// Authenticated account subject.
    subject: String,
}

/// Exact record discriminator; never accepts ChatGPT or API-key records.
#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
enum Kind {
    /// Native xAI OAuth.
    #[serde(rename = "grok_oauth")]
    GrokOAuth,
}

impl Credential {
    /// Construct a renewable login record from authenticated userinfo and a
    /// successful device grant. The caller must validate the subject remotely.
    pub fn from_login(
        tokens: TokenResponse,
        validated_subject: String,
        now_ms: u64,
    ) -> Result<Self, Error> {
        if tokens.expires_in == Some(0) {
            return Err(Error::InvalidRecord);
        }
        let record = Self {
            version: 0,
            kind: Kind::GrokOAuth,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.ok_or(Error::InvalidRecord)?,
            expires_at_ms: expiry(tokens.expires_in, now_ms),
            subject: validated_subject,
        };
        record.validate()?;
        Ok(record)
    }

    /// Decode one bounded, closed Secret record without exposing parser errors.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if 384 * 1024 < bytes.len() {
            return Err(Error::InvalidRecord);
        }
        let wire: WireCredential =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidRecord)?;
        let record = Self {
            version: wire.version,
            kind: wire.kind,
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_at_ms: wire.expires_at_ms,
            subject: wire.subject,
        };
        record.validate()?;
        Ok(record)
    }

    /// Encode exclusively for Secret storage, never diagnostics or journals.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("validated Grok credentials serialize")
    }

    /// Borrow the current bearer token without copying it into diagnostics.
    #[must_use]
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Borrow the rotating credential for exactly one serialized exchange.
    #[must_use]
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// Return the known expiry, leaving omitted provider metadata unknown.
    #[must_use]
    pub fn expires_at_ms(&self) -> Option<u64> {
        self.expires_at_ms
    }

    /// Borrow the authenticated account binding.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Unknown expiry is not evidence that an access token has expired.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|expiry| expiry <= now_ms)
    }

    /// Merge a successful exchange without inventing omitted token metadata.
    pub(super) fn refreshed(&self, tokens: TokenResponse, now_ms: u64) -> Result<Self, Error> {
        if tokens.expires_in == Some(0) {
            return Err(Error::InvalidRecord);
        }
        let record = Self {
            version: self.version,
            kind: self.kind,
            access_token: tokens.access_token,
            refresh_token: tokens
                .refresh_token
                .unwrap_or_else(|| self.refresh_token.clone()),
            expires_at_ms: expiry(tokens.expires_in, now_ms).or(self.expires_at_ms),
            subject: self.subject.clone(),
        };
        record.validate()?;
        Ok(record)
    }

    /// Validate both wire records and programmatically constructed exchanges.
    fn validate(&self) -> Result<(), Error> {
        if self.version != 0
            || !valid_value(&self.access_token, 64 * 1024)
            || !valid_value(&self.refresh_token, 64 * 1024)
            || !valid_value(&self.subject, 1024)
        {
            return Err(Error::InvalidRecord);
        }
        Ok(())
    }
}

/// Reject unusable values with a closed error, never the credential text.
fn valid_value(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

/// Saturate unrepresentably distant advertised expiry instead of wrapping it.
fn expiry(seconds: Option<u64>, now_ms: u64) -> Option<u64> {
    seconds.map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1000)))
}
