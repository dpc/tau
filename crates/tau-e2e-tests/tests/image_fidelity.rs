use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tau_e2e_tests::VcrFixture;
use tau_proto::CborValue;

/// Expected half-open crop in oriented-source pixels.
#[derive(Clone, Copy)]
struct ExpectedRegion {
    /// Left coordinate.
    x: i64,
    /// Top coordinate.
    y: i64,
    /// Crop width.
    width: i64,
    /// Crop height.
    height: i64,
}

/// Expected prepared raster geometry and patch accounting.
struct ExpectedOutput {
    /// Prepared width.
    width: i64,
    /// Prepared height.
    height: i64,
    /// Rounded-up patch count.
    patches: i64,
}

/// Runs isolated real-model observations for overview triage and high/crop
/// fine-detail inspection without disclosing answer values in the prompts.
///
/// Hermetic CI skips this test. Future proposals to make overview the default
/// or recommend it generally must explicitly opt in and satisfy the fuller
/// matrix in `docs/read-image-fidelity-oracle.md`; this smoke test alone is
/// insufficient.
#[test]
fn read_image_live_fidelity_oracle() -> Result<(), Box<dyn std::error::Error>> {
    if !oracle_preflight()? {
        return Ok(());
    }
    let png = fidelity_fixture_png()?;

    let overview = run_case(
        "read_image_fidelity_overview",
        &png,
        "Use read_image exactly once on fidelity.png with mode overview and no region. Report the \
         two large panel colors from left to right as exactly OVERVIEW=<LEFT>-<RIGHT>, replacing \
         the placeholders with uppercase color names and adding no other text.",
    )?;
    assert_transform(
        &overview,
        "overview",
        None,
        ExpectedOutput {
            width: 1024,
            height: 576,
            patches: 576,
        },
    );
    assert_eq!(overview.response.trim(), "OVERVIEW=RED-GREEN");

    let high = run_case(
        "read_image_fidelity_high",
        &png,
        "Use read_image exactly once on fidelity.png with mode high and no region. Count the \
         narrow black vertical bars inside the white target card near the lower right. Answer \
         exactly HIGH=<COUNT>, replacing the placeholder with the integer and adding no other text.",
    )?;
    assert_transform(
        &high,
        "high",
        None,
        ExpectedOutput {
            width: 1920,
            height: 1080,
            patches: 2040,
        },
    );
    assert_eq!(high.response.trim(), "HIGH=5");

    let crop = run_case(
        "read_image_fidelity_crop",
        &png,
        "Use read_image exactly once on fidelity.png with mode high and region \
         x=1450,y=750,width=200,height=120. Count the narrow black vertical bars in that crop. \
         Answer exactly CROP=<COUNT>, replacing the placeholder with the integer and adding no \
         other text.",
    )?;
    assert_transform(
        &crop,
        "high",
        Some(ExpectedRegion {
            x: 1450,
            y: 750,
            width: 200,
            height: 120,
        }),
        ExpectedOutput {
            width: 200,
            height: 120,
            patches: 28,
        },
    );
    assert_eq!(crop.response.trim(), "CROP=5");
    Ok(())
}

/// Creates one isolated provider turn with the same deterministic raster.
fn run_case(
    name: &str,
    png: &[u8],
    prompt: &str,
) -> Result<tau_harness::InteractionOutcome, Box<dyn std::error::Error>> {
    let trial = std::env::var("TAU_IMAGE_FIDELITY_TRIAL_ID")
        .map_err(|_| "oracle preflight must provide a trial id")?;
    let fixture = VcrFixture::from_env(name)?
        .ok_or("active oracle preflight must produce an enabled VCR fixture")?
        .with_session_id(&format!("{trial}-{name}"));
    fixture.write_work_file(Path::new("fidelity.png"), png)?;
    Ok(fixture.run_turn(prompt)?)
}

/// Assert exact provider-requested arguments and byte-free terminal metadata.
fn assert_transform(
    outcome: &tau_harness::InteractionOutcome,
    mode: &str,
    requested_region: Option<ExpectedRegion>,
    output: ExpectedOutput,
) {
    let calls = outcome
        .tool_calls
        .iter()
        .filter(|call| call.name.as_str() == "read_image")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "expected one read_image call");
    assert_eq!(map_text(&calls[0].arguments, "mode"), Some(mode));
    match requested_region {
        Some(region) => assert!(region_matches(
            map_value(&calls[0].arguments, "region"),
            region
        )),
        None => assert!(map_value(&calls[0].arguments, "region").is_none()),
    }

    let results = outcome
        .tool_results
        .iter()
        .filter(|result| result.tool_name.as_str() == "read_image")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "expected one read_image result");
    assert!(results[0].provider_content.is_empty());
    assert!(metadata_matches(&results[0].result, mode, output));
    assert!(region_matches(
        map_value(&results[0].result, "region"),
        requested_region.unwrap_or(ExpectedRegion {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080
        })
    ));
}

