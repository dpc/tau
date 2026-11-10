//! Selected-account image production and main-loop-owned artifact publication.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tau_client::{ArtifactClient, ArtifactUpload, ClientHandle, ClientResult, ToolNameScope};
use tau_proto::{
    ArtifactKey, ArtifactOp, ArtifactRequestId, ArtifactResult, ArtifactValue, CborValue,
    ProviderName, SessionId, ToolCallId, ToolName, ToolStarted,
};
use tau_provider_codex::image_generation::{
    self, GenerationCancellation, GenerationError, InputImage,
};

use crate::{BuiltinProviderProfile, BuiltinProviderProfiles, ProviderRuntime, WorkerMessage};

mod inputs;
#[cfg(test)]
mod tests;
use inputs::Inputs;

/// Fixed bound shared by availability checks, network workers and uploads.
const MAX_CALLS: usize = 4;

/// Fixed production backend with a deterministic test seam, never
/// configuration.
type ImageExecutor = fn(
    tau_provider_codex::ResolvedCredentials,
    &str,
    &str,
    &tau_provider::OutboundNetworkPolicy,
    &GenerationCancellation,
    Vec<InputImage>,
) -> Result<Vec<u8>, GenerationError>;

/// Runtime authority and transfer correlations; no credential-bearing Debug.
pub(super) struct ImageTools {
    /// Startup-frozen wire name to selected provider namespace.
    declarations: HashMap<ToolName, ProviderName>,
    /// Current harness session announcement, not a model argument.
    pub(super) session: Option<SessionId>,
    /// One bounded call state until its ordinary terminal is reported.
    calls: HashMap<ToolCallId, ImageCall>,
    /// Exactly one outstanding artifact operation per call.
    requests: HashMap<ArtifactRequestId, ToolCallId>,
    /// Open allocation cleanup survives cancellation; orphan slots bound churn.
    opens: HashMap<ArtifactRequestId, SessionId>,
    /// Worker slots survive cancellation until their finite completion arrives.
    workers: HashSet<ToolCallId>,
    /// Backend function; tests replace it without live endpoint access.
    executor: ImageExecutor,
}

impl Default for ImageTools {
    fn default() -> Self {
        Self {
            declarations: HashMap::new(),
            session: None,
            calls: HashMap::new(),
            requests: HashMap::new(),
            opens: HashMap::new(),
            workers: HashSet::new(),
            executor: image_generation::generate_with_images,
        }
    }
}

/// One accepted call, owning private bounded originals until worker dispatch.
struct ImageCall {
    /// Ordinary tool identity and terminal reporting authority.
    started: ToolStarted,
    /// Captured session for directed Artifact operations.
    session: SessionId,
    /// Selected declaration's account namespace.
    provider: ProviderName,
    /// Bounded instruction moved into the finite worker.
    prompt: String,
    /// Ordered artifact occurrences, consumed after availability.
    attachments: Vec<ArtifactKey>,
    /// Shared latched worker cancellation.
    cancel: Arc<GenerationCancellation>,
    /// Dropping a completed call wakes its deadline thread immediately.
    deadline: Option<mpsc::Sender<()>>,
    /// Current sequential transfer or paid-operation ownership.
    phase: Phase,
}

/// Paid effect can begin only after a correlated availability success.
enum Phase {
    Availability,
    Resolving(Inputs),
    Generating,
    Uploading(ArtifactUpload),
}

/// Pure declaration projection also used by credential-free inspection.
pub(super) fn declarations(
    profiles: &BuiltinProviderProfiles,
) -> Vec<tau_proto::ToolRegistrationDeclared> {
    profiles.providers.iter().enumerate().filter_map(|(index, (provider, profile))| {
        if !matches!(profile, BuiltinProviderProfile::Chatgpt(profile) if profile.image_generation) {
            return None;
        }
        Some(tau_proto::ToolRegistrationDeclared {
            tool: tau_proto::ToolSpec {
                name: ToolName::new(format!("codex_image_{index}")),
                provider_scope: Some(provider.clone()),
                model_visible_name: Some(ToolName::new("generate_image")),
                description: Some("Generate one original PNG using the selected ChatGPT account, or edit with optional ordered attachments (up to 5 artifact keys for static PNG, JPEG or WebP originals). Omitted or empty attachments generate a new image. Attached original bytes, including metadata, are disclosed to that account; references can expire. Returns a bare artifact key, size and MIME headers; pass directly to import or read_image. Originals persist independently of this session, including ephemeral sessions. No masks or batches.".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "minLength": 1, "maxLength": image_generation::MAX_PROMPT_BYTES},
                        "attachments": {"type": "array", "maxItems": image_generation::MAX_ATTACHMENTS,
                            "items": {"type": "string", "pattern": tau_proto::ARTIFACT_ARGUMENT_PATTERN},
                            "description": "Ordered 24-lowercase-hex-digit artifact keys; also accepts tau-artifact:KEY or <tau-artifact:KEY>. Image 1, image 2, etc. No paths, URLs, previews, file IDs or automatic history selection."}
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                })),
                format: None,
                tags: Vec::new(),
                enabled_by_default: false,
                background_support: None,
                examples: Vec::new(),
            },
            tool_group: None,
            prompt_fragment: None,
        })
    }).collect()
}

