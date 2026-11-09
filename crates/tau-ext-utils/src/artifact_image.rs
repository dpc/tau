//! Artifact download, bounded preparation, and policy-selected prepared upload.

mod prepared_upload;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use prepared_upload::PreparedImageUpload;
use tau_client::{ArtifactClient, ArtifactDownload, ClientHandle};
use tau_proto::{
    ArtifactError, ArtifactRequestId, ArtifactResult, CborValue, Event, HarnessOutputMessage,
    ToolCallId, ToolCancelled, ToolError, ToolResult, ToolResultKind, ToolStarted, ToolType,
};

use crate::read_image::{MAX_SOURCE_BYTES, ReadImageOutput, ReadImageRequest};

/// Combined image calls and canceled allocation responses, including uploads.
pub(crate) const MAX_ACTIVE_READS: usize = 8;
const IMAGE_TOO_LARGE_MESSAGE: &str =
    "typed image result exceeds the complete terminal frame byte limit";

/// Main-loop owner for verified transfers and one bounded decoder worker.
pub(crate) struct ArtifactImageManager {
    /// Typed Artifact RPC writer.
    client: ArtifactClient,
    /// Outbound terminal-report handle.
    handle: ClientHandle,
    /// Immutable current session used by Artifact request admission.
    session: Option<tau_proto::SessionId>,
    /// Active tool calls and their exact transfer state.
    calls: HashMap<ToolCallId, ActiveCall>,
    /// Outstanding Artifact requests mapped back to their tool calls.
    requests: HashMap<ArtifactRequestId, ForegroundRequest>,
    /// Canceled Open/Begin requests retained until their allocations can be
    /// closed or aborted, under the same combined call bound.
    cleanup_allocations: HashMap<ArtifactRequestId, AllocationCleanup>,
    /// Verified jobs waiting for the single decoder.
    queued_decodes: VecDeque<DecodeJob>,
    /// Whether one decoder worker currently owns the process-wide decode
    /// permit.
    decode_active: bool,
    /// Worker completion queue drained only by the extension main loop.
    completions: Arc<Mutex<VecDeque<DecodeCompletion>>>,
    /// Manual runtime wake handle installed after startup.
    waker: Option<tau_client::ManualRuntimeWaker>,
    /// First terminal-report failure that must stop the extension loop.
    output_error: Option<tau_client::ClientError>,
}

/// One live tool call.
enum ActiveCall {
    /// Artifact bytes are still transferring and being verified.
    Downloading {
        /// Original routed invocation identity.
        invoke: ToolStarted,
        /// Parsed preparation request.
        request: ReadImageRequest,
        /// Verified-download state machine.
        download: Box<ArtifactDownload>,
    },
    /// Verified bytes are queued or executing in the decoder worker.
    Decoding {
        /// Original routed invocation identity.
        invoke: ToolStarted,
        /// Cancellation observed directly by a running worker.
        cancelled: Arc<AtomicBool>,
    },
    /// Prepared bytes are uploading under the exact invocation's Artifact
    /// policy.
    Uploading {
        /// Original routed identity and captured image-storage policy.
        invoke: ToolStarted,
        /// One prepared payload and metadata awaiting verified publication.
        upload: Box<PreparedImageUpload>,
    },
}

/// Cleanup to issue when a canceled allocation's exact response arrives.
#[derive(Clone, Copy)]
enum AllocationKind {
    /// A pending Open may create a read requiring Close.
    Read,
    /// A pending Begin may create an upload requiring Abort.
    Upload,
}

/// Captured session and allocation type, never rebound to a later session.
struct AllocationCleanup {
    /// Session in the original allocation request.
    session: tau_proto::SessionId,
    /// Operation that determines which returned identity may be released.
    kind: AllocationKind,
}

/// Correlation retained for one foreground Artifact request.
struct ForegroundRequest {
    /// Tool call that owns the foreground request.
    call_id: ToolCallId,
    /// Exact session used when the request was sent.
    session: tau_proto::SessionId,
}

/// One verified original awaiting off-loop decode.
struct DecodeJob {
    /// Original routed invocation identity.
    invoke: ToolStarted,
    /// Parsed preparation request.
    request: ReadImageRequest,
    /// Complete verified original bytes.
    bytes: Vec<u8>,
    /// Cancellation shared with the main loop.
    cancelled: Arc<AtomicBool>,
}

/// One decoder worker completion.
struct DecodeCompletion {
    /// Owning call.
    call_id: ToolCallId,
    /// Exact decode incarnation, also carrying sticky cancellation authority.
    cancelled: Arc<AtomicBool>,
    /// Prepared result or bounded decoder error.
    result: Result<ReadImageOutput, String>,
}

