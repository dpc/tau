//! Persistent bounded FIFO handoff for post-ACK Slack admission.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

/// Maximum number of accepted Slack occurrences awaiting a terminal outcome.
pub(crate) const CAPACITY: usize = 64;

/// One persistent bounded FIFO shared by the websocket reader and serial
/// worker.
pub(crate) struct AdmissionQueue<T> {
    /// Protected queue state, including reservations not yet committed.
    state: Mutex<QueueState<T>>,
    /// Wakes the serial worker when committed work or closure becomes
    /// available.
    ready: Condvar,
}

/// Mutable queue state protected by [`AdmissionQueue::state`].
struct QueueState<T> {
    /// Successfully ACKed occurrences in global arrival order.
    queued: VecDeque<T>,
    /// Slots reserved before their websocket ACK completes.
    outstanding: usize,
    /// Whether new reservations are permanently disabled.
    closed: bool,
}

impl<T> AdmissionQueue<T> {
    /// Create an open, empty admission queue.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                queued: VecDeque::new(),
                outstanding: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        })
    }

    /// Reserve one of the 64 outstanding slots before acknowledging an
    /// envelope.
    pub(crate) fn reserve(self: &Arc<Self>) -> Result<Reservation<T>, ReserveError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(ReserveError::Closed);
        }
        if state.outstanding >= CAPACITY {
            return Err(ReserveError::Full);
        }
        state.outstanding += 1;
        Ok(Reservation {
            queue: Arc::clone(self),
            active: true,
        })
    }

    /// Wait for and remove the oldest committed occurrence.
    ///
    /// Closure drains already-ACKed work before returning `None`.
    pub(crate) fn pop(self: &Arc<Self>) -> Option<(T, OutstandingPermit<T>)> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(item) = state.queued.pop_front() {
                return Some((
                    item,
                    OutstandingPermit {
                        queue: Arc::clone(self),
                    },
                ));
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    /// Permanently stop new reservations and wake the serial worker.
    pub(crate) fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        self.ready.notify_all();
    }

    /// Return a stable low-cardinality depth bucket.
    pub(crate) fn depth_bucket(&self) -> QueueDepthBucket {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        depth_bucket(state.outstanding)
    }

    /// Reserve one outstanding permit without queueing work for deterministic
    /// canonical-confirmation tests.
    #[cfg(test)]
    pub(crate) fn retain_test_permit(self: &Arc<Self>) -> OutstandingPermit<T> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        assert!(state.outstanding < CAPACITY, "test permit capacity");
        state.outstanding += 1;
        OutstandingPermit {
            queue: Arc::clone(self),
        }
    }
}

/// A slot acquired before ACK and committed only after local ACK write success.
pub(crate) struct Reservation<T> {
    /// Queue that owns this outstanding slot.
    queue: Arc<AdmissionQueue<T>>,
    /// Whether dropping still needs to release the reservation.
    active: bool,
}

impl<T> Reservation<T> {
    /// Atomically convert this reserved slot into committed FIFO work.
    pub(crate) fn commit(mut self, item: T) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.queued.push_back(item);
        self.active = false;
        drop(state);
        self.queue.ready.notify_one();
    }
}

impl<T> Drop for Reservation<T> {
    /// Release an uncommitted slot after ACK failure or reader cancellation.
    fn drop(&mut self) {
        if self.active {
            let mut state = self
                .queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            debug_assert!(state.outstanding > 0);
            state.outstanding -= 1;
        }
    }
}

/// One dequeued occurrence that retains capacity until its terminal outcome.
pub(crate) struct OutstandingPermit<T> {
    /// Queue whose outstanding permit remains held.
    queue: Arc<AdmissionQueue<T>>,
}

impl<T> Drop for OutstandingPermit<T> {
    /// Release outstanding capacity after processing reaches a terminal
    /// outcome.
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        debug_assert!(state.outstanding > 0);
        state.outstanding -= 1;
    }
}

/// Bounded reasons a pre-ACK reservation can fail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReserveError {
    /// All 64 accepted-but-not-applied slots are occupied.
    Full,
    /// The persistent serial worker is shutting down.
    Closed,
}

/// Map an exact outstanding count to the approved bounded trace buckets.
fn depth_bucket(depth: usize) -> QueueDepthBucket {
    match depth {
        0 => QueueDepthBucket::Empty,
        1 => QueueDepthBucket::One,
        2..=3 => QueueDepthBucket::TwoThree,
        4..=7 => QueueDepthBucket::FourSeven,
        8..=15 => QueueDepthBucket::EightFifteen,
        16..=31 => QueueDepthBucket::SixteenThirtyOne,
        32..=63 => QueueDepthBucket::ThirtyTwoSixtyThree,
        _ => QueueDepthBucket::Full,
    }
}

/// Stable bounded queue-depth classes for payload-free TRACE records.
#[derive(Clone, Copy)]
pub(crate) enum QueueDepthBucket {
    /// No outstanding occurrences.
    Empty,
    /// One outstanding occurrence.
    One,
    /// Two or three outstanding occurrences.
    TwoThree,
    /// Four through seven outstanding occurrences.
    FourSeven,
    /// Eight through fifteen outstanding occurrences.
    EightFifteen,
    /// Sixteen through thirty-one outstanding occurrences.
    SixteenThirtyOne,
    /// Thirty-two through sixty-three outstanding occurrences.
    ThirtyTwoSixtyThree,
    /// All 64 slots are occupied.
    Full,
}

impl QueueDepthBucket {
    /// Return the approved stable trace spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "0",
            Self::One => "1",
            Self::TwoThree => "2_3",
            Self::FourSeven => "4_7",
            Self::EightFifteen => "8_15",
            Self::SixteenThirtyOne => "16_31",
            Self::ThirtyTwoSixtyThree => "32_63",
            Self::Full => "full",
        }
    }
}

#[cfg(test)]
mod tests;