impl ImageTools {
    pub(super) fn timeout(&mut self, id: &ToolCallId, handle: &ClientHandle) -> ClientResult<()> {
        let Some(call) = self.calls.remove(id) else {
            return Ok(());
        };
        self.requests.retain(|_, owner| owner != id);
        call.cancel.cancel();
        abort_transfers(&call, handle);
        report_error(
            &call.started,
            "image generation or artifact publication timed out; generation will not be retried",
            handle,
        )
    }
    pub(super) fn configure(
        &mut self,
        profiles: &BuiltinProviderProfiles,
        configure: &tau_proto::Configure,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let scope = ToolNameScope::from_configure(configure);
        for registration in declarations(profiles) {
            let wire = scope.wire_tool_name(&registration.tool.name)?;
            let provider = registration
                .tool
                .provider_scope
                .clone()
                .expect("scoped declaration");
            handle.register_local_tool(registration)?;
            self.declarations.insert(wire, provider);
        }
        Ok(())
    }

    pub(super) fn start(
        &mut self,
        started: ToolStarted,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let Some(provider) = self.declarations.get(&started.tool_name).cloned() else {
            return Ok(());
        };
        if self.calls.contains_key(&started.call_id) || self.workers.contains(&started.call_id) {
            return Ok(());
        }
        let Some((prompt, attachments)) = parse_arguments(&started.arguments) else {
            return report_error(
                &started,
                "expected a nonempty prompt up to 32 KiB and optional attachments of at most 5 artifact references",
                handle,
            );
        };
        let Some(session) = self.session.clone() else {
            return report_error(&started, "image artifact session is unavailable", handle);
        };
        if self.calls.len()
            + self
                .opens
                .keys()
                .filter(|id| !self.requests.contains_key(*id))
                .count()
            + self
                .workers
                .iter()
                .filter(|id| !self.calls.contains_key(*id))
                .count()
            >= MAX_CALLS
        {
            return report_error(&started, "image generation capacity is busy", handle);
        }
        handle.report_tool_progress(tau_proto::ToolProgress {
            call_id: started.call_id.clone(),
            tool_name: started.tool_name.clone(),
            message: None,
            progress: None,
            display: request_display(&started, tau_proto::ToolUseStatus::InProgress, "pending"),
        })?;
        let request = ArtifactClient::new(handle.clone())
            .start_request(session.clone(), ArtifactOp::Available)?;
        self.requests.insert(request, started.call_id.clone());
        self.calls.insert(
            started.call_id.clone(),
            ImageCall {
                started,
                session,
                provider,
                prompt,
                attachments,
                cancel: Arc::new(GenerationCancellation::default()),
                deadline: None,
                phase: Phase::Availability,
            },
        );
        Ok(())
    }

