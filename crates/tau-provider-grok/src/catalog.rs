//! Bounded public xAI model discovery without credential persistence.

mod model;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;

pub use self::model::{Capabilities, Model};

/// Public API origin; no CLI proxy or client impersonation headers.
const ORIGIN: &str = "https://api.x.ai";
/// Bound the decoded discovery response independently of transfer compression.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// A caller-owned model discovery client using frozen outbound network policy.
///
/// Successful discovery describes server metadata, not subscription
/// eligibility, Fast availability, free usage, quota or permission to consume
/// extra usage. No access token is retained by this client.
pub struct Catalog {
    /// Fixed-origin client with redirects and transparent retries disabled.
    http: reqwest::Client,
    /// Production origin, substituted only by private loopback tests.
    origin: String,
}

/// Content-free discovery failures without provider bodies, tokens or URLs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Network policy, dispatch, deadline or body reading failed.
    Transport,
    /// Credentials were missing or rejected; caller owns any refresh decision.
    Unauthorized,
    /// The server rejected discovery; no entitlement or billing inference.
    Rejected,
    /// A successful reply exceeded bounds or contained invalid metadata.
    InvalidResponse,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Grok model discovery {self:?}")
    }
}

impl std::error::Error for Error {}

impl Catalog {
    /// Build discovery for the fixed public xAI API.
    pub fn new(network: &tau_provider::OutboundNetworkPolicy) -> Result<Self, Error> {
        Self::at_origin(network, ORIGIN)
    }

    /// Private loopback substitution retains production network policy.
    fn at_origin(
        network: &tau_provider::OutboundNetworkPolicy,
        origin: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            http: network
                .client_for_without_retries(origin)
                .map_err(|_| Error::Transport)?,
            origin: origin.to_owned(),
        })
    }

    /// Fetch once using the caller's current bearer generation.
    ///
    /// Dropping this future cancels local waiting. There is one 30-second total
    /// HTTP deadline, no automatic credential refresh and no paid-key fallback.
    pub async fn fetch(&self, access_token: &str) -> Result<Vec<Model>, Error> {
        parse(&self.get("/v1/models", access_token).await?)
    }

    /// Discover only language routes with explicit text input/output metadata.
    ///
    /// The general catalog also contains image generation routes, including
    /// routes with positive context lengths. Join by exact canonical
    /// identifier; neither names nor prices establish a text route or image
    /// capability.
    pub async fn fetch_language_models(
        &self,
        access_token: &str,
    ) -> Result<Vec<LanguageModel>, Error> {
        let models = self.fetch(access_token).await?;
        let body = self.get("/v1/language-models", access_token).await?;
        join_language_models(models, &body)
    }

    /// One bounded, fixed-origin request without retained bearer material.
    async fn get(&self, path: &str, access_token: &str) -> Result<Vec<u8>, Error> {
        if access_token.trim().is_empty() {
            return Err(Error::Unauthorized);
        }
        let mut response = self
            .http
            .get(format!("{}{path}", self.origin))
            .bearer_auth(access_token)
            .header("accept", "application/json")
            .header("user-agent", concat!("tau/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(Error::Rejected);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if MAX_BODY_BYTES.saturating_sub(body.len()) < chunk.len() {
                return Err(Error::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// A text-inference route positively identified by the language catalog.
#[derive(Clone, Debug)]
pub struct LanguageModel {
    /// Exact general-catalog metadata, including optional context and prices.
    pub metadata: Model,
    /// Explicit image input modality; not native tool-result image authority.
    pub image_input: bool,
}

/// Language catalog envelope differs from the OpenAI-compatible general list.
#[derive(Deserialize)]
struct LanguageEnvelope {
    /// Explicit language routes.
    models: Vec<LanguageRow>,
}

/// Only modality facts are consumed from the language-specific catalog.
#[derive(Deserialize)]
struct LanguageRow {
    /// Canonical identifier joined without alias rewriting.
    id: String,
    /// Explicit input modalities.
    input_modalities: Vec<String>,
    /// Explicit output modalities.
    output_modalities: Vec<String>,
}

/// Intersect the two catalogs without guessing missing metadata.
fn join_language_models(models: Vec<Model>, body: &[u8]) -> Result<Vec<LanguageModel>, Error> {
    let language: LanguageEnvelope =
        serde_json::from_slice(body).map_err(|_| Error::InvalidResponse)?;
    let mut rows = BTreeMap::new();
    for row in language.models {
        if !valid_id(&row.id) || rows.contains_key(&row.id) {
            return Err(Error::InvalidResponse);
        }
        rows.insert(row.id.clone(), row);
    }
    let mut result = Vec::new();
    for metadata in models {
        let Some(row) = rows.remove(&metadata.id) else {
            continue;
        };
        if row.input_modalities.iter().any(|value| value == "text")
            && row.output_modalities.iter().any(|value| value == "text")
        {
            result.push(LanguageModel {
                metadata,
                image_input: row.input_modalities.iter().any(|value| value == "image"),
            });
        }
    }
    Ok(result)
}

/// Typed list envelope; unknown upstream metadata does not invent capabilities.
#[derive(Deserialize)]
struct Envelope {
    /// Required upstream list discriminator.
    object: String,
    /// Exact model metadata in server order.
    data: Vec<Model>,
}

/// Validate identifiers before callers publish them into a model catalog.
fn parse(body: &[u8]) -> Result<Vec<Model>, Error> {
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| Error::InvalidResponse)?;
    let mut identifiers = BTreeSet::new();
    if envelope.object != "list"
        || envelope.data.iter().any(|model| {
            !valid_id(&model.id)
                || !identifiers.insert(&model.id)
                || model.aliases.iter().any(|alias| !valid_id(alias))
        })
    {
        return Err(Error::InvalidResponse);
    }
    Ok(envelope.data)
}

/// Model identifiers cannot contain controls or provider/model separators.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && !id.chars().any(|character| {
            character.is_whitespace() || character.is_control() || character == '/'
        })
}
