//! Bounded local raster decoding for the `read_image` tool.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};

use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader};
use tau_proto::{
    CborValue, ImageContent, ImageDetail, ImageMediaType, ToolResultContentPart, ToolUseStats,
};

use crate::argument::argument_text;
use crate::display::{ToolFailure, ToolOutput, ok_display};
use crate::tools::world::ShellWorld;

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 8192;
const MAX_SOURCE_PIXELS: u64 = 16_777_216;
const MAX_WEBP_SOURCE_PIXELS: u64 = 4_194_304;
const MAX_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HIGH_SIDE: u32 = 2048;
const MAX_HIGH_PATCHES: u64 = 2500;
const PATCH_SIDE: u64 = 32;
const MAX_CONCURRENT_DECODES: usize = 1;

static DECODE_PERMITS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

/// RAII permit limiting aggregate decoded-image memory in this extension.
struct DecodePermit;

impl DecodePermit {
    /// Wait for one of the fixed decode-memory slots.
    fn acquire() -> Self {
        let (mutex, changed) =
            DECODE_PERMITS.get_or_init(|| (Mutex::new(MAX_CONCURRENT_DECODES), Condvar::new()));
        let available = mutex.lock().expect("image decode permit lock poisoned");
        let mut available = changed
            .wait_while(available, |available| *available == 0)
            .expect("image decode permit lock poisoned");
        *available -= 1;
        Self
    }
}

impl Drop for DecodePermit {
    fn drop(&mut self) {
        let (mutex, changed) =
            DECODE_PERMITS.get_or_init(|| (Mutex::new(MAX_CONCURRENT_DECODES), Condvar::new()));
        let mut available = mutex.lock().expect("image decode permit lock poisoned");
        *available += 1;
        changed.notify_one();
    }
}

/// Read, validate, normalize, and return one local raster image.
pub(crate) fn read_image(
    arguments: &CborValue,
    world: &mut ShellWorld,
) -> Result<ToolOutput, ToolFailure> {
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let path = PathBuf::from(path);
    let display_path = path.display().to_string();
    let bytes = world
        .read_file_limited(&path, MAX_SOURCE_BYTES)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_path.clone()))?;

    let _permit = DecodePermit::acquire();
    let image = prepare_image(&bytes)
        .map_err(|message| ToolFailure::from(message).with_args(display_path.clone()))?;
    let format = image.media_type.mime_type();
    let byte_count = image.data.len();
    let summary = format!(
        "{format} image, {}x{}, {byte_count} bytes, high detail",
        image.width, image.height
    );
    let result = CborValue::Map(vec![
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(summary),
        ),
        (
            CborValue::Text("media_type".to_owned()),
            CborValue::Text(format.to_owned()),
        ),
        (
            CborValue::Text("width".to_owned()),
            CborValue::Integer(i64::from(image.width).into()),
        ),
        (
            CborValue::Text("height".to_owned()),
            CborValue::Integer(i64::from(image.height).into()),
        ),
        (
            CborValue::Text("bytes".to_owned()),
            CborValue::Integer(i64::try_from(byte_count).unwrap_or(i64::MAX).into()),
        ),
        (
            CborValue::Text("detail".to_owned()),
            CborValue::Text("high".to_owned()),
        ),
    ]);
    let display_args = format!(
        "{display_path}  {format} {}x{}  {byte_count} bytes  high",
        image.width, image.height
    );
    let mut display = ok_display(display_args);
    display.stats = ToolUseStats {
        bytes: Some(byte_count as u64),
        ..ToolUseStats::default()
    };
    Ok(ToolOutput {
        result,
        provider_content: vec![ToolResultContentPart::Image(image)],
        display,
    })
}

fn prepare_image(bytes: &[u8]) -> Result<ImageContent, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader = reader
        .with_guessed_format()
        .map_err(|error| format!("cannot inspect image format: {error}"))?;
    let source_format = reader
        .format()
        .and_then(supported_format)
        .ok_or_else(|| "unsupported image format; expected PNG, JPEG, or WebP".to_owned())?;
    reject_animation(bytes, source_format)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("cannot decode image header: {error}"))?;
    let (width, height) = decoder.dimensions();
    validate_source_dimensions(width, height, source_format)?;
    let decoded_bytes = decoder.total_bytes();
    validate_decoded_allocation(source_format, decoded_bytes)?;
    let orientation = decoder
        .orientation()
        .map_err(|error| format!("cannot inspect image orientation: {error}"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("cannot decode image: {error}"))?;
    decoded.apply_orientation(orientation);
    decoded = resize_for_high_detail(decoded);
    let (width, height) = decoded.dimensions();

    let mut data = Cursor::new(Vec::new());
    decoded
        .write_to(&mut data, image_format(source_format))
        .map_err(|error| format!("cannot normalize image: {error}"))?;
    let data = data.into_inner();
    if MAX_ENCODED_BYTES < data.len() {
        return Err(format!(
            "normalized image exceeds {MAX_ENCODED_BYTES} byte limit"
        ));
    }
    Ok(ImageContent {
        media_type: source_format,
        data: data.into(),
        width,
        height,
        detail: ImageDetail::High,
    })
}

