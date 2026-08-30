use std::io::{self, Write};

use crate::ClientResult;

#[cfg(test)]
thread_local! {
    /// Number of counting-encoder traversals on this test thread.
    static MEASUREMENT_TRAVERSALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One owned peer-to-harness message with its exact encoded frame size.
///
/// Preparing an output traverses the protocol value once without retaining
/// serialized bytes. Callers can carry the result across local scheduling and
/// admission boundaries while the client writer retains typed message
/// ownership and performs the eventual transport encoding.
pub struct PeerOutput {
    /// Typed protocol message retained for transport serialization.
    message: tau_proto::HarnessInputMessage,
    /// Exact byte count produced by the current protocol encoder.
    encoded_bytes: u64,
}

impl PeerOutput {
    /// Prepare one typed message and measure its complete encoded frame.
    ///
    /// # Errors
    ///
    /// Returns an error when protocol encoding or byte accounting fails.
    pub fn prepare(message: tau_proto::HarnessInputMessage) -> ClientResult<Self> {
        let encoded_bytes = measure_message(&message)?;
        Ok(Self {
            message,
            encoded_bytes,
        })
    }

    /// Return the exact encoded frame size measured during preparation.
    #[must_use]
    pub fn encoded_bytes(&self) -> u64 {
        self.encoded_bytes
    }

    /// Borrow the typed protocol message.
    #[must_use]
    pub fn message(&self) -> &tau_proto::HarnessInputMessage {
        &self.message
    }
}

/// Measure one borrowed typed message without retaining serialized bytes.
pub(crate) fn measure_message(message: &tau_proto::HarnessInputMessage) -> ClientResult<u64> {
    #[cfg(test)]
    MEASUREMENT_TRAVERSALS.set(MEASUREMENT_TRAVERSALS.get() + 1);
    let mut counter = CountingWriter::default();
    tau_proto::PeerOutputWriter::new(&mut counter).write_message(message)?;
    Ok(counter.bytes)
}

#[cfg(test)]
pub(crate) fn measurement_traversals() -> u64 {
    MEASUREMENT_TRAVERSALS.get()
}

/// Discard-only writer that counts serialized protocol bytes.
#[derive(Default)]
struct CountingWriter {
    /// Total bytes accepted by [`Write::write`].
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let bytes = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("encoded frame size exceeds u64"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("encoded frame size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
