//! Shell-facing artifact transfer state and off-loop filesystem preparation.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tau_client::{ArtifactClient, ArtifactDownload, ArtifactUpload};
use tau_proto::{
    ARTIFACT_MAX_BYTES, ArtifactError, ArtifactKey, ArtifactRequestId, ArtifactResult, CborValue,
    Event, HarnessOutputMessage, ToolCancelled, ToolError, ToolResult, ToolResultKind, ToolStarted,
};

use crate::Output;
use crate::scheduler::{WorkMeta, WorkPriority, WorkScheduler};
use crate::tool_lifecycle::ToolLifecycle;
use crate::tools::{EXPORT_TOOL_NAME, IMPORT_TOOL_NAME};

#[cfg(test)]
mod tests;

/// Maximum verified import bytes waiting for or executing local temp writes.
const IMPORT_WRITE_BYTES_LIMIT: u64 = ARTIFACT_MAX_BYTES;
/// Covers all scheduler-queued and running artifact jobs without blocking them.
const ARTIFACT_COMMAND_LIMIT: usize = 96;

/// Cloneable worker-side entry point for preparing artifact operations.
#[derive(Clone)]
pub(crate) struct ArtifactTransferControl {
    /// Bounded nonblocking completion queue consumed by the main loop.
    queue: Arc<Mutex<VecDeque<Command>>>,
    /// Manual-loop wake handle installed after startup.
    waker: Arc<Mutex<Option<tau_client::ManualRuntimeWaker>>>,
}

/// Main-loop-owned artifact transfers and exact request correlation.
pub(crate) struct ArtifactTransferManager {
    /// Typed nonblocking RPC writer, absent only in direct state-machine tests.
    client: Option<ArtifactClient>,
    /// Immutable current session used for exact request admission.
    session: Option<tau_proto::SessionId>,
    /// Worker-side control handle.
    control: ArtifactTransferControl,
    /// Active transfers by tool call.
    transfers: HashMap<tau_proto::ToolCallId, Transfer>,
    /// Exactly one outstanding request mapped back to its transfer.
    requests: HashMap<ArtifactRequestId, tau_proto::ToolCallId>,
    /// Independent byte budget for imported originals moved into shell workers.
    import_write_budget: ImportWriteBudget,
}

/// One off-loop preparation or finalization completion.
enum Command {
    /// A local regular file has been read and bounded.
    StartExport {
        /// Original invocation identity.
        invoke: ToolStarted,
        /// Lifecycle retained until the terminal is flushed.
        lifecycle: ToolLifecycle,
        /// Retry-safe upload state.
        upload: ArtifactUpload,
        /// Bounded per-use filename hint.
        filename: Option<String>,
    },
    /// A key has been validated and is ready to open.
    StartImport {
        /// Original invocation identity.
        invoke: ToolStarted,
        /// Lifecycle retained until the terminal is flushed.
        lifecycle: ToolLifecycle,
        /// Verified download state.
        download: ArtifactDownload,
    },
    /// Imported bytes have been written to a private local temporary file.
    ImportWritten {
        /// Owning tool call retained by the main-loop transfer.
        call_id: tau_proto::ToolCallId,
        /// Descriptor size from the verified download.
        size: u64,
        /// Completed local path or filesystem failure.
        result: Result<RetainedTemp, String>,
    },
}

/// Main-loop transfer state.
enum Transfer {
    /// Upload of one already-read original.
    Export {
        /// Original invocation identity.
        invoke: ToolStarted,
        /// Lifecycle retained until terminal publication.
        lifecycle: ToolLifecycle,
        /// Retry-safe typed upload helper.
        upload: ArtifactUpload,
        /// Bounded per-use filename hint.
        filename: Option<String>,
    },
    /// Download of one validated key.
    Import {
        /// Original invocation identity.
        invoke: ToolStarted,
        /// Lifecycle retained until terminal publication.
        lifecycle: ToolLifecycle,
        /// Verified typed download helper.
        download: ArtifactDownload,
    },
    /// Verified bytes are being written to a private local temporary file.
    ImportWriting {
        /// Original invocation identity.
        invoke: ToolStarted,
        /// Lifecycle retained until terminal publication.
        lifecycle: ToolLifecycle,
        /// Cancellation observed directly by the off-loop writer.
        cancel: Arc<AtomicBool>,
    },
}

