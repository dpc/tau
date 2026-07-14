//! ChatGPT account-quota acquisition and rolling transport normalization.
//!
//! Provider-controlled prose, credits, spend controls, and per-response token
//! usage are intentionally excluded from these records.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use tau_proto::{ProviderQuotaLimitId, ProviderQuotaWindowId};

/// Maximum accepted `/wham/usage` response body.
pub const MAX_USAGE_BODY_BYTES: u64 = 256 * 1024;
/// Timeout for the best-effort account-usage request.
pub const USAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One provider-normalized sparse or complete quota-window observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaWindowObservation {
    /// Stable normalized pool id.
    pub limit_id: ProviderQuotaLimitId,
    /// Stable `primary` or `secondary` window id.
    pub window_id: ProviderQuotaWindowId,
    /// Observed usage in basis points.
    pub used_basis_points: u16,
    /// Server-declared window duration, when present in this observation.
    pub window_seconds: Option<u64>,
    /// Server-declared absolute reset, when present.
    pub reset_at_unix_seconds: Option<u64>,
    /// Server-declared remaining seconds, available from full snapshots.
    pub remaining_seconds: Option<i64>,
}

/// One supported rolling quota observation from HTTP headers or WebSocket.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RollingQuotaObservation {
    /// Complete list of pool/window fragments present in this observation.
    pub windows: Vec<QuotaWindowObservation>,
    /// Exact active pool proving applicability to the requested model.
    pub active_limit_id: Option<ProviderQuotaLimitId>,
    /// Evidence establishing the exact model binding, paired with the active
    /// id.
    pub binding_provenance: Option<tau_proto::ProviderQuotaBindingProvenance>,
}

/// Full account snapshot returned by `/wham/usage`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FullQuotaSnapshot {
    /// All valid normalized pool/window records in the bounded response.
    pub windows: Vec<QuotaWindowObservation>,
}

/// Sanitized error returned by account-quota acquisition.
#[derive(Debug)]
pub enum UsageFetchError {
    /// Request construction or transport failed.
    Transport,
    /// Upstream returned a non-success status.
    Status(u16),
    /// Response exceeded the quota-specific body cap.
    BodyTooLarge,
    /// Top-level response JSON was malformed.
    InvalidJson,
}

impl std::fmt::Display for UsageFetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => formatter.write_str("ChatGPT quota request failed"),
            Self::Status(status) => {
                write!(formatter, "ChatGPT quota request returned HTTP {status}")
            }
            Self::BodyTooLarge => formatter.write_str("ChatGPT quota response exceeded size limit"),
            Self::InvalidJson => formatter.write_str("ChatGPT quota response was invalid JSON"),
        }
    }
}

impl std::error::Error for UsageFetchError {}

/// Fetches and normalizes the authenticated ChatGPT account-usage snapshot.
///
/// Redirects are disabled so bearer and account headers cannot cross origins.
pub fn fetch_usage(
    base_url: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<FullQuotaSnapshot, UsageFetchError> {
    let url = format!("{}/wham/usage", base_url.trim_end_matches('/'));
    let mut request = tau_provider::oauth::proxy_agent()
        .get(&url)
        .config()
        .timeout_global(Some(USAGE_REQUEST_TIMEOUT))
        .max_redirects(0)
        .build()
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"));
    if let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) {
        request = request.header("chatgpt-account-id", account_id);
    }
    let mut response = request.call().map_err(|_| UsageFetchError::Transport)?;
    if !response.status().is_success() {
        return Err(UsageFetchError::Status(response.status().as_u16()));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_USAGE_BODY_BYTES)
        .read_to_string()
        .map_err(|error| {
            if error.to_string().contains("limit") {
                UsageFetchError::BodyTooLarge
            } else {
                UsageFetchError::Transport
            }
        })?;
    parse_full_usage_json(&body)
}

#[derive(Deserialize)]
struct RawAdditionalRateLimit {
    metered_feature: String,
    #[serde(default)]
    rate_limit: Option<RawRateLimit>,
}

