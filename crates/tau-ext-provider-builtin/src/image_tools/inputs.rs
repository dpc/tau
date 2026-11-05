//! Sequential original downloads with a single aggregate byte allowance.

use std::collections::VecDeque;

use tau_client::ArtifactDownload;
use tau_proto::{ArtifactError, ArtifactKey, ArtifactOp, ArtifactValue};
use tau_provider_codex::image_generation::MAX_INPUT_BYTES;

/// One call's ordered original resolution, before selected-account dispatch.
pub(super) struct Inputs {
    /// References not yet opened; duplicates deliberately remain separate.
    remaining: VecDeque<ArtifactKey>,
    /// Sole current read, always closed before moving to the next input.
    download: ArtifactDownload,
    /// Verified originals, never preview bytes or caller-provided metadata.
    originals: Vec<Vec<u8>>,
    /// Completed original bytes charged against the aggregate allowance.
    completed_bytes: usize,
}

impl Inputs {
    /// Starts a nonempty, already count-validated ordered reference list.
    pub(super) fn new(keys: Vec<ArtifactKey>) -> Self {
        let mut remaining: VecDeque<_> = keys.into();
        let download = ArtifactDownload::new(remaining.pop_front().expect("nonempty inputs"));
        Self {
            remaining,
            download,
            originals: Vec::new(),
            completed_bytes: 0,
        }
    }

    /// Returns the next finite Open/Read/Close, or completion.
    pub(super) fn next_op(&self) -> Option<ArtifactOp> {
        self.download.next_op()
    }

    /// Validates correlated replies and descriptor budgets before reading
    /// bytes.
    pub(super) fn accept(&mut self, value: ArtifactValue) -> Result<(), ArtifactError> {
        if let ArtifactValue::Opened { descriptor, .. } = &value
            && descriptor.size.get() > (MAX_INPUT_BYTES - self.completed_bytes) as u64
        {
            return Err(ArtifactError::Invalid);
        }
        self.download.accept(value)?;
        if self.download.next_op().is_none()
            && let Some(key) = self.remaining.pop_front()
        {
            let finished = std::mem::replace(&mut self.download, ArtifactDownload::new(key));
            let bytes = finished.into_bytes()?;
            self.completed_bytes += bytes.len();
            self.originals.push(bytes);
        }
        Ok(())
    }

    /// Supplies cleanup authority even after a malformed chunk or cancellation.
    pub(super) fn close_op(&self) -> Option<ArtifactOp> {
        self.download.close_op()
    }

    /// Moves only digest-verified originals whose Close was acknowledged.
    pub(super) fn into_originals(mut self) -> Result<Vec<Vec<u8>>, ArtifactError> {
        self.originals.push(self.download.into_bytes()?);
        Ok(self.originals)
    }
}