/// Shared explicit admission budget for imported bytes owned by write jobs.
#[derive(Clone, Default)]
struct ImportWriteBudget {
    /// Bytes currently retained by queued or running import writers.
    used: Arc<Mutex<u64>>,
}

/// Reservation that releases imported-byte admission on every worker exit path.
struct ImportWriteReservation {
    /// Shared budget counter.
    budget: ImportWriteBudget,
    /// Bytes charged by this job.
    bytes: u64,
}

/// Retained private temp that unlinks itself unless a reported tool result
/// takes it.
struct RetainedTemp {
    /// Owned path removed on drop until successfully published to the caller.
    path: Option<PathBuf>,
}

impl ArtifactTransferManager {
    /// Creates the production transfer manager.
    pub(crate) fn new(client: ArtifactClient) -> Self {
        let waker = Arc::new(Mutex::new(None));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        Self {
            client: Some(client),
            session: None,
            control: ArtifactTransferControl { queue, waker },
            transfers: HashMap::new(),
            requests: HashMap::new(),
            import_write_budget: ImportWriteBudget::default(),
        }
    }

    /// Creates a manager that reports Artifact RPC unavailable in direct tests.
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn unavailable() -> Self {
        let waker = Arc::new(Mutex::new(None));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        Self {
            client: None,
            session: None,
            control: ArtifactTransferControl { queue, waker },
            transfers: HashMap::new(),
            requests: HashMap::new(),
            import_write_budget: ImportWriteBudget::default(),
        }
    }

    /// Returns the worker-side transfer control.
    pub(crate) fn control(&self) -> ArtifactTransferControl {
        self.control.clone()
    }

    /// Installs the manual-loop wake handle used after worker completions.
    pub(crate) fn install_waker(&self, waker: tau_client::ManualRuntimeWaker) {
        *self.control.waker.lock().expect("artifact waker poisoned") = Some(waker);
    }

    /// Binds artifact requests to the runtime's immutable current session.
    pub(crate) fn bind_session(&mut self, session: tau_proto::SessionId) {
        self.session = Some(session);
    }

    /// Drains bounded worker completions without filesystem I/O on the main
    /// loop.
    pub(crate) fn drain(&mut self, scheduler: &WorkScheduler, output: &Output) {
        loop {
            let command = self
                .control
                .queue
                .lock()
                .expect("artifact command queue poisoned")
                .pop_front();
            let Some(command) = command else {
                break;
            };
            match command {
                Command::StartExport {
                    invoke,
                    lifecycle,
                    upload,
                    filename,
                } => {
                    if lifecycle.effect_cancel_requested() {
                        report_cancelled(output, invoke, lifecycle);
                        continue;
                    }
                    let call_id = invoke.call_id.clone();
                    self.transfers.insert(
                        call_id.clone(),
                        Transfer::Export {
                            invoke,
                            lifecycle,
                            upload,
                            filename,
                        },
                    );
                    self.start_next(&call_id, output);
                }
                Command::StartImport {
                    invoke,
                    lifecycle,
                    download,
                } => {
                    if lifecycle.effect_cancel_requested() {
                        report_cancelled(output, invoke, lifecycle);
                        continue;
                    }
                    let call_id = invoke.call_id.clone();
                    self.transfers.insert(
                        call_id.clone(),
                        Transfer::Import {
                            invoke,
                            lifecycle,
                            download,
                        },
                    );
                    self.start_next(&call_id, output);
                }
                Command::ImportWritten {
                    call_id,
                    size,
                    result,
                } => {
                    let Some(Transfer::ImportWriting {
                        invoke,
                        lifecycle,
                        cancel,
                    }) = self.transfers.remove(&call_id)
                    else {
                        drop(result);
                        continue;
                    };
                    if cancel.load(Ordering::Acquire) || lifecycle.effect_cancel_requested() {
                        drop(result);
                        report_cancelled(output, invoke, lifecycle);
                        continue;
                    }
                    match result {
                        Ok(mut temp) => {
                            let reported = report_result(
                                output,
                                &invoke,
                                CborValue::Map(vec![
                                    text_entry("path", temp.path().display().to_string()),
                                    int_entry("size", size),
                                ]),
                            );
                            if reported {
                                let _ = temp.publish();
                            }
                        }
                        Err(message) => report_error(output, &invoke, message),
                    }
                    lifecycle.finish();
                }
            }
        }
        let _ = scheduler;
    }

