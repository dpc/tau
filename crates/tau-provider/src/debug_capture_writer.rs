//! Bounded best-effort transport for compressed provider debug captures.

use std::thread as path_std_thread;

#[cfg(test)]
mod tests;
use std::io;
use std::sync::{OnceLock, mpsc};

pub use tau_proto::ProviderDebugCaptureClass;
/// Maximum number of captures waiting for background compression and writing.
const CAPTURE_QUEUE_CAPACITY: usize = 64;
/// Compression level used for private diagnostic captures.
const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// One typed serialized provider capture awaiting best-effort admission.
pub struct ProviderDebugCapture {
    /// Validated durable-session identity.
    session_id: tau_proto::SessionId,
    /// Closed prompt or private operation attribution retained in the basename.
    attribution: tau_proto::ProviderCaptureAttribution,
    /// Valid transport/direction class.
    class: ProviderDebugCaptureClass,
    /// Uncompressed serialized JSON metadata.
    json: Vec<u8>,
}

impl ProviderDebugCapture {
    /// Construct one capture after a backend serializes its private metadata.
    #[must_use]
    pub fn new(
        session_id: tau_proto::SessionId,
        agent_prompt_id: tau_proto::AgentPromptId,
        class: ProviderDebugCaptureClass,
        json: Vec<u8>,
    ) -> Self {
        Self {
            session_id,
            attribution: tau_proto::ProviderCaptureAttribution::Prompt(agent_prompt_id),
            class,
            json,
        }
    }

    /// Return the validated session identity.
    #[must_use]
    pub fn session_id(&self) -> &tau_proto::SessionId {
        &self.session_id
    }

    /// Construct a scalar operation capture without inventing a prompt
    /// identity.
    pub fn cache_operation(
        session_id: tau_proto::SessionId,
        operation_id: tau_proto::CacheOperationId,
        json: Vec<u8>,
    ) -> Self {
        Self {
            session_id,
            attribution: tau_proto::ProviderCaptureAttribution::CacheOperation(operation_id),
            class: ProviderDebugCaptureClass::CacheDiagnostic,
            json,
        }
    }

    /// Return existing prompt attribution; operation captures have none.
    #[must_use]
    pub fn agent_prompt_id(&self) -> Option<&tau_proto::AgentPromptId> {
        match &self.attribution {
            tau_proto::ProviderCaptureAttribution::Prompt(id) => Some(id),
            tau_proto::ProviderCaptureAttribution::CacheOperation(_) => None,
        }
    }

    /// Return the valid transport/direction class.
    #[must_use]
    pub fn class(&self) -> ProviderDebugCaptureClass {
        self.class
    }

    /// Return the uncompressed serialized JSON metadata.
    #[must_use]
    pub fn json(&self) -> &[u8] {
        &self.json
    }
}

/// One already-serialized provider capture awaiting background persistence.
struct CaptureJob {
    /// Capture metadata and uncompressed JSON accepted by the bounded queue.
    capture: ProviderDebugCapture,
    /// Metadata-only admission budget, retained through the in-flight send.
    metadata_reservation: Option<crate::cache_diagnostic::Reservation>,
}

impl CaptureJob {
    /// Build one capture transport job.
    fn new(capture: ProviderDebugCapture) -> Self {
        Self {
            capture,
            metadata_reservation: None,
        }
    }
}

/// Nonblocking producer handle for the process-wide capture worker.
struct CaptureQueue {
    /// Bounded FIFO sender; admission always uses `try_send`.
    sender: mpsc::SyncSender<CaptureJob>,
}

/// Single transport queue owned by one supervised Provider process.
static CAPTURE_QUEUE: OnceLock<CaptureQueue> = OnceLock::new();

impl CaptureQueue {
    /// Build a queue around one bounded sender.
    fn with_sender(sender: mpsc::SyncSender<CaptureJob>) -> Self {
        Self { sender }
    }

    /// Start one detached worker that sends compressed protocol messages.
    fn spawn(handle: tau_client::ClientHandle) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(CAPTURE_QUEUE_CAPACITY);
        let queue = Self::with_sender(sender);
        path_std_thread::Builder::new()
            .name("tau-provider-capture".to_owned())
            .spawn(move || {
                run_worker(receiver, |job| send_capture(&handle, job));
            })
            .map(|_| queue)
    }

    /// Admit a job immediately or return it when the queue is
    /// full/disconnected.
    fn try_submit(&self, job: CaptureJob) -> Result<(), mpsc::TrySendError<CaptureJob>> {
        self.sender.try_send(job)
    }
}

/// Submit a provider capture without waiting for worker capacity or I/O.
fn submit(job: CaptureJob) {
    let Some(queue) = CAPTURE_QUEUE.get() else {
        tracing::warn!(
            target: "tau_provider::debug_capture_writer",
            "provider debug capture transport is unavailable; dropping capture"
        );
        return;
    };
    match queue.try_submit(job) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(job)) => {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                attribution = ?job.capture.attribution,
                "provider debug capture queue is full; dropping capture"
            );
        }
        Err(mpsc::TrySendError::Disconnected(job)) => {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                attribution = ?job.capture.attribution,
                "provider debug capture worker stopped; dropping capture"
            );
        }
    }
}

