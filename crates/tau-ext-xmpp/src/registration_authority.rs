//! Process-local generation-bound XMPP registration authority.

use std::collections::HashMap;
use std::sync::Mutex;

use tau_proto::AgentId;

/// Opaque process-local identity for one agent registration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RegistrationLease(u64);

impl RegistrationLease {
    /// Advances this lease with the allocator's checked exhaustion behavior.
    #[must_use]
    fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Whether a current lease can publish inbound messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseState {
    /// Remote setup has not completed.
    Pending,
    /// Remote setup completed and reader-side registration is installed.
    Active,
}

/// Mutable authority state protected by one lock.
#[derive(Default)]
struct AuthorityState {
    /// Last process-local lease ordinal allocated.
    next_lease: RegistrationLease,
    /// Current lease and publication state per agent.
    current: HashMap<AgentId, (RegistrationLease, LeaseState)>,
}

/// Shared registration authority used by reader lifecycle and worker routing.
#[derive(Default)]
pub(super) struct RegistrationAuthority {
    /// Current registrations and the monotonic lease allocator.
    state: Mutex<AuthorityState>,
}

impl RegistrationAuthority {
    /// Reserve a fresh non-routable lease, superseding any previous generation.
    pub(super) fn reserve(&self, agent_id: AgentId) -> RegistrationLease {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.next_lease = state
            .next_lease
            .checked_next()
            .expect("XMPP registration lease ordinal exhausted");
        let lease = state.next_lease;
        state.current.insert(agent_id, (lease, LeaseState::Pending));
        lease
    }

    /// Activate the exact current pending lease.
    pub(super) fn activate(&self, agent_id: &AgentId, lease: RegistrationLease) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some((current, lease_state)) = state.current.get_mut(agent_id) else {
            return false;
        };
        if *current != lease {
            return false;
        }
        *lease_state = LeaseState::Active;
        true
    }

    /// Revoke an exact lease without disturbing a newer generation.
    pub(super) fn revoke(&self, agent_id: &AgentId, lease: RegistrationLease) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .current
            .get(agent_id)
            .is_some_and(|(current, _)| *current == lease)
        {
            state.current.remove(agent_id);
            true
        } else {
            false
        }
    }

    /// Revoke and return one agent's exact current lease.
    pub(super) fn revoke_current(&self, agent_id: &AgentId) -> Option<RegistrationLease> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current
            .remove(agent_id)
            .map(|(lease, _)| lease)
    }

    /// Revoke and return every current lease.
    pub(super) fn revoke_all(&self) -> Vec<(AgentId, RegistrationLease)> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current
            .drain()
            .map(|(agent_id, (lease, _))| (agent_id, lease))
            .collect()
    }

    /// Run publication only while the exact lease remains current and active.
    pub(super) fn publish_if_active<T>(
        &self,
        agent_id: &AgentId,
        lease: RegistrationLease,
        publish: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state
            .current
            .get(agent_id)
            .is_some_and(|(current, status)| *current == lease && *status == LeaseState::Active)
        {
            return None;
        }
        Some(publish())
    }
}

#[cfg(test)]
mod registration_authority_tests;