fn validate_decoded_allocation(
    media_type: ImageMediaType,
    decoded_bytes: u64,
) -> Result<(), String> {
    let decoded_byte_limit = if media_type == ImageMediaType::Webp {
        MAX_DECODE_ALLOC_BYTES / 2
    } else {
        MAX_DECODE_ALLOC_BYTES
    };
    if decoded_byte_limit < decoded_bytes {
        Err(format!(
            "decoded image requires {decoded_bytes} bytes; limit is {decoded_byte_limit}"
        ))
    } else {
        Ok(())
    }
}

fn supported_format(format: ImageFormat) -> Option<ImageMediaType> {
    match format {
        ImageFormat::Png => Some(ImageMediaType::Png),
        ImageFormat::Jpeg => Some(ImageMediaType::Jpeg),
        ImageFormat::WebP => Some(ImageMediaType::Webp),
        _ => None,
    }
}

fn image_format(media_type: ImageMediaType) -> ImageFormat {
    match media_type {
        ImageMediaType::Png => ImageFormat::Png,
        ImageMediaType::Jpeg => ImageFormat::Jpeg,
        ImageMediaType::Webp => ImageFormat::WebP,
    }
}

fn validate_source_dimensions(
    width: u32,
    height: u32,
    media_type: ImageMediaType,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("image dimensions must be non-zero".to_owned());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "image dimensions overflow".to_owned())?;
    if MAX_SOURCE_PIXELS < pixels {
        return Err(format!(
            "image has {pixels} decoded pixels; limit is {MAX_SOURCE_PIXELS}"
        ));
    }
    if media_type == ImageMediaType::Webp && MAX_WEBP_SOURCE_PIXELS < pixels {
        return Err(format!(
            "WebP image has {pixels} decoded pixels; limit is {MAX_WEBP_SOURCE_PIXELS}"
        ));
    }
    Ok(())
}

fn resize_for_high_detail(mut image: DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    let side_scale = f64::from(MAX_HIGH_SIDE) / f64::from(width.max(height));
    let patches = patch_count(width, height);
    let patch_scale = (MAX_HIGH_PATCHES as f64 / patches as f64).sqrt();
    let scale = 1.0_f64.min(side_scale).min(patch_scale);
    if scale < 1.0 {
        let target_width = (f64::from(width) * scale).floor().max(1.0) as u32;
        let target_height = (f64::from(height) * scale).floor().max(1.0) as u32;
        image = image.resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        );
    }
    while MAX_HIGH_PATCHES < patch_count(image.width(), image.height()) {
        let width = image.width().saturating_sub(1).max(1);
        let height = image.height().saturating_sub(1).max(1);
        image = image.resize_exact(width, height, image::imageops::FilterType::Triangle);
    }
    image
}

fn patch_count(width: u32, height: u32) -> u64 {
    u64::from(width)
        .div_ceil(PATCH_SIDE)
        .saturating_mul(u64::from(height).div_ceil(PATCH_SIDE))
}

fn reject_animation(bytes: &[u8], media_type: ImageMediaType) -> Result<(), String> {
    let animated = match media_type {
        ImageMediaType::Png => png_has_animation_chunk(bytes),
        ImageMediaType::Webp => webp_has_animation_chunk(bytes),
        ImageMediaType::Jpeg => false,
    };
    if animated {
        Err("animated images are not supported".to_owned())
    } else {
        Ok(())
    }
}

fn png_has_animation_chunk(bytes: &[u8]) -> bool {
    let mut offset = 8_usize;
    while offset
        .checked_add(12)
        .is_some_and(|chunk_end| chunk_end <= bytes.len())
    {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        let Some(next) = offset
            .checked_add(12)
            .and_then(|offset| offset.checked_add(length))
        else {
            return false;
        };
        if bytes.len() < next {
            return false;
        }
        offset = next;
    }
    false
}

