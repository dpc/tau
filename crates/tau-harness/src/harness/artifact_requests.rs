//! Separate configured-extension and authenticated UI artifact authority.

use super::*;
use crate::artifact_worker::ArtifactWorker;

/// Exact session binding accepted by the UI's existing Hello handshake.
pub(super) struct UiArtifactAdmission {
    /// Textual session name is not enough to survive a binding replacement.
    pub(super) session_id: tau_proto::SessionId,
    /// Process-local generation captured at successful exact-session admission.
    pub(super) generation: SessionGeneration,
}

impl Harness {
    /// Admits upload-only requests from the authenticated attached socket UI.
    pub(super) fn handle_ui_artifact_request(
        &mut self,
        connection: &tau_proto::ConnectionId,
        request: tau_proto::ArtifactRequest,
    ) {
        use tau_proto::{ArtifactError, ArtifactOp};
        let id = request.request_id.clone();
        let result = (|| {
            if !self.is_attached_socket_ui(connection)
                || self.session_runtime.storage_mode.is_memory_only()
                || !matches!(
                    request.op,
                    ArtifactOp::Available
                        | ArtifactOp::Begin { .. }
                        | ArtifactOp::Write { .. }
                        | ArtifactOp::Finalize { .. }
                        | ArtifactOp::Abort { .. }
                )
            {
                return Err(ArtifactError::Permission);
            }
            let admission = self
                .ui_runtime
                .artifact_admissions
                .get(connection)
                .ok_or(ArtifactError::Permission)?;
            if request.expected_session_id != self.session_runtime.current_session_id
                || admission.session_id != request.expected_session_id
                || admission.generation != self.session_runtime.current_session_generation
            {
                return Err(ArtifactError::SessionMismatch);
            }
            let owner = format!("ui/{connection}/{}", request.expected_session_id);
            self.submit_artifact_request(connection, request, owner)
        })();
        if let Err(error) = result {
            self.send_artifact_result(
                connection,
                tau_proto::ArtifactResult {
                    request_id: id,
                    result: Err(error),
                },
            );
        }
    }

    /// Captures exact peer/session authority before bounded asynchronous I/O.
    pub(super) fn handle_artifact_request(
        &mut self,
        connection: &tau_proto::ConnectionId,
        request: tau_proto::ArtifactRequest,
        admission: ExtensionFrameAdmission,
    ) {
        let id = request.request_id.clone();
        let result = self.admit_artifact_request(connection, request, admission);
        if let Err(error) = result {
            self.send_artifact_result(
                connection,
                tau_proto::ArtifactResult {
                    request_id: id,
                    result: Err(error),
                },
            );
        }
    }

    fn admit_artifact_request(
        &mut self,
        connection: &tau_proto::ConnectionId,
        request: tau_proto::ArtifactRequest,
        admission: ExtensionFrameAdmission,
    ) -> Result<(), tau_proto::ArtifactError> {
        use tau_proto::ArtifactError;
        // Actual harness mode, not session journal persistence, owns this test.
        // No root construction, stat, startup cleanup, or worker I/O precedes
        // it.
        if self.session_runtime.storage_mode.is_memory_only() {
            return Err(ArtifactError::Permission);
        }
        let entry = self
            .extensions
            .entries
            .get(connection)
            .ok_or(ArtifactError::Permission)?;
        if admission.session_id != self.session_runtime.current_session_id
            || admission.session_generation != self.session_runtime.current_session_generation
            || request.expected_session_id != admission.session_id
        {
            return Err(ArtifactError::SessionMismatch);
        }
        let owner = format!("{}/{}", entry.name, request.expected_session_id);
        self.submit_artifact_request(connection, request, owner)
    }

    /// Shares bounded worker admission without sharing peer authority.
    fn submit_artifact_request(
        &mut self,
        connection: &tau_proto::ConnectionId,
        request: tau_proto::ArtifactRequest,
        owner: String,
    ) -> Result<(), tau_proto::ArtifactError> {
        use tau_proto::ArtifactError;
        if !request.is_bounded()
            || !tau_proto::artifact_frame_fits(&HarnessInputMessage::ArtifactRequest(
                request.clone(),
            ))
        {
            return Err(ArtifactError::Invalid);
        }
        if self.runtime_io.artifacts.is_none() {
            self.runtime_io.artifacts = Some(ArtifactWorker::start(
                &self.session_runtime.state_dir,
                self.runtime_io.tx.clone(),
            )?);
        }
        self.runtime_io
            .artifacts
            .as_mut()
            .ok_or(ArtifactError::Io)?
            .submit(owner, connection.clone(), request)
    }

    /// Routes only a complete bounded directed frame; no bytes enter
    /// publication.
    pub(super) fn send_artifact_result(
        &mut self,
        connection: &tau_proto::ConnectionId,
        result: tau_proto::ArtifactResult,
    ) {
        if (!self.extensions.entries.contains_key(connection)
            && !self.is_attached_socket_ui(connection))
            || self.runtime_io.bus.connection(connection).is_none()
        {
            return;
        }
        let id = result.request_id.clone();
        let mut message = HarnessOutputMessage::ArtifactResult(Box::new(result));
        if !tau_proto::artifact_frame_fits(&message) {
            message = HarnessOutputMessage::ArtifactResult(Box::new(tau_proto::ArtifactResult {
                request_id: id,
                result: Err(tau_proto::ArtifactError::Invalid),
            }));
        }
        let delivered = self
            .runtime_io
            .bus
            .send_to(connection, None, message)
            .is_ok_and(|report| report.delivered_to.contains(connection));
        if !delivered {
            // Never queue another Artifact error on the already-full lane.
            // Retiring this exact recipient releases its retained charges and
            // revokes further transfer admission without disturbing other
            // peers.
            self.handle_disconnect(connection);
        }
    }
}
