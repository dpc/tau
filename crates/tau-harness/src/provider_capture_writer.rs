//! Bounded asynchronous writer for opaque Provider debug captures.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use tau_config::provider_debug_capture::{
    ProviderDebugCaptureFilename, ProviderDebugCaptureFormat,
};

#[cfg(test)]
mod tests;

/// Existing Provider capture queue capacity, retained across the process
/// boundary rollover.
const CAPTURE_QUEUE_CAPACITY: usize = 64;

/// One harness-attributed opaque capture awaiting filesystem I/O.
struct CaptureWriteJob {
    /// Existing durable session root selected by the harness.
    session_dir: PathBuf,
    /// Authenticated configured Provider instance.
    provider_instance: tau_proto::ExtensionName,
    /// Harness-owned safe capture basename.
    filename: ProviderDebugCaptureFilename,
    /// Opaque zstd bytes written without parsing or decompression.
    zstd: Vec<u8>,
}

/// Process-wide bounded capture filesystem worker.
struct CaptureWriter {
    /// Nonblocking bounded admission channel.
    sender: mpsc::SyncSender<CaptureWriteJob>,
}

/// Content-free result of bounded filesystem queue admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureSubmitError {
    /// Queue capacity is currently exhausted.
    Full,
    /// Detached filesystem worker has stopped.
    Disconnected,
}

/// Lazily initialized harness capture writer.
static CAPTURE_WRITER: OnceLock<Option<CaptureWriter>> = OnceLock::new();

impl CaptureWriter {
    /// Build a writer around one bounded sender.
    fn with_sender(sender: mpsc::SyncSender<CaptureWriteJob>) -> Self {
        Self { sender }
    }

    /// Spawn the detached filesystem worker.
    fn spawn() -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(CAPTURE_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("tau-provider-capture-write".to_owned())
            .spawn(move || run_worker(receiver, write_capture))?;
        Ok(Self::with_sender(sender))
    }

    /// Admit one job without waiting for filesystem progress.
    fn try_submit(&self, job: CaptureWriteJob) -> Result<(), CaptureSubmitError> {
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => CaptureSubmitError::Full,
            mpsc::TrySendError::Disconnected(_) => CaptureSubmitError::Disconnected,
        })
    }
}

/// Queue one already-authenticated capture for best-effort persistence.
pub(crate) fn enqueue(
    session_dir: PathBuf,
    provider_instance: tau_proto::ExtensionName,
    capture: tau_proto::ProviderDebugCapture,
) {
    let timestamp_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let filename = ProviderDebugCaptureFilename::new(
        timestamp_micros,
        &capture.agent_prompt_id,
        capture.class,
        ProviderDebugCaptureFormat::ZstdJson,
    );
    let agent_prompt_id = capture.agent_prompt_id.clone();
    let job = CaptureWriteJob {
        session_dir,
        provider_instance,
        filename,
        zstd: capture.zstd,
    };
    let writer = CAPTURE_WRITER.get_or_init(|| match CaptureWriter::spawn() {
        Ok(writer) => Some(writer),
        Err(error) => {
            tracing::warn!(
                target: "tau_harness::provider_capture",
                %error,
                "provider capture writer could not start; captures will be dropped"
            );
            None
        }
    });
    let Some(writer) = writer else {
        return;
    };
    match writer.try_submit(job) {
        Ok(()) => {}
        Err(CaptureSubmitError::Full) => tracing::warn!(
            target: "tau_harness::provider_capture",
            agent_prompt_id = %agent_prompt_id,
            "provider capture writer queue is full; dropping capture"
        ),
        Err(CaptureSubmitError::Disconnected) => tracing::warn!(
            target: "tau_harness::provider_capture",
            agent_prompt_id = %agent_prompt_id,
            "provider capture writer stopped; dropping capture"
        ),
    }
}

/// Drain accepted jobs until the harness process drops every producer.
fn run_worker(
    receiver: mpsc::Receiver<CaptureWriteJob>,
    mut write: impl FnMut(&CaptureWriteJob) -> io::Result<()>,
) {
    while let Ok(job) = receiver.recv() {
        if let Err(error) = write(&job) {
            tracing::warn!(
                target: "tau_harness::provider_capture",
                filename = %job.filename.as_str(),
                %error,
                "failed to write provider debug capture"
            );
        }
    }
}

/// Write opaque compressed bytes to one harness-derived path.
fn write_capture(job: &CaptureWriteJob) -> io::Result<()> {
    ensure_real_directory(&job.session_dir)?;
    let debug_dir = job.session_dir.join("debug");
    let captures_dir = debug_dir.join("provider-requests");
    let instance_dir = captures_dir.join(job.provider_instance.as_str());
    ensure_or_create_real_directory(&debug_dir)?;
    ensure_or_create_real_directory(&captures_dir)?;
    ensure_or_create_real_directory(&instance_dir)?;
    let path = instance_dir.join(job.filename.as_str());
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&job.zstd)
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
