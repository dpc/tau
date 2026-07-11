//! Shared provider management and runtime utilities.
//!
//! This crate supports multiple named provider instances with API key or OAuth
//! credentials stored in `~/.local/share/tau/auth.json`, and exposes small
//! dependency-light helpers shared by provider backends. Streaming providers
//! can use [`StreamRepetitionGuard`] to abort high-confidence tight exact
//! output loops before they become durable assistant output.

pub mod oauth;
pub mod repetition_guard;
pub mod retry_policy;
pub mod storage;

pub use repetition_guard::{
    RepetitionMode, StreamRepetition, StreamRepetitionGuard, StreamRepetitionKey,
};
