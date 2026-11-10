//! One closed fixed-image upload for the durable-reference acceptance fixture.

use tau_client::{ArtifactClient, ArtifactUpload, ClientHandle, ClientResult};
use tau_proto::{
    ArtifactRequestId, ArtifactResult, PreparedImageStorage, SessionId, ToolCallId, ToolStarted,
};

/// A single bounded fixture invocation, independent of deterministic hold
/// modes.
#[derive(Default)]
pub(super) struct TypedImageUpload {
    /// Current harness session announcement, never a tool argument.
    pub(super) session: Option<SessionId>,
    /// Exact sent request whose reply may advance the upload.
    request: Option<ArtifactRequestId>,
    /// Retained invocation until publication, failure or canceled cleanup.
    active: Option<Active>,
}

/// Captured upload identity and its immutable compiled payload.
struct Active {
    /// Original routed call and storage policy.
    invoke: ToolStarted,
    /// Session captured before the first upload operation.
    session: SessionId,
    /// Existing verified descriptor-producing uploader.
    upload: ArtifactUpload,
    /// Cancellation already owns the tool terminal.
    canceled: bool,
}

impl TypedImageUpload {
    /// Honors the harness policy without changing the fixture's fixed bytes.
    pub(super) fn start(&mut self, invoke: ToolStarted, handle: &ClientHandle) -> ClientResult<()> {
        if invoke.invocation_policy.prepared_image_storage == PreparedImageStorage::Inline {
            return handle.report_tool_result(super::typed_image_success(invoke));
        }
        let Some(session) = self.session.clone() else {
            return report_error(&invoke, handle);
        };
        if let Some(active) = &self.active {
            return if active.invoke.call_id == invoke.call_id {
                Ok(())
            } else {
                report_error(&invoke, handle)
            };
        }
        self.active = Some(Active {
            invoke,
            session,
            upload: ArtifactUpload::new(super::TYPED_IMAGE_PNG.to_vec())
                .expect("compiled image is within the artifact bound"),
            canceled: false,
        });
        self.submit(handle)
    }

    /// Advances only an exact correlated bounded Artifact reply.
    pub(super) fn result(
        &mut self,
        result: ArtifactResult,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        if self.request.as_ref() != Some(&result.request_id) {
            return Ok(());
        }
        self.request = None;
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let bounded = tau_proto::artifact_frame_fits(
            &tau_proto::HarnessOutputMessage::ArtifactResult(Box::new(result.clone())),
        );
        let accepted = bounded
            && result
                .result
                .and_then(|value| active.upload.accept(value))
                .is_ok();
        if !accepted || active.canceled {
            let active = self.active.take().expect("matched upload");
            abort(&active, handle);
            return if active.canceled {
                Ok(())
            } else {
                report_error(&active.invoke, handle)
            };
        }
        if let Some(blob) = active.upload.descriptor().cloned() {
            let active = self.active.take().expect("completed upload");
            let mut result = super::typed_image_success(active.invoke);
            result.provider_content = vec![tau_proto::ToolResultContentPart::ImageReference(
                tau_proto::ImageReference {
                    blob,
                    media_type: tau_proto::ImageMediaType::Png,
                    width: 1,
                    height: 1,
                    detail: tau_proto::ImageDetail::High,
                },
            )];
            return handle.report_tool_result(result);
        }
        self.submit(handle)
    }

    /// Cancellation keeps pending allocation correlation until best-effort
    /// cleanup can observe its returned upload identity.
    pub(super) fn cancel(
        &mut self,
        call_id: &ToolCallId,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| &active.invoke.call_id == call_id)
        else {
            return Ok(());
        };
        if active.canceled {
            return Ok(());
        }
        active.canceled = true;
        abort(active, handle);
        handle.report_tool_cancelled(tau_proto::ToolCancelled {
            output: None,
            captured_output: None,
            presentation: Default::default(),
            call_id: active.invoke.call_id.clone(),
            tool_name: active.invoke.tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            display: None,
        })
    }

    /// Teardown releases known staging; lost allocations retain server expiry.
    pub(super) fn shutdown(&mut self, handle: &ClientHandle) {
        if let Some(active) = self.active.take() {
            abort(&active, handle);
        }
        self.request = None;
    }

    /// Sends the next operation through the ordinary bounded SDK writer.
    fn submit(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        let active = self.active.as_ref().expect("upload");
        let op = active.upload.next_op().expect("unfinished upload");
        self.request =
            Some(ArtifactClient::new(handle.clone()).start_request(active.session.clone(), op)?);
        Ok(())
    }
}

/// Releases only staging, never a finalized shared object.
fn abort(active: &Active, handle: &ClientHandle) {
    if let Some(op) = active.upload.abort_op() {
        let _ = ArtifactClient::new(handle.clone()).start_request(active.session.clone(), op);
    }
}

/// Failure text is fixed and never carries fetchable keys or private paths.
fn report_error(invoke: &ToolStarted, handle: &ClientHandle) -> ClientResult<()> {
    handle.report_tool_error(tau_proto::ToolError {
        captured_output: None,
        presentation: Default::default(),
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: tau_proto::ToolType::Function,
        message: "image-publication-unavailable".to_owned(),
        details: Some(tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Text("output".to_owned()),
            tau_proto::CborValue::Text("fixed image artifact publication unavailable".to_owned()),
        )])),
        display: None,
        originator: invoke.originator.clone(),
    })
}