    /// Accepts one exact correlated, bounded Artifact response.
    pub(crate) fn handle_result(
        &mut self,
        result: ArtifactResult,
        scheduler: &WorkScheduler,
        output: &Output,
    ) {
        let frame = HarnessOutputMessage::ArtifactResult(Box::new(result.clone()));
        let frame_fits = tau_proto::artifact_frame_fits(&frame);
        let Some(call_id) = self.requests.remove(&result.request_id) else {
            return;
        };
        let Some(mut transfer) = self.transfers.remove(&call_id) else {
            return;
        };
        if !frame_fits {
            self.best_effort_release(&transfer);
            finish_error(
                output,
                transfer,
                "artifact response exceeded the complete frame limit".to_owned(),
            );
            return;
        }
        let value = match result.result {
            Ok(value) => value,
            Err(error) => {
                finish_error(output, transfer, artifact_error_message(error));
                return;
            }
        };
        let accepted = match &mut transfer {
            Transfer::Export { upload, .. } => upload.accept(value),
            Transfer::Import { download, .. } => download.accept(value),
            Transfer::ImportWriting { .. } => Err(ArtifactError::Invalid),
        };
        if let Err(error) = accepted {
            self.best_effort_release(&transfer);
            finish_error(output, transfer, artifact_error_message(error));
            return;
        }
        match transfer {
            Transfer::Export {
                invoke,
                lifecycle,
                upload,
                filename,
            } if upload.descriptor().is_some() => {
                let descriptor = upload.descriptor().expect("checked descriptor");
                let mut entries = vec![
                    text_entry("key", descriptor.key.to_string()),
                    int_entry("size", descriptor.size.get()),
                ];
                if let Some(filename) = filename {
                    entries.push(text_entry("filename", filename));
                }
                report_result(output, &invoke, CborValue::Map(entries));
                lifecycle.finish();
            }
            Transfer::Import {
                invoke,
                lifecycle,
                download,
            } if download.next_op().is_none() => {
                let size = download
                    .into_bytes()
                    .map(|bytes| (bytes.len() as u64, bytes));
                match size {
                    Ok((size, bytes)) => {
                        let reservation = match self.import_write_budget.reserve(size) {
                            Ok(reservation) => reservation,
                            Err(message) => {
                                report_error(output, &invoke, message);
                                lifecycle.finish();
                                return;
                            }
                        };
                        let tx = self.control.clone();
                        let call_id = invoke.call_id.clone();
                        let cancel = Arc::new(AtomicBool::new(false));
                        self.transfers.insert(
                            call_id.clone(),
                            Transfer::ImportWriting {
                                invoke: invoke.clone(),
                                lifecycle: lifecycle.clone(),
                                cancel: Arc::clone(&cancel),
                            },
                        );
                        let meta = WorkMeta {
                            call_id: Some(call_id.clone()),
                            agent_id: Some(invoke.agent_id.clone()),
                            queued_bytes: 0,
                        };
                        let enqueue = scheduler.enqueue(WorkPriority::Cheap, meta, move || {
                            let result = write_private_temp(&bytes, &cancel);
                            drop(reservation);
                            let _ = tx.send(Command::ImportWritten {
                                call_id,
                                size,
                                result,
                            });
                        });
                        if let Err(error) = enqueue {
                            self.transfers.remove(&invoke.call_id);
                            report_error(output, &invoke, error.message);
                            lifecycle.finish();
                        }
                    }
                    Err(error) => {
                        report_error(output, &invoke, artifact_error_message(error));
                        lifecycle.finish();
                    }
                }
            }
            transfer => {
                self.transfers.insert(call_id.clone(), transfer);
                self.start_next(&call_id, output);
            }
        }
    }