impl ArtifactImageManager {
    /// Creates an artifact-backed image manager.
    pub(crate) fn new(handle: ClientHandle) -> Self {
        Self {
            client: ArtifactClient::new(handle.clone()),
            handle,
            session: None,
            calls: HashMap::new(),
            requests: HashMap::new(),
            cleanup_allocations: HashMap::new(),
            queued_decodes: VecDeque::new(),
            decode_active: false,
            completions: Arc::new(Mutex::new(VecDeque::new())),
            waker: None,
            output_error: None,
        }
    }

    /// Installs the runtime wake handle used by decoder completions.
    pub(crate) fn install_waker(&mut self, waker: tau_client::ManualRuntimeWaker) {
        self.waker = Some(waker);
    }

    /// Binds Artifact requests to the current authenticated session.
    pub(crate) fn bind_session(&mut self, session: tau_proto::SessionId) {
        if self.session.as_ref() != Some(&session) {
            self.shutdown_session();
        }
        self.session = Some(session);
    }

    /// Starts one bounded foreground artifact image read.
    pub(crate) fn start(&mut self, invoke: ToolStarted) {
        if self.calls.contains_key(&invoke.call_id) {
            // A duplicate start cannot replace captured policy or redirect an
            // outstanding transfer to another invocation.
            return;
        }
        if MAX_ACTIVE_READS <= self.calls.len() + self.cleanup_allocations.len() {
            self.report_error(&invoke, "too many active image reads".to_owned());
            return;
        }
        let request = match ReadImageRequest::from_arguments(&invoke.arguments) {
            Ok(request) => request,
            Err(message) => {
                self.report_error(&invoke, message);
                return;
            }
        };
        let call_id = invoke.call_id.clone();
        let download = Box::new(ArtifactDownload::new(request.key.clone()));
        self.calls.insert(
            call_id.clone(),
            ActiveCall::Downloading {
                invoke,
                request,
                download,
            },
        );
        self.start_next(&call_id);
    }

    /// Accepts one exact correlated Artifact response.
    pub(crate) fn handle_result(&mut self, result: ArtifactResult) {
        let frame = HarnessOutputMessage::ArtifactResult(Box::new(result.clone()));
        if let Some(cleanup) = self.cleanup_allocations.remove(&result.request_id) {
            if tau_proto::artifact_frame_fits(&frame) {
                let op = match (cleanup.kind, result.result) {
                    (AllocationKind::Read, Ok(tau_proto::ArtifactValue::Opened { read, .. })) => {
                        Some(tau_proto::ArtifactOp::Close { read })
                    }
                    (AllocationKind::Upload, Ok(tau_proto::ArtifactValue::Upload { upload })) => {
                        Some(tau_proto::ArtifactOp::Abort { upload })
                    }
                    _ => None,
                };
                if let Some(op) = op {
                    let _ = self.client.start_request(cleanup.session, op);
                }
            }
            return;
        }
        let Some(request) = self.requests.remove(&result.request_id) else {
            return;
        };
        let Some(mut call) = self.calls.remove(&request.call_id) else {
            return;
        };
        if !tau_proto::artifact_frame_fits(&frame) {
            self.release(&call);
            self.finish_error(
                call,
                "artifact response exceeded the complete frame limit".to_owned(),
            );
            return;
        }
        let value = match result.result {
            Ok(value) => value,
            Err(error) => {
                self.release(&call);
                self.finish_error(call, artifact_error_message(error));
                return;
            }
        };
        let accepted = match &mut call {
            ActiveCall::Downloading { download, .. } => download.accept(value),
            ActiveCall::Uploading { upload, .. } => upload.accept(value),
            ActiveCall::Decoding { .. } => Err(ArtifactError::Invalid),
        };
        if let Err(error) = accepted {
            self.release(&call);
            self.finish_error(call, artifact_error_message(error));
            return;
        }
        if let ActiveCall::Downloading { download, .. } = &call
            && download
                .descriptor()
                .is_some_and(|descriptor| (MAX_SOURCE_BYTES as u64) < descriptor.size.get())
        {
            self.release(&call);
            self.finish_error(
                call,
                format!(
                    "image source exceeds the {} byte inspection limit",
                    MAX_SOURCE_BYTES
                ),
            );
            return;
        }
        if call.is_complete() {
            match call {
                ActiveCall::Downloading {
                    invoke,
                    request,
                    download,
                } => match (*download).into_bytes() {
                    Ok(bytes) => self.queue_decode(invoke, request, bytes),
                    Err(error) => self.report_error(&invoke, artifact_error_message(error)),
                },
                ActiveCall::Uploading { invoke, upload } => match (*upload).into_result() {
                    Ok(result) => self.report_prepared_result(result),
                    Err(error) => self.report_error(&invoke, artifact_error_message(error)),
                },
                ActiveCall::Decoding { .. } => unreachable!("decoder has no Artifact requests"),
            }
            return;
        }
        self.calls.insert(request.call_id.clone(), call);
        self.start_next(&request.call_id);
    }

