//! Native xAI device OAuth and refresh, with no credential-store authority.
//!
//! Endpoints and scopes follow xAI discovery and the endorsed OpenCode device
//! integration inspected on 2026-09-24. Public client registration is selected
//! by the caller; this module never impersonates Grok Build or reads its files.

mod device_authorization;
mod error;
mod token_response;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use device_authorization::DeviceAuthorization;
pub use error::Error;
pub use token_response::TokenResponse;
use tokio::time::Instant;
use url::form_urlencoded;

/// Public xAI OIDC issuer; no CLI proxy or browser-cookie authentication.
const ISSUER: &str = "https://auth.x.ai";
/// Minimal inference scopes used by xAI's endorsed third-party integration.
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Maximum decoded OAuth response size, including success and error bodies.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// One finite operation cannot occupy its worker indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A fixed-origin OAuth client without automatic HTTP or refresh retries.
pub struct Client {
    /// Network-policy client with redirects and transparent retries disabled.
    http: reqwest::Client,
    /// Caller-selected public OAuth registration; not a client secret.
    client_id: String,
    /// Fixed production issuer; a loopback override is available only to tests.
    issuer: String,
}

impl Client {
    /// Build a fixed xAI-origin client using Tau's frozen outbound network
    /// policy.
    pub fn new(
        client_id: &str,
        network: &tau_provider::OutboundNetworkPolicy,
    ) -> Result<Self, Error> {
        if client_id.trim().is_empty() {
            return Err(Error::InvalidClientId);
        }
        Self::at_issuer(client_id, network, ISSUER)
    }

    /// Keep origin substitution private while sharing production network
    /// policy.
    fn at_issuer(
        client_id: &str,
        network: &tau_provider::OutboundNetworkPolicy,
        issuer: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            http: network
                .client_for_without_retries(issuer)
                .map_err(|_| Error::Transport)?,
            client_id: client_id.to_owned(),
            issuer: issuer.to_owned(),
        })
    }

    /// Begin a device grant without opening a browser or reading local
    /// accounts.
    pub async fn authorize_device(&self) -> Result<DeviceAuthorization, Error> {
        let started = Instant::now();
        let body = self
            .post(
                "/oauth2/device/code",
                &[("client_id", &self.client_id), ("scope", SCOPE)],
            )
            .await?;
        DeviceAuthorization::parse(&body, started)
    }

    /// Poll until approved, denied, expired, canceled, or a finite request
    /// fails.
    ///
    /// Polling waits before the first request and increases every later
    /// interval by five seconds after `slow_down`. Transport failures stop
    /// the flow rather than guessing whether the server consumed an
    /// authorization code.
    pub async fn finish_device(
        &self,
        device: DeviceAuthorization,
        canceled: impl std::future::Future<Output = ()>,
    ) -> Result<TokenResponse, Error> {
        let device_code = device.device_code.clone();
        let fields = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", &self.client_id),
            ("device_code", &device_code),
        ];
        poll_device(device, canceled, || self.post("/oauth2/token", &fields)).await
    }

    /// Perform exactly one refresh exchange; the caller owns locking and
    /// saving.
    ///
    /// A lost response may have consumed the refresh token. This method does
    /// not resend it, even for a transport error or server-side transient
    /// failure.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, Error> {
        if refresh_token.trim().is_empty() {
            return Err(Error::InvalidGrant);
        }
        let body = self
            .post(
                "/oauth2/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", &self.client_id),
                    ("refresh_token", refresh_token),
                ],
            )
            .await?;
        TokenResponse::parse(&body)
    }

    /// Obtain the server-validated account subject, never unsigned JWT claims.
    pub async fn subject(&self, access_token: &str) -> Result<String, Error> {
        if access_token.trim().is_empty() {
            return Err(Error::InvalidResponse);
        }
        let request = self
            .http
            .get(format!("{}/oauth2/userinfo", self.issuer))
            .bearer_auth(access_token);
        let body = read_response(request).await?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| Error::InvalidResponse)?;
        value
            .get("sub")
            .and_then(serde_json::Value::as_str)
            .filter(|subject| {
                !subject.trim().is_empty()
                    && subject.len() <= 1_024
                    && !subject.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .ok_or(Error::InvalidResponse)
    }

    /// Form-encode opaque values and send only to the configured issuer.
    async fn post(&self, path: &str, fields: &[(&str, &str)]) -> Result<Vec<u8>, Error> {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        read_response(
            self.http
                .post(format!("{}{path}", self.issuer))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body),
        )
        .await
    }
}

/// Apply one absolute request deadline and a bound on decoded response bytes.
async fn read_response(request: reqwest::RequestBuilder) -> Result<Vec<u8>, Error> {
    let mut response = request
        .header("accept", "application/json")
        .header("user-agent", concat!("tau/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| Error::Transport)?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if MAX_RESPONSE_BYTES.saturating_sub(body.len()) < chunk.len() {
            return Err(Error::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(Error::from_body(&body));
    }
    Ok(body)
}

/// Schedule finite exchanges using real deadlines and notification
/// cancellation.
async fn poll_device<Exchange>(
    mut device: DeviceAuthorization,
    canceled: impl std::future::Future<Output = ()>,
    mut exchange: impl FnMut() -> Exchange,
) -> Result<TokenResponse, Error>
where
    Exchange: std::future::Future<Output = Result<Vec<u8>, Error>>,
{
    tokio::pin!(canceled);
    loop {
        let wake = (Instant::now() + device.interval).min(device.deadline);
        tokio::select! {
            biased;
            () = &mut canceled => return Err(Error::Canceled),
            () = tokio::time::sleep_until(wake) => {}
        }
        if device.deadline <= Instant::now() {
            return Err(Error::Expired);
        }
        let result = tokio::select! {
            biased;
            () = &mut canceled => return Err(Error::Canceled),
            () = tokio::time::sleep_until(device.deadline) => return Err(Error::Expired),
            result = exchange() => result,
        };
        match result {
            Ok(body) => {
                let tokens = TokenResponse::parse(&body)?;
                // A subscription login must be renewable before it is saved.
                if tokens.refresh_token.is_none() {
                    return Err(Error::InvalidResponse);
                }
                return Ok(tokens);
            }
            Err(Error::AuthorizationPending) => {}
            Err(Error::SlowDown) => device.slow_down(),
            Err(error) => return Err(error),
        }
    }
}
