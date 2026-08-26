# tau-blocking-notify-channel

A small coalescing notification channel with multiple senders and one receiver.

The channel stores one notification bit. Calling `Sender::notify` sets that bit;
`Receiver::recv` blocks until the bit is set, then resets it. Multiple
notifications before a receive coalesce into a single wakeup, so bursty producers
do not build an unbounded queue.

When every sender is dropped, the channel becomes disconnected. A pending
notification is still delivered before `recv` or `try_recv` reports
`Disconnected`. Non-blocking receives use `TryRecvStatus::Notified` and
`TryRecvStatus::Empty` to describe the connected channel state.
`recv_timeout` waits until an exact relative deadline while preserving the same
notification-before-disconnect priority. Durations beyond the platform
`Instant` range behave as interruptible unbounded waits instead of panicking.

The receiving half may be moved to another thread, but it is intentionally not
`Sync`: clone `Sender` for multiple producers rather than sharing one receiver
between concurrent consumers. Dropping the receiver is not observable by senders;
later `notify` calls still set the coalesced bit and return normally.

## Why this exists

Tau uses this primitive for wakeups where only “something changed” matters, such
as terminal redraw notifications in `tau-cli-term-raw`. A standard
`std::sync::mpsc::channel::<()>()` would require draining queued wakeups to
preserve coalescing and could grow under burst load.

## Example

```rust
use tau_blocking_notify_channel::TryRecvStatus;

let (tx, rx) = tau_blocking_notify_channel::channel();

tx.notify();
assert_eq!(rx.recv(), Ok(()));
assert_eq!(rx.try_recv(), Ok(TryRecvStatus::Empty));

drop(tx);
assert!(rx.recv().is_err());
```
