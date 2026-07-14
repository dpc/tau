//! Bounded local raster decoding for the `read_image` tool.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};

use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader};
use tau_proto::{
    CborValue, ImageContent, ImageDetail, ImageMediaType, ToolResultContentPart, ToolUseStats,
};

use crate::argument::{argument_text, optional_argument_text};
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
const MAX_OVERVIEW_SIDE: u32 = 1024;
const MAX_OVERVIEW_PATCHES: u64 = 600;
const PATCH_SIDE: u64 = 32;
const MAX_CONCURRENT_DECODES: usize = 1;

static DECODE_PERMITS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

/// Named local image-preparation profile selected by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImageMode {
    /// Compatibility profile with the existing 2048-side/2,500-patch limits.
    High,
    /// Experimental coarse-inspection profile with tighter 1024/600 limits.
    Overview,
}

impl ImageMode {
    /// Parse the optional model-visible mode, preserving high as the default.
    fn from_arguments(arguments: &CborValue) -> Result<Self, String> {
        match optional_argument_text(arguments, "mode")?.as_deref() {
            None | Some("high") => Ok(Self::High),
            Some("overview") => Ok(Self::Overview),
            Some(_) => Err("argument `mode` must be `high` or `overview`".to_owned()),
        }
    }

    /// Stable metadata spelling for the selected profile.
    fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Overview => "overview",
        }
    }

    /// Maximum prepared side and patch count for this profile.
    fn bounds(self) -> (u32, u64) {
        match self {
            Self::High => (MAX_HIGH_SIDE, MAX_HIGH_PATCHES),
            Self::Overview => (MAX_OVERVIEW_SIDE, MAX_OVERVIEW_PATCHES),
        }
    }

    /// Calculate the initial proportional resize and final patch-bounded size.
    fn dimensions(self, width: u32, height: u32) -> ResizePlan {
        let (max_side, max_patches) = self.bounds();
        let side_scale = f64::from(max_side) / f64::from(width.max(height));
        let patches = patch_count(width, height);
        let patch_scale = (max_patches as f64 / patches as f64).sqrt();
        let scale = 1.0_f64.min(side_scale).min(patch_scale);
        let initial = if scale < 1.0 {
            ImageDimensions {
                width: (f64::from(width) * scale).floor().max(1.0) as u32,
                height: (f64::from(height) * scale).floor().max(1.0) as u32,
            }
        } else {
            ImageDimensions { width, height }
        };
        let mut target = initial;
        while max_patches < patch_count(target.width, target.height) {
            target.width = target.width.saturating_sub(1).max(1);
            target.height = target.height.saturating_sub(1).max(1);
        }
        ResizePlan { initial, target }
    }
}

/// One raster size used while calculating a bounded resize.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageDimensions {
    /// Raster width in pixels.
    width: u32,
    /// Raster height in pixels.
    height: u32,
}

/// Compatibility-preserving initial and final dimensions for one resize.
struct ResizePlan {
    /// First proportional resize performed by the historical algorithm.
    initial: ImageDimensions,
    /// Final dimensions after any one-pixel patch-budget reductions.
    target: ImageDimensions,
}

/// Half-open crop rectangle in pixels of the EXIF-oriented source raster.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImageRegion {
    /// Left edge in oriented-source pixels.
    x: u32,
    /// Top edge in oriented-source pixels.
    y: u32,
    /// Non-zero crop width in oriented-source pixels.
    width: u32,
    /// Non-zero crop height in oriented-source pixels.
    height: u32,
}

