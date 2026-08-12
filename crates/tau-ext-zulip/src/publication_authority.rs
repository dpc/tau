//! Lifecycle ordering for canonical report publication.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Serializes canonical publication against authority retirement.
pub(crate) struct PublicationAuthority(RwLock<()>);

/// Guard proving one ingress transaction retains publication authority.
pub(crate) struct PublicationGuard<'a> {
    /// Held shared lock; dropping it releases publication authority.
    _guard: RwLockReadGuard<'a, ()>,
}

/// Guard proving one lifecycle operation exclusively owns authority retirement.
pub(crate) struct RetirementGuard<'a> {
    /// Held exclusive lock; dropping it completes authority retirement.
    _guard: RwLockWriteGuard<'a, ()>,
}

impl PublicationAuthority {
    /// Create an unowned authority gate.
    pub(crate) fn new() -> Self {
        Self(RwLock::new(()))
    }

    /// Retain authority through an ingress transaction.
    pub(crate) fn publish(&self) -> PublicationGuard<'_> {
        PublicationGuard {
            _guard: self.0.read().unwrap_or_else(|error| error.into_inner()),
        }
    }

    /// Exclude canonical publication while retiring authority.
    pub(crate) fn retire(&self) -> RetirementGuard<'_> {
        RetirementGuard {
            _guard: self.0.write().unwrap_or_else(|error| error.into_inner()),
        }
    }
}
