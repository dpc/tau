//! Shared client-side helpers for Tau extension and UI protocol peers.
//!
//! This crate owns the first reusable slice of extension runtime behavior: the
//! startup prelude, typed configuration handling, replay-aware typed and raw
//! event dispatch, live tool/action dispatch, prompt interception replies, and
//! a cloneable outbound [`ClientHandle`]. Extensions with timers or other
//! custom scheduling needs can use [`ManualExtensionRuntime`] to receive with
//! timeouts, wake a reactive loop from side-channel work, and dispatch messages
//! one at a time while preserving the same protocol semantics, including
//! correlated extension-data RPC through
//! [`ExtensionDataClient`]. It also provides the standard extension `TAU_LOG`
//! subscriber helpers. First-party extensions now use this crate directly; the
//! old compatibility startup helper crate has been removed after the migration
//! completed.
//!
//! Every runner sends `Hello`, requires the harness's initial `Configure`,
//! installs its immutable [`ToolNameScope`], and only then emits declarations
//! and `Ready`. Builder tool APIs accept logical/local names and scope
//! structural identifiers automatically. Raw [`ClientHandle::send`] and
//! [`ClientHandle::emit`] are wire-level APIs and never rewrite names.

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
mod protocol_io;
mod runner;
mod tool_name_scope;
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
    ManualRuntimeInput, ManualRuntimePoll, ManualRuntimeWaker,
};
pub use protocol_io::{
    PROTOCOL_IO_MAX_KEYS_PER_DIRECTION, PROTOCOL_IO_OVERFLOW_KEY, ProtocolIoCumulativeStats,
    ProtocolIoDirection, ProtocolIoFrameStats, ProtocolIoMeter, ProtocolIoRollingStats,
    ProtocolIoSample, ProtocolIoTracker, format_protocol_io_breakdown, format_protocol_io_bytes,
    format_protocol_io_cumulative_stats, harness_input_message_name, input_message_key,
    output_message_key, sorted_protocol_io_frame_stats, total_protocol_io_frame_stats,
};
pub use runner::TauExtensionRunner;
pub use tool_name_scope::ToolNameScope;

#[cfg(test)]
mod tests;
