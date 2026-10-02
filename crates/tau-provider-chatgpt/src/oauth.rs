//! Fixed-origin finite OAuth operations; callers own storage and serialization.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::jwk::JwkSet;
use serde::Deserialize;
use url::form_urlencoded::Serializer;

use crate::authorization::Callback;
use crate::credential::Credential;
use crate::token_response::TokenResponse;
use crate::{Error, ISSUER, RESOURCE, identity};

#[cfg(test)]
mod tests;

/// Bounded OpenAI OAuth client without implicit retries or redirects.
pub struct Client {
    /// Frozen-policy transport, never carries credentials across origins.
    http: reqwest::Client,
    /// Fixed production issuer; only private unit tests substitute loopback.
    issuer: String,
}

impl Client {
    /// Build the fixed production OAuth client under Tau's network policy.
    pub fn new(network: &tau_provider::OutboundNetworkPolicy) -> Result<Self, Error> {
        Self::at_issuer(network, ISSUER)
    }

    /// Private construction seam for deterministic HTTP contract tests.
    fn at_issuer(
        network: &tau_provider::OutboundNetworkPolicy,
        issuer: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            http: network
                .client_for_without_retries(issuer)
                .map_err(|_| Error::Transport)?,
            issuer: issuer.to_owned(),
        })
    }

    /// Redeem a verified callback once and bind its signed identity to the
    /// selected registration before returning publishable credentials.
    pub async fn exchange(&self, callback: Callback) -> Result<Credential, Error> {
        let tokens = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", &callback.client_id),
                ("code", &callback.code),
                ("code_verifier", &callback.authorization.verifier),
                ("redirect_uri", &callback.authorization.redirect_uri),
                ("resource", RESOURCE),
            ])
            .await?;
        let keys = self.keys().await?;
        let now_ms = now_ms()?;
        let identity = identity::verify(
            tokens.id_token.as_deref().ok_or(Error::InvalidIdentity)?,
            &keys,
            &callback.client_id,
            Some(&callback.authorization.nonce),
            now_ms,
        )?;
        if callback
            .authorization
            .subject
            .as_deref()
            .is_some_and(|subject| subject != identity.sub)
        {
            return Err(Error::AccountChanged);
        }
        Ok(Credential::from_login(
            callback.client_id,
            identity,
            tokens,
            now_ms,
        ))
    }

    /// Exchange once under the caller's exclusive renewable-session claim.
    /// A network failure may consume the old token and must not be retried.
    pub async fn refresh(&self, current: &Credential) -> Result<Credential, Error> {
        let tokens = self
            .token(&[
                ("grant_type", "refresh_token"),
                ("client_id", current.client_id()),
                ("refresh_token", current.refresh_token()?),
                ("resource", RESOURCE),
            ])
            .await?;
        if tokens.refresh_token.is_none() {
            return Err(Error::InvalidResponse);
        }
        if let Some(token) = &tokens.id_token {
            let identity = identity::verify(
                token,
                &self.keys().await?,
                current.client_id(),
                None,
                now_ms()?,
            )?;
            if identity.sub != current.subject() {
                return Err(Error::AccountChanged);
            }
        }
        let mut refreshed = current.clone();
        refreshed.replace(tokens, now_ms()?);
        Ok(refreshed)
    }

    /// Attempt renewable-session revocation using same-origin discovery.
    /// Callers still clear local tokens if remote revocation is unconfirmed.
    pub async fn revoke(&self, current: &Credential) -> Result<(), Error> {
        let discovery: Discovery = serde_json::from_slice(
            &read(
                self.http
                    .get(format!("{}/.well-known/openid-configuration", self.issuer)),
                64 * 1024,
            )
            .await?,
        )
        .map_err(|_| Error::InvalidResponse)?;
        let endpoint =
            url::Url::parse(&discovery.revocation_endpoint).map_err(|_| Error::InvalidResponse)?;
        if discovery.issuer != ISSUER
            || endpoint.scheme() != "https"
            || endpoint.host_str() != Some("auth.openai.com")
            || endpoint.port().is_some()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || endpoint.query().is_some()
        {
            return Err(Error::InvalidResponse);
        }
        read(
            self.form(
                endpoint.as_str(),
                &[
                    ("token", current.refresh_token()?),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", current.client_id()),
                ],
            ),
            64 * 1024,
        )
        .await?;
        Ok(())
    }

    /// Retrieve current signing keys for this finite login/refresh, avoiding
    /// stale-key fallback or keys supplied through untrusted token headers.
    async fn keys(&self) -> Result<JwkSet, Error> {
        serde_json::from_slice(
            &read(
                self.http
                    .get(format!("{}/.well-known/jwks.json", self.issuer)),
                256 * 1024,
            )
            .await?,
        )
        .map_err(|_| Error::InvalidIdentity)
    }

    /// Parse one token response without preserving an error body.
    async fn token(&self, fields: &[(&str, &str)]) -> Result<TokenResponse, Error> {
        TokenResponse::parse(
            &read(
                self.form(&format!("{}/api/accounts/oauth/token", self.issuer), fields),
                64 * 1024,
            )
            .await?,
        )
    }

    /// Form-encode opaque values; never interpolate tokens into URLs.
    fn form(&self, endpoint: &str, fields: &[(&str, &str)]) -> reqwest::RequestBuilder {
        self.http
            .post(endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(
                Serializer::new(String::new())
                    .extend_pairs(fields.iter().copied())
                    .finish(),
            )
    }
}

/// Timestamp token receipt rather than request start so a slow exchange does
/// not make a freshly issued identity look future-dated.
fn now_ms() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::InvalidResponse)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::InvalidResponse)
}

/// Minimal OIDC discovery projection for renewable-session logout.
#[derive(Deserialize)]
struct Discovery {
    /// Exact production issuer required before using a discovered endpoint.
    issuer: String,
    /// Same-origin revocation endpoint, never a caller-provided URL.
    revocation_endpoint: String,
}

/// Bound both the total HTTP operation and decoded response allocation.
pub(crate) async fn read(request: reqwest::RequestBuilder, limit: usize) -> Result<Vec<u8>, Error> {
    let mut response = request
        .header("accept", "application/json")
        .header("user-agent", concat!("tau/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| Error::Transport)?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(Error::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    if status != reqwest::StatusCode::OK {
        return Err(Error::from_body(&body));
    }
    Ok(body)
}
