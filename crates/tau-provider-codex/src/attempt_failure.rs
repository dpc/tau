//! Bounded diagnostics for one finite Codex inference attempt.
//!
//! This facade exposes typed correlation and opaque redacted output. Cohesive
//! submodules separately own correlation, evidence, redaction, and capture.

mod capture;
mod correlation;
mod evidence;
mod redacted_detail;
mod shape;

#[cfg(test)]
use capture::{BoundedRecord, serialize_bounded_record, submit_capture_with, validated_identifier};
pub(crate) use capture::{CaptureInput, submit_capture};
pub use correlation::LogicalAttempt;
pub(crate) use correlation::{
    AttemptCaptureCorrelation, AttemptCaptureSnapshot, DispatchCorrelation,
};
pub(crate) use evidence::{
    AttemptFailureEvidence, FrameFailure, FrameFailureKind, ProviderEvidenceMode,
    TransportFailureKind, TransportPhase, WsTermination,
};
pub use redacted_detail::RedactedProviderDetail;
#[cfg(test)]
#[path = "attempt_failure_tests.rs"]
mod tests;
