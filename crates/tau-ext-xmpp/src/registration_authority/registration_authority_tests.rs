use tau_proto::AgentId;

use super::{RegistrationAuthority, RegistrationLease};

/// Ensures the typed allocator retains the existing zero sentinel and allocates
/// its first externally usable lease as one.
#[test]
fn reserve_allocates_first_lease_after_zero_sentinel() {
    let authority = RegistrationAuthority::default();

    let lease = authority.reserve(agent_id("agent-1"));

    assert_eq!(lease, RegistrationLease(1));
}

/// Ensures lease allocation keeps failing rather than wrapping a process-local
/// authority after its final representable identity.
#[test]
#[should_panic(expected = "XMPP registration lease ordinal exhausted")]
fn reserve_rejects_lease_ordinal_exhaustion() {
    let authority = RegistrationAuthority::default();
    authority
        .state
        .lock()
        .expect("authority state lock")
        .next_lease = RegistrationLease(u64::MAX);

    let _ = authority.reserve(agent_id("agent-1"));
}

fn agent_id(value: &str) -> AgentId {
    value.parse().expect("known-safe test agent id")
}
