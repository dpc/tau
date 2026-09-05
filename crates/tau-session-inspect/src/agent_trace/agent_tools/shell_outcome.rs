//! Structured shell outcomes projected from canonical terminal CBOR.

pub(super) use tau_proto::ShellProcessOutcome as ShellOutcome;
#[cfg(test)]
use tau_proto::{
    CborValue, Event, ShellProcessOutcomeSource as ShellOutcomeSource, ShellTerminationReason,
};

#[cfg(test)]
mod tests;