    pub(super) fn generated(
        &mut self,
        call_id: ToolCallId,
        result: Result<Vec<u8>, GenerationError>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        self.workers.remove(&call_id);
        if !self
            .calls
            .get(&call_id)
            .is_some_and(|call| matches!(call.phase, Phase::Generating))
        {
            return Ok(());
        }
        let mut call = self
            .calls
            .remove(&call_id)
            .expect("checked generating call");
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(error) => return report_error(&call.started, &error.to_string(), handle),
        };
        let Ok(upload) = ArtifactUpload::with_metadata(bytes, None, Some("image/png".into()))
        else {
            return report_error(
                &call.started,
                "generated image exceeds artifact limits",
                handle,
            );
        };
        call.phase = Phase::Uploading(upload);
        self.submit_upload(call, handle)
    }

    fn submit_upload(&mut self, call: ImageCall, handle: &ClientHandle) -> ClientResult<()> {
        let Phase::Uploading(upload) = &call.phase else {
            unreachable!("upload phase");
        };
        if let Some(descriptor) = upload.descriptor() {
            let display = request_display(&call.started, tau_proto::ToolUseStatus::Success, "ok");
            return handle.report_tool_result(tau_proto::ToolResult {
                captured_output: None,
                call_id: call.started.call_id,
                tool_name: call.started.tool_name,
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Map(vec![
                    (
                        CborValue::Text("artifact".into()),
                        CborValue::Text(descriptor.key.to_string()),
                    ),
                    (
                        CborValue::Text("size".into()),
                        CborValue::Integer(descriptor.size.get().into()),
                    ),
                    (
                        CborValue::Text("mime_type".into()),
                        CborValue::Text("image/png".into()),
                    ),
                ]),
                presentation: Default::default(),
                provider_content: Vec::new(),
                kind: Default::default(),
                display,
                originator: call.started.originator,
            });
        }
        let request = ArtifactClient::new(handle.clone()).start_request(
            call.session.clone(),
            upload.next_op().expect("unfinished upload"),
        )?;
        self.requests.insert(request, call.started.call_id.clone());
        self.calls.insert(call.started.call_id.clone(), call);
        Ok(())
    }

    pub(super) fn cancel(&mut self, id: &ToolCallId, handle: &ClientHandle) -> ClientResult<()> {
        let Some(call) = self.calls.remove(id) else {
            return Ok(());
        };
        self.requests.retain(|_, owner| owner != id);
        call.cancel.cancel();
        abort_transfers(&call, handle);
        let display = request_display(
            &call.started,
            tau_proto::ToolUseStatus::Warning,
            "cancelled",
        );
        handle.report_tool_cancelled(tau_proto::ToolCancelled {
            output: None,
            captured_output: None,
            call_id: call.started.call_id,
            tool_name: call.started.tool_name,
            tool_type: tau_proto::ToolType::Function,
            presentation: Default::default(),
            display,
        })
    }

    pub(super) fn cancel_all(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        for id in self.calls.keys().cloned().collect::<Vec<_>>() {
            self.cancel(&id, handle)?;
        }
        Ok(())
    }

    pub(super) fn abandon_all(&mut self) {
        for call in self.calls.values() {
            call.cancel.cancel();
        }
        self.calls.clear();
        self.requests.clear();
        self.opens.clear();
        // In-flight workers retain capacity until completion, even on restart.
    }
}

