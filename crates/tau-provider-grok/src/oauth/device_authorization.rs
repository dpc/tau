//! Device grant state and bounded RFC 8628 polling schedule.

use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;

use super::Error;

/// An outstanding device grant; device credentials are deliberately not Debug.
pub struct DeviceAuthorization {
    /// Server-issued credential used only at the token endpoint.
    pub(super) device_code: String,
    /// Short code the user enters in their own browser.
    pub user_code: String,
    /// HTTPS verification page returned by the fixed, authenticated xAI issuer.
    pub verification_uri: String,
    /// Absolute monotonic expiry measured from device request dispatch.
    pub(super) deadline: Instant,
    /// Current minimum wait between polling attempts.
    pub(super) interval: Duration,
}

/// Device endpoint fields; no optional complete URI is needed for headless
/// login.
#[derive(Deserialize)]
struct WireDevice {
    /// Opaque token-endpoint credential.
    device_code: String,
    /// Human-facing short code.
    user_code: String,
    /// Human-facing verification page.
    verification_uri: String,
    /// Required grant lifetime in seconds.
    expires_in: u64,
    /// Optional polling interval in seconds, defaulting to RFC 8628's five.
    interval: Option<u64>,
}

impl DeviceAuthorization {
    /// Validate server values and bound the login to at most thirty minutes.
    pub(super) fn parse(body: &[u8], started: Instant) -> Result<Self, Error> {
        let wire: WireDevice = serde_json::from_slice(body).map_err(|_| Error::InvalidResponse)?;
        let uri = url::Url::parse(&wire.verification_uri).map_err(|_| Error::InvalidResponse)?;
        if wire.device_code.trim().is_empty()
            || wire.user_code.trim().is_empty()
            || wire.user_code.chars().any(char::is_control)
            || uri.scheme() != "https"
            || uri.host_str().is_none()
            || uri.port_or_known_default() != Some(443)
            || !uri.username().is_empty()
            || uri.password().is_some()
            || wire.verification_uri.chars().any(char::is_control)
            || wire.expires_in == 0
            || wire.interval == Some(0)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(Self {
            device_code: wire.device_code,
            user_code: wire.user_code,
            verification_uri: wire.verification_uri,
            deadline: started + Duration::from_secs(wire.expires_in.min(1_800)),
            interval: Duration::from_secs(wire.interval.unwrap_or(5).min(1_800)),
        })
    }

    /// Advance the persistent polling interval after an RFC 8628 slow-down.
    pub(super) fn slow_down(&mut self) {
        self.interval = self.interval.saturating_add(Duration::from_secs(5));
    }
}
