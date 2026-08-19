//! Shared provider management and runtime utilities.
//!
//! This crate exposes small dependency-light helpers shared by provider
//! backends. Streaming providers can use [`StreamRepetitionGuard`] to abort
//! high-confidence tight exact output loops before they become durable
//! assistant output.

pub mod debug_capture_writer;
pub mod local_summary_compaction;
pub mod outbound_network;
pub mod repetition_guard;
pub mod retry_policy;

pub use outbound_network::{
    OutboundError, OutboundErrorKind, OutboundNetworkPolicy, OutboundPhase, OutboundRouteKind,
};
pub use repetition_guard::{
    RepetitionMode, StreamRepetition, StreamRepetitionGuard, StreamRepetitionKey,
};