    /// Cancels one active transfer and submits Close/Abort best-effort.
    pub(crate) fn cancel(&mut self, call_id: &tau_proto::ToolCallId, output: &Output) {
        self.requests.retain(|_, owner| owner != call_id);
        let Some(transfer) = self.transfers.remove(call_id) else {
            return;
        };
        if let Transfer::ImportWriting { cancel, .. } = &transfer {
            cancel.store(true, Ordering::Release);
        }
        self.best_effort_release(&transfer);
        let (invoke, lifecycle) = transfer.parts();
        report_cancelled(output, invoke, lifecycle);
    }

    /// Drops all correlation and submits transfer release best-effort.
    pub(crate) fn shutdown(&mut self) {
        let transfers = self
            .transfers
            .drain()
            .map(|(_, transfer)| transfer)
            .collect::<Vec<_>>();
        self.requests.clear();
        for transfer in &transfers {
            if let Transfer::ImportWriting { cancel, .. } = transfer {
                cancel.store(true, Ordering::Release);
            }
            self.best_effort_release(transfer);
        }
        for transfer in transfers {
            transfer.parts().1.finish();
        }
    }

    fn start_next(&mut self, call_id: &tau_proto::ToolCallId, output: &Output) {
        let Some(transfer) = self.transfers.get(call_id) else {
            return;
        };
        let op = match transfer {
            Transfer::Export { upload, .. } => upload.next_op(),
            Transfer::Import { download, .. } => download.next_op(),
            Transfer::ImportWriting { .. } => None,
        };
        let Some(op) = op else {
            return;
        };
        let result = self
            .client
            .as_ref()
            .ok_or(tau_client::ClientError::InvalidArtifactRequest)
            .and_then(|client| {
                let session = self
                    .session
                    .clone()
                    .ok_or(tau_client::ClientError::InvalidArtifactRequest)?;
                client.start_request(session, op)
            });
        match result {
            Ok(request_id) => {
                self.requests.insert(request_id, call_id.clone());
            }
            Err(error) => {
                if let Some(transfer) = self.transfers.remove(call_id) {
                    finish_error(
                        output,
                        transfer,
                        format!("artifact request failed: {error}"),
                    );
                }
            }
        }
    }

    fn best_effort_release(&self, transfer: &Transfer) {
        let Some(client) = &self.client else {
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        let op = match transfer {
            Transfer::Export { upload, .. } => upload.abort_op(),
            Transfer::Import { download, .. } => download.close_op(),
            Transfer::ImportWriting { .. } => None,
        };
        if let Some(op) = op {
            let _ = client.start_request(session, op);
        }
    }
}

impl ArtifactTransferControl {
    /// Reads or validates local inputs off-loop and hands typed state to the
    /// main loop.
    ///
    /// Returns true when the main loop now owns the lifecycle.
    pub(crate) fn prepare(
        &self,
        invoke: ToolStarted,
        lifecycle: ToolLifecycle,
        workdir: &Path,
        output: &Output,
    ) -> bool {
        let failure_invoke = invoke.clone();
        let command = if invoke.tool_name == EXPORT_TOOL_NAME {
            prepare_export(invoke, lifecycle, workdir)
        } else if invoke.tool_name == IMPORT_TOOL_NAME {
            prepare_import(invoke, lifecycle)
        } else {
            return false;
        };
        match command {
            Ok(command) => {
                if self.send(command).is_ok() {
                    true
                } else {
                    report_error(
                        output,
                        &failure_invoke,
                        "artifact completion capacity is busy; retry this tool call".to_owned(),
                    );
                    false
                }
            }
            Err((invoke, lifecycle, message)) => {
                report_error(output, &invoke, message);
                let _ = lifecycle;
                false
            }
        }
    }

