use std::collections::VecDeque;
use std::sync::Mutex;

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
    /// Whether the writer may drain accepted frames.
    active: bool,
    /// Whether admission has terminated.
    closed: bool,
}

impl DetachedOutput {
    /// Creates an inactive queue that retains output until startup terminates.
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                encoded_bytes: EncodedBytes::default(),
                active: false,
                closed: false,
            }),
        }
    }

    /// Admits one measured detached frame without blocking on transport
    /// progress.
    pub(crate) fn admit(&self, frame: QueuedFrame) -> ClientResult<()> {
        let mut state = self.state.lock().expect("lock detached output");
        if state.closed {
            return Err(ClientError::WriterClosed);
        }
        let Some(total_bytes) = state.encoded_bytes.checked_add(frame.encoded_bytes()) else {
            return Err(ClientError::Overloaded);
        };
        if MAX_FRAMES <= state.queue.len() || MAX_ENCODED_BYTES.bytes < total_bytes.bytes {
            return Err(ClientError::Overloaded);
        }
        state.queue.push_back(frame);
        state.encoded_bytes = total_bytes;
        Ok(())
    }

    /// Enables ordered draining after `Ready` or a pre-Ready `ConfigError`.
    pub(crate) fn activate(&self) -> ClientResult<()> {
        let mut state = self.state.lock().expect("lock detached output");
        if state.closed {
            return Err(ClientError::WriterClosed);
        }
        state.active = true;
        Ok(())
    }

    /// Closes admission and enables final draining of every accepted frame.
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().expect("lock detached output");
        state.closed = true;
        state.active = true;
    }

    /// Captures the active queue length for one fair writer batch.
    pub(crate) fn active_batch_len(&self) -> usize {
        let state = self.state.lock().expect("lock detached output");
        if state.active { state.queue.len() } else { 0 }
    }

    /// Removes the next active frame and releases its queue budget.
    pub(crate) fn pop(&self) -> Option<PeerOutput> {
        let mut state = self.state.lock().expect("lock detached output");
        if !state.active {
            return None;
        }
        let frame = state.queue.pop_front()?;
        state.encoded_bytes.remove(frame.encoded_bytes());
        Some(frame.into_output())
    }
}
