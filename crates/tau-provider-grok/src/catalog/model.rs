//! Exact public model metadata; discovery is not an entitlement assertion.

use serde::Deserialize;

/// One model returned by xAI's public model catalog.
///
/// Prices remain exact integers in USD cents per 100 million tokens. Missing
/// prices and context lengths stay unknown, rather than becoming zero. Image
/// generation entries can occur in this list and must not be published blindly
/// as text-inference models. Aliases may have different reasoning defaults.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Model {
    /// Exact upstream request identifier.
    pub id: String,
    /// Alternate request identifiers reported by the server.
    pub aliases: Vec<String>,
    /// Advertised token context limit, if known.
    pub context_length: Option<u64>,
    /// Prompt text price, including explicit zero when reported.
    pub prompt_text_token_price: Option<u64>,
    /// Cached prompt text price, if known.
    pub cached_prompt_text_token_price: Option<u64>,
    /// Generated text price, if known.
    pub completion_text_token_price: Option<u64>,
    /// Input image token price; not proof of native tool-image support.
    pub prompt_image_token_price: Option<u64>,
    /// Token count at which the long-context prices apply.
    pub long_context_threshold: Option<u64>,
    /// Long-context prompt price, if known.
    pub prompt_text_token_price_long_context: Option<u64>,
    /// Long-context cached prompt price, if known.
    pub cached_prompt_text_token_price_long_context: Option<u64>,
    /// Long-context generated text price, if known.
    pub completion_text_token_price_long_context: Option<u64>,
    /// Advertised controls, absent when the catalog makes no capability claim.
    pub capabilities: Option<Capabilities>,
}

/// Model-owned reasoning selectors, never derived from a model-name pattern.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Capabilities {
    /// Exact accepted `reasoning.effort` strings; unknown future values
    /// survive.
    pub reasoning_effort: Vec<String>,
    /// Model-wide default; must not be imposed on aliases.
    pub default_reasoning_effort: Option<String>,
}
