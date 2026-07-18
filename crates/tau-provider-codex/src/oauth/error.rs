//! Typed, bounded OAuth failures and provider-envelope parsing.

use std::fmt;

/// Maximum OAuth response body accepted before parsing.
pub(super) const MAX_OAUTH_RESPONSE_BODY_BYTES: u64 = 16 * 1024;
/// Maximum retained provider error-code characters, including truncation
/// marker.
const MAX_OAUTH_ERROR_CODE_CHARS: usize = 64;
/// Maximum retained provider message characters, including truncation marker.
const MAX_OAUTH_ERROR_MESSAGE_CHARS: usize = 256;

/// Stable category for a bounded OAuth operation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthErrorKind {
    /// The request could not be completed at the HTTP transport layer.
    Transport,
    /// The OAuth server returned a non-success HTTP status.
    Http,
    /// A successful HTTP response was oversized, malformed, incorrectly
    /// encoded, or missing required OAuth fields.
    InvalidResponse,
}

/// Bounded OAuth failure with credential-safe [`Display`](fmt::Display) and
/// [`Debug`](fmt::Debug) projections.
///
/// Parsed provider fields are retained only for typed handling. They remain
/// untrusted provider content and must not be logged directly through the
/// accessors. The default formatting projections expose only local categories,
/// safe HTTP status, and closed known provider codes.
#[derive(Clone, Eq, PartialEq)]
pub struct OAuthError {
    /// Stable failure category for programmatic handling.
    kind: OAuthErrorKind,
    /// Safe HTTP status code, when the server returned one.
    http_status: Option<u16>,
    /// Bounded untrusted provider error code from a recognized envelope.
    provider_code: Option<String>,
    /// Bounded single-line untrusted provider message or local diagnostic.
    message: Option<String>,
    /// Shared route/phase failure, retained only through its safe projection.
    outbound: Option<tau_provider::OutboundError>,
}

impl OAuthError {
    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> OAuthErrorKind {
        self.kind
    }

    /// Returns the HTTP status associated with the failure, when available.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        self.http_status
    }

    /// Returns the bounded provider error code, when available.
    ///
    /// This remains untrusted provider content. Prefer formatting the complete
    /// error for credential-safe logs.
    #[must_use]
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }

    /// Returns the bounded, single-line provider or local diagnostic, when
    /// available.
    ///
    /// Provider-originated values remain untrusted. All messages, including
    /// local diagnostics, are intentionally excluded from
    /// [`Display`](fmt::Display) and [`Debug`](fmt::Debug).
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Return the shared route failure when transport setup or I/O failed.
    #[must_use]
    pub fn outbound(&self) -> Option<&tau_provider::OutboundError> {
        self.outbound.as_ref()
    }

    /// Builds a typed error from an OAuth HTTP rejection envelope.
    ///
    /// The supplied body is parsed only for recognized code and message fields;
    /// the returned error never retains the raw body. Bodies over 16 KiB skip
    /// parsing and produce a status-only HTTP error.
    #[must_use]
    pub fn from_http_response(status: u16, body: &str) -> Self {
        if body.len() > MAX_OAUTH_RESPONSE_BODY_BYTES as usize {
            return Self::http(status, None);
        }
        Self::http(status, Some(body))
    }

    /// Returns whether a token refresh was rejected for a credential condition
    /// that cannot succeed again until the credential generation changes.
    #[must_use]
    pub fn is_permanent_refresh_rejection(&self) -> bool {
        matches!(self.http_status(), Some(400 | 401))
            && matches!(
                self.provider_code(),
                Some(
                    "invalid_grant"
                        | "invalid_refresh_token"
                        | "refresh_token_reused"
                        | "refresh_token_revoked"
                )
            )
    }

    pub(super) fn transport(error: impl fmt::Display) -> Self {
        Self {
            kind: OAuthErrorKind::Transport,
            http_status: None,
            provider_code: None,
            message: bounded_oauth_text(&error.to_string(), MAX_OAUTH_ERROR_MESSAGE_CHARS),
            outbound: None,
        }
    }

    pub(super) fn from_outbound(error: tau_provider::OutboundError) -> Self {
        Self {
            kind: OAuthErrorKind::Transport,
            http_status: None,
            provider_code: None,
            message: None,
            outbound: Some(error),
        }
    }

    pub(super) fn invalid_response(message: impl fmt::Display) -> Self {
        Self {
            kind: OAuthErrorKind::InvalidResponse,
            http_status: None,
            provider_code: None,
            message: bounded_oauth_text(&message.to_string(), MAX_OAUTH_ERROR_MESSAGE_CHARS),
            outbound: None,
        }
    }

    pub(super) fn http(status: u16, body: Option<&str>) -> Self {
        let fields = body.map(parse_oauth_error_fields).unwrap_or_default();
        Self {
            kind: OAuthErrorKind::Http,
            http_status: Some(status),
            provider_code: fields.provider_code,
            message: fields.message,
            outbound: None,
        }
    }

    fn safe_provider_code(&self) -> Option<&'static str> {
        match self.provider_code() {
            Some("access_denied") => Some("access_denied"),
            Some("invalid_grant") => Some("invalid_grant"),
            Some("invalid_refresh_token") => Some("invalid_refresh_token"),
            Some("refresh_token_reused") => Some("refresh_token_reused"),
            Some("refresh_token_revoked") => Some("refresh_token_revoked"),
            Some("temporarily_unavailable") => Some("temporarily_unavailable"),
            _ => None,
        }
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            OAuthErrorKind::Transport => {
                if let Some(error) = &self.outbound {
                    return error.fmt(formatter);
                }
                formatter.write_str("OAuth transport failed")?;
            }
            OAuthErrorKind::Http => formatter.write_str("OAuth request was rejected")?,
            OAuthErrorKind::InvalidResponse => {
                formatter.write_str("OAuth response was invalid")?;
            }
        }
        if let Some(status) = self.http_status {
            write!(formatter, " (HTTP {status})")?;
        }
        if let Some(code) = self.safe_provider_code() {
            write!(formatter, " [{code}]")?;
        }
        Ok(())
    }
}

