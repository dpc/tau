//! Validated, bounded originals for the subscription image edit endpoint.

use std::io::Cursor;

use image::ImageFormat;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;

use super::{GenerationError, MAX_IMAGE_BYTES};

/// Maximum ordered input occurrences, including repeated originals.
pub const MAX_ATTACHMENTS: usize = 5;
/// Aggregate original-byte budget, independently of encoded request limits.
pub const MAX_INPUT_BYTES: usize = 24 * 1024 * 1024;
/// Aggregate data URLs, including MIME prefixes and base64 padding.
pub(super) const MAX_DATA_URL_BYTES: usize = 32 * 1024 * 1024;
/// Serialized JSON budget, independently of source and data URL budgets.
pub(super) const MAX_REQUEST_BYTES: usize = 33 * 1024 * 1024;

/// One fully decoded, non-animated image whose original bytes remain unchanged.
///
/// No Debug projection exposes original bytes or metadata.
pub struct InputImage {
    /// Media type established by decoding, never supplied by the caller.
    pub(super) mime: &'static str,
    /// Bounded original including any embedded private metadata.
    pub(super) bytes: Vec<u8>,
}

impl InputImage {
    /// Validates PNG, JPEG or static WebP without resizing or re-encoding.
    ///
    /// Call on a worker: decoding is byte-heavy, bounded to one raster at a
    /// time.
    pub fn validate(bytes: Vec<u8>) -> Result<Self, GenerationError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(GenerationError::TooLarge);
        }
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(256 * 1024 * 1024);
        let format = image::guess_format(&bytes).map_err(|_| GenerationError::InvalidInput)?;
        let mime = match format {
            ImageFormat::Png => {
                let decoder = PngDecoder::with_limits(Cursor::new(&bytes), limits.clone())
                    .map_err(|_| GenerationError::InvalidInput)?;
                if decoder
                    .is_apng()
                    .map_err(|_| GenerationError::InvalidInput)?
                {
                    return Err(GenerationError::InvalidInput);
                }
                "image/png"
            }
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => {
                let decoder = WebPDecoder::new(Cursor::new(&bytes))
                    .map_err(|_| GenerationError::InvalidInput)?;
                if decoder.has_animation() {
                    return Err(GenerationError::InvalidInput);
                }
                "image/webp"
            }
            _ => return Err(GenerationError::InvalidInput),
        };
        let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
        reader.limits(limits);
        reader.decode().map_err(|_| GenerationError::InvalidInput)?;
        Ok(Self { mime, bytes })
    }
}