impl ImageRegion {
    /// Parse an optional strict region object from tool arguments.
    fn from_arguments(arguments: &CborValue) -> Result<Option<Self>, String> {
        let CborValue::Map(arguments) = arguments else {
            return Ok(None);
        };
        let Some(value) = arguments.iter().find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == "region").then_some(value)
        }) else {
            return Ok(None);
        };
        let CborValue::Map(entries) = value else {
            return Err("argument `region` must be an object".to_owned());
        };
        for (key, _) in entries {
            match key {
                CborValue::Text(key) if matches!(key.as_str(), "x" | "y" | "width" | "height") => {}
                CborValue::Text(key) => {
                    return Err(format!("argument `region` has unknown field `{key}`"));
                }
                _ => return Err("argument `region` field names must be strings".to_owned()),
            }
        }
        Ok(Some(Self {
            x: region_u32(entries, "x")?,
            y: region_u32(entries, "y")?,
            width: region_extent(entries, "width")?,
            height: region_extent(entries, "height")?,
        }))
    }

    /// Select the complete oriented source raster.
    fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Validate this half-open extent against an oriented source raster.
    fn validate(self, width: u32, height: u32) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("argument `region` width and height must be non-zero".to_owned());
        }
        let right = self
            .x
            .checked_add(self.width)
            .ok_or_else(|| "argument `region` horizontal extent overflows".to_owned())?;
        let bottom = self
            .y
            .checked_add(self.height)
            .ok_or_else(|| "argument `region` vertical extent overflows".to_owned())?;
        if width < right || height < bottom {
            return Err(format!(
                "argument `region` ({},{} {}x{}) is outside oriented source {}x{}",
                self.x, self.y, self.width, self.height, width, height
            ));
        }
        Ok(())
    }

    /// Encode safe region metadata for generic result surfaces.
    fn into_value(self) -> CborValue {
        CborValue::Map(vec![
            (
                CborValue::Text("x".to_owned()),
                CborValue::Integer(i64::from(self.x).into()),
            ),
            (
                CborValue::Text("y".to_owned()),
                CborValue::Integer(i64::from(self.y).into()),
            ),
            (
                CborValue::Text("width".to_owned()),
                CborValue::Integer(i64::from(self.width).into()),
            ),
            (
                CborValue::Text("height".to_owned()),
                CborValue::Integer(i64::from(self.height).into()),
            ),
        ])
    }
}

