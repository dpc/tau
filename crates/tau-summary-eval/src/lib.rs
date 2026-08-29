//! Offline, deterministic scoring for synthetic summary-quality trials.
//!
//! This crate has no provider or network dependency. Live model output must be
//! generated through an independently and explicitly initiated provider
//! workflow, then imported as a candidate set with complete provenance and
//! opt-in metadata.

mod candidates;
mod corpus;
mod result_record;

pub use candidates::*;
pub use corpus::*;
pub use result_record::*;

#[cfg(test)]
mod tests;
