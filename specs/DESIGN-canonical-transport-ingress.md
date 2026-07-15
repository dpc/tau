# DESIGN-canonical-transport-ingress: Canonical transport ingress authority

Status: confirmed, 2026-07-15, dpc

The harness owns transport-ingress deduplication across all retained durable
agent history. Its scope is authenticated extension instance, transport family,
and native dedup key. The first durable incoming envelope fixes the original
target, immutable authority, and presentation snapshot. A retry can neither move
the occurrence to another agent nor rewrite that snapshot.

The protocol returns a closed committed-or-rejected result. Committed results
carry the canonical message id, Accepted or Duplicate outcome, exact first route,
and typed reply activation. Active is granted only after durable commit to at
most one exact current connection/session-generation waiter with a still-live
target, capability, and reply tool. All other committed results are Inactive
with a bounded reason. `MessageId` remains the opaque selector; no second token
is introduced because completion revalidates live authority.

The durable envelope remains canonical. A harness-owned checksummed append-only
locator makes global misses bounded. All harness processes sharing an agents root
serialize a transaction lock from prospective count/byte reservation and a
parent-fsynced dirty marker through journal publication, locator append/head
commit, and marker removal. Ambiguous journal failure retains dirty state.
Missing, dirty, corrupt, or older locator state rebuilds once from every retained
journal; failures latch for the process. Runtime acceleration may evict committed
entries, but the global locator does not. Unreadable canonical history, ambiguous
ownership, a pruned referenced envelope, locator persistence failure, concurrent
reservation pressure, or prospective count/byte capacity fails closed. Retention,
import, or pruning must take the same transaction lock and rebuild atomically.

Every capability registration receives a monotonic process epoch. A waiter may
activate only if that exact epoch remains current at commit, preventing
revoke/re-register ABA.

Dedup equality uses an explicit authority projection. It includes original
target, stable endpoint id and kind, conversation kind/stable id/thread/reply,
operation and payload, trust and policy, native identity, ordering and
occurrence time, and send tool. It excludes only endpoint and conversation
presentation labels.

Adapters have no authority before commit. They may install private reply state
only after validating an Active canonical result against their pending stable
native occurrence and current adapter identity. Inactive, Rejected, orphaned,
or invalid canonical results install nothing.

This decision refines
[ARCH-external-message-boundary](ARCH-external-message-boundary.md) and is
implemented by
[ARCH-tau-harness](../crates/tau-harness/specs/ARCH-tau-harness.md) and
[ARCH-tau-proto](../crates/tau-proto/specs/ARCH-tau-proto.md).
