//! Selected-account image production and main-loop-owned artifact publication.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tau_client::{ArtifactClient, ArtifactUpload, ClientHandle, ClientResult, ToolNameScope};
use tau_proto::{
    ArtifactOp, ArtifactRequestId, ArtifactResult, ArtifactValue, CborValue, ProviderName,
    SessionId, ToolCallId, ToolName, ToolStarted,
};
use tau_provider_codex::image_generation::{self, GenerationCancellation, GenerationError};

use crate::{BuiltinProviderProfile, BuiltinProviderProfiles, ProviderRuntime, WorkerMessage};

#[cfg(test)]
mod tests;

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
            executor: image_generation::generate,
        }
    }
}

/// One accepted call; original bytes only live in upload state.
struct ImageCall {
    started: ToolStarted,
    session: SessionId,
    provider: ProviderName,
    prompt: String,
    cancel: Arc<GenerationCancellation>,
    /// Dropping a completed call wakes its deadline thread immediately.
    deadline: Option<mpsc::Sender<()>>,
    phase: Phase,
}

/// Paid effect can begin only after a correlated availability success.
enum Phase {
    Availability,
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
                description: Some("Generate one original PNG from a prompt using the selected ChatGPT account. Returns an artifact key, size and MIME type; use import(key) to obtain a local path. Original bytes persist independently of this session, including ephemeral sessions. No edits or batches.".to_owned()),
                tool_type: tau_proto::ToolType::Function,
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"prompt": {"type": "string", "minLength": 1, "maxLength": image_generation::MAX_PROMPT_BYTES}},
                    "required": ["prompt"],
                    "additionalProperties": false
                })),
                format: None,
                tags: Vec::new(),
                enabled_by_default: true,
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
        abort_upload(&call, handle);
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
        if self.calls.contains_key(&started.call_id) {
            return Ok(());
        }
        let Some(prompt) = parse_prompt(&started.arguments) else {
            return report_error(
                &started,
                "image prompt must be the only argument, nonempty and at most 32 KiB",
                handle,
            );
        };
        let Some(session) = self.session.clone() else {
            return report_error(&started, "image artifact session is unavailable", handle);
        };
        if self.calls.len() >= MAX_CALLS {
            return report_error(&started, "image generation capacity is busy", handle);
        }
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
        let Ok(upload) = ArtifactUpload::new(bytes) else {
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
            return handle.report_tool_result(tau_proto::ToolResult {
                call_id: call.started.call_id,
                tool_name: call.started.tool_name,
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Map(vec![
                    (
                        CborValue::Text("key".into()),
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
                display: None,
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
        abort_upload(&call, handle);
        handle.report_tool_cancelled(tau_proto::ToolCancelled {
            call_id: call.started.call_id,
            tool_name: call.started.tool_name,
            tool_type: tau_proto::ToolType::Function,
            presentation: Default::default(),
            display: None,
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
        if self.images.calls.contains_key(&id) {
            return Ok(());
        }
        self.images.start(started, handle)?;
        if let Some(call) = self.images.calls.get_mut(&id)
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
        let Some(id) = self.images.requests.remove(&result.request_id) else {
            return Ok(());
        };
        let Some(mut call) = self.images.calls.remove(&id) else {
            return Ok(());
        };
        let frame = tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(result.clone()));
        if !tau_proto::artifact_frame_fits(&frame) {
            abort_upload(&call, handle);
            return report_error(
                &call.started,
                "image artifact response exceeded limits",
                handle,
            );
        }
        let Ok(value) = result.result else {
            abort_upload(&call, handle);
            return report_error(
                &call.started,
                "image artifact storage is unavailable; generation will not be retried",
                handle,
            );
        };
        match &mut call.phase {
            Phase::Availability if value == ArtifactValue::Done => {
                self.start_available_image(call, handle)
            }
            Phase::Uploading(upload) => {
                if upload.accept(value).is_err() {
                    abort_upload(&call, handle);
                    return report_error(
                        &call.started,
                        "image artifact publication failed; generation will not be retried",
                        handle,
                    );
                }
                self.images.submit_upload(call, handle)
            }
            _ => report_error(&call.started, "unexpected image artifact response", handle),
        }
    }

    /// Resolves selected credentials only after storage admitted this call.
    fn start_available_image(
        &mut self,
        mut call: ImageCall,
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
        let executor = self.images.executor;
        let id = call.started.call_id.clone();
        call.phase = Phase::Generating;
        self.images.calls.insert(id.clone(), call);
        std::thread::spawn(move || {
            let result = executor(credentials, &prompt, id.as_str(), &network, &cancel);
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

fn parse_prompt(arguments: &CborValue) -> Option<String> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    let [(CborValue::Text(key), CborValue::Text(prompt))] = entries.as_slice() else {
        return None;
    };
    (key == "prompt"
        && !prompt.trim().is_empty()
        && prompt.len() <= image_generation::MAX_PROMPT_BYTES)
        .then(|| prompt.clone())
}

fn abort_upload(call: &ImageCall, handle: &ClientHandle) {
    if let Phase::Uploading(upload) = &call.phase
        && let Some(op) = upload.abort_op()
    {
        let _ = ArtifactClient::new(handle.clone()).start_request(call.session.clone(), op);
    }
}

fn report_error(started: &ToolStarted, message: &str, handle: &ClientHandle) -> ClientResult<()> {
    handle.report_tool_error(tau_proto::ToolError {
        call_id: started.call_id.clone(),
        tool_name: started.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        presentation: Default::default(),
        display: None,
        originator: started.originator.clone(),
    })
}
