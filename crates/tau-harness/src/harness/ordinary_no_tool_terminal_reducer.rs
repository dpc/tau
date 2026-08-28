//! Typed eager reducer state for ordinary provider no-tool terminals.

/// Exact ordinary no-tool terminal state retained across classification.
pub(crate) struct EagerOrdinaryNoToolTerminal {
    /// Canonical provider response offered for publication before reduction.
    pub(super) response: tau_proto::ProviderResponseFinished,
    /// Display-only assistant text used by the existing terminal projection.
    pub(super) assistant_text: Option<String>,
}
