//! Artifact download and bounded off-loop preparation for `read_image`.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tau_client::{ArtifactClient, ArtifactDownload, ClientHandle};
use tau_proto::{
    ArtifactError, ArtifactRequestId, ArtifactResult, Event, HarnessOutputMessage, ToolCallId,
    ToolCancelled, ToolError, ToolResult, ToolResultKind, ToolStarted, ToolType,
};

use crate::read_image::{MAX_SOURCE_BYTES, ReadImageOutput, ReadImageRequest};

pub(crate) const MAX_ACTIVE_READS: usize = 8;
const IMAGE_TOO_LARGE_MESSAGE: &str =
    "typed image result exceeds the complete terminal frame byte limit";

/// Main-loop owner for verified artifact reads and one bounded decoder worker.
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
    requests: HashMap<ArtifactRequestId, ToolCallId>,
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
        download: ArtifactDownload,
    },
    /// Verified bytes are queued or executing in the decoder worker.
    Decoding {
        /// Original routed invocation identity.
        invoke: ToolStarted,
        /// Cancellation observed directly by a running worker.
        cancelled: Arc<AtomicBool>,
    },
}

/// One verified original awaiting off-loop decode.
struct DecodeJob {
    /// Original routed invocation identity.
    invoke: ToolStarted,
    /// Parsed preparation request.
    request: ReadImageRequest,
    /// Complete verified original bytes.
    bytes: Vec<u8>,
    /// Descriptor size retained as safe result metadata.
    original_size: u64,
    /// Cancellation shared with the main loop.
    cancelled: Arc<AtomicBool>,
}

/// One decoder worker completion.
struct DecodeCompletion {
    /// Owning call.
    call_id: ToolCallId,
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
        self.session = Some(session);
    }

    /// Starts one bounded foreground artifact image read.
    pub(crate) fn start(&mut self, invoke: ToolStarted) {
        if MAX_ACTIVE_READS <= self.calls.len() {
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
        let download = ArtifactDownload::new(request.key.clone());
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
        let Some(call_id) = self.requests.remove(&result.request_id) else {
            return;
        };
        let Some(mut call) = self.calls.remove(&call_id) else {
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
        let ActiveCall::Downloading { download, .. } = &mut call else {
            return;
        };
        if let Err(error) = download.accept(value) {
            self.release(&call);
            self.finish_error(call, artifact_error_message(error));
            return;
        }
        if download
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
        let complete = matches!(
            &call,
            ActiveCall::Downloading { download, .. } if download.next_op().is_none()
        );
        if complete {
            let ActiveCall::Downloading {
                invoke,
                request,
                download,
            } = call
            else {
                unreachable!("checked download state")
            };
            let original_size = download
                .descriptor()
                .map(|descriptor| descriptor.size.get())
                .unwrap_or_default();
            match download.into_bytes() {
                Ok(bytes) => self.queue_decode(invoke, request, bytes, original_size),
                Err(error) => self.report_error(&invoke, artifact_error_message(error)),
            }
            return;
        }
        self.calls.insert(call_id.clone(), call);
        self.start_next(&call_id);
    }

    /// Cancels one active read and reports one ordinary foreground
    /// cancellation.
    pub(crate) fn cancel(&mut self, cancelled: &ToolCancelled) {
        self.requests.retain(|_, owner| owner != &cancelled.call_id);
        self.queued_decodes
            .retain(|job| job.invoke.call_id != cancelled.call_id);
        let Some(call) = self.calls.remove(&cancelled.call_id) else {
            return;
        };
        if let ActiveCall::Decoding { cancelled, .. } = &call {
            cancelled.store(true, Ordering::Release);
        }
        self.release(&call);
        let invoke = call.invoke();
        if let Err(error) = self.handle.report_tool_cancelled(ToolCancelled {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            presentation: Default::default(),
            display: cancelled.display.clone(),
        }) {
            self.output_error.get_or_insert(error);
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
                Ok(output) => {
                    let result = ToolResult {
                        presentation: Default::default(),
                        call_id: invoke.call_id,
                        tool_name: invoke.tool_name,
                        tool_type: ToolType::Function,
                        result: output.result,
                        provider_content: output.provider_content,
                        kind: ToolResultKind::Final,
                        display: Some(output.display),
                        originator: invoke.originator,
                    };
                    self.report_prepared_result(result);
                }
                Err(message) => self.report_error(&invoke, message),
            }
            self.start_queued_decode();
        }
    }

    /// Takes the first mandatory terminal-report failure.
    pub(crate) fn take_output_error(&mut self) -> Option<tau_client::ClientError> {
        self.output_error.take()
    }

    /// Releases all live transfers during session or process shutdown.
    pub(crate) fn shutdown(&mut self) {
        self.requests.clear();
        self.queued_decodes.clear();
        let calls = self.calls.drain().map(|(_, call)| call).collect::<Vec<_>>();
        for call in &calls {
            if let ActiveCall::Decoding { cancelled, .. } = call {
                cancelled.store(true, Ordering::Release);
            }
            self.release(call);
        }
        self.session = None;
    }

    fn start_next(&mut self, call_id: &ToolCallId) {
        let Some(ActiveCall::Downloading { download, .. }) = self.calls.get(call_id) else {
            return;
        };
        let Some(op) = download.next_op() else {
            return;
        };
        let result = self
            .session
            .clone()
            .ok_or(tau_client::ClientError::InvalidArtifactRequest)
            .and_then(|session| self.client.start_request(session, op));
        match result {
            Ok(request_id) => {
                self.requests.insert(request_id, call_id.clone());
            }
            Err(error) => {
                if let Some(call) = self.calls.remove(call_id) {
                    self.finish_error(call, format!("artifact request failed: {error}"));
                }
            }
        }
    }

    fn queue_decode(
        &mut self,
        invoke: ToolStarted,
        request: ReadImageRequest,
        bytes: Vec<u8>,
        original_size: u64,
    ) {
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
            original_size,
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
                let result = job.request.prepare(&job.bytes, job.original_size);
                completions
                    .lock()
                    .expect("read_image completion queue poisoned")
                    .push_back(DecodeCompletion { call_id, result });
                if let Some(waker) = waker {
                    waker.wake();
                }
            });
            break;
        }
    }

    fn release(&self, call: &ActiveCall) {
        let ActiveCall::Downloading { download, .. } = call else {
            return;
        };
        let (Some(session), Some(op)) = (self.session.clone(), download.close_op()) else {
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
            presentation: Default::default(),
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            message: message.clone(),
            details: None,
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
        presentation: result.presentation,
        call_id: result.call_id,
        tool_name: result.tool_name,
        tool_type: result.tool_type,
        message: IMAGE_TOO_LARGE_MESSAGE.to_owned(),
        details: None,
        display,
        originator: result.originator,
    }))
}

impl ActiveCall {
    /// Returns the routed invocation identity shared by every state.
    fn invoke(&self) -> &ToolStarted {
        match self {
            Self::Downloading { invoke, .. } | Self::Decoding { invoke, .. } => invoke,
        }
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
