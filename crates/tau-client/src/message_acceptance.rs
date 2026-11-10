//! Nonblocking opt-in incoming message acceptance.

use std::sync::atomic::{AtomicU64, Ordering};

use tau_proto::{
    HarnessInputMessage, MessageAcceptanceRequest, MessageDelivered, ProtocolVersion,
    RawMessagePublisherId, SessionId,
};

use crate::{ClientError, ClientHandle};

/// Bounded request sender; the manual receive loop owns correlated results.
///
/// Retain the submitted semantic message until its result arrives. An output
/// error, timeout, cancellation or disconnect is not a negative
/// acknowledgement. Retrying the same message ID and content is safe within
/// retained history; never fall back to ordinary report emission.
#[derive(Clone)]
pub struct MessageAcceptanceClient {
    /// Existing configured runtime's bounded detached writer.
    handle: ClientHandle,
}

impl MessageAcceptanceClient {
    /// Attach without performing I/O or registering a separate receive loop.
    #[must_use]
    pub fn new(handle: ClientHandle) -> Self {
        Self { handle }
    }

    /// Enqueue after Ready and return correlation for
    /// `MessageAcceptanceResult`.
    ///
    /// Missing or incompatible Configure revision fails locally without
    /// sending. Successful enqueue is not acceptance. Only `Accepted`
    /// establishes that the recipient owns input, without any filesystem or
    /// execution guarantee.
    pub fn start_request(
        &self,
        session_id: SessionId,
        report: MessageDelivered<RawMessagePublisherId>,
        harness_protocol_version: Option<ProtocolVersion>,
    ) -> Result<String, ClientError> {
        validate_version(harness_protocol_version)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let request_id = format!(
            "message-acceptance-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        self.handle
            .send_detached(HarnessInputMessage::MessageAcceptanceRequest(Box::new(
                MessageAcceptanceRequest {
                    request_id: request_id.clone(),
                    session_id,
                    report,
                },
            )))?;
        Ok(request_id)
    }
}

/// Gate the new wire shape before any output on older or unknown runtimes.
fn validate_version(version: Option<ProtocolVersion>) -> Result<(), ClientError> {
    if !version.is_some_and(|version| version.major == 23 && version.minor >= 2) {
        return Err(ClientError::handler(
            "canonical message acceptance requires harness protocol 23.2 or later in major 23",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
