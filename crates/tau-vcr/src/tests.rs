use serde_json::json;
use tempfile::TempDir;

use super::*;

/// Cassette keys are logical identifiers, not paths. Rejecting unsupported
/// characters avoids lossy filename normalization where distinct logical keys
/// could collapse onto the same cassette file.
#[test]
fn store_rejects_invalid_keys() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());

    let error = store
        .put("tc-main/0001", &json!({"value": true}))
        .expect_err("invalid key should fail");

    assert!(matches!(error, VcrError::InvalidKey(key) if key == "tc-main/0001"));
}

/// The store is intentionally schema-agnostic: callers own the cassette shape,
/// while `tau-vcr` only persists and loads reviewable YAML by stable key.
#[test]
fn store_puts_and_gets_caller_owned_yaml_schema() {
    #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
    struct ToolCassette {
        request: serde_json::Value,
        response: String,
    }

    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    let cassette = ToolCassette {
        request: json!({"command": "cargo check"}),
        response: "ok".to_owned(),
    };

    store.put("tc-main-0001", &cassette).expect("put cassette");
    let loaded: ToolCassette = store
        .get("tc-main-0001")
        .expect("get cassette")
        .expect("cassette exists");

    assert_eq!(loaded, cassette);
}

/// Missing cassettes are reported as `None` rather than an IO error so callers
/// can implement record-if-missing at the provider/tool boundary that owns the
/// live request path.
#[test]
fn store_get_returns_none_for_missing_cassette() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());

    let loaded: Option<serde_json::Value> = store.get("missing").expect("missing should be ok");

    assert!(loaded.is_none());
}

/// Only true absent cassette files should be treated as missing. Other IO
/// failures must surface so replay/record-if-missing callers do not silently
/// fall through to the live path when the cassette path is present but
/// unreadable.
#[cfg(unix)]
#[test]
fn store_get_reports_read_errors_instead_of_treating_them_as_missing() {
    let tempdir = TempDir::new().expect("tempdir");
    std::os::unix::fs::symlink("loop.yaml", tempdir.path().join("loop.yaml"))
        .expect("create symlink loop cassette");
    let store = VcrStore::new(tempdir.path());

    let error = store
        .get::<serde_json::Value>("loop")
        .expect_err("symlink loop should be a read error");

    assert!(matches!(error, VcrError::UnsafePath { .. }));
}

/// A symlinked cassette root is outside the configured trust boundary and must
/// not redirect either replay reads or private recordings.
#[cfg(unix)]
#[test]
fn store_rejects_symlinked_root_directory() {
    let tempdir = TempDir::new().expect("tempdir");
    let actual = tempdir.path().join("actual");
    std::fs::create_dir(&actual).expect("actual directory");
    let linked = tempdir.path().join("linked");
    std::os::unix::fs::symlink(&actual, &linked).expect("linked root");
    let store = VcrStore::new(&linked);

    assert!(matches!(
        store.get::<serde_json::Value>("cassette"),
        Err(VcrError::UnsafePath { .. })
    ));
    assert!(matches!(
        store.put("cassette", &json!({"safe": true})),
        Err(VcrError::UnsafePath { .. })
    ));
}

/// Request mismatch diagnostics must help correlate a failure without exposing
/// prompt or tool payloads that can appear in either request.
#[test]
fn request_mismatch_error_carries_only_redacted_payload_summaries() {
    let error = request_mismatch("tc-main-0001", &json!({"old": true}), &json!({"new": true}));

    match error {
        VcrError::RequestMismatch {
            expected, actual, ..
        } => {
            assert!(expected.contains("redacted payload"));
            assert!(actual.contains("redacted payload"));
            assert!(!expected.contains("old"));
            assert!(!actual.contains("new"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

/// Most callers convert VCR errors directly to user-visible strings. Display
/// must remain bounded and must not disclose either mismatched request.
#[test]
fn request_mismatch_display_redacts_serialized_payloads() {
    let error = request_mismatch("tc-main-0001", &json!({"old": true}), &json!({"new": true}));
    let display = error.to_string();

    assert!(display.contains("tc-main-0001"));
    assert!(display.contains("redacted payload"));
    assert!(!display.contains("old"));
    assert!(!display.contains("new"));
}

/// Recording is deliberately create-only so concurrent refreshers and CI
/// misconfiguration cannot overwrite reviewed evidence.
#[test]
fn store_put_is_exclusive_and_preserves_existing_cassette() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    store
        .put("evidence", &json!({"version": 1}))
        .expect("first write");

    let error = store
        .put("evidence", &json!({"version": 2}))
        .expect_err("overwrite must fail");
    assert!(matches!(error, VcrError::Write { .. }));
    let loaded: serde_json::Value = store
        .get("evidence")
        .expect("read")
        .expect("cassette exists");
    assert_eq!(loaded, json!({"version": 1}));
}

/// Oversized files are rejected from metadata before YAML parsing, bounding
/// memory use for corrupted or malicious cassette directories.
#[test]
fn store_rejects_oversized_cassette() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::write(
        tempdir.path().join("large.yaml"),
        vec![b'x'; (MAX_CASSETTE_BYTES + 1) as usize],
    )
    .expect("write oversized fixture");
    let store = VcrStore::new(tempdir.path());

    let error = store
        .get::<serde_json::Value>("large")
        .expect_err("oversized cassette must fail");
    assert!(matches!(error, VcrError::TooLarge { .. }));
}

/// Tau's safe automatic recording workflow is record-if-missing: existing
/// fixtures replay, while absent fixtures allow callers to hit the live path
/// and create a new cassette.
#[test]
fn mode_parses_record_if_missing_without_record_overwrite_mode() {
    assert_eq!(VcrMode::parse("off").expect("off"), VcrMode::Off);
    assert_eq!(
        VcrMode::parse("record-if-missing").expect("record-if-missing"),
        VcrMode::RecordIfMissing
    );
    assert_eq!(
        VcrMode::parse("replay-only").expect("replay-only"),
        VcrMode::ReplayOnly
    );
    assert!(VcrMode::parse("record").is_err());
}

/// Escaped byte strings keep common UTF-8 cassette data readable while still
/// round-tripping rare invalid UTF-8 bytes without YAML byte lists.
#[test]
fn escaped_bytes_serialize_as_single_readable_string() {
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct Cassette {
        bytes: EscapedBytes,
    }

    let cassette = Cassette {
        bytes: EscapedBytes::new(b"hello \\ path \xFF".to_vec()),
    };

    let yaml = serde_yaml_ng::to_string(&cassette).expect("serialize");
    assert!(yaml.contains("hello"));
    assert!(yaml.contains("\\\\ path"));
    assert!(yaml.contains("\\uDCFF"));
    assert!(!yaml.contains("- 255"));

    let loaded: Cassette = serde_yaml_ng::from_str(&yaml).expect("deserialize");
    assert_eq!(loaded.bytes.as_slice(), b"hello \\ path \xFF");
}

#[test]
fn escaped_byte_helpers_round_trip_mixed_utf8_and_invalid_bytes() {
    let bytes = b"snowman: \xE2\x98\x83 bad: \xF0( slash: \\";

    let encoded = encode_escaped_bytes(bytes);
    assert_eq!(encoded, "snowman: ☃ bad: \\uDCF0( slash: \\\\");
    assert_eq!(decode_escaped_bytes(&encoded).expect("decode"), bytes);
}