fn webp_has_animation_chunk(bytes: &[u8]) -> bool {
    let mut offset = 12_usize;
    while offset
        .checked_add(8)
        .is_some_and(|header_end| header_end <= bytes.len())
    {
        let kind = &bytes[offset..offset + 4];
        if kind == b"ANIM" || kind == b"ANMF" {
            return true;
        }
        let length = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let Some(next) = offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(length))
            .and_then(|offset| offset.checked_add(length % 2))
        else {
            return false;
        };
        if bytes.len() < next {
            return false;
        }
        offset = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures every v1 format is decoded and deterministically re-encoded as
    /// the same closed media type while retaining truthful dimensions.
    #[test]
    fn prepares_png_jpeg_and_webp() {
        for (format, expected) in [
            (ImageFormat::Png, ImageMediaType::Png),
            (ImageFormat::Jpeg, ImageMediaType::Jpeg),
            (ImageFormat::WebP, ImageMediaType::Webp),
        ] {
            let source = DynamicImage::new_rgb8(3, 2);
            let mut bytes = Cursor::new(Vec::new());
            source.write_to(&mut bytes, format).expect("encode fixture");
            let prepared = prepare_image(bytes.get_ref()).expect("prepare image");
            assert_eq!(prepared.media_type, expected);
            assert_eq!((prepared.width, prepared.height), (3, 2));
            assert!(!prepared.data.is_empty());
        }
    }

    /// Ensures extension or filename claims cannot make unsupported bytes cross
    /// the typed image boundary.
    #[test]
    fn rejects_non_raster_input() {
        let error = prepare_image(b"<svg/>").expect_err("SVG is unsupported");
        assert!(error.contains("unsupported image format"));
    }

    /// Ensures high-detail preparation resizes large square images to the patch
    /// budget rather than allowing provider cost to escape the local bound.
    #[test]
    fn high_detail_resize_obeys_patch_budget() {
        let image = DynamicImage::new_rgba8(3000, 3000);
        let resized = resize_for_high_detail(image);
        assert!(resized.width() <= MAX_HIGH_SIDE);
        assert!(resized.height() <= MAX_HIGH_SIDE);
        assert!(patch_count(resized.width(), resized.height()) <= MAX_HIGH_PATCHES);
    }

    /// Ensures a 4096-square RGBA16 decode is rejected from decoder-reported
    /// bytes before allocating its roughly 128 MiB output buffer.
    #[test]
    fn rejects_rgba16_sized_decode_allocation() {
        let rgba16_bytes = 4096_u64 * 4096 * 8;
        assert!(
            validate_decoded_allocation(ImageMediaType::Png, rgba16_bytes)
                .expect_err("oversized decoded allocation")
                .contains("limit")
        );
    }

    /// Ensures WebP's decoder workspace uncertainty receives a stricter source
    /// pixel limit than PNG and JPEG.
    #[test]
    fn rejects_webp_above_workspace_pixel_budget() {
        assert!(validate_source_dimensions(2048, 2049, ImageMediaType::Webp).is_err());
        assert!(validate_source_dimensions(2048, 2049, ImageMediaType::Png).is_ok());
    }

    /// Ensures APNG and animated-WebP control chunks are rejected before any
    /// frame decode, even when the generic image decoder would expose frame
    /// one.
    #[test]
    fn rejects_png_and_webp_animation_chunks() {
        let mut apng = b"\x89PNG\r\n\x1a\n".to_vec();
        apng.extend_from_slice(&0_u32.to_be_bytes());
        apng.extend_from_slice(b"acTL");
        apng.extend_from_slice(&0_u32.to_be_bytes());
        assert!(reject_animation(&apng, ImageMediaType::Png).is_err());

        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"ANIM");
        webp.extend_from_slice(&0_u32.to_le_bytes());
        assert!(reject_animation(&webp, ImageMediaType::Webp).is_err());
    }

    /// Ensures image debug output exposes metadata but never raw pixel bytes.
    #[test]
    fn image_content_debug_redacts_bytes() {
        let image = ImageContent {
            media_type: ImageMediaType::Png,
            data: vec![1, 2, 3, 4].into(),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        };
        let debug = format!("{image:?}");
        assert!(debug.contains("<4 bytes>"));
        assert!(!debug.contains("[1, 2, 3, 4]"));
    }
}