#[derive(Deserialize)]
struct RawRateLimit {
    #[serde(default)]
    primary_window: Option<RawWindow>,
    #[serde(default)]
    secondary_window: Option<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
    #[serde(default)]
    reset_after_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

/// Parses a full account snapshot while rejecting malformed pools
/// independently.
pub fn parse_full_usage_json(body: &str) -> Result<FullQuotaSnapshot, UsageFetchError> {
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|_| UsageFetchError::InvalidJson)?;
    let payload = payload.as_object().ok_or(UsageFetchError::InvalidJson)?;
    let mut pools = Vec::new();
    if let Some(rate_limit) = payload
        .get("rate_limit")
        .cloned()
        .and_then(|value| serde_json::from_value::<RawRateLimit>(value).ok())
    {
        pools.push(("codex".to_owned(), rate_limit));
    }
    pools.extend(
        payload
            .get("additional_rate_limits")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|additional| {
                serde_json::from_value::<RawAdditionalRateLimit>(additional.clone()).ok()
            })
            .filter_map(|additional| {
                additional
                    .rate_limit
                    .map(|rate_limit| (additional.metered_feature, rate_limit))
            }),
    );
    let mut normalized = BTreeMap::<ProviderQuotaLimitId, Vec<QuotaWindowObservation>>::new();
    let mut collisions = BTreeSet::new();
    for (raw_id, rate_limit) in pools {
        let Some(limit_id) = normalize_limit_id(&raw_id) else {
            continue;
        };
        if normalized.contains_key(&limit_id) {
            collisions.insert(limit_id);
            continue;
        }
        let mut windows = Vec::new();
        if let Some(window) = rate_limit
            .primary_window
            .and_then(|window| normalize_window(&limit_id, "primary", window, true))
        {
            windows.push(window);
        }
        if let Some(window) = rate_limit
            .secondary_window
            .and_then(|window| normalize_window(&limit_id, "secondary", window, true))
        {
            windows.push(window);
        }
        normalized.insert(limit_id, windows);
    }
    for collision in collisions {
        normalized.remove(&collision);
    }
    let windows = normalized.into_values().flatten().collect::<Vec<_>>();
    if windows.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS {
        return Err(UsageFetchError::InvalidJson);
    }
    Ok(FullQuotaSnapshot { windows })
}

/// Parses every supported quota header family from one HTTP response.
pub fn parse_http_headers(headers: &ureq::http::HeaderMap) -> RollingQuotaObservation {
    let mut raw_ids = BTreeSet::from(["codex".to_owned()]);
    for name in headers.keys() {
        let name = name.as_str().to_ascii_lowercase();
        if let Some(prefix) = name
            .strip_suffix("-primary-used-percent")
            .or_else(|| name.strip_suffix("-secondary-used-percent"))
            && let Some(raw_id) = prefix.strip_prefix("x-")
        {
            raw_ids.insert(raw_id.replace('-', "_"));
        }
    }
    let mut windows = Vec::new();
    for raw_id in raw_ids {
        let Some(limit_id) = normalize_limit_id(&raw_id) else {
            continue;
        };
        let header_id = limit_id.as_str().replace('_', "-");
        for window_id in ["primary", "secondary"] {
            let prefix = format!("x-{header_id}-{window_id}");
            let Some(used_percent) = header_f64(headers, &format!("{prefix}-used-percent")) else {
                continue;
            };
            let window_minutes = header_i64(headers, &format!("{prefix}-window-minutes"));
            let reset_at = header_i64(headers, &format!("{prefix}-reset-at"));
            if used_percent == 0.0
                && window_minutes.is_none_or(|minutes| minutes == 0)
                && reset_at.is_none_or(|reset| reset == 0)
            {
                continue;
            }
            let raw = RawWindow {
                used_percent,
                limit_window_seconds: window_minutes.and_then(|minutes| minutes.checked_mul(60)),
                reset_after_seconds: None,
                reset_at,
            };
            if let Some(window) = normalize_window(&limit_id, window_id, raw, false) {
                windows.push(window);
            }
        }
    }
    let active_limit_id = headers
        .get("x-codex-active-limit")
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_limit_id);
    if windows.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS {
        return RollingQuotaObservation::default();
    }
    let binding_provenance = active_limit_id
        .as_ref()
        .map(|_| tau_proto::ProviderQuotaBindingProvenance::ActiveLimitHeader);
    RollingQuotaObservation {
        windows,
        active_limit_id,
        binding_provenance,
    }
}

