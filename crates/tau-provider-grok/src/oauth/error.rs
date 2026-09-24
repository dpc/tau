//! Closed, credential-free OAuth failure categories.

use std::fmt;

/// An OAuth failure that never retains provider bodies, tokens, or URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The configured public client identifier is empty.
    InvalidClientId,
    /// Network setup, dispatch, or response reading failed.
    Transport,
    /// A response exceeded the size bound or failed structural validation.
    InvalidResponse,
    /// The OAuth server rejected the request with no recognized error code.
    Rejected,
    /// The user has not yet approved the device grant.
    AuthorizationPending,
    /// The server requires at least five more seconds between polls.
    SlowDown,
    /// The user declined the device grant.
    AccessDenied,
    /// The device grant expired before approval.
    Expired,
    /// The refresh grant is no longer accepted; a new login is required.
    InvalidGrant,
    /// The public client registration is not accepted.
    InvalidClient,
    /// The caller canceled the operation.
    Canceled,
}

impl Error {
    /// Whether this response permanently rejects the current refresh
    /// generation.
    #[must_use]
    pub const fn rejects_refresh_generation(self) -> bool {
        matches!(self, Self::InvalidGrant | Self::InvalidClient)
    }

    /// Convert only known protocol codes; never retain untrusted descriptions.
    pub(super) fn from_body(body: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Self::Rejected;
        };
        match value.get("error").and_then(serde_json::Value::as_str) {
            Some("authorization_pending") => Self::AuthorizationPending,
            Some("slow_down") => Self::SlowDown,
            Some("access_denied" | "authorization_denied") => Self::AccessDenied,
            Some("expired_token") => Self::Expired,
            Some("invalid_grant") => Self::InvalidGrant,
            Some("invalid_client") => Self::InvalidClient,
            _ => Self::Rejected,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Grok OAuth {self:?}")
    }
}

impl std::error::Error for Error {}
