//! Regression coverage for bounded image preparation.

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
        let prepared =
            prepare_image(bytes.get_ref(), ImageMode::High, None).expect("prepare image");
        assert_eq!(prepared.content.media_type, expected);
        assert_eq!((prepared.content.width, prepared.content.height), (3, 2));
        assert!(!prepared.content.data.is_empty());
    }
}

/// Ensures extension or filename claims cannot make unsupported bytes cross
/// the typed image boundary.
#[test]
fn rejects_non_raster_input() {
    let error = prepare_image(b"<svg/>", ImageMode::High, None)
        .err()
        .expect("SVG is unsupported");
    assert!(error.contains("unsupported image format"));
}

/// Ensures high-detail preparation resizes large square images to the patch
/// budget rather than allowing provider cost to escape the local bound.
#[test]
fn high_detail_resize_obeys_patch_budget() {
    let image = DynamicImage::new_rgba8(3000, 3000);
    let resized = resize_for_mode(image, ImageMode::High);
    assert!(resized.width() <= MAX_HIGH_SIDE);
    assert!(resized.height() <= MAX_HIGH_SIDE);
    assert!(patch_count(resized.width(), resized.height()) <= MAX_HIGH_PATCHES);
}

/// Locks the compatibility profile's representative geometry so adding the
/// experimental profile cannot silently lower bare-call fidelity.
#[test]
fn high_mode_preserves_existing_geometry() {
    for (source, expected) in [
        ((1920, 1080), (1920, 1080)),
        ((2560, 1440), (2048, 1152)),
        ((3840, 2160), (2048, 1152)),
        ((2048, 2048), (1600, 1600)),
    ] {
        let target = ImageMode::High.dimensions(source.0, source.1).target;
        assert_eq!((target.width, target.height), expected);
    }
}

/// Quantifies representative experimental overview outputs and proves both
/// documented geometry budgets hold for desktop, square, and panoramic
/// views.
#[test]
fn overview_fixture_geometry_is_deterministic_and_bounded() {
    for (source, expected, expected_patches) in [
        ((1920, 1080), (1024, 576), 576),
        ((2560, 1440), (1024, 576), 576),
        ((2048, 2048), (768, 768), 576),
        ((1440, 6000), (245, 1024), 256),
    ] {
        let resized = ImageMode::Overview.dimensions(source.0, source.1).target;
        assert_eq!((resized.width, resized.height), expected);
        assert_eq!(patch_count(resized.width, resized.height), expected_patches);
        assert!(resized.width.max(resized.height) <= MAX_OVERVIEW_SIDE);
        assert!(expected_patches <= MAX_OVERVIEW_PATCHES);
    }
}

/// Ensures crop extents reject zero, out-of-bounds, and overflowing inputs
/// rather than wrapping or relying on image-library clipping behavior.
#[test]
fn region_validation_rejects_invalid_extents() {
    assert!(
        ImageRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 1
        }
        .validate(10, 10)
        .is_err()
    );
    assert!(
        ImageRegion {
            x: 9,
            y: 0,
            width: 2,
            height: 1
        }
        .validate(10, 10)
        .is_err()
    );
    assert!(
        ImageRegion {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        }
        .validate(u32::MAX, 10)
        .is_err()
    );
    assert!(
        ImageRegion {
            x: 2,
            y: 3,
            width: 8,
            height: 7,
        }
        .validate(10, 10)
        .is_ok()
    );
}

/// Ensures the argument boundary rejects signed and oversized coordinates
/// before file I/O rather than coercing them into a different crop.
#[test]
fn region_parser_rejects_negative_and_oversized_coordinates() {
    let arguments = |x: i64| {
        CborValue::Map(vec![(
            CborValue::Text("region".to_owned()),
            CborValue::Map(vec![
                (
                    CborValue::Text("x".to_owned()),
                    CborValue::Integer(x.into()),
                ),
                (
                    CborValue::Text("y".to_owned()),
                    CborValue::Integer(0.into()),
                ),
                (
                    CborValue::Text("width".to_owned()),
                    CborValue::Integer(1.into()),
                ),
                (
                    CborValue::Text("height".to_owned()),
                    CborValue::Integer(1.into()),
                ),
            ]),
        )])
    };
    assert!(ImageRegion::from_arguments(&arguments(-1)).is_err());
    assert!(ImageRegion::from_arguments(&arguments(i64::from(u32::MAX) + 1)).is_err());
}

/// Proves an oriented-source crop retains native selected pixels before the
/// experimental overview transform is considered.
#[test]
fn crop_precedes_mode_resize() {
    let source = DynamicImage::new_rgb8(1200, 800);
    let mut bytes = Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, ImageFormat::Png)
        .expect("encode crop-order fixture");
    let prepared = prepare_image(
        bytes.get_ref(),
        ImageMode::Overview,
        Some(ImageRegion {
            x: 300,
            y: 200,
            width: 800,
            height: 400,
        }),
    )
    .expect("prepare overview crop");
    assert_eq!(
        (prepared.content.width, prepared.content.height),
        (800, 400)
    );
    assert_eq!(
        patch_count(prepared.content.width, prepared.content.height),
        325
    );
}

/// Ensures an explicit full-source region is a semantic no-op and therefore
/// can avoid allocating a duplicate full decoded raster.
#[test]
fn explicit_full_region_preserves_canonical_output() {
    let source = DynamicImage::new_rgb8(32, 24);
    let mut bytes = Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, ImageFormat::Png)
        .expect("encode full-region fixture");
    let implicit =
        prepare_image(bytes.get_ref(), ImageMode::High, None).expect("implicit full image");
    let explicit = prepare_image(
        bytes.get_ref(),
        ImageMode::High,
        Some(ImageRegion::full(32, 24)),
    )
    .expect("explicit full image");
    assert_eq!(implicit.content.data, explicit.content.data);
}

/// Ensures region coordinates are validated against the EXIF-oriented
/// raster, not the decoder's pre-orientation dimensions, and crop
/// before preparation.
#[test]
fn exif_orientation_precedes_region_crop() {
    let source = DynamicImage::new_rgb8(4, 2);
    let mut jpeg = Cursor::new(Vec::new());
    source
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .expect("encode JPEG fixture");
    let jpeg = jpeg_with_orientation(jpeg.into_inner(), 6);
    let region = ImageRegion {
        x: 0,
        y: 2,
        width: 2,
        height: 2,
    };

    let prepared =
        prepare_image(&jpeg, ImageMode::High, Some(region)).expect("prepare oriented crop");
    assert_eq!((prepared.source_width, prepared.source_height), (4, 2));
    assert_eq!((prepared.oriented_width, prepared.oriented_height), (2, 4));
    assert_eq!(prepared.region, region);
    assert_eq!(
        (prepared.content.width, prepared.content.height),
        (region.width, region.height)
    );
}

/// Add a minimal little-endian EXIF APP1 orientation segment to a JPEG
/// generated by image-rs, keeping the binary fixture deterministic.
fn jpeg_with_orientation(jpeg: Vec<u8>, orientation: u16) -> Vec<u8> {
    assert!(jpeg.starts_with(&[0xff, 0xd8]));
    let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let segment_length = u16::try_from(exif.len() + 2).expect("small EXIF segment");
    let mut oriented = Vec::with_capacity(jpeg.len() + exif.len() + 4);
    oriented.extend_from_slice(&jpeg[..2]);
    oriented.extend_from_slice(&[0xff, 0xe1]);
    oriented.extend_from_slice(&segment_length.to_be_bytes());
    oriented.extend_from_slice(&exif);
    oriented.extend_from_slice(&jpeg[2..]);
    oriented
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
