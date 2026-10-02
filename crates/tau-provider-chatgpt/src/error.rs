//! Closed OAuth errors keep tokens and provider text out of diagnostics.

/// Content-free sign-in and renewal failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Network policy, dispatch, or a finite deadline failed.
    Transport,
    /// The server returned an unsupported response shape.
    InvalidResponse,
    /// The callback did not match its one-time pending transaction.
    InvalidCallback,
    /// A signature or required identity claim failed validation.
    InvalidIdentity,
    /// The user denied consent.
    AccessDenied,
    /// The grant is unusable; retain registration and request sign-in again.
    InvalidGrant,
    /// The issued client is not accepted.
    InvalidClient,
    /// The request failed without a recognized stable recovery code.
    Rejected,
    /// The validated identity differs from the selected saved registration.
    AccountChanged,
    /// The selected account has not granted ChatGPT plan usage.
    PlanDisabled,
}

impl Error {
    /// Classify only stable machine-readable token endpoint codes.
    pub(crate) fn from_body(body: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Self::Rejected;
        };
        match value.get("error").and_then(serde_json::Value::as_str) {
            Some("access_denied") => Self::AccessDenied,
            Some(
                "invalid_grant"
                | "invalid_refresh_token"
                | "token_expired"
                | "refresh_token_expired"
                | "refresh_token_invalidated"
                | "refresh_token_reused",
            ) => Self::InvalidGrant,
            Some("invalid_client") => Self::InvalidClient,
            _ => Self::Rejected,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChatGPT sign-in {self:?}")
    }
}

impl std::error::Error for Error {}