impl<F> ProviderRuntime<F>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    /// A deadline wakes the ordinary loop without borrowing or polling its
    /// input.
    pub(super) fn start_image_call(
        &mut self,
        started: ToolStarted,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let id = started.call_id.clone();
        if self.images.tools.calls.contains_key(&id) {
            return Ok(());
        }
        self.images.tools.start(started, handle)?;
        if let Some(call) = self.images.tools.calls.get_mut(&id)
            && let Some(waker) = self.worker_waker.clone()
        {
            let (finished, completion) = mpsc::channel();
            call.deadline = Some(finished);
            let tx = self.worker_tx.clone();
            std::thread::spawn(move || {
                if completion.recv_timeout(Duration::from_secs(420))
                    != Err(mpsc::RecvTimeoutError::Timeout)
                {
                    return;
                }
                let _ = crate::send_worker_message(
                    &tx,
                    &waker,
                    WorkerMessage::ImageTimedOut { call_id: id },
                );
            });
        }
        Ok(())
    }
    pub(super) fn handle_image_artifact_result(
        &mut self,
        result: ArtifactResult,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let Some(id) = self.images.tools.requests.remove(&result.request_id) else {
            if let Some(session) = self.images.tools.opens.remove(&result.request_id) {
                close_opened(&session, &result, handle);
            }
            return Ok(());
        };
        let opened_session = self.images.tools.opens.remove(&result.request_id);
        let Some(mut call) = self.images.tools.calls.remove(&id) else {
            return Ok(());
        };
        let frame = tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(result.clone()));
        if !tau_proto::artifact_frame_fits(&frame) {
            if let Some(session) = &opened_session {
                close_opened(session, &result, handle);
            }
            abort_transfers(&call, handle);
            return report_error(
                &call.started,
                "image artifact response exceeded limits",
                handle,
            );
        }
        // An Open may allocate even if its descriptor fails validation.
        let opened_read = match &result.result {
            Ok(ArtifactValue::Opened { read, .. }) if opened_session.is_some() => {
                Some(read.clone())
            }
            _ => None,
        };
        let Ok(value) = result.result else {
            abort_transfers(&call, handle);
            return report_error(
                &call.started,
                "image artifact storage is unavailable; generation will not be retried",
                handle,
            );
        };
        match &mut call.phase {
            Phase::Availability if value == ArtifactValue::Done => {
                if call.attachments.is_empty() {
                    self.start_available_image(call, Vec::new(), handle)
                } else {
                    call.phase =
                        Phase::Resolving(Inputs::new(std::mem::take(&mut call.attachments)));
                    self.submit_image_input(call, handle)
                }
            }
            Phase::Resolving(inputs) => {
                if inputs.accept(value).is_err() {
                    if let Some(read) = opened_read {
                        close_read(&call.session, ArtifactOp::Close { read }, handle);
                    }
                    abort_transfers(&call, handle);
                    return report_error(
                        &call.started,
                        "image attachment transfer failed; no image request sent",
                        handle,
                    );
                }
                self.submit_image_input(call, handle)
            }
            Phase::Uploading(upload) => {
                if upload.accept(value).is_err() {
                    abort_transfers(&call, handle);
                    return report_error(
                        &call.started,
                        "image artifact publication failed; generation will not be retried",
                        handle,
                    );
                }
                self.images.tools.submit_upload(call, handle)
            }
            _ => report_error(&call.started, "unexpected image artifact response", handle),
        }
    }

    /// Keeps exactly one original transfer operation outstanding per call.
    fn submit_image_input(
        &mut self,
        mut call: ImageCall,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let Phase::Resolving(inputs) = &call.phase else {
            unreachable!("input phase")
        };
        if let Some(op) = inputs.next_op() {
            let is_open = matches!(op, ArtifactOp::Open { .. });
            let request =
                match ArtifactClient::new(handle.clone()).start_request(call.session.clone(), op) {
                    Ok(request) => request,
                    Err(error) => {
                        abort_transfers(&call, handle);
                        return Err(error);
                    }
                };
            if is_open {
                self.images
                    .tools
                    .opens
                    .insert(request.clone(), call.session.clone());
            }
            self.images
                .tools
                .requests
                .insert(request, call.started.call_id.clone());
            self.images
                .tools
                .calls
                .insert(call.started.call_id.clone(), call);
            Ok(())
        } else {
            let Phase::Resolving(inputs) = std::mem::replace(&mut call.phase, Phase::Availability)
            else {
                unreachable!("input phase")
            };
            match inputs.into_originals() {
                Ok(originals) => self.start_available_image(call, originals, handle),
                Err(_) => report_error(
                    &call.started,
                    "image attachment verification failed; no image request sent",
                    handle,
                ),
            }
        }
    }

    /// Resolves selected credentials only after storage admitted this call.
    fn start_available_image(
        &mut self,
        mut call: ImageCall,
        originals: Vec<Vec<u8>>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let profiles = self.load_selected_profile(&call.provider, handle)?;
        let Some(BuiltinProviderProfile::Chatgpt(profile)) = profiles.providers.get(&call.provider)
        else {
            return report_error(
                &call.started,
                "selected image account is unavailable",
                handle,
            );
        };
        if profile.auth.access_token.is_empty()
            || (profile.auth.expires_at_ms != 0 && profile.auth.expires_at_ms <= crate::now_ms())
        {
            return report_error(
                &call.started,
                "selected image account requires authentication",
                handle,
            );
        }
        let credentials = tau_provider_codex::ResolvedCredentials::new(
            profile.auth.access_token.clone(),
            profile.auth.account_id.clone(),
        );
        let Some(waker) = self.worker_waker.clone() else {
            return report_error(&call.started, "image worker is unavailable", handle);
        };
        let tx = self.worker_tx.clone();
        let network = self.codex_runtime.network_arc();
        let cancel = Arc::clone(&call.cancel);
        let prompt = std::mem::take(&mut call.prompt);
        let executor = self.images.tools.executor;
        let id = call.started.call_id.clone();
        call.phase = Phase::Generating;
        self.images.tools.calls.insert(id.clone(), call);
        self.images.tools.workers.insert(id.clone());
        std::thread::spawn(move || {
            let result = (|| {
                let mut images = Vec::with_capacity(originals.len());
                for original in originals {
                    if cancel.is_cancelled() {
                        return Err(GenerationError::Cancelled);
                    }
                    images.push(InputImage::validate(original)?);
                }
                if cancel.is_cancelled() {
                    return Err(GenerationError::Cancelled);
                }
                executor(credentials, &prompt, id.as_str(), &network, &cancel, images)
            })();
            let _ = crate::send_worker_message(
                &tx,
                &waker,
                WorkerMessage::ImageGenerated {
                    call_id: id,
                    result,
                },
            );
        });
        Ok(())
    }
}

