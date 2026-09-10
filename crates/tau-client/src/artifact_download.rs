//! Bounded verified original-byte reads for shell, image, and attachment
//! consumers.

use tau_proto::{
    ARTIFACT_CHUNK_BYTES, ARTIFACT_MAX_BYTES, ArtifactDescriptor, ArtifactError, ArtifactKey,
    ArtifactOp, ArtifactReadId, ArtifactValue,
};

#[cfg(test)]
mod tests;

/// One pinned read; verification precedes access to the completed original
/// bytes.
pub struct ArtifactDownload {
    /// Requested canonical content identity.
    key: ArtifactKey,
    /// Open transfer identity, owned by the current connection.
    read: Option<ArtifactReadId>,
    /// Immutable original descriptor returned by Open.
    descriptor: Option<ArtifactDescriptor>,
    /// Accepted bounded original ranges.
    bytes: Vec<u8>,
    /// Whether digest and exact original size have been verified.
    verified: bool,
    /// Whether Close acknowledged the end of the bounded read.
    closed: bool,
}

impl ArtifactDownload {
    /// Creates a download for an already validated digest key.
    #[must_use]
    pub fn new(key: ArtifactKey) -> Self {
        Self {
            key,
            read: None,
            descriptor: None,
            bytes: Vec::new(),
            verified: false,
            closed: false,
        }
    }

    /// Returns the next request without advancing; read retries never renew
    /// age.
    #[must_use]
    pub fn next_op(&self) -> Option<ArtifactOp> {
        if self.closed {
            return None;
        }
        let Some(read) = &self.read else {
            return Some(ArtifactOp::Open {
                key: self.key.clone(),
            });
        };
        if self.verified {
            return Some(ArtifactOp::Close { read: read.clone() });
        }
        Some(ArtifactOp::Read {
            read: read.clone(),
            offset: self.bytes.len() as u64,
            length: ARTIFACT_CHUNK_BYTES as u32,
        })
    }

    /// Accepts one correlated response and validates exact ranges and final
    /// digest.
    ///
    /// Callers enforce response correlation and encoded frame bounds first.
    pub fn accept(&mut self, value: ArtifactValue) -> Result<(), ArtifactError> {
        match (self.next_op(), value) {
            (Some(ArtifactOp::Open { .. }), ArtifactValue::Opened { read, descriptor })
                if tau_proto::artifact_identifier(&read)
                    && descriptor.key == self.key
                    && descriptor.size.get() <= ARTIFACT_MAX_BYTES =>
            {
                self.read = Some(read);
                self.descriptor = Some(descriptor);
            }
            (
                Some(ArtifactOp::Read {
                    offset: expected, ..
                }),
                ArtifactValue::Chunk { offset, bytes, eof },
            ) => {
                let size = self
                    .descriptor
                    .as_ref()
                    .ok_or(ArtifactError::Invalid)?
                    .size
                    .get();
                let end = expected
                    .checked_add(bytes.len() as u64)
                    .ok_or(ArtifactError::Invalid)?;
                if offset != expected
                    || bytes.len() > ARTIFACT_CHUNK_BYTES
                    || size < end
                    || eof != (end == size)
                    || (bytes.is_empty() && !eof)
                {
                    return Err(ArtifactError::Integrity);
                }
                self.bytes.extend_from_slice(&bytes);
                if eof {
                    if tau_proto::artifact_digest(&self.key)
                        != Some(blake3::hash(&self.bytes).to_hex().as_str())
                    {
                        return Err(ArtifactError::Integrity);
                    }
                    self.verified = true;
                }
            }
            (Some(ArtifactOp::Close { .. }), ArtifactValue::Done) => self.closed = true,
            _ => return Err(ArtifactError::Invalid),
        }
        Ok(())
    }

    /// Supplies best-effort close on cancellation or verification failure.
    #[must_use]
    pub fn close_op(&self) -> Option<ArtifactOp> {
        self.read
            .as_ref()
            .map(|read| ArtifactOp::Close { read: read.clone() })
    }

    /// Returns verified original bytes only after releasing the read transfer.
    pub fn into_bytes(self) -> Result<Vec<u8>, ArtifactError> {
        if self.closed && self.verified {
            Ok(self.bytes)
        } else {
            Err(ArtifactError::Invalid)
        }
    }
}