    fn send(&self, command: Command) -> Result<(), Box<Command>> {
        {
            let mut queue = self.queue.lock().expect("artifact command queue poisoned");
            if queue.len() >= ARTIFACT_COMMAND_LIMIT {
                return Err(Box::new(command));
            }
            queue.push_back(command);
        }
        if let Some(waker) = self.waker.lock().expect("artifact waker poisoned").as_ref() {
            waker.wake();
        }
        Ok(())
    }
}

impl Transfer {
    fn parts(self) -> (ToolStarted, ToolLifecycle) {
        match self {
            Self::Export {
                invoke, lifecycle, ..
            }
            | Self::Import {
                invoke, lifecycle, ..
            }
            | Self::ImportWriting {
                invoke, lifecycle, ..
            } => (invoke, lifecycle),
        }
    }
}

#[expect(
    clippy::result_large_err,
    reason = "the error retains the exact invocation and lifecycle needed for one terminal"
)]
fn prepare_export(
    invoke: ToolStarted,
    lifecycle: ToolLifecycle,
    workdir: &Path,
) -> Result<Command, (ToolStarted, ToolLifecycle, String)> {
    let Some(path) = tau_proto::cbor_text_field(&invoke.arguments, "path") else {
        return Err((invoke, lifecycle, "path must be a string".to_owned()));
    };
    let path = absolute_path(workdir, &path);
    let mut file = File::open(&path).map_err(|error| {
        (
            invoke.clone(),
            lifecycle.clone(),
            format!("failed to open {}: {error}", path.display()),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        (
            invoke.clone(),
            lifecycle.clone(),
            format!("failed to inspect {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err((
            invoke,
            lifecycle,
            "export path is not a regular file".to_owned(),
        ));
    }
    if metadata.len() > ARTIFACT_MAX_BYTES {
        return Err((
            invoke,
            lifecycle,
            "artifact exceeds the 16 MiB limit".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(ARTIFACT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            (
                invoke.clone(),
                lifecycle.clone(),
                format!("failed to read {}: {error}", path.display()),
            )
        })?;
    let upload = ArtifactUpload::new(bytes).map_err(|error| {
        (
            invoke.clone(),
            lifecycle.clone(),
            artifact_error_message(error),
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(bounded_hint);
    Ok(Command::StartExport {
        invoke,
        lifecycle,
        upload,
        filename,
    })
}

#[expect(
    clippy::result_large_err,
    reason = "the error retains the exact invocation and lifecycle needed for one terminal"
)]
fn prepare_import(
    invoke: ToolStarted,
    lifecycle: ToolLifecycle,
) -> Result<Command, (ToolStarted, ToolLifecycle, String)> {
    let Some(key) = tau_proto::cbor_text_field(&invoke.arguments, "key") else {
        return Err((invoke, lifecycle, "key must be a string".to_owned()));
    };
    let key = ArtifactKey::parse(key).map_err(|_| {
        (
            invoke.clone(),
            lifecycle.clone(),
            "invalid artifact key".to_owned(),
        )
    })?;
    Ok(Command::StartImport {
        invoke,
        lifecycle,
        download: ArtifactDownload::new(key),
    })
}

fn write_private_temp(bytes: &[u8], cancel: &AtomicBool) -> Result<RetainedTemp, String> {
    let mut temp = tempfile::Builder::new()
        .prefix("tau-artifact-")
        .tempfile()
        .map_err(|error| format!("failed to create private import file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("failed to secure import file: {error}"))?;
    }
    temp.write_all(bytes)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| format!("failed to write imported original: {error}"))?;
    let (_, path) = temp
        .keep()
        .map_err(|error| format!("failed to retain imported original: {}", error.error))?;
    if cancel.load(Ordering::Acquire) {
        let _ = fs::remove_file(&path);
        return Err("artifact import was cancelled".to_owned());
    }
    Ok(RetainedTemp { path: Some(path) })
}

fn finish_error(output: &Output, transfer: Transfer, message: String) {
    let (invoke, lifecycle) = transfer.parts();
    report_error(output, &invoke, message);
    lifecycle.finish();
}

fn report_result(output: &Output, invoke: &ToolStarted, result: CborValue) -> bool {
    output
        .report_tool_terminal(Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            result,
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: invoke.originator.clone(),
        }))
        .is_ok()
}

fn report_error(output: &Output, invoke: &ToolStarted, message: String) {
    let _ = output.report_tool_terminal(Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        message,
        details: None,
        display: None,
        originator: invoke.originator.clone(),
    }));
}

fn report_cancelled(output: &Output, invoke: ToolStarted, lifecycle: ToolLifecycle) {
    let _ = output.report_tool_terminal(Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    lifecycle.finish();
}

fn artifact_error_message(error: ArtifactError) -> String {
    match error {
        ArtifactError::Permission => {
            "artifact storage is unavailable; a persistent harness state root is required"
                .to_owned()
        }
        ArtifactError::SessionMismatch => "artifact request session no longer matches".to_owned(),
        ArtifactError::Invalid => "artifact request or response was invalid".to_owned(),
        ArtifactError::Busy => "artifact storage is busy; retry this tool call".to_owned(),
        ArtifactError::Unavailable => "artifact or transfer is unavailable".to_owned(),
        ArtifactError::Integrity => "artifact size or digest verification failed".to_owned(),
        ArtifactError::Io => "artifact storage I/O failed".to_owned(),
    }
}

fn absolute_path(workdir: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workdir.join(path)
    }
}

fn bounded_hint(value: &str) -> String {
    let mut end = value.len().min(255);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn text_entry(name: &str, value: String) -> (CborValue, CborValue) {
    (CborValue::Text(name.to_owned()), CborValue::Text(value))
}

fn int_entry(name: &str, value: u64) -> (CborValue, CborValue) {
    (
        CborValue::Text(name.to_owned()),
        CborValue::Integer(i64::try_from(value).unwrap_or(i64::MAX).into()),
    )
}

impl ImportWriteBudget {
    /// Reserves bounded bytes before moving an original into a scheduler job.
    fn reserve(&self, bytes: u64) -> Result<ImportWriteReservation, String> {
        let mut used = self.used.lock().expect("import write budget poisoned");
        let next = used.saturating_add(bytes);
        if IMPORT_WRITE_BYTES_LIMIT < next {
            return Err("artifact import write capacity is busy; retry this tool call".to_owned());
        }
        *used = next;
        Ok(ImportWriteReservation {
            budget: self.clone(),
            bytes,
        })
    }
}

impl Drop for ImportWriteReservation {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .expect("import write budget poisoned");
        *used = used.saturating_sub(self.bytes);
    }
}

impl RetainedTemp {
    /// Borrows the private path while the guard still owns cleanup.
    fn path(&self) -> &Path {
        self.path.as_deref().expect("retained temp path")
    }

    /// Transfers cleanup ownership to the successful tool result.
    fn publish(&mut self) -> PathBuf {
        self.path.take().expect("retained temp path")
    }
}

impl Drop for RetainedTemp {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}
