//! A coalescing notify channel with multiple senders and a single receiver.
//!
//! The shared state is conceptually a single `bool`. Senders set it to `true`;
//! the receiver blocks until it becomes `true`, then atomically resets it to
//! `false`. Multiple sends before a receive coalesce into one notification.
//!
//! When every `Sender` has been dropped the channel becomes *disconnected*.
//! A pending notification always takes priority over disconnection: the
//! receiver will see `Ok(())` first and only get `Err(Disconnected)` on the
//! next call. Dropping the receiver is not observable by senders; later
//! notifications still set the coalesced bit and return normally.
//! These reliability invariants are governed by
//! `ARCH-tau-blocking-notify-channel`.
//!
//! # Example
//!
//! ```rust
//! let (tx, rx) = tau_blocking_notify_channel::channel();
//!
//! tx.notify();
//! assert_eq!(rx.recv(), Ok(()));
//! assert_eq!(
//!     rx.try_recv(),
//!     Ok(tau_blocking_notify_channel::TryRecvStatus::Empty)
//! );
//!
//! drop(tx);
//! assert!(rx.recv().is_err());
//! ```
//!
//! # Why a custom primitive
//!
//! `std::sync::mpsc::channel::<()>()` would require the receiver to drain the
//! queue on each wakeup to preserve coalescing, and the queue would grow
//! unboundedly under burst load. A `parking_lot::Condvar` would remove the
//! poisoning boilerplate but is not currently a workspace dependency. This
//! crate's contract — single coalesced bit, multi-producer, blocking receive,
//! observable disconnect — is small enough that a direct `Mutex` + `Condvar`
//! implementation is the simplest fit.

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Creates a new notify channel, returning `(Sender, Receiver)`.
pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            notified: false,
            disconnected: false,
        }),
        condvar: Condvar::new(),
        sender_count: AtomicUsize::new(1),
    });
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver {
            shared,
            single_consumer: PhantomData,
        },
    )
}

/// Error returned by [`Receiver::recv`] and [`Receiver::try_recv`] when all
/// senders have been dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

impl std::fmt::Display for Disconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("channel disconnected")
    }
}

impl std::error::Error for Disconnected {}

/// Result status returned by [`Receiver::try_recv`] when the channel is still
/// connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvStatus {
    /// A notification was pending and has been consumed.
    Notified,
    /// No notification was pending.
    Empty,
}

struct State {
    // Whether a notification is currently pending for the receiver.
    notified: bool,
    // Whether all senders have been dropped.
    disconnected: bool,
}

// Synchronization invariants:
// - `State` fields are read and written only while holding `state`.
// - `sender_count` counts live `Sender` handles; it deliberately does not
//   include the receiver.
// - The last sender drop sets `disconnected` while holding `state` before
//   notifying the condition variable.
// - Receivers must check `notified` before `disconnected` so a pending
//   notification is delivered before disconnect.
// - Receiver drop is intentionally not stored in shared state; senders keep
//   accepting notifications after the receiver is gone.
struct Shared {
    // Protected notification/disconnection state.
    state: Mutex<State>,
    // Waits and wakes the single blocking receiver.
    condvar: Condvar,
    // Number of live sender handles, including clones.
    sender_count: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("notify channel mutex poisoned")
    }
}

/// Sending half of a notify channel. Cloneable for multiple producers.
pub struct Sender {
    // Shared channel state retained by each producer handle.
    shared: Arc<Shared>,
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);
        Sender {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if self.shared.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut state = self.shared.lock();
            state.disconnected = true;
            // Wake a parked `recv()` that hasn't yet observed disconnect.
            // Harmless if no waiter is parked.
            self.shared.condvar.notify_one();
        }
    }
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

impl Sender {
    /// Signal the receiver. If the flag is already set, this is a no-op
    /// (coalescing).
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the channel mutex.
    ///
    /// Dropping the receiver is not observable here; `notify` still sets the
    /// coalesced bit and returns normally.
    pub fn notify(&self) {
        let mut state = self.shared.lock();
        if state.notified {
            return;
        }
        state.notified = true;
        self.shared.condvar.notify_one();
    }
}

/// Receiving half of a notify channel. Not cloneable — single consumer.
///
/// `Receiver` is intentionally [`Send`] but not [`Sync`], so safe code cannot
/// call `recv` concurrently through shared references such as `Arc<Receiver>`
/// without adding its own synchronization.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use std::thread;
///
/// let (_tx, rx) = tau_blocking_notify_channel::channel();
/// let rx = Arc::new(rx);
/// let worker_rx = Arc::clone(&rx);
/// thread::spawn(move || worker_rx.recv()).join().unwrap();
/// ```
pub struct Receiver {
    // Shared channel state retained by the single consumer handle.
    shared: Arc<Shared>,
    // This marker shapes auto-traits: `Cell<()>` is `Send` but not `Sync`, so
    // `Receiver` remains movable to another thread while preventing shared
    // references from being used concurrently without external synchronization.
    single_consumer: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

impl Receiver {
    /// Block until the flag is `true`, then atomically reset it to `false`.
    ///
    /// Returns `Err(Disconnected)` when all senders have been dropped **and**
    /// no pending notification remains.
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the channel mutex.
    pub fn recv(&self) -> Result<(), Disconnected> {
        let mut state = self.shared.lock();
        loop {
            if state.notified {
                state.notified = false;
                return Ok(());
            }
            if state.disconnected {
                return Err(Disconnected);
            }
            state = self
                .shared
                .condvar
                .wait(state)
                .expect("notify channel mutex poisoned");
        }
    }

    /// Attempts to receive a pending notification without blocking.
    ///
    /// Returns [`TryRecvStatus::Notified`] if a notification was pending (and
    /// resets it), [`TryRecvStatus::Empty`] if nothing was pending, or
    /// `Err(Disconnected)` when all senders have been dropped and no
    /// notification remains.
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the channel mutex.
    #[must_use = "discarding the result drops a pending notification"]
    pub fn try_recv(&self) -> Result<TryRecvStatus, Disconnected> {
        let mut state = self.shared.lock();
        if state.notified {
            state.notified = false;
            return Ok(TryRecvStatus::Notified);
        }
        if state.disconnected {
            return Err(Disconnected);
        }
        Ok(TryRecvStatus::Empty)
    }
}
