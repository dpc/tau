//! Retry-safe upload state, independent of runtime threading and tool
//! lifecycle.

use tau_proto::{
    ARTIFACT_CHUNK_BYTES, ArtifactDescriptor, ArtifactError, ArtifactOp, ArtifactUploadId,
    ArtifactValue,
};

#[cfg(test)]
mod tests;

/// A producer's original bytes and next idempotent upload operation.
///
/// Keep this state until the correlated response arrives. `next_op` does not
/// advance state, so response loss can resend a matching chunk or finalization.
/// Before Begin succeeds an unknown Begin can only leave expiring staging.
pub struct ArtifactUpload {
    /// Original bytes retained without transformations or media interpretation.
    bytes: Vec<u8>,
    /// Validated original byte length, retained through request construction.
    size: tau_proto::ArtifactSize,
    /// Upload identity after successful Begin.
    upload: Option<ArtifactUploadId>,
    /// Next acknowledged byte offset.
    offset: usize,
    /// Final descriptor only after verified successful publication.
    descriptor: Option<ArtifactDescriptor>,
}

impl ArtifactUpload {
    /// Validates the object bound before beginning any upload.
    pub fn new(bytes: Vec<u8>) -> Result<Self, ArtifactError> {
        let size = tau_proto::ArtifactSize::new(bytes.len() as u64)?;
        Ok(Self {
            bytes,
            size,
            upload: None,
            offset: 0,
            descriptor: None,
        })
    }

    /// Returns the next request without changing state; safe to resend on loss.
    #[must_use]
    pub fn next_op(&self) -> Option<ArtifactOp> {
        if self.descriptor.is_some() {
            return None;
        }
        let Some(upload) = &self.upload else {
            return Some(ArtifactOp::Begin { size: self.size });
        };
        if self.offset == self.bytes.len() {
            Some(ArtifactOp::Finalize {
                upload: upload.clone(),
            })
        } else {
            let end = (self.offset + ARTIFACT_CHUNK_BYTES).min(self.bytes.len());
            Some(ArtifactOp::Write {
                upload: upload.clone(),
                offset: self.offset as u64,
                bytes: self.bytes[self.offset..end].to_vec(),
            })
        }
    }

    /// Accepts only the successful value for the currently outstanding request.
    ///
    /// Callers must first validate response correlation and enforce the
    /// complete encoded response frame bound. Errors leave this state
    /// unchanged.
    pub fn accept(&mut self, value: ArtifactValue) -> Result<(), ArtifactError> {
        match (self.next_op(), value) {
            (Some(ArtifactOp::Begin { .. }), ArtifactValue::Upload { upload })
                if tau_proto::artifact_identifier(&upload) =>
            {
                self.upload = Some(upload);
            }
            (Some(ArtifactOp::Write { bytes, .. }), ArtifactValue::Written { next_offset })
                if next_offset == (self.offset + bytes.len()) as u64 =>
            {
                self.offset = next_offset as usize;
            }
            (Some(ArtifactOp::Finalize { .. }), ArtifactValue::Descriptor(descriptor)) => {
                if descriptor.size.get() != self.bytes.len() as u64
                    || tau_proto::artifact_digest(&descriptor.key)
                        != Some(blake3::hash(&self.bytes).to_hex().as_str())
                {
                    return Err(ArtifactError::Integrity);
                }
                self.descriptor = Some(descriptor);
            }
            _ => return Err(ArtifactError::Invalid),
        }
        Ok(())
    }

    /// Supplies best-effort cancellation, never deletion of committed
    /// originals.
    #[must_use]
    pub fn abort_op(&self) -> Option<ArtifactOp> {
        self.upload.as_ref().map(|upload| ArtifactOp::Abort {
            upload: upload.clone(),
        })
    }

    /// Returns the synced-publication result, absent while any stage is
    /// pending.
    #[must_use]
    pub fn descriptor(&self) -> Option<&ArtifactDescriptor> {
        self.descriptor.as_ref()
    }
}
