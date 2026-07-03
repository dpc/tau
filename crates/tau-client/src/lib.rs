//! Shared client-side helpers for Tau extension and UI protocol peers.
//!
//! This crate owns the first reusable slice of extension runtime behavior: the
//! startup prelude, typed configuration handling, replay-aware typed and raw
//! event dispatch, live tool dispatch, prompt interception replies, and a
//! cloneable outbound [`ClientHandle`]. It intentionally keeps the existing Tau
//! wire protocol and `tau_extension::Handshake` intact while new extensions
//! migrate incrementally.

mod builder;
mod client_error;
mod client_handle;
mod contexts;
mod event_payload;
mod handler;
mod intercept_decision;
mod runner;
mod tau_extension_trait;
mod writer_thread;

pub use builder::ExtensionBuilder;
pub use client_error::{ClientError, ClientResult};
pub use client_handle::ClientHandle;
pub use contexts::{
    ConfigureContext, EventContext, InterceptContext, RawEventContext, ToolContext,
};
pub use event_payload::EventPayload;
pub use intercept_decision::InterceptDecision;
pub use runner::TauExtensionRunner;
pub use tau_extension_trait::{ExtensionPlugin, TauExtension};

#[cfg(test)]
mod tests;
