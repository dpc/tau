//! Shared client-side helpers for Tau extension and UI protocol peers.
//!
//! This crate owns the first reusable slice of extension runtime behavior: the
//! startup prelude, typed configuration handling, replay-aware typed and raw
//! event dispatch, live tool/action dispatch, prompt interception replies, and
//! a cloneable outbound [`ClientHandle`]. Extensions with timers or other
//! custom scheduling needs can use [`ManualExtensionRuntime`] to receive with
//! timeouts and dispatch messages one at a time while preserving the same
//! protocol semantics, including correlated extension-data RPC through
//! [`ExtensionDataClient`]. It also provides the standard extension `TAU_LOG`
//! subscriber helpers. First-party extensions now use this crate directly; the
//! old compatibility startup helper crate has been removed after the migration
//! completed.

mod builder;
mod client_error;
mod client_handle;
mod config;
mod contexts;
mod event_payload;
mod extension_trait;
mod handler;
mod intercept_decision;
mod logging;
mod manual_runtime;
mod runner;
mod writer_thread;

pub use builder::ExtensionBuilder;
pub use client_error::{ClientError, ClientResult};
pub use client_handle::ClientHandle;
pub use contexts::{
    ActionContext, ConfigureContext, ConfigureErrorContext, EventContext, InterceptContext,
    RawConfigureContext, RawEventContext, ToolContext,
};
pub use event_payload::EventPayload;
pub use extension_trait::{ExtensionPlugin, TauExtension};
pub use intercept_decision::InterceptDecision;
pub use logging::{DEFAULT_FILTER, ENV_VAR, init_logging, init_logging_for};
pub use manual_runtime::{
    DispatchOutcome, ExtensionDataClient, ExtensionDataRpcError, ManualExtensionRuntime,
    ManualRuntimeInput,
};
pub use runner::TauExtensionRunner;

#[cfg(test)]
mod tests;
