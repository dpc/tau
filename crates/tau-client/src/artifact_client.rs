//! Nonblocking correlated artifact RPC for manual loops and worker producers.

use std::sync::atomic::{AtomicU64, Ordering};

use tau_proto::{ArtifactOp, ArtifactRequest, ArtifactRequestId, HarnessInputMessage, SessionId};

use crate::{ClientError, ClientHandle};

/// Cloneable outbound artifact client; the caller's normal loop owns responses.
///
/// Unlike synchronous extension-data reads this helper does not borrow or steal
/// runtime input. Match `HarnessOutputMessage::ArtifactResult.request_id` in
/// the existing loop so cancellation, disconnect, and other calls remain
/// responsive.
#[derive(Clone)]
pub struct ArtifactClient {
    /// Existing transport writer with its normal startup and frame bounds.
    handle: ClientHandle,
}

impl ArtifactClient {
    /// Reuses an already configured runtime's outbound handle.
    #[must_use]
    pub fn new(handle: ClientHandle) -> Self {
        Self { handle }
    }

    /// Sends one exact-session bounded request and returns its correlation id.
    ///
    /// A transport timeout or disconnect is an unknown operation outcome. Retry
    /// the same Write or Finalize upload identity, never a producer's paid
    /// effect. New Begin requests are distinct uploads. Availability must
    /// be checked before expensive generation in a possibly memory-only
    /// harness.
    pub fn start_request(
        &self,
        session: SessionId,
        op: ArtifactOp,
    ) -> Result<ArtifactRequestId, ClientError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request_id =
            ArtifactRequestId::parse(format!("artifact-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
                .map_err(|_| ClientError::InvalidArtifactRequest)?;
        let request = ArtifactRequest {
            request_id: request_id.clone(),
            expected_session_id: session,
            op,
        };
        if !request.is_bounded() {
            return Err(ClientError::InvalidArtifactRequest);
        }
        let message = HarnessInputMessage::ArtifactRequest(request);
        if !tau_proto::artifact_frame_fits(&message) {
            return Err(ClientError::InvalidArtifactRequest);
        }
        self.handle.send_detached(message)?;
        Ok(request_id)
    }
}
