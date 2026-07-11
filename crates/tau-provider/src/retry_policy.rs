//! Provider-independent retry classifications and trusted timing-hint parsing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The normalized reason that a provider attempt could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryClass {
    /// A connection, I/O, or per-attempt watchdog failure.
    Transport,
    /// Temporary provider overload or server failure.
    Overload,
    /// Request-rate throttling, normally HTTP 429.
    Throttle,
    /// A provider usage window that is expected to reset.
    UsageWindow,
    /// Billing, quota, credits, plan, or entitlement state.
    Account,
    /// Authentication or mutable provider configuration.
    Auth,
    /// An unrecognized provider or protocol failure.
    Unknown,
}

impl RetryClass {
    /// Human-readable, provider-content-free explanation suitable for status.
    #[must_use]
    pub const fn public_reason(self) -> &'static str {
        match self {
            Self::Transport => "provider connection interrupted",
            Self::Overload => "provider temporarily unavailable",
            Self::Throttle => "provider request rate limited",
            Self::UsageWindow => "provider usage window reached",
            Self::Account => "provider account, quota, or billing limit reached",
            Self::Auth => "provider authentication or configuration needs attention",
            Self::Unknown => "provider returned an unrecognized failure",
        }
    }

    /// Maximum policy-generated interval for this class.
    #[must_use]
    pub const fn generated_delay_ceiling(self) -> Duration {
        match self {
            Self::Transport | Self::Overload | Self::Throttle => Duration::from_secs(60),
            Self::UsageWindow | Self::Account | Self::Auth | Self::Unknown => {
                Duration::from_secs(30 * 60)
            }
        }
    }

    /// Whether failures should impose an account/profile-scoped cooldown.
    #[must_use]
    pub const fn shares_cooldown(self) -> bool {
        matches!(
            self,
            Self::Throttle | Self::UsageWindow | Self::Account | Self::Auth
        )
    }
}

/// Structured instruction to retry one logical prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryDecision {
    /// Normalized failure class controlling cadence and status.
    pub class: RetryClass,
    /// Trusted lower bound relative to the time the attempt failed.
    pub retry_after: Option<Duration>,
}

impl RetryDecision {
    /// Construct a retry decision without a server timing hint.
    #[must_use]
    pub const fn new(class: RetryClass) -> Self {
        Self {
            class,
            retry_after: None,
        }
    }

    /// Attach a trusted lower-bound delay.
    #[must_use]
    pub const fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }
}

/// Parse a standard `Retry-After` delta or HTTP date into a relative delay.
#[must_use]
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Find known structured reset metadata at any JSON object nesting level.
///
/// Provider prose is deliberately ignored. Only numeric `resets_in_seconds`
/// and `resets_at` fields are trusted.
#[must_use]
pub fn parse_json_reset_hint(body: &str, now: SystemTime) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    find_reset_hint(&value, now)
}

fn find_reset_hint(value: &serde_json::Value, now: SystemTime) -> Option<Duration> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(seconds) = map
                .get("resets_in_seconds")
                .and_then(serde_json::Value::as_u64)
            {
                return Some(Duration::from_secs(seconds));
            }
            if let Some(reset_at) = map.get("resets_at").and_then(serde_json::Value::as_u64) {
                let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
                return Some(Duration::from_secs(reset_at.saturating_sub(now)));
            }
            map.values().find_map(|value| find_reset_hint(value, now))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_reset_hint(value, now))
        }
        _ => None,
    }
}

/// Return the first known structured provider error identifier in JSON.
#[must_use]
pub fn parse_json_error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    find_error_code(&value).map(ToOwned::to_owned)
}

fn find_error_code(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["type", "code"] {
                if let Some(value) = map.get(key).and_then(serde_json::Value::as_str) {
                    return Some(value);
                }
            }
            map.values().find_map(find_error_code)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_error_code),
        _ => None,
    }
}

/// Classify a provider error identifier without treating unknown values as
/// terminal.
#[must_use]
pub fn classify_error_code(code: &str) -> RetryClass {
    match code {
        "usage_limit_reached" => RetryClass::UsageWindow,
        "rate_limit_exceeded" => RetryClass::Throttle,
        "quota_exceeded"
        | "billing_hard_limit_reached"
        | "insufficient_quota"
        | "usage_not_included"
        | "credits_exhausted" => RetryClass::Account,
        "invalid_api_key"
        | "authentication_error"
        | "invalid_authentication"
        | "token_expired"
        | "unauthorized" => RetryClass::Auth,
        "overloaded_error" | "server_error" | "upstream_timeout" => RetryClass::Overload,
        _ => RetryClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures standard delta and HTTP-date hints survive transport boundaries.
    #[test]
    fn parses_retry_after_forms() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after("123", now),
            Some(Duration::from_secs(123))
        );
        let date = httpdate::fmt_http_date(now + Duration::from_secs(321));
        assert_eq!(
            parse_retry_after(&date, now),
            Some(Duration::from_secs(321))
        );
        assert_eq!(parse_retry_after("-1", now), None);
    }

    /// Ensures reset fields remain usable even when providers change nesting.
    #[test]
    fn parses_nested_reset_fields() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(
            parse_json_reset_hint(r#"{"event":{"error":{"resets_at":160}}}"#, now),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_json_reset_hint(r#"{"error":{"resets_in_seconds":90}}"#, now),
            Some(Duration::from_secs(90))
        );
    }

    /// Ensures billing and unfamiliar provider codes remain retryable classes.
    #[test]
    fn classifies_account_and_unknown_errors() {
        assert_eq!(
            classify_error_code("insufficient_quota"),
            RetryClass::Account
        );
        assert_eq!(
            classify_error_code("usage_limit_s_reached"),
            RetryClass::Unknown
        );
    }
}