/// Check mode and prepared geometry in terminal metadata.
fn metadata_matches(result: &CborValue, mode: &str, output: ExpectedOutput) -> bool {
    map_text(result, "mode") == Some(mode)
        && map_int(result, "width") == Some(output.width)
        && map_int(result, "height") == Some(output.height)
        && map_int(result, "patches") == Some(output.patches)
}

/// Check one exact half-open region object.
fn region_matches(value: Option<&CborValue>, region: ExpectedRegion) -> bool {
    value.is_some_and(|value| {
        map_int(value, "x") == Some(region.x)
            && map_int(value, "y") == Some(region.y)
            && map_int(value, "width") == Some(region.width)
            && map_int(value, "height") == Some(region.height)
    })
}

/// Read one text field from a CBOR map.
fn map_text<'a>(value: &'a CborValue, name: &str) -> Option<&'a str> {
    match map_value(value, name) {
        Some(CborValue::Text(value)) => Some(value),
        _ => None,
    }
}

/// Read one integer field from a CBOR map.
fn map_int(value: &CborValue, name: &str) -> Option<i64> {
    match map_value(value, name) {
        Some(CborValue::Integer(value)) => i128::from(*value).try_into().ok(),
        _ => None,
    }
}

/// Read one field from a CBOR map.
fn map_value<'a>(value: &'a CborValue, name: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(key, value)| {
        matches!(key, CborValue::Text(key) if key == name).then_some(value)
    })
}

/// Require a deliberate fresh-live trial and candidate binary; ordinary and
/// replay-only test runs skip or fail before contacting a provider.
fn oracle_preflight() -> Result<bool, Box<dyn std::error::Error>> {
    if std::env::var("TAU_IMAGE_FIDELITY_ORACLE").as_deref() != Ok("1") {
        eprintln!("skipping image oracle: set TAU_IMAGE_FIDELITY_ORACLE=1 to opt in");
        return Ok(false);
    }
    if std::env::var("TAU_VCR").as_deref() != Ok("record-if-missing") {
        return Err("live image oracle requires TAU_VCR=record-if-missing".into());
    }
    let tau_bin = std::env::var("TAU_E2E_TAU_BIN")
        .map_err(|_| "live image oracle requires TAU_E2E_TAU_BIN pinned to the candidate binary")?;
    if !Path::new(&tau_bin).is_absolute() || !Path::new(&tau_bin).is_file() {
        return Err("TAU_E2E_TAU_BIN must be an absolute candidate binary path".into());
    }
    let trial = std::env::var("TAU_IMAGE_FIDELITY_TRIAL_ID")
        .map_err(|_| "live image oracle requires a unique TAU_IMAGE_FIDELITY_TRIAL_ID")?;
    if trial.trim().is_empty() {
        return Err("TAU_IMAGE_FIDELITY_TRIAL_ID must not be empty".into());
    }
    let vcr_dir = std::env::var("TAU_VCR_DIR")
        .map_err(|_| "live image oracle requires a fresh empty TAU_VCR_DIR")?;
    let vcr_dir = Path::new(&vcr_dir);
    if vcr_dir.exists() && std::fs::read_dir(vcr_dir)?.next().is_some() {
        return Err("live image oracle requires an empty per-trial TAU_VCR_DIR".into());
    }
    Ok(true)
}

/// Builds a stable 1920x1080 UI-like raster with large layout signals and a
/// source-scale fine-detail target whose answer key is five bars.
fn fidelity_fixture_png() -> Result<Vec<u8>, image::ImageError> {
    let mut image = RgbImage::from_pixel(1920, 1080, Rgb([245, 245, 245]));
    fill(&mut image, 0, 0, 960, 700, Rgb([220, 40, 40]));
    fill(&mut image, 960, 0, 960, 700, Rgb([35, 180, 70]));
    fill(&mut image, 1450, 750, 200, 120, Rgb([255, 255, 255]));
    for bar in 0..5 {
        fill(&mut image, 1500 + bar * 12, 790, 4, 40, Rgb([0, 0, 0]));
    }
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image).write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

/// Paint one in-bounds rectangle into a deterministic RGB fixture.
fn fill(image: &mut RgbImage, x: u32, y: u32, width: u32, height: u32, color: Rgb<u8>) {
    for py in y..y + height {
        for px in x..x + width {
            image.put_pixel(px, py, color);
        }
    }
}

/// A mismatched terminal mode must not pass the oracle's metadata check.
#[test]
fn oracle_rejects_wrong_profile_metadata() {
    let metadata = CborValue::Map(vec![
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text("overview".to_owned()),
        ),
        (
            CborValue::Text("width".to_owned()),
            CborValue::Integer(1024.into()),
        ),
        (
            CborValue::Text("height".to_owned()),
            CborValue::Integer(576.into()),
        ),
        (
            CborValue::Text("patches".to_owned()),
            CborValue::Integer(576.into()),
        ),
    ]);
    assert!(!metadata_matches(
        &metadata,
        "high",
        ExpectedOutput {
            width: 1024,
            height: 576,
            patches: 576
        }
    ));
}
