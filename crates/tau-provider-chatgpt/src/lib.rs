//! Public Sign in with ChatGPT protocol; never imports legacy Codex tokens.

pub mod authorization;
pub mod catalog;
pub mod credential;
pub mod oauth;
pub mod request;

mod error;
mod identity;
#[cfg(test)]
mod tests;
mod token_response;

pub use error::Error;

/// Fixed resource for ChatGPT plan usage, not an API-key billing fallback.
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// Fixed production OIDC issuer.
pub const ISSUER: &str = "https://auth.openai.com";
/// Permission required before any account catalog or inference request.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
