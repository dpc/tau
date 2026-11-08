//! Bounded verified original-byte reads for shell, image, and attachment
//! consumers.

use tau_proto::{
    ARTIFACT_CHUNK_BYTES, ArtifactDescriptor, ArtifactError, ArtifactKey, ArtifactOp,
    ArtifactReadId, ArtifactSize, ArtifactValue,
};

#[cfg(test)]
mod tests;

/// One pinned read; verification precedes access to the completed original
/// bytes.
pub struct ArtifactDownload {
    /// Consumer-specific allocation ceiling, checked before reading any chunks.
    max_bytes: ArtifactSize,
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
        Self::with_max_bytes(
            key,
            ArtifactSize::new(16 * 1024 * 1024).expect("legacy limit"),
        )
    }

    /// Creates a download with an explicit consumer byte budget. The ordinary
    /// constructor retains its 16 MiB limit for image and clipboard consumers.
    #[must_use]
    pub fn with_max_bytes(key: ArtifactKey, max_bytes: ArtifactSize) -> Self {
        Self {
            max_bytes,
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

    /// Returns the immutable descriptor after the open response is accepted.
    #[must_use]
    pub fn descriptor(&self) -> Option<&ArtifactDescriptor> {
        self.descriptor.as_ref()
    }

    /// Accepts one correlated response and validates exact ranges and final
    /// digest.
    ///
    /// Callers enforce response correlation and encoded frame bounds first.
    pub fn accept(&mut self, value: ArtifactValue) -> Result<(), ArtifactError> {
        match (self.next_op(), value) {
            (Some(ArtifactOp::Open { .. }), ArtifactValue::Opened { read, descriptor }) => {
                if !tau_proto::artifact_identifier(&read) {
                    return Err(ArtifactError::Invalid);
                }
                // Retain Close authority even when this consumer rejects the
                // descriptor; do not leave a valid server read pinned to
                // expiry.
                self.read = Some(read);
                if descriptor.key != self.key || descriptor.size.get() > self.max_bytes.get() {
                    return Err(ArtifactError::Invalid);
                }
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
                    self.descriptor
                        .as_ref()
                        .ok_or(ArtifactError::Integrity)?
                        .verify(&self.bytes)?;
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