#[derive(Deserialize)]
struct WsQuotaEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    rate_limits: Option<WsRateLimits>,
    #[serde(default)]
    metered_limit_name: Option<String>,
    #[serde(default)]
    limit_name: Option<String>,
}

#[derive(Deserialize)]
struct WsRateLimits {
    #[serde(default)]
    primary: Option<WsWindow>,
    #[serde(default)]
    secondary: Option<WsWindow>,
}

#[derive(Deserialize)]
struct WsWindow {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<i64>,
    #[serde(default)]
    reset_at: Option<i64>,
}

/// Parses one `codex.rate_limits` WebSocket event.
///
/// JSON `null` is treated as field absence, matching the official optional
/// string contract. A non-null explicit string must normalize successfully:
/// empty, whitespace-only, or otherwise invalid values reject the observation
/// and cannot fall through to a lower-precedence or default pool.
pub fn parse_ws_event(body: &str) -> Option<RollingQuotaObservation> {
    let event: WsQuotaEvent = serde_json::from_str(body).ok()?;
    if event.kind != "codex.rate_limits" {
        return None;
    }
    let metered_limit_id = match event.metered_limit_name.as_deref() {
        Some(raw) => Some(normalize_limit_id(raw)?),
        None => None,
    };
    let legacy_limit_id = match event.limit_name.as_deref() {
        Some(raw) => Some(normalize_limit_id(raw)?),
        None => None,
    };
    // The official Codex contract assigns otherwise-valid nameless
    // `codex.rate_limits` events to the canonical default pool. This is an
    // in-band turn observation, not an inference from account pool presence.
    // Any malformed *present* id still rejects the observation rather than
    // silently changing its meaning to the default pool.
    let limit_id = metered_limit_id
        .or(legacy_limit_id)
        .or_else(|| normalize_limit_id("codex"))?;
    let mut windows = Vec::new();
    if let Some(rate_limits) = event.rate_limits {
        for (window_id, window) in [
            ("primary", rate_limits.primary),
            ("secondary", rate_limits.secondary),
        ] {
            let Some(window) = window else {
                continue;
            };
            let raw = RawWindow {
                used_percent: window.used_percent,
                limit_window_seconds: window
                    .window_minutes
                    .and_then(|minutes| minutes.checked_mul(60)),
                reset_after_seconds: None,
                reset_at: window.reset_at,
            };
            if let Some(window) = normalize_window(&limit_id, window_id, raw, false) {
                windows.push(window);
            }
        }
    }
    Some(RollingQuotaObservation {
        windows,
        active_limit_id: Some(limit_id),
        binding_provenance: Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent),
    })
}

fn normalize_window(
    limit_id: &ProviderQuotaLimitId,
    window_id: &str,
    raw: RawWindow,
    require_duration: bool,
) -> Option<QuotaWindowObservation> {
    if !raw.used_percent.is_finite() || !(-0.5..=100.5).contains(&raw.used_percent) {
        return None;
    }
    let window_seconds = match raw.limit_window_seconds {
        Some(seconds) if seconds > 0 => Some(u64::try_from(seconds).ok()?),
        Some(_) => return None,
        None if require_duration => return None,
        None => None,
    };
    let reset_at_unix_seconds = match raw.reset_at {
        Some(value) if value > 0 => Some(u64::try_from(value).ok()?),
        Some(_) => return None,
        None => None,
    };
    let used_basis_points = (raw.used_percent.clamp(0.0, 100.0) * 100.0).round() as u16;
    Some(QuotaWindowObservation {
        limit_id: limit_id.clone(),
        window_id: ProviderQuotaWindowId::parse(window_id.to_owned()).ok()?,
        used_basis_points,
        window_seconds,
        reset_at_unix_seconds,
        remaining_seconds: raw.reset_after_seconds,
    })
}

fn normalize_limit_id(raw: &str) -> Option<ProviderQuotaLimitId> {
    let mut value = String::with_capacity(raw.len());
    for byte in raw.trim().bytes() {
        let byte = byte.to_ascii_lowercase();
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') {
            value.push(if byte == b'-' { '_' } else { char::from(byte) });
        } else {
            return None;
        }
    }
    ProviderQuotaLimitId::parse(value).ok()
}

fn header_f64(headers: &ureq::http::HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn header_i64(headers: &ureq::http::HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

#[cfg(test)]
#[path = "quota/tests.rs"]
mod tests;