    /// Cancels one active image call and reports one ordinary foreground
    /// cancellation.
    pub(crate) fn cancel(&mut self, cancelled: &ToolCancelled) {
        self.queued_decodes
            .retain(|job| job.invoke.call_id != cancelled.call_id);
        let Some(call) = self.calls.remove(&cancelled.call_id) else {
            return;
        };
        self.retire_requests(&call);
        if let ActiveCall::Decoding { cancelled, .. } = &call {
            cancelled.store(true, Ordering::Release);
        }
        self.release(&call);
        let invoke = call.invoke();
        if let Err(error) = self.handle.report_tool_cancelled(ToolCancelled {
            output: None,
            captured_output: None,
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            presentation: Default::default(),
            display: cancelled.display.clone(),
        }) {
            self.output_error.get_or_insert(error);
        }
    }

    /// Handles the actual harness cancellation request while retaining the
    /// original routed identity for the terminal report.
    pub(crate) fn cancel_request(&mut self, request: &tau_proto::ToolCancelRequest) {
        let Some(call) = self.calls.get(&request.target_call_id) else {
            return;
        };
        let invoke = call.invoke();
        let cancelled = ToolCancelled {
            output: None,
            captured_output: None,
            presentation: Default::default(),
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            display: None,
        };
        self.cancel(&cancelled);
    }

    /// Retires correlation while retaining late Open/Begin cleanup authority.
    fn retire_requests(&mut self, call: &ActiveCall) {
        let allocation = call.pending_allocation();
        let request_ids = self
            .requests
            .iter()
            .filter(|(_, request)| request.call_id == call.invoke().call_id)
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in request_ids {
            if let Some(request) = self.requests.remove(&request_id)
                && let Some(kind) = allocation
            {
                self.cleanup_allocations.insert(
                    request_id,
                    AllocationCleanup {
                        session: request.session,
                        kind,
                    },
                );
            }
        }
    }

    /// Drains worker completions and starts the next queued decode.
    pub(crate) fn drain(&mut self) {
        loop {
            let completion = self
                .completions
                .lock()
                .expect("read_image completion queue poisoned")
                .pop_front();
            let Some(completion) = completion else {
                break;
            };
            self.decode_active = false;
            if !matches!(
                self.calls.get(&completion.call_id),
                Some(ActiveCall::Decoding { cancelled, .. })
                    if Arc::ptr_eq(cancelled, &completion.cancelled)
            ) {
                // A canceled worker may finish after this call id was reused.
                // It must not remove, upload for, or report another
                // incarnation.
                self.start_queued_decode();
                continue;
            }
            let Some(ActiveCall::Decoding { invoke, cancelled }) =
                self.calls.remove(&completion.call_id)
            else {
                self.start_queued_decode();
                continue;
            };
            if cancelled.load(Ordering::Acquire) {
                self.start_queued_decode();
                continue;
            }
            match completion.result {
                Ok(output) => self.finish_prepared(invoke, output),
                Err(message) => self.report_error(&invoke, message),
            }
            self.start_queued_decode();
        }
    }

    /// Chooses transport only from the captured harness invocation policy.
    fn finish_prepared(&mut self, invoke: ToolStarted, output: ReadImageOutput) {
        let result = ToolResult {
            captured_output: None,
            presentation: Default::default(),
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            result: output.result,
            provider_content: output.provider_content,
            kind: ToolResultKind::Final,
            display: Some(output.display),
            originator: invoke.originator.clone(),
        };
        match invoke.invocation_policy.prepared_image_storage {
            tau_proto::PreparedImageStorage::Inline => self.report_prepared_result(result),
            tau_proto::PreparedImageStorage::Artifact => match PreparedImageUpload::new(result) {
                Ok(upload) => {
                    let call_id = invoke.call_id.clone();
                    self.calls.insert(
                        call_id.clone(),
                        ActiveCall::Uploading {
                            invoke,
                            upload: Box::new(upload),
                        },
                    );
                    self.start_next(&call_id);
                }
                Err(error) => self.report_error(&invoke, artifact_error_message(error)),
            },
        }
    }

