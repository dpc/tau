# ARCH-tau-blocking-notify-channel: tau-blocking-notify-channel architecture

`tau-blocking-notify-channel` is a local, in-process synchronization primitive.
It does not cross filesystem, network, subprocess, persistence, credential, or
other security boundaries.

The reliability contract is the security-sensitive part of this crate:

- The channel is a coalesced one-bit multi-producer, single-consumer wakeup.
- Multiple notifications before a receive collapse into one pending notification.
- A pending notification is delivered before disconnect is reported.
- Timed receives use one exact deadline and still observe a notification pending
  at that deadline before reporting timeout.
- A duration beyond the platform's representable monotonic deadline is an
  interruptible unbounded wait rather than an input-triggered panic.
- Dropping the receiver is not observable by senders; later notifications return normally.
- `Receiver` must remain `Send` but intentionally not `Sync`.

Future changes to wakeup, coalescing, disconnect, receiver-drop, or auto-trait
semantics must update this file, `README.md`, rustdoc, and tests together.
Keep focused tests for blocking wakeups, coalescing, disconnect priority,
`Receiver: Send + !Sync`, and notification after receiver drop.