fn parse_arguments(arguments: &CborValue) -> Option<(String, Vec<ArtifactKey>)> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    let mut prompt = None;
    let mut attachments = None;
    for (key, value) in entries {
        match (key, value) {
            (CborValue::Text(key), CborValue::Text(value))
                if key == "prompt"
                    && prompt.is_none()
                    && !value.trim().is_empty()
                    && value.len() <= image_generation::MAX_PROMPT_BYTES =>
            {
                prompt = Some(value.clone())
            }
            (CborValue::Text(key), CborValue::Array(values))
                if key == "attachments"
                    && attachments.is_none()
                    && values.len() <= image_generation::MAX_ATTACHMENTS =>
            {
                attachments = Some(
                    values
                        .iter()
                        .map(|value| {
                            value
                                .deserialized::<tau_proto::ArtifactReference>()
                                .ok()
                                .map(tau_proto::ArtifactReference::into_key)
                        })
                        .collect::<Option<Vec<_>>>()?,
                );
            }
            _ => return None,
        }
    }
    Some((prompt?, attachments.unwrap_or_default()))
}

/// Projects only validated public request fields, never image bytes or worker
/// state. Read the original arguments because resolution consumes attachments.
fn request_display(
    started: &ToolStarted,
    status: tau_proto::ToolUseStatus,
    status_text: &str,
) -> Option<tau_proto::ToolUseState> {
    let (prompt, attachments) = parse_arguments(&started.arguments)?;
    let count = attachments.len();
    let mut text = format!("Prompt:\n{prompt}");
    if !attachments.is_empty() {
        text.push_str("\n\nAttachments:");
        for (index, key) in attachments.iter().enumerate() {
            let _ = write!(
                &mut text,
                "\n{}. {}",
                index + 1,
                tau_proto::artifact_uri(key)
            );
        }
    }
    Some(tau_proto::ToolUseState {
        stats: tau_proto::ToolUseStats {
            bytes: Some(prompt.len() as u64),
            ..Default::default()
        },
        progress_counters: if count == 0 {
            Vec::new()
        } else {
            vec![tau_proto::ProgressCounter {
                label: Some("att".into()),
                unit: tau_proto::ProgressUnit::Count,
                complete: Some(count as u64),
                total: None,
            }]
        },
        status,
        status_text: status_text.to_owned(),
        payload: Some(tau_proto::ToolUsePayload::Text { text }),
        ..Default::default()
    })
}

/// Releases any owned input read or output upload without retrying paid work.
fn abort_transfers(call: &ImageCall, handle: &ClientHandle) {
    if let Phase::Resolving(inputs) = &call.phase
        && let Some(op) = inputs.close_op()
    {
        close_read(&call.session, op, handle);
    }
    if let Phase::Uploading(upload) = &call.phase
        && let Some(op) = upload.abort_op()
    {
        let _ = ArtifactClient::new(handle.clone()).start_request(call.session.clone(), op);
    }
}

/// Closes only a correlated Open allocation; malformed identifiers are ignored.
fn close_opened(session: &SessionId, result: &ArtifactResult, handle: &ClientHandle) {
    if let Ok(ArtifactValue::Opened { read, .. }) = &result.result {
        close_read(session, ArtifactOp::Close { read: read.clone() }, handle);
    }
}

/// Best-effort cleanup does not retry paid work or retain more correlations.
fn close_read(session: &SessionId, op: ArtifactOp, handle: &ClientHandle) {
    if let ArtifactOp::Close { read } = &op
        && tau_proto::artifact_identifier(read)
    {
        let _ = ArtifactClient::new(handle.clone()).start_request(session.clone(), op);
    }
}
fn report_error(started: &ToolStarted, message: &str, handle: &ClientHandle) -> ClientResult<()> {
    handle.report_tool_error(tau_proto::ToolError {
        captured_output: None,
        call_id: started.call_id.clone(),
        tool_name: started.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        message: "image-generation-failed".to_owned(),
        details: Some(tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Text("output".to_owned()),
            tau_proto::CborValue::Text(message.to_owned()),
        )])),
        presentation: Default::default(),
        display: request_display(started, tau_proto::ToolUseStatus::Error, message),
        originator: started.originator.clone(),
    })
}
