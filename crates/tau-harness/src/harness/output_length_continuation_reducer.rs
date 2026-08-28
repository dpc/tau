//! Typed post-commit reducer state for output-length continuations.

/// Exact state used to reduce one committed response in an output-length
/// continuation lineage.
#[derive(Clone)]
pub(crate) struct CommittedOutputLengthContinuation {
    /// Exact response retained for common terminal handling.
    pub(super) response: Box<tau_proto::ProviderResponseFinished>,
    /// Display-only assistant text retained for common terminal handling.
    pub(super) assistant_text: Option<String>,
}
