//! Typed Grok credentials and one-shot rotation through caller-owned storage.
//!
//! This module never opens Secret files. Callers supply bounded authoritative
//! read/CAS operations and retain a worker until rotation publication
//! completes, even after its inference waiter is canceled.

mod record;
mod rotation;

pub use record::Credential;
pub use rotation::{Error, RefreshExchange, RefreshReason, Storage, StorageError, resolve};

#[cfg(test)]
mod tests;
