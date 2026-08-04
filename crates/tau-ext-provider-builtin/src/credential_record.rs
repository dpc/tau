//! Opaque version-zero provider credential records.

mod api_key;
mod chatgpt_oauth;

pub(crate) use api_key::ApiKeyCredential;
pub(crate) use chatgpt_oauth::ChatGptOAuthCredential;
