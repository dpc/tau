use sha2::{Digest as _, Sha256};

/// Maximum decoded response-body bytes retained in one private capture.
pub(super) const MAX_RETAINED_BODY_BYTES: usize = 64 * 1024;
/// Maximum credential length retained as cutoff lookahead.
pub(super) const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;
/// Prefix admitted before redaction, reserving room for a complete marker.
pub(super) const MAX_UNREDACTED_PREFIX_BYTES: usize =
    MAX_RETAINED_BODY_BYTES - b"<redacted-credential>".len();

/// Streaming bounded evidence collected from one non-success response body.
pub(in crate::responses) struct BodyCapture {
    /// Prefix retained for local forensic inspection.
    retained: Vec<u8>,
    /// Total bytes delivered by the transport before EOF or termination.
    decoded_bytes_received: u64,
    /// Digest over exactly `decoded_bytes_received`.
    hasher: Sha256,
    /// Bounded prefix plus credential-boundary lookahead.
    retain_limit: usize,
}

impl BodyCapture {
    /// Construct an empty bounded body capture.
    pub(in crate::responses) fn new(credential_lookahead: usize) -> Self {
        let credential_lookahead = credential_lookahead.min(MAX_CREDENTIAL_BYTES.saturating_sub(1));
        Self {
            retained: Vec::new(),
            decoded_bytes_received: 0,
            hasher: Sha256::new(),
            retain_limit: MAX_UNREDACTED_PREFIX_BYTES.saturating_add(credential_lookahead),
        }
    }

    /// Observe one decoded chunk, hashing/counting it completely while
    /// retaining only the 64-KiB evidence prefix plus bounded credential
    /// lookahead.
    pub(in crate::responses) fn push(&mut self, chunk: &[u8]) {
        self.decoded_bytes_received = self
            .decoded_bytes_received
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        self.hasher.update(chunk);
        let remaining = self.retain_limit.saturating_sub(self.retained.len());
        self.retained
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }

    /// Finish with honest EOF coverage; `complete` is false on cancellation,
    /// read failure, or size-bound termination.
    pub(in crate::responses) fn finish(self, complete: bool) -> CapturedBody {
        let truncated =
            u64::try_from(self.retained.len()).unwrap_or(u64::MAX) < self.decoded_bytes_received;
        let retention_limit_reached = self.retain_limit <= self.retained.len();
        CapturedBody {
            retained: self.retained,
            decoded_bytes_received: self.decoded_bytes_received,
            sha256_decoded_received: self.hasher.finalize().into(),
            complete,
            truncated,
            retention_limit_reached,
        }
    }

    /// Return whether the bounded evidence-plus-lookahead buffer is full.
    pub(in crate::responses) fn reached_retention_limit(&self) -> bool {
        self.retain_limit <= self.retained.len()
    }
}

/// Finished body evidence ready for serialization.
pub(in crate::responses) struct CapturedBody {
    /// Bounded decoded response prefix before credential redaction.
    pub(in crate::responses) retained: Vec<u8>,
    /// Exact bytes delivered before capture completion.
    pub(in crate::responses) decoded_bytes_received: u64,
    /// SHA-256 over exactly the delivered bytes.
    pub(in crate::responses) sha256_decoded_received: [u8; 32],
    /// Whether EOF established that the digest covers the complete body.
    pub(in crate::responses) complete: bool,
    /// Whether the retained prefix omitted delivered bytes.
    pub(in crate::responses) truncated: bool,
    /// Whether bounded lookahead was filled before capture stopped.
    pub(in crate::responses) retention_limit_reached: bool,
}

impl CapturedBody {
    /// Recover the legacy provider error text only for a complete, bounded
    /// UTF-8 body; other outcomes previously normalized to an empty string.
    pub(in crate::responses) fn provider_error_text(&self) -> String {
        (self.complete && !self.truncated)
            .then(|| String::from_utf8(self.retained.clone()).ok())
            .flatten()
            .unwrap_or_default()
    }
}
