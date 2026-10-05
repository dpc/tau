//! Correlated artifact transfers on the existing interactive UI connection.

mod attempt;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use attempt::Attempt;
use tau_proto::{
    ArtifactOp, ArtifactRequest, ArtifactRequestId, ArtifactResult, ArtifactValue,
    HarnessInputMessage,
};

use super::{WriterHandle, send_frame};

/// Bounded commands to the sole upload worker; no payload reaches the renderer.
enum Command {
    /// Start or explicitly retry the source owned by the frozen editor.
    Start {
        /// Local editor attempt identity.
        id: u64,
        /// Exact current UI session.
        session: tau_proto::SessionId,
        /// Normalized source, retained outside the editor buffer.
        text: tau_cli_term::PasteContent,
        /// Completion destination for this attachment only.
        handle: tau_cli_term::TermHandle,
    },
    /// Cancel only this transfer, not the UI connection or an agent prompt.
    Cancel,
    /// One matching directed result, admitted at most once per outstanding
    /// request.
    Result(ArtifactResult),
    /// Stop after the interactive transport has been closed.
    Shutdown,
}

/// Cloneable control and result-demultiplexing handle for one attachment.
#[derive(Clone)]
pub(super) struct PasteUpload {
    /// Bounded queue includes user controls and the sole correlated response.
    tx: mpsc::SyncSender<Command>,
    /// Exact currently outstanding wire request; take-on-delivery prevents
    /// floods.
    expected: Arc<Mutex<Option<ArtifactRequestId>>>,
    /// Monotonic cancellation watermark also invalidates queued starts.
    cancelled_through: Arc<AtomicU64>,
}

impl PasteUpload {
    /// Starts one worker using the existing writer, without another
    /// Hello/client.
    pub(super) fn spawn(writer: WriterHandle) -> (Self, JoinHandle<()>) {
        let (tx, rx) = mpsc::sync_channel(4);
        let expected = Arc::new(Mutex::new(None));
        let worker_expected = expected.clone();
        let cancelled_through = Arc::new(AtomicU64::new(0));
        let worker_cancelled = cancelled_through.clone();
        let worker = std::thread::spawn(move || run(writer, rx, worker_expected, worker_cancelled));
        (
            Self {
                tx,
                expected,
                cancelled_through,
            },
            worker,
        )
    }

    /// Starts a bounded paste or resumes the exact failed upload on explicit
    /// retry.
    pub(super) fn start(
        &self,
        session: tau_proto::SessionId,
        id: u64,
        text: Arc<str>,
        handle: tau_cli_term::TermHandle,
    ) {
        self.start_content(session, id, tau_cli_term::PasteContent::Text(text), handle);
    }

    /// Starts completed text or PNG bytes without rereading the clipboard on
    /// retry.
    pub(super) fn start_content(
        &self,
        session: tau_proto::SessionId,
        id: u64,
        text: tau_cli_term::PasteContent,
        handle: tau_cli_term::TermHandle,
    ) {
        if text.bytes().len() as u64 > tau_proto::ARTIFACT_MAX_BYTES {
            handle.finish_paste_upload(
                id,
                Err("paste exceeds the 16 MiB artifact limit".to_owned()),
            );
            return;
        }
        if let Err(error) = self.tx.try_send(Command::Start {
            id,
            session,
            text,
            handle,
        }) {
            let command = match error {
                mpsc::TrySendError::Full(command) | mpsc::TrySendError::Disconnected(command) => {
                    command
                }
            };
            if let Command::Start { id, handle, .. } = command {
                handle.finish_paste_upload(
                    id,
                    Err("upload worker unavailable; retry after it settles".to_owned()),
                );
            }
        }
    }

    /// Requests best-effort Abort without closing the shared UI connection.
    pub(super) fn cancel(&self, id: u64) {
        self.cancelled_through.fetch_max(id, Ordering::Release);
        let _ = self.tx.try_send(Command::Cancel);
    }

    /// Demultiplexes one matching result directly, never into event
    /// presentation.
    pub(super) fn deliver(&self, result: ArtifactResult) {
        let mut expected = self.expected.lock().expect("paste correlation poisoned");
        if expected.as_ref() != Some(&result.request_id) {
            return;
        }
        *expected = None;
        // A full control queue fails via the fixed transfer deadline rather
        // than blocking the UI reader or retaining an unbounded
        // response suffix.
        let _ = self.tx.try_send(Command::Result(result));
    }

