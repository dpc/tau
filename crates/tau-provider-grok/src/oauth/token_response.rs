//! Token exchange values without persistence or unverified identity claims.

use serde::Deserialize;

use super::Error;

/// A validated OAuth exchange; intentionally has no credential-bearing Debug.
///
/// A caller must serialize refreshes and durably save rotation before
/// publishing the new generation. No method here reads credentials or retries
/// an exchange.
pub struct TokenResponse {
    /// New bearer token accepted from the token endpoint.
    pub access_token: String,
    /// Optional rotated refresh token; omission preserves the previous value.
    pub refresh_token: Option<String>,
    /// Advertised lifetime in seconds; omission is unknown, not a guessed TTL.
    pub expires_in: Option<u64>,
}

/// Wire fields shared by the device and refresh grants.
#[derive(Deserialize)]
struct WireTokens {
    /// Required bearer credential.
    access_token: String,
    /// Replacement credential, absent when the server keeps the old one.
    refresh_token: Option<String>,
    /// Optional server-advertised access lifetime.
    expires_in: Option<u64>,
    /// OAuth token type, when supplied by the server.
    token_type: Option<String>,
}

impl TokenResponse {
    /// Parse successful exchange fields without deriving identity from a JWT.
    pub(super) fn parse(body: &[u8]) -> Result<Self, Error> {
        let wire: WireTokens = serde_json::from_slice(body).map_err(|_| Error::InvalidResponse)?;
        if wire.access_token.trim().is_empty()
            || wire.access_token.chars().any(char::is_control)
            || wire
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || wire
                .token_type
                .as_ref()
                .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
            || wire.expires_in == Some(0)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(Self {
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_in: wire.expires_in,
        })
    }
}
