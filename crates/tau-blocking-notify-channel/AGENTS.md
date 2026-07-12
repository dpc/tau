Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-blocking-notify-channel agent notes

Before changing this crate, read:

- `README.md` for the public channel contract and examples.
- the applicable trust-boundary records under `specs/` for reliability-sensitive synchronization invariants.

Preserve the coalesced one-bit MPSC semantics, pending-notification-before-disconnect
ordering, receiver-drop behavior, and `Receiver: Send + !Sync` auto-trait contract.
Update docs and focused tests whenever those semantics change.
