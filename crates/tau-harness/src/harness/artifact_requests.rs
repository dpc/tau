//! Configured-extension admission to shared artifacts, not extension-private
//! data.

use super::*;
use crate::artifact_worker::ArtifactWorker;

impl Harness {
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
        if !request.is_bounded()
            || !tau_proto::artifact_frame_fits(&HarnessInputMessage::ArtifactRequest(
                request.clone(),
            ))
        {
            return Err(ArtifactError::Invalid);
        }
        let owner = format!("{}/{}", entry.name, request.expected_session_id);
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
        if !self.extensions.entries.contains_key(connection)
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