    /// Takes the first mandatory terminal-report failure.
    pub(crate) fn take_output_error(&mut self) -> Option<tau_client::ClientError> {
        self.output_error.take()
    }

    /// Releases foreground work while retaining late allocation cleanup on a
    /// still-receiving transport. Already published uploads remain shared
    /// objects.
    pub(crate) fn shutdown_session(&mut self) {
        self.queued_decodes.clear();
        let calls = self.calls.drain().map(|(_, call)| call).collect::<Vec<_>>();
        for call in &calls {
            self.retire_requests(call);
            if let ActiveCall::Decoding { cancelled, .. } = call {
                cancelled.store(true, Ordering::Release);
            }
            self.release(call);
        }
        self.requests.clear();
        self.session = None;
    }

    /// Discards all ownership when the transport can no longer receive replies.
    pub(crate) fn shutdown_transport(&mut self) {
        self.shutdown_session();
        self.cleanup_allocations.clear();
    }

    fn start_next(&mut self, call_id: &ToolCallId) {
        let Some(call) = self.calls.get(call_id) else {
            return;
        };
        let Some(op) = call.next_op() else {
            return;
        };
        let result = self
            .session
            .clone()
            .ok_or(tau_client::ClientError::InvalidArtifactRequest)
            .and_then(|session| {
                self.client
                    .start_request(session.clone(), op)
                    .map(|request_id| (request_id, session))
            });
        match result {
            Ok((request_id, session)) => {
                self.requests.insert(
                    request_id,
                    ForegroundRequest {
                        call_id: call_id.clone(),
                        session,
                    },
                );
            }
            Err(error) => {
                if let Some(call) = self.calls.remove(call_id) {
                    self.release(&call);
                    self.finish_error(call, format!("artifact request failed: {error}"));
                }
            }
        }
    }

    fn queue_decode(&mut self, invoke: ToolStarted, request: ReadImageRequest, bytes: Vec<u8>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.calls.insert(
            invoke.call_id.clone(),
            ActiveCall::Decoding {
                invoke: invoke.clone(),
                cancelled: Arc::clone(&cancelled),
            },
        );
        self.queued_decodes.push_back(DecodeJob {
            invoke,
            request,
            bytes,
            cancelled,
        });
        self.start_queued_decode();
    }

    fn start_queued_decode(&mut self) {
        if self.decode_active {
            return;
        }
        while let Some(job) = self.queued_decodes.pop_front() {
            if job.cancelled.load(Ordering::Acquire) {
                continue;
            }
            self.decode_active = true;
            let completions = Arc::clone(&self.completions);
            let waker = self.waker.clone();
            std::thread::spawn(move || {
                let call_id = job.invoke.call_id.clone();
                let result = job.request.prepare(&job.bytes);
                completions
                    .lock()
                    .expect("read_image completion queue poisoned")
                    .push_back(DecodeCompletion {
                        call_id,
                        cancelled: job.cancelled,
                        result,
                    });
                if let Some(waker) = waker {
                    waker.wake();
                }
            });
            break;
        }
    }

    fn release(&self, call: &ActiveCall) {
        let op = match call {
            ActiveCall::Downloading { download, .. } => download.close_op(),
            ActiveCall::Uploading { upload, .. } => upload.abort_op(),
            ActiveCall::Decoding { .. } => None,
        };
        let (Some(session), Some(op)) = (self.session.clone(), op) else {
            return;
        };
        let _ = self.client.start_request(session, op);
    }

    fn finish_error(&mut self, call: ActiveCall, message: String) {
        let invoke = call.invoke().clone();
        self.report_error(&invoke, message);
    }

    fn report_error(&mut self, invoke: &ToolStarted, message: String) {
        if let Err(error) = self.handle.report_tool_error(ToolError {
            captured_output: None,
            presentation: Default::default(),
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            message: image_error_identifier(&message).to_owned(),
            details: Some(CborValue::Map(vec![(
                CborValue::Text("output".to_owned()),
                CborValue::Text(message.clone()),
            )])),
            display: Some(tau_proto::ToolUseState {
                status: tau_proto::ToolUseStatus::Error,
                status_text: message.lines().next().unwrap_or_default().to_owned(),
                ..Default::default()
            }),
            originator: invoke.originator.clone(),
        }) {
            self.output_error.get_or_insert(error);
        }
    }

