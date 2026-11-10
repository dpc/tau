//! Nonblocking opt-in incoming message acceptance.

use std::sync::atomic::{AtomicU64, Ordering};

use tau_proto::{
    HarnessInputMessage, MessageAcceptanceMode, MessageAcceptanceRequest, MessageDelivered,
    ProtocolVersion, RawMessagePublisherId, SessionId,
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
        self.start_request_with_mode(
            session_id,
            report,
            harness_protocol_version,
            MessageAcceptanceMode::AcceptNew,
        )
    }

    /// Consult retained acceptance without admitting new input.
    ///
    /// Use after local policy declines new traffic while still allowing a
    /// matching retry. The same serialized core admission point waits behind
    /// earlier pending submissions; this is not a separate lookup followed by
    /// an emit. Unavailable authoritative history produces `Unknown`, never an
    /// inferred absence. Rejection refuses this RPC, not every earlier attempt.
    /// As with `start_request`, unsupported runtimes fail before enqueueing.
    pub fn start_existing_request(
        &self,
        session_id: SessionId,
        report: MessageDelivered<RawMessagePublisherId>,
        harness_protocol_version: Option<ProtocolVersion>,
    ) -> Result<String, ClientError> {
        self.start_request_with_mode(
            session_id,
            report,
            harness_protocol_version,
            MessageAcceptanceMode::ExistingOnly,
        )
    }

    /// Share version gating, correlation and bounded enqueue for both policies.
    fn start_request_with_mode(
        &self,
        session_id: SessionId,
        report: MessageDelivered<RawMessagePublisherId>,
        harness_protocol_version: Option<ProtocolVersion>,
        mode: MessageAcceptanceMode,
    ) -> Result<String, ClientError> {
        validate_version(harness_protocol_version)?;
        if report.incoming_notice.is_some()
            && !harness_protocol_version.is_some_and(|version| version.minor >= 3)
        {
            return Err(ClientError::handler(
                "incoming notices require harness protocol 23.3 or later in major 23",
            ));
        }
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
                    mode,
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
