//! Closed, payload-free classes used by private Slack admission latency traces.

use crate::{SlackConnectionGeneration, SlackTraceSequence};

/// Payload-free fields shared by one occurrence's latency markers.
#[derive(Clone, Copy)]
pub(super) struct LatencyTrace {
    /// Socket generation local to this extension process.
    pub(super) connection_generation: SlackConnectionGeneration,
    /// Occurrence ordinal local to this extension process.
    pub(super) trace_seq: SlackTraceSequence,
    /// Stable low-cardinality decoded event class.
    pub(super) event_class: EventClass,
}

/// Closed terminal classes emitted by private admission latency traces.
#[derive(Clone, Copy)]
pub(super) enum AdmissionOutcome {
    /// Work reached a stale lifecycle or configuration path.
    StaleEpoch,
    /// Work reached an identity-rejection path.
    RejectedIdentity,
    /// Work reached duplicate-ingress or stale local-effect handling.
    DuplicateIngress,
    /// Work reached a route or writer rejection path.
    RejectedRoute,
    /// Work reached a policy, malformed-input, or worker-failure path.
    RejectedPolicy,
    /// Work reached an authorized extension-local effect attempt.
    LocalEffect,
    /// Work reached successful report submission.
    Submitted,
}

impl AdmissionOutcome {
    /// Return the approved privacy-safe trace spelling.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::StaleEpoch => "stale_epoch",
            Self::RejectedIdentity => "rejected_identity",
            Self::DuplicateIngress => "duplicate_ingress",
            Self::RejectedRoute => "rejected_route",
            Self::RejectedPolicy => "rejected_policy",
            Self::LocalEffect => "local_effect",
            Self::Submitted => "submitted",
        }
    }
}

/// Closed payload-free event classes emitted by private latency traces.
#[derive(Clone, Copy)]
pub(super) enum EventClass {
    /// The envelope could not be decoded.
    Malformed,
    /// No supported event has been classified at this trace stage.
    Unsupported,
    /// A message shaped as a bridge-local command.
    LocalCommand,
    /// A message create occurrence.
    Create,
    /// A reaction occurrence.
    Reaction,
    /// A message edit occurrence.
    Edit,
    /// A message deletion occurrence.
    Delete,
}

impl EventClass {
    /// Return the approved privacy-safe trace spelling.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::LocalCommand => "local_command",
            Self::Create => "create",
            Self::Reaction => "reaction",
            Self::Edit => "edit",
            Self::Delete => "delete",
        }
    }
}