    /// Wakes the worker for joining after the transport shutdown unblocks
    /// writes.
    pub(super) fn stop(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

/// Whether an admitted harness advertises the upload-only UI authority.
pub(super) fn supported(version: Option<tau_proto::ProtocolVersion>) -> bool {
    version.is_some_and(|version| version >= tau_proto::ProtocolVersion::new(8, 1))
}

/// Allocates an independent wire correlation for each request, including
/// retries.
fn request(session: &tau_proto::SessionId, op: ArtifactOp) -> ArtifactRequest {
    ArtifactRequest {
        request_id: crate::ui_client::next_request_id("ui-paste")
            .parse()
            .expect("generated request identifier"),
        expected_session_id: session.clone(),
        op,
    }
}

/// Drives a single bounded transfer independently of terminal input and
/// rendering.
fn run(
    writer: WriterHandle,
    rx: mpsc::Receiver<Command>,
    expected: Arc<Mutex<Option<ArtifactRequestId>>>,
    cancelled_through: Arc<AtomicU64>,
) {
    let mut attempt: Option<Attempt> = None;
    loop {
        let command = match attempt.as_ref().filter(|attempt| !attempt.paused) {
            Some(attempt) => {
                rx.recv_timeout(attempt.deadline.saturating_duration_since(Instant::now()))
            }
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        if attempt
            .as_ref()
            .is_some_and(|current| current.id <= cancelled_through.load(Ordering::Acquire))
        {
            if let Some(current) = attempt.take() {
                current.abort(&writer);
            }
            *expected.lock().expect("paste correlation poisoned") = None;
        }
        match command {
            Ok(Command::Start {
                id,
                session,
                text,
                handle,
            }) => {
                if id <= cancelled_through.load(Ordering::Acquire) {
                    continue;
                }
                let resumable = attempt.as_ref().is_some_and(|old| {
                    old.paused && old.session == session && old.text.same_source(&text)
                });
                if !resumable {
                    if let Some(old) = attempt.take() {
                        old.abort(&writer);
                    }
                    attempt = Some(Attempt {
                        id,
                        session,
                        upload: tau_client::ArtifactUpload::new(text.bytes().to_vec())
                            .expect("source validated before worker admission"),
                        text,
                        handle: handle.clone(),
                        available: false,
                        paused: false,
                        deadline: Instant::now() + Duration::from_secs(60),
                        request: None,
                    });
                }
                let current = attempt.as_mut().expect("started attempt");
                current.id = id;
                current.handle = handle;
                current.paused = false;
                current.deadline = Instant::now() + Duration::from_secs(60);
                current.send_next(&writer, &expected);
            }
            Ok(Command::Result(result)) => {
                let Some(current) = attempt.as_mut().filter(|current| {
                    !current.paused && current.request.as_ref() == Some(&result.request_id)
                }) else {
                    continue;
                };
                if Instant::now() >= current.deadline {
                    current.fail("upload timed out");
                } else if let Err(error) = current.accept(result) {
                    current.fail(error);
                } else if let Some(descriptor) = current.upload.descriptor() {
                    current.handle.finish_paste_upload(
                        current.id,
                        Ok(paste_reference(&descriptor.key, &current.text)),
                    );
                    attempt = None;
                } else {
                    current.send_next(&writer, &expected);
                }
            }
            Ok(Command::Cancel) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                *expected.lock().expect("paste correlation poisoned") = None;
                if let Some(current) = attempt.as_mut() {
                    current.fail("upload timed out");
                }
            }
            Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Keeps known source media beside the editable reference, not in blob
/// identity.
fn paste_reference(key: &tau_proto::ArtifactKey, content: &tau_cli_term::PasteContent) -> String {
    let mime_type = match content {
        tau_cli_term::PasteContent::Text(_) => "text/plain;charset=utf-8",
        tau_cli_term::PasteContent::Png(_) => "image/png",
    };
    format!(
        "{} (mime_type: {mime_type})",
        tau_proto::artifact_reference(key)
    )
}
