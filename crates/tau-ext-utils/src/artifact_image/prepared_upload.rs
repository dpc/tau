//! Upload exactly the prepared encoded image, retaining metadata but no second
//! inline copy while the directed transfer is pending.

use tau_client::ArtifactUpload;
use tau_proto::{
    ArtifactError, ArtifactOp, ArtifactValue, ImageDetail, ImageMediaType, ImageReference,
    ToolResult, ToolResultContentPart,
};

use crate::read_image::MAX_ENCODED_BYTES;

/// A bounded prepared payload and its terminal metadata awaiting durable
/// upload.
pub(super) struct PreparedImageUpload {
    /// Existing retry-safe Begin/Write/Finalize state machine.
    upload: ArtifactUpload,
    /// Original causal result with its inline content removed during transfer.
    result: ToolResult,
    /// Actual prepared image media type, not an original artifact hint.
    media_type: ImageMediaType,
    /// Prepared decoded width.
    width: u32,
    /// Prepared decoded height.
    height: u32,
    /// Provider detail selected during preparation.
    detail: ImageDetail,
}

impl PreparedImageUpload {
    /// Takes one bounded prepared image without retaining a second inline copy.
    pub(super) fn new(mut result: ToolResult) -> Result<Self, ArtifactError> {
        if result.provider_content.len() != 1 {
            return Err(ArtifactError::Invalid);
        }
        let Some(ToolResultContentPart::Image(image)) = result.provider_content.pop() else {
            return Err(ArtifactError::Invalid);
        };
        if image.data.is_empty() || MAX_ENCODED_BYTES < image.data.len() {
            return Err(ArtifactError::Invalid);
        }
        Ok(Self {
            upload: ArtifactUpload::with_metadata(
                image.data.to_vec(),
                None,
                Some(image.media_type.mime_type().into()),
            )?,
            result,
            media_type: image.media_type,
            width: image.width,
            height: image.height,
            detail: image.detail,
        })
    }

    /// Returns the next bounded directed operation without advancing state.
    pub(super) fn next_op(&self) -> Option<ArtifactOp> {
        self.upload.next_op()
    }

    /// Verifies the correlated response, including final prepared size and
    /// digest.
    pub(super) fn accept(&mut self, value: ArtifactValue) -> Result<(), ArtifactError> {
        self.upload.accept(value)
    }

    /// Returns best-effort cancellation; committed shared bytes are never
    /// deleted.
    pub(super) fn abort_op(&self) -> Option<ArtifactOp> {
        self.upload.abort_op()
    }

    /// Reports whether a late Begin can still allocate an unknown server
    /// upload.
    pub(super) fn awaiting_allocation(&self) -> bool {
        self.upload.abort_op().is_none()
    }

    /// Reports verified publication without allocating another outgoing chunk.
    pub(super) fn is_complete(&self) -> bool {
        self.upload.descriptor().is_some()
    }

    /// Produces reference-only typed content only after verified Finalize
    /// success.
    pub(super) fn into_result(mut self) -> Result<ToolResult, ArtifactError> {
        let blob = self
            .upload
            .descriptor()
            .cloned()
            .ok_or(ArtifactError::Invalid)?;
        self.result.provider_content =
            vec![ToolResultContentPart::ImageReference(ImageReference {
                blob,
                media_type: self.media_type,
                width: self.width,
                height: self.height,
                detail: self.detail,
            })];
        Ok(self.result)
    }
}
