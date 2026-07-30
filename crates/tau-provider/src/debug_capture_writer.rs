//! Bounded best-effort writer for compressed provider debug captures.

use std::{thread as path_std_thread, time as path_std_time};

use zstd::stream::write as path_zstd_stream_write;

#[cfg(test)]
mod tests;

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};

pub use tau_config::provider_debug_capture::ProviderDebugCaptureClass;
use tau_config::provider_debug_capture::{
    ProviderDebugCaptureFilename, ProviderDebugCaptureFormat,
};

/// Maximum number of captures waiting for background compression and writing.
const CAPTURE_QUEUE_CAPACITY: usize = 64;
/// Compression level used for private diagnostic captures.
const ZSTD_COMPRESSION_LEVEL: i32 = 3;

/// One typed serialized provider capture awaiting best-effort admission.
pub struct ProviderDebugCapture {
    /// Validated durable-session identity.
    session_id: tau_proto::SessionId,
    /// Validated prompt identity retained in the basename.
    agent_prompt_id: tau_proto::AgentPromptId,
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
            agent_prompt_id,
            class,
            json,
        }
    }

    /// Return the validated session identity.
    #[must_use]
    pub fn session_id(&self) -> &tau_proto::SessionId {
        &self.session_id
    }

    /// Return the validated prompt identity.
    #[must_use]
    pub fn agent_prompt_id(&self) -> &tau_proto::AgentPromptId {
        &self.agent_prompt_id
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
    /// Durable session directory that must already exist as a real directory.
    session_dir: PathBuf,
    /// Unique capture filename below `debug/provider-requests`.
    filename: ProviderDebugCaptureFilename,
    /// Uncompressed pretty-printed JSON record.
    json: Vec<u8>,
}

impl CaptureJob {
    /// Build one capture job for a durable session candidate.
    fn new(session_dir: PathBuf, filename: ProviderDebugCaptureFilename, json: Vec<u8>) -> Self {
        Self {
            session_dir,
            filename,
            json,
        }
    }
}

/// Nonblocking producer handle for the process-wide capture worker.
struct CaptureQueue {
    /// Bounded FIFO sender; admission always uses `try_send`.
    sender: mpsc::SyncSender<CaptureJob>,
}

impl CaptureQueue {
    /// Start one detached worker using the production filesystem writer.
    fn spawn() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(CAPTURE_QUEUE_CAPACITY);
        path_std_thread::Builder::new()
            .name("tau-provider-capture".to_owned())
            .spawn(move || run_worker(receiver, write_capture))
            .map(|_| Self { sender })
    }

    /// Admit a job immediately or return it when the queue is
    /// full/disconnected.
    fn try_submit(&self, job: CaptureJob) -> Result<(), mpsc::TrySendError<CaptureJob>> {
        self.sender.try_send(job)
    }
}

/// Submit a provider capture without waiting for worker capacity or I/O.
fn submit(job: CaptureJob) {
    static QUEUE: OnceLock<io::Result<CaptureQueue>> = OnceLock::new();
    let queue = QUEUE.get_or_init(CaptureQueue::spawn);
    match queue {
        Ok(queue) => match queue.try_submit(job) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(job)) => {
                tracing::warn!(
                    target: "tau_provider::debug_capture_writer",
                    filename = %job.filename.as_str(),
                    "provider debug capture queue is full; dropping capture"
                );
            }
            Err(mpsc::TrySendError::Disconnected(job)) => {
                tracing::warn!(
                    target: "tau_provider::debug_capture_writer",
                    filename = %job.filename.as_str(),
                    "provider debug capture worker stopped; dropping capture"
                );
            }
        },
        Err(error) => {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                %error,
                "provider debug capture worker is unavailable; dropping capture"
            );
        }
    }
}

/// Submit one serialized provider diagnostic without waiting for compression,
/// queue capacity, or filesystem work.
///
/// Callers must invoke this only after the harness permits durable-session
/// capture. An unavailable state directory silently omits the best-effort
/// diagnostic. The worker independently requires the corresponding durable
/// session directory to exist as a real directory before it creates debug
/// descendants.
pub fn submit_provider_debug_capture(capture: ProviderDebugCapture) {
    let Some(state_dir) = tau_config::settings::state_dir() else {
        return;
    };
    let timestamp = path_std_time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let filename = ProviderDebugCaptureFilename::new(
        timestamp,
        &capture.agent_prompt_id,
        capture.class,
        ProviderDebugCaptureFormat::ZstdJson,
    );
    let session_dir =
        tau_config::settings::sessions_dir_of(&state_dir).join(capture.session_id.as_str());
    submit(CaptureJob::new(session_dir, filename, capture.json));
}

/// Drain accepted jobs until every producer disconnects.
fn run_worker(
    receiver: mpsc::Receiver<CaptureJob>,
    mut write: impl FnMut(&CaptureJob) -> io::Result<()>,
) {
    while let Ok(job) = receiver.recv() {
        if let Err(error) = write(&job) {
            tracing::warn!(
                target: "tau_provider::debug_capture_writer",
                filename = %job.filename.as_str(),
                %error,
                "failed to write compressed provider debug capture"
            );
        }
    }
}

/// Compress and write one capture entirely on the worker thread.
fn write_capture(job: &CaptureJob) -> io::Result<()> {
    write_capture_with(job, |file, json| {
        let mut encoder = path_zstd_stream_write::Encoder::new(file, ZSTD_COMPRESSION_LEVEL)?;
        encoder.write_all(json)?;
        encoder.finish()?;
        Ok(())
    })
}

/// Write one capture with an injectable compression/write operation.
fn write_capture_with(
    job: &CaptureJob,
    write: impl FnOnce(std::fs::File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    ensure_real_directory(&job.session_dir)?;
    let debug_dir = job.session_dir.join("debug");
    let dir = debug_dir.join("provider-requests");
    ensure_or_create_real_directory(&debug_dir)?;
    ensure_or_create_real_directory(&dir)?;

    let path = dir.join(job.filename.as_str());
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    write(file, &job.json)
}

/// Create one directory when absent, then require a non-symlink directory.
fn ensure_or_create_real_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => ensure_real_directory(path),
        Err(error) => Err(error),
    }
}

/// Require `path` to identify a directory without following a final symlink.
fn ensure_real_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        )))
    }
}