    fn report_prepared_result(&mut self, result: ToolResult) {
        match budget_prepared_result(result) {
            Ok(Ok(message)) => {
                if let Err(error) = self.handle.send(message) {
                    self.output_error.get_or_insert(error);
                }
            }
            Ok(Err(error)) => {
                if let Err(error) = self.handle.report_tool_error(error) {
                    self.output_error.get_or_insert(error);
                }
            }
            Err(error) => {
                self.output_error.get_or_insert(error);
            }
        }
    }
}

fn budget_prepared_result(
    result: ToolResult,
) -> tau_client::ClientResult<Result<tau_proto::HarnessInputMessage, ToolError>> {
    let message =
        tau_proto::HarnessInputMessage::emit_with_persist(Event::ToolResultReported(result), false);
    if tau_client::encoded_outbound_frame_bytes(&message)? <= tau_client::MAX_OUTBOUND_FRAME_BYTES {
        return Ok(Ok(message));
    }
    let tau_proto::HarnessInputMessage::Emit(emit) = message else {
        unreachable!("constructed terminal emit")
    };
    let Event::ToolResultReported(result) = *emit.event else {
        unreachable!("constructed tool result")
    };
    let mut display = result.display;
    if let Some(display) = &mut display {
        display.status = tau_proto::ToolUseStatus::Error;
        display.status_text = IMAGE_TOO_LARGE_MESSAGE.to_owned();
    }
    Ok(Err(ToolError {
        captured_output: None,
        presentation: result.presentation,
        call_id: result.call_id,
        tool_name: result.tool_name,
        tool_type: result.tool_type,
        message: "image-limit-exceeded".to_owned(),
        details: Some(CborValue::Map(vec![(
            CborValue::Text("output".to_owned()),
            CborValue::Text(IMAGE_TOO_LARGE_MESSAGE.to_owned()),
        )])),
        display,
        originator: result.originator,
    }))
}

impl ActiveCall {
    /// Returns the routed invocation identity shared by every state.
    fn invoke(&self) -> &ToolStarted {
        match self {
            Self::Downloading { invoke, .. }
            | Self::Decoding { invoke, .. }
            | Self::Uploading { invoke, .. } => invoke,
        }
    }

    /// Selects one sequential operation for the active transfer.
    fn next_op(&self) -> Option<tau_proto::ArtifactOp> {
        match self {
            Self::Downloading { download, .. } => download.next_op(),
            Self::Uploading { upload, .. } => upload.next_op(),
            Self::Decoding { .. } => None,
        }
    }

    /// Detects completion without retaining or allocating a speculative chunk.
    fn is_complete(&self) -> bool {
        match self {
            Self::Downloading { download, .. } => download.next_op().is_none(),
            Self::Uploading { upload, .. } => upload.is_complete(),
            Self::Decoding { .. } => false,
        }
    }

    /// Identifies operations whose late response can allocate unknown state.
    fn pending_allocation(&self) -> Option<AllocationKind> {
        match self {
            Self::Downloading { download, .. } if download.descriptor().is_none() => {
                Some(AllocationKind::Read)
            }
            Self::Uploading { upload, .. } if upload.awaiting_allocation() => {
                Some(AllocationKind::Upload)
            }
            _ => None,
        }
    }
}

/// Select a useful error category while retaining the original diagnostic body.
fn image_error_identifier(message: &str) -> &'static str {
    if message.starts_with("argument `region`") && message.contains("outside oriented source") {
        "image-region-out-of-bounds"
    } else if message.starts_with("argument `region`") {
        "invalid-image-region"
    } else if message.starts_with("argument `key`") {
        "invalid-artifact-reference"
    } else if message.starts_with("argument ") || message.starts_with("missing string argument:") {
        "invalid-image-arguments"
    } else if message.starts_with("artifact ") {
        "image-artifact-transfer-failed"
    } else if message.contains("limit") || message == "too many active image reads" {
        "image-limit-exceeded"
    } else {
        "image-decode-failed"
    }
}

fn artifact_error_message(error: ArtifactError) -> String {
    match error {
        ArtifactError::Permission => "artifact storage is unavailable".to_owned(),
        ArtifactError::SessionMismatch => "artifact session did not match".to_owned(),
        ArtifactError::Unavailable => "artifact storage is unavailable".to_owned(),
        ArtifactError::Integrity => "artifact failed size or digest verification".to_owned(),
        ArtifactError::Invalid => "artifact transfer returned an invalid response".to_owned(),
        ArtifactError::Io => "artifact storage I/O failed".to_owned(),
        ArtifactError::Busy => "artifact storage is busy".to_owned(),
    }
}
