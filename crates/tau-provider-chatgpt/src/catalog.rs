//! Account-specific catalog; never substitutes bundled Codex or API-key models.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
#[cfg(test)]
mod tests;

use crate::credential::Credential;
use crate::{Error, RESOURCE};

/// A displayable model route from this account's catalog, in server order.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Model {
    /// Exact inference identifier.
    pub slug: String,
    /// Human-readable account-picker label.
    pub display_name: String,
    /// Optional positive context limit supplied by the catalog.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Server-owned picker visibility; only `list` entries are returned.
    visibility: String,
}

/// Fetch one finite catalog using the same scoped bearer as inference.
pub async fn fetch(
    network: &tau_provider::OutboundNetworkPolicy,
    credential: &Credential,
) -> Result<Vec<Model>, Error> {
    let client = network
        .client_for_without_retries(RESOURCE)
        .map_err(|_| Error::Transport)?;
    let body = crate::oauth::read(
        client
            .get(format!("{RESOURCE}/models"))
            .bearer_auth(credential.access_token()?),
        1024 * 1024,
    )
    .await?;
    parse(&body)
}

/// Retain server order and reject malformed or duplicate visible routes.
fn parse(body: &[u8]) -> Result<Vec<Model>, Error> {
    #[derive(Deserialize)]
    struct Catalog {
        /// Account-scoped model entries, not the public API's `data` array.
        models: Vec<Model>,
    }
    let catalog: Catalog = serde_json::from_slice(body).map_err(|_| Error::InvalidResponse)?;
    let mut slugs = BTreeSet::new();
    catalog
        .models
        .into_iter()
        .filter(|model| model.visibility == "list")
        .map(|model| {
            if !crate::authorization::valid_opaque(&model.slug)
                || !crate::authorization::valid_opaque(&model.display_name)
                || !slugs.insert(model.slug.clone())
                || model.context_window == Some(0)
            {
                return Err(Error::InvalidResponse);
            }
            Ok(model)
        })
        .collect()
}
