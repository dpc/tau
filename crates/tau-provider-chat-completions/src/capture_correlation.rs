//! Exact-capture correlation without changing capture eligibility or timing.

use serde_json::Value;
use tau_provider::cache_diagnostic::DiagnosticId;

/// An observed wire index and optional private finite-attempt identity.
#[derive(Clone, Copy)]
pub(crate) struct CaptureCorrelation {
    /// Actual dispatch observation, never a prospective request index.
    pub(crate) wire_dispatch_index: Option<u64>,
    /// Private identity available for persistable ordinary inference.
    pub(crate) attempt_id: Option<DiagnosticId>,
}

impl From<u64> for CaptureCorrelation {
    fn from(index: u64) -> Self {
        Self {
            wire_dispatch_index: Some(index),
            attempt_id: None,
        }
    }
}

impl CaptureCorrelation {
    /// Attach generated correlation after the existing payload projection.
    pub(crate) fn annotate(self, metadata: &mut Value) {
        metadata["wire_dispatch_index"] = self.wire_dispatch_index.into();
        if let Some(id) = self.attempt_id {
            metadata["attempt_id"] =
                serde_json::to_value(id).expect("diagnostic identity serializes");
        }
    }
}