/// Canonical image plus source-transform metadata safe for generic surfaces.
struct PreparedImage {
    /// Typed canonical content directed only to image-capable providers.
    content: ImageContent,
    /// Decoder-reported width before EXIF orientation.
    source_width: u32,
    /// Decoder-reported height before EXIF orientation.
    source_height: u32,
    /// Full raster width after EXIF orientation.
    oriented_width: u32,
    /// Full raster height after EXIF orientation.
    oriented_height: u32,
    /// Exact oriented-source selection prepared for output.
    region: ImageRegion,
}

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
    let mode = ImageMode::from_arguments(arguments).map_err(ToolFailure::from)?;
    let requested_region = ImageRegion::from_arguments(arguments).map_err(ToolFailure::from)?;
    let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
    let path = PathBuf::from(path);
    let display_path = path.display().to_string();
    let bytes = world
        .read_file_limited(&path, MAX_SOURCE_BYTES)
        .map_err(|error| ToolFailure::from(error.to_string()).with_args(display_path.clone()))?;

    let _permit = DecodePermit::acquire();
    let prepared = prepare_image(&bytes, mode, requested_region)
        .map_err(|message| ToolFailure::from(message).with_args(display_path.clone()))?;
    let image = prepared.content;
    let format = image.media_type.mime_type();
    let byte_count = image.data.len();
    let patches = patch_count(image.width, image.height);
    let summary = format!(
        "{format} image, {}x{}, {patches} patches, {byte_count} bytes, {} mode",
        image.width,
        image.height,
        mode.as_str()
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
            CborValue::Text("patches".to_owned()),
            CborValue::Integer(i64::try_from(patches).unwrap_or(i64::MAX).into()),
        ),
        (
            CborValue::Text("detail".to_owned()),
            CborValue::Text("high".to_owned()),
        ),
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text(mode.as_str().to_owned()),
        ),
        (
            CborValue::Text("source_width".to_owned()),
            CborValue::Integer(i64::from(prepared.source_width).into()),
        ),
        (
            CborValue::Text("source_height".to_owned()),
            CborValue::Integer(i64::from(prepared.source_height).into()),
        ),
        (
            CborValue::Text("oriented_width".to_owned()),
            CborValue::Integer(i64::from(prepared.oriented_width).into()),
        ),
        (
            CborValue::Text("oriented_height".to_owned()),
            CborValue::Integer(i64::from(prepared.oriented_height).into()),
        ),
        (
            CborValue::Text("region".to_owned()),
            prepared.region.into_value(),
        ),
    ]);
    let display_args = format!(
        "{display_path}  {format}  mode={}  source={}x{}  oriented={}x{}  \
         region={},{} {}x{}  output={}x{}  {patches} patches  {byte_count} bytes",
        mode.as_str(),
        prepared.source_width,
        prepared.source_height,
        prepared.oriented_width,
        prepared.oriented_height,
        prepared.region.x,
        prepared.region.y,
        prepared.region.width,
        prepared.region.height,
        image.width,
        image.height
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

fn prepare_image(
    bytes: &[u8],
    mode: ImageMode,
    requested_region: Option<ImageRegion>,
) -> Result<PreparedImage, String> {
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
    let (source_width, source_height) = decoder.dimensions();
    validate_source_dimensions(source_width, source_height, source_format)?;
    let decoded_bytes = decoder.total_bytes();
    validate_decoded_allocation(source_format, decoded_bytes)?;
    let orientation = decoder
        .orientation()
        .map_err(|error| format!("cannot inspect image orientation: {error}"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("cannot decode image: {error}"))?;
    decoded.apply_orientation(orientation);
    let (oriented_width, oriented_height) = decoded.dimensions();
    let region =
        requested_region.unwrap_or_else(|| ImageRegion::full(oriented_width, oriented_height));
    region.validate(oriented_width, oriented_height)?;
    if region != ImageRegion::full(oriented_width, oriented_height) {
        decoded = decoded.crop_imm(region.x, region.y, region.width, region.height);
    }
    decoded = resize_for_mode(decoded, mode);
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
    Ok(PreparedImage {
        content: ImageContent {
            media_type: source_format,
            data: data.into(),
            width,
            height,
            // Overview is a local transform, not provider `low` detail.
            detail: ImageDetail::High,
        },
        source_width,
        source_height,
        oriented_width,
        oriented_height,
        region,
    })
}

fn region_extent(entries: &[(CborValue, CborValue)], name: &str) -> Result<u32, String> {
    let value = region_u32(entries, name)?;
    if value == 0 {
        return Err(format!("argument `region.{name}` must be non-zero"));
    }
    Ok(value)
}

fn region_u32(entries: &[(CborValue, CborValue)], name: &str) -> Result<u32, String> {
    let value = entries
        .iter()
        .find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == name).then_some(value)
        })
        .ok_or_else(|| format!("argument `region.{name}` is required"))?;
    let CborValue::Integer(value) = value else {
        return Err(format!("argument `region.{name}` must be an integer"));
    };
    let value = u32::try_from(i128::from(*value))
        .map_err(|_| format!("argument `region.{name}` must fit in an unsigned 32-bit integer"))?;
    Ok(value)
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

fn resize_for_mode(mut image: DynamicImage, mode: ImageMode) -> DynamicImage {
    let (width, height) = image.dimensions();
    let plan = mode.dimensions(width, height);
    if plan.initial != (ImageDimensions { width, height }) {
        image = image.resize_exact(
            plan.initial.width,
            plan.initial.height,
            image::imageops::FilterType::Triangle,
        );
    }
    // Preserve the high profile's existing sequence of one-pixel resamples so
    // bare calls retain byte-for-byte preparation behavior.
    while (image.width(), image.height()) != (plan.target.width, plan.target.height) {
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
mod tests;
