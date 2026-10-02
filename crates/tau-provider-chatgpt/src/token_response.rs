//! Token endpoint decoding keeps token material out of Debug and diagnostics.

use serde::Deserialize;

use crate::Error;
use crate::authorization::valid_opaque;

/// One complete token endpoint reply, never a persisted account identity.
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    /// Bearer token for the public API resource.
    pub(crate) access_token: String,
    /// Rotating renewable-session secret, issued with offline access.
    pub(crate) refresh_token: Option<String>,
    /// Signed identity artifact, required at login and optional on refresh.
    pub(crate) id_token: Option<String>,
    /// Must select bearer authentication.
    token_type: String,
    /// Relative bearer lifetime in seconds.
    pub(crate) expires_in: u64,
    /// Granted scope list, not the originally requested permissions.
    pub(crate) scope: String,
}

impl TokenResponse {
    /// Require sane token bounds; a valid identity grant need not include
    /// offline access or a renewable session.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let tokens: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)?;
        if !tokens.token_type.eq_ignore_ascii_case("bearer")
            || !valid_opaque(&tokens.access_token)
            || tokens
                .refresh_token
                .as_deref()
                .is_some_and(|token| !valid_opaque(token))
            || (tokens
                .scope
                .split_ascii_whitespace()
                .any(|scope| scope == "offline_access")
                && tokens.refresh_token.is_none())
            || tokens.expires_in == 0
            || tokens.expires_in > 86400
            || tokens.scope.len() > 8192
        {
            return Err(Error::InvalidResponse);
        }
        Ok(tokens)
    }
}