impl fmt::Debug for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthError")
            .field("kind", &self.kind)
            .field("http_status", &self.http_status)
            .field("provider_code", &self.safe_provider_code())
            .field("outbound", &self.outbound)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for OAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.outbound
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

/// Bounded fields recognized in one OAuth error response envelope.
#[derive(Default)]
struct ParsedOAuthErrorFields {
    /// Provider error code or type, when present as a string.
    provider_code: Option<String>,
    /// Provider error description or message, when present as a string.
    message: Option<String>,
}

fn parse_oauth_error_fields(body: &str) -> ParsedOAuthErrorFields {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return ParsedOAuthErrorFields::default();
    };
    let nested = value.get("error").and_then(serde_json::Value::as_object);
    let code = nested
        .and_then(|error| error.get("code").and_then(serde_json::Value::as_str))
        .or_else(|| nested.and_then(|error| error.get("type").and_then(serde_json::Value::as_str)))
        .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("code").and_then(serde_json::Value::as_str));
    let message = nested
        .and_then(|error| error.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| {
            nested.and_then(|error| {
                error
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
            })
        })
        .or_else(|| {
            value
                .get("error_description")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str));
    ParsedOAuthErrorFields {
        provider_code: code.and_then(|value| bounded_oauth_text(value, MAX_OAUTH_ERROR_CODE_CHARS)),
        message: message.and_then(|value| bounded_oauth_text(value, MAX_OAUTH_ERROR_MESSAGE_CHARS)),
    }
}

fn bounded_oauth_text(value: &str, max_chars: usize) -> Option<String> {
    let mut output = String::new();
    let mut output_chars = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for character in value.trim().chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            if output_chars >= max_chars {
                truncated = true;
                break;
            }
            output.push(' ');
            output_chars += 1;
            pending_space = false;
        }
        if output_chars >= max_chars {
            truncated = true;
            break;
        }
        output.push(character);
        output_chars += 1;
    }
    if output.is_empty() {
        return None;
    }
    if truncated && max_chars > 0 {
        output.pop();
        output.push('…');
    }
    Some(output)
}

#[cfg(test)]
mod tests;