/// Idempotently attempt to install the process-wide Provider capture transport.
///
/// A supervised Provider process owns exactly one harness connection. Repeated
/// calls leave the first transport installed. Worker startup failure is logged
/// and leaves captures unavailable without returning an error to the caller.
pub fn initialize_provider_debug_capture_transport(handle: tau_client::ClientHandle) {
    if CAPTURE_QUEUE.get().is_some() {
        return;
    }
    let Some(queue) = start_transport_with(|| CaptureQueue::spawn(handle)) else {
        return;
    };
    if CAPTURE_QUEUE.set(queue).is_err() {
        tracing::warn!(
            target: "tau_provider::debug_capture_writer",
            "provider debug capture transport was initialized concurrently"
        );
    }
}

/// Convert worker startup failure into best-effort capture unavailability.
fn start_transport_with(spawn: impl FnOnce() -> io::Result<CaptureQueue>) -> Option<CaptureQueue> {
    match spawn() {
        Ok(queue) => Some(queue),
        Err(error) => {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                %error,
                "provider debug capture worker is unavailable; captures will be dropped"
            );
            None
        }
    }
}

/// Submit one serialized Provider diagnostic through nonblocking bounded
/// admission.
///
/// Callers must invoke this only after the harness permits durable-session
/// capture. The detached worker compresses accepted JSON independently of
/// terminal generation and sends a dedicated attributed protocol message on the
/// ordinary non-preemptive extension stream. A terminal queued behind an
/// already-started capture frame follows normal FIFO ordering; no
/// capture-specific gate or priority exists. The harness alone selects and
/// asynchronously writes the filesystem path. Any unavailable worker, overload,
/// compression, protocol, or harness I/O failure may omit the best-effort
/// artifact.
pub fn submit_provider_debug_capture(capture: ProviderDebugCapture) {
    if capture.class == ProviderDebugCaptureClass::CacheDiagnostic {
        // Metadata producers must reserve before constructing the payload.
        return;
    }
    if !capture_fits_raw_bound(&capture) {
        tracing::warn!(
            target: "tau_provider::debug_capture_writer",
            attribution = ?capture.attribution,
            json_bytes = capture.json.len(),
            "provider debug capture exceeds the protocol bound; dropping capture"
        );
        return;
    }
    submit(CaptureJob::new(capture));
}

/// Submit bounded scalar metadata on the existing worker without altering raw
/// capture budgets. Dropping any rejected job releases and records its loss.
pub fn submit_cache_diagnostic(
    capture: ProviderDebugCapture,
    reservation: crate::cache_diagnostic::Reservation,
) {
    if !cache_capture_fits_bound(&capture) {
        return;
    }
    submit(CaptureJob {
        capture,
        metadata_reservation: Some(reservation),
    });
}

/// Check both class and inclusive scalar byte bound without inspecting content.
fn cache_capture_fits_bound(capture: &ProviderDebugCapture) -> bool {
    capture.class == ProviderDebugCaptureClass::CacheDiagnostic
        && capture.json.len() <= crate::cache_diagnostic::MAX_RECORD_BYTES
}

/// Return whether one uncompressed job fits the established protocol bound
/// before it can consume a queue slot.
fn capture_fits_raw_bound(capture: &ProviderDebugCapture) -> bool {
    capture.json.len() as u64 <= tau_proto::MAX_PROTOCOL_MESSAGE_BYTES
}

/// Drain accepted jobs until every producer disconnects.
fn run_worker(
    receiver: mpsc::Receiver<CaptureJob>,
    mut write: impl FnMut(&CaptureJob) -> io::Result<()>,
) {
    while let Ok(mut job) = receiver.recv() {
        if let Err(error) = write(&job) {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                attribution = ?job.capture.attribution,
                %error,
                "failed to send compressed provider debug capture"
            );
        } else if let Some(reservation) = job.metadata_reservation.as_mut() {
            reservation.delivered();
        }
    }
}

/// Compress and synchronously flush one capture from the detached worker.
fn send_capture(handle: &tau_client::ClientHandle, job: &CaptureJob) -> io::Result<()> {
    let message = compressed_message(job)?;
    handle
        .send(message)
        .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))
}

/// Compress one job and enforce the complete encoded protocol-frame bound.
fn compressed_message(job: &CaptureJob) -> io::Result<tau_proto::HarnessInputMessage> {
    let zstd = zstd::stream::encode_all(&job.capture.json[..], ZSTD_COMPRESSION_LEVEL)?;
    let message =
        tau_proto::HarnessInputMessage::ProviderDebugCapture(tau_proto::ProviderDebugCapture {
            session_id: job.capture.session_id.clone(),
            attribution: job.capture.attribution.clone(),
            class: job.capture.class,
            zstd,
        });
    let encoded = tau_proto::encode_harness_input_to_vec(&message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    enforce_encoded_bound(message, encoded.len() as u64)
}

/// Enforce the complete encoded-frame ceiling after compression.
fn enforce_encoded_bound(
    message: tau_proto::HarnessInputMessage,
    encoded_len: u64,
) -> io::Result<tau_proto::HarnessInputMessage> {
    if tau_proto::MAX_PROTOCOL_MESSAGE_BYTES < encoded_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compressed provider debug capture exceeds the protocol bound",
        ));
    }
    Ok(message)
}
