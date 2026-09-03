use std::collections::VecDeque;
use std::sync::Mutex;

use crate::output_cost::AdmissionObservation;
use crate::peer_output::measure_message;
use crate::{ClientError, ClientResult, PeerOutput};

/// Maximum detached frames retained for the protocol writer.
pub(crate) const MAX_FRAMES: usize = 64;

/// Maximum encoded size accepted for one complete outbound protocol frame.
pub const MAX_OUTBOUND_FRAME_BYTES: u64 = 8 * 1024 * 1024;

/// Measure the complete encoded peer-to-harness protocol frame.
///
/// Prefer [`PeerOutput::prepare`] when the caller will subsequently submit the
/// same message because it carries this measurement into client admission.
pub fn encoded_outbound_frame_bytes(message: &tau_proto::HarnessInputMessage) -> ClientResult<u64> {
    measure_message(message)
}

/// Maximum aggregate encoded bytes retained for the protocol writer.
const MAX_ENCODED_BYTES: EncodedBytes = EncodedBytes {
    bytes: MAX_OUTBOUND_FRAME_BYTES,
};

/// Carried encoded protocol-frame size used for admission accounting.
#[derive(Clone, Copy, Default)]
pub(crate) struct EncodedBytes {
    /// Number of bytes produced by protocol encoding.
    bytes: u64,
}

impl EncodedBytes {
    /// Extracts the measurement already carried by one prepared output.
    pub(crate) fn from_output(output: &PeerOutput) -> Self {
        Self {
            bytes: output.encoded_bytes(),
        }
    }

    /// Returns whether this one frame exceeds the individual byte limit.
    pub(crate) fn exceeds_frame_limit(self) -> bool {
        MAX_ENCODED_BYTES.bytes < self.bytes
    }

    /// Adds one encoded size while detecting arithmetic overflow.
    fn checked_add(self, other: Self) -> Option<Self> {
        self.bytes
            .checked_add(other.bytes)
            .map(|bytes| Self { bytes })
    }

    /// Removes one queued frame's size from aggregate accounting.
    fn remove(&mut self, other: Self) {
        self.bytes -= other.bytes;
    }
}

/// One measured frame retained in the detached-output FIFO.
pub(crate) struct QueuedFrame {
    /// Protocol frame retained until transport writing drains it.
    output: PeerOutput,
    /// Encoded size charged to the shared byte budget.
    encoded_bytes: EncodedBytes,
}

impl QueuedFrame {
    /// Checks and wraps one prepared frame for detached FIFO admission.
    pub(crate) fn admit(output: PeerOutput) -> ClientResult<Self> {
        let encoded_bytes = EncodedBytes::from_output(&output);
        if encoded_bytes.exceeds_frame_limit() {
            return Err(ClientError::Overloaded);
        }
        Ok(Self {
            output,
            encoded_bytes,
        })
    }

    /// Returns the measured frame's encoded size.
    fn encoded_bytes(&self) -> EncodedBytes {
        self.encoded_bytes
    }

    /// Releases the protocol message for transport writing.
    fn into_output(self) -> PeerOutput {
        self.output
    }
}

/// Shared bounded FIFO for nonblocking detached output.
pub(crate) struct DetachedOutput {
    /// Mutable admission and lifecycle state.
    state: Mutex<State>,
}

/// Mutable detached-output state protected by [`DetachedOutput::state`].
struct State {
    /// Accepted frames in admission order.
    queue: VecDeque<QueuedFrame>,
    /// Aggregate encoded size of queued frames.
    encoded_bytes: EncodedBytes,
    /// Admission and drain phase of the detached FIFO.
    lifecycle: DetachedLifecycle,
}

/// Process-local admission and drain phase of the detached FIFO.
#[derive(Clone, Copy)]
enum DetachedLifecycle {
    /// Admission is open, but startup still withholds draining.
    Inactive,
    /// Admission is open and the writer may drain accepted frames.
    Active,
    /// Admission is closed and the writer may perform the final drain.
    Closed,
}

impl DetachedLifecycle {
    /// Returns whether admission has terminated.
    fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Enables draining while preserving terminal closure.
    fn activate(&mut self) -> ClientResult<()> {
        match self {
            Self::Inactive | Self::Active => {
                *self = Self::Active;
                Ok(())
            }
            Self::Closed => Err(ClientError::WriterClosed),
        }
    }

    /// Terminates admission and enables final draining.
    fn close(&mut self) {
        *self = Self::Closed;
    }

    /// Returns whether the writer may drain accepted frames.
    fn can_drain(self) -> bool {
        matches!(self, Self::Active | Self::Closed)
    }
}

impl DetachedOutput {
    /// Creates an inactive queue that retains output until startup terminates.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                encoded_bytes: EncodedBytes::default(),
                lifecycle: DetachedLifecycle::Inactive,
            }),
        }
    }

    /// Admits one measured detached frame without blocking on transport
    /// progress.
    pub(crate) fn admit(
        &self,
        frame: QueuedFrame,
        observation: Option<AdmissionObservation>,
    ) -> ClientResult<()> {
        let mut state = self.state.lock().expect("lock detached output");
        if state.lifecycle.is_closed() {
            if let Some(observation) = observation {
                observation.rejected("writer_closed");
            }
            return Err(ClientError::WriterClosed);
        }
        let Some(total_bytes) = state.encoded_bytes.checked_add(frame.encoded_bytes()) else {
            if let Some(observation) = observation {
                observation.rejected("detached_overloaded");
            }
            return Err(ClientError::Overloaded);
        };
        if MAX_FRAMES <= state.queue.len() || MAX_ENCODED_BYTES.bytes < total_bytes.bytes {
            if let Some(observation) = observation {
                observation.rejected("detached_overloaded");
            }
            return Err(ClientError::Overloaded);
        }
        state.queue.push_back(frame);
        state.encoded_bytes = total_bytes;
        if let Some(observation) = observation {
            observation.admitted();
        }
        Ok(())
    }

    /// Enables ordered draining after `Ready` or a pre-Ready `ConfigError`.
    pub(crate) fn activate(&self) -> ClientResult<()> {
        let mut state = self.state.lock().expect("lock detached output");
        state.lifecycle.activate()
    }

    /// Closes admission and enables final draining of every accepted frame.
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().expect("lock detached output");
        state.lifecycle.close();
    }

    /// Captures the active queue length for one fair writer batch.
    pub(crate) fn active_batch_len(&self) -> usize {
        let state = self.state.lock().expect("lock detached output");
        if state.lifecycle.can_drain() {
            state.queue.len()
        } else {
            0
        }
    }

    /// Removes the next active frame and releases its queue budget.
    pub(crate) fn pop(&self) -> Option<PeerOutput> {
        let mut state = self.state.lock().expect("lock detached output");
        if !state.lifecycle.can_drain() {
            return None;
        }
        let frame = state.queue.pop_front()?;
        state.encoded_bytes.remove(frame.encoded_bytes());
        Some(frame.into_output())
    }
}
