use std::os::unix as path_std_os_unix;
use std::sync::{Arc, Barrier};
use std::thread;

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

    for key in ["", ".", "..", "tc-main/0001", r"tc-main\0001", "日本"] {
        let error = store
            .put(key, &json!({"value": true}))
            .expect_err("invalid key should fail");
        assert!(matches!(error, VcrError::InvalidKey(rejected) if rejected == key));
    }
    for key in ["a", "A9", "tc-main-0001", "tc_main_0001"] {
        assert!(store.path(key).is_ok(), "valid key {key:?} was rejected");
    }
}

/// Side-artifact kinds are filename suffixes rather than paths. Their stricter
/// grammar prevents a caller from selecting another cassette-owned file.
#[test]
fn artifact_kind_rejects_invalid_values() {
    for kind in [
        "",
        ".",
        "..",
        "shell/output",
        r"shell\output",
        "日本",
        "shell_output",
    ] {
        let error = ArtifactKind::new(kind).expect_err("invalid artifact kind should fail");
        assert!(matches!(error, VcrError::InvalidArtifactKind(rejected) if rejected == kind));
    }
    for kind in ["a", "A9", "shell-output"] {
        assert_eq!(
            ArtifactKind::new(kind)
                .expect("valid artifact kind")
                .as_str(),
            kind
        );
    }
}

/// Side limits preserve the full raw `u64` domain while constructing a
/// one-byte read-growth probe wherever that successor is representable.
#[test]
fn byte_limit_preserves_raw_values_and_saturates_read_probe() {
    assert_eq!(ByteLimit::new(0).get(), 0);
    assert_eq!(ByteLimit::new(3).read_probe(), 4);
    assert_eq!(ByteLimit::new(u64::MAX).get(), u64::MAX);
    assert_eq!(ByteLimit::new(u64::MAX).read_probe(), u64::MAX);
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
    path_std_os_unix::fs::symlink("loop.yaml", tempdir.path().join("loop.yaml"))
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
    path_std_os_unix::fs::symlink(&actual, &linked).expect("linked root");
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
    let expected_marker = "expected-token-4f2a";
    let actual_marker = "actual-prompt-61ce";
    let expected_secret = format!("{expected_marker}_host_internal_example");
    let actual_secret = format!("{actual_marker}_host_private_example");
    let error = request_mismatch(
        "tc-main-0001",
        &json!({"authorization": expected_secret}),
        &json!({"prompt": actual_secret}),
    );
    let debug = format!("{error:?}");

    match &error {
        VcrError::RequestMismatch {
            expected, actual, ..
        } => {
            assert!(expected.contains("redacted payload"));
            assert!(actual.contains("redacted payload"));
            assert!(!expected.contains(expected_marker));
            assert!(!actual.contains(actual_marker));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(debug.contains("redacted payload"));
    assert!(!debug.contains(expected_marker));
    assert!(!debug.contains(actual_marker));
}

/// Most callers convert VCR errors directly to user-visible strings. Display
/// must remain bounded and must not disclose either mismatched request.
#[test]
fn request_mismatch_display_redacts_serialized_payloads() {
    let expected_marker = "expected-secret-marker";
    let actual_marker = "actual-secret-marker";
    let expected_secret = format!("{expected_marker}{}", "x".repeat(4096));
    let actual_secret = format!("{actual_marker}{}", "y".repeat(4096));
    let error = request_mismatch(
        "tc-main-0001",
        &json!({"authorization": expected_secret}),
        &json!({"prompt": actual_secret}),
    );
    let debug = format!("{error:?}");
    let display = error.to_string();

    match &error {
        VcrError::RequestMismatch {
            expected, actual, ..
        } => {
            assert!(!expected.contains(expected_marker));
            assert!(!actual.contains(actual_marker));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(!debug.contains(expected_marker));
    assert!(!debug.contains(actual_marker));
    assert!(display.contains("tc-main-0001"));
    assert!(display.contains("redacted payload"));
    assert!(!display.contains(expected_marker));
    assert!(!display.contains(actual_marker));
    assert!(
        display.len() <= 512,
        "redacted mismatch display grew to {} bytes",
        display.len()
    );
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

    let race_root = tempdir.path().join("race");
    let race_store = VcrStore::new(&race_root);
    let barrier = Arc::new(Barrier::new(2));
    let first_store = race_store.clone();
    let first_barrier = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_store.put("race", &json!({"winner": "first"}))
    });
    let second_store = race_store.clone();
    let second_barrier = Arc::clone(&barrier);
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_store.put("race", &json!({"winner": "second"}))
    });
    let results = [
        first.join().expect("first writer"),
        second.join().expect("second writer"),
    ];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "same-key race must publish exactly one cassette"
    );
    assert!(
        results
            .iter()
            .filter(|result| result.is_err())
            .all(|result| matches!(result, Err(VcrError::Write { .. }))),
        "losing publisher must receive the exclusive-publication error"
    );
    let winning_value = race_store
        .get::<serde_json::Value>("race")
        .expect("read race winner")
        .expect("race winner exists");
    let winning_bytes = std::fs::read(race_root.join("race.yaml")).expect("read winner bytes");
    assert!(
        winning_value == json!({"winner": "first"}) || winning_value == json!({"winner": "second"}),
        "winner must remain a complete cassette"
    );
    assert_eq!(
        winning_bytes,
        serde_yaml_ng::to_string(&winning_value)
            .expect("serialize winning cassette")
            .into_bytes()
    );
    assert!(!race_root.join(".race.yaml.stage").exists());
}

/// Oversized files are rejected from metadata before YAML parsing, bounding
/// memory use for corrupted or malicious cassette directories.
#[test]
fn store_rejects_oversized_cassette() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());

    std::fs::write(
        tempdir.path().join("exact.yaml"),
        vec![b'x'; MAX_CASSETTE_BYTES as usize],
    )
    .expect("write exact-limit fixture");
    let exact: String = store
        .get("exact")
        .expect("exact-limit read")
        .expect("exact-limit cassette exists");
    assert_eq!(exact.len(), MAX_CASSETTE_BYTES as usize);

    std::fs::write(
        tempdir.path().join("large.yaml"),
        vec![b'x'; (MAX_CASSETTE_BYTES + 1) as usize],
    )
    .expect("write oversized fixture");
    let error = store
        .get::<serde_json::Value>("large")
        .expect_err("oversized cassette must fail");
    assert!(matches!(
        error,
        VcrError::TooLarge {
            bytes,
            limit: MAX_CASSETTE_BYTES,
            ..
        } if bytes == MAX_CASSETTE_BYTES + 1
    ));

    let exact_value = yaml_string_at_limit(MAX_CASSETTE_BYTES as usize);
    store
        .put("exact-write", &exact_value)
        .expect("exact-limit write");
    let oversized_value = yaml_string_at_limit(MAX_CASSETTE_BYTES as usize + 1);
    let error = store
        .put("large-write", &oversized_value)
        .expect_err("oversized write must fail");
    assert!(matches!(
        error,
        VcrError::TooLarge {
            bytes,
            limit: MAX_CASSETTE_BYTES,
            ..
        } if bytes == MAX_CASSETTE_BYTES + 1
    ));
    assert!(!tempdir.path().join("large-write.yaml").exists());
    assert!(!tempdir.path().join(".large-write.yaml.stage").exists());
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

/// Escaped byte helpers preserve valid UTF-8, invalid byte prefixes, and
/// literal backslashes without changing the caller-owned byte sequence.
#[test]
fn escaped_byte_helpers_round_trip_mixed_utf8_and_invalid_bytes() {
    let bytes = b"snowman: \xE2\x98\x83 bad: \xF0( slash: \\";

    let encoded = encode_escaped_bytes(bytes);
    assert_eq!(encoded, "snowman: ☃ bad: \\uDCF0( slash: \\\\");
    assert_eq!(decode_escaped_bytes(&encoded).expect("decode"), bytes);
}

/// Bundled side artifacts round-trip within their independent bound and publish
/// the cassette and private side exactly once.
#[test]
fn bundled_side_artifact_round_trips_and_is_exclusive() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    let cassette = json!({"value": true});
    store
        .put_with_side(
            "call",
            &ArtifactKind::new("shell-output").expect("kind"),
            &cassette,
            b"payload",
            ByteLimit::new(16),
        )
        .expect("publish bundle");
    assert_eq!(
        store
            .get_side(
                "call",
                &ArtifactKind::new("shell-output").expect("kind"),
                ByteLimit::new(16)
            )
            .expect("read side"),
        b"payload"
    );
    let error = store
        .put_with_side(
            "call",
            &ArtifactKind::new("shell-output").expect("kind"),
            &json!({"value": false}),
            b"other",
            ByteLimit::new(16),
        )
        .expect_err("rewrite must fail");
    assert!(matches!(error, VcrError::Write { .. }));
    let loaded: serde_json::Value = store
        .get("call")
        .expect("read original cassette")
        .expect("original cassette exists");
    assert_eq!(loaded, cassette);
    assert_eq!(
        store
            .get_side(
                "call",
                &ArtifactKind::new("shell-output").expect("kind"),
                ByteLimit::new(16)
            )
            .expect("read original side"),
        b"payload"
    );
    let error = store
        .get_side(
            "call",
            &ArtifactKind::new("shell-output").expect("kind"),
            ByteLimit::new(3),
        )
        .expect_err("small side limit must fail");
    assert!(matches!(
        error,
        VcrError::TooLarge {
            bytes: 7,
            limit: 3,
            ..
        }
    ));

    let race_root = tempdir.path().join("bundle-race");
    let race_store = VcrStore::new(&race_root);
    let kind = ArtifactKind::new("shell-output").expect("kind");
    let barrier = Arc::new(Barrier::new(2));
    let first_store = race_store.clone();
    let first_kind = kind.clone();
    let first_barrier = Arc::clone(&barrier);
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_store.put_with_side(
            "race",
            &first_kind,
            &json!({"winner": "first"}),
            b"first-side",
            ByteLimit::new(16),
        )
    });
    let second_store = race_store.clone();
    let second_kind = kind.clone();
    let second_barrier = Arc::clone(&barrier);
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_store.put_with_side(
            "race",
            &second_kind,
            &json!({"winner": "second"}),
            b"second-side",
            ByteLimit::new(16),
        )
    });
    let results = [
        first.join().expect("first bundle"),
        second.join().expect("second bundle"),
    ];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "same-key bundle race must publish exactly one pair"
    );
    assert!(
        results
            .iter()
            .filter(|result| result.is_err())
            .all(|result| matches!(result, Err(VcrError::Write { .. }))),
        "losing bundle publisher must receive the exclusive-publication error"
    );
    let winner: serde_json::Value = race_store
        .get("race")
        .expect("read bundle winner")
        .expect("bundle winner exists");
    let side = race_store
        .get_side("race", &kind, ByteLimit::new(16))
        .expect("read bundle winner side");
    match winner {
        value if value == json!({"winner": "first"}) => assert_eq!(side, b"first-side"),
        value if value == json!({"winner": "second"}) => assert_eq!(side, b"second-side"),
        other => panic!("unexpected bundle winner: {other:?}"),
    }
    assert!(!race_root.join(".race.yaml.stage").exists());
    assert!(!race_root.join(".race.shell-output.stage").exists());
}

/// Unix side reads reject a symlink instead of following it outside the VCR
/// root.
#[cfg(unix)]
#[test]
fn bundled_side_artifact_rejects_symlink() {
    use std::os::unix::fs::symlink;

    let tempdir = TempDir::new().expect("tempdir");
    let outside = TempDir::new().expect("outside");
    std::fs::write(outside.path().join("payload"), b"secret").expect("outside file");
    symlink(
        outside.path().join("payload"),
        tempdir.path().join("call.shell-output"),
    )
    .expect("symlink");
    let store = VcrStore::new(tempdir.path());
    assert!(matches!(
        store.get_side(
            "call",
            &ArtifactKind::new("shell-output").expect("kind"),
            ByteLimit::new(16)
        ),
        Err(VcrError::UnsafePath { .. }) | Err(VcrError::Read { .. })
    ));
}

/// Oversized sides publish neither side nor cassette.
#[test]
fn bundled_side_artifact_rejects_oversize_before_publication() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");
    store
        .put_with_side("exact", &kind, &json!({}), b"123", ByteLimit::new(3))
        .expect("exact side limit publishes");
    assert_eq!(
        store
            .get_side("exact", &kind, ByteLimit::new(3))
            .expect("exact side"),
        b"123"
    );
    let error = store
        .put_with_side("call", &kind, &json!({}), b"too large", ByteLimit::new(3))
        .expect_err("oversized side must fail");
    assert!(matches!(
        error,
        VcrError::TooLarge {
            bytes: 9,
            limit: 3,
            ..
        }
    ));
    assert!(!tempdir.path().join("call.yaml").exists());
    assert!(!tempdir.path().join("call.shell-output").exists());
    assert!(!tempdir.path().join(".call.yaml.stage").exists());
    assert!(!tempdir.path().join(".call.shell-output.stage").exists());
}

/// Newly created VCR paths hold captured requests and outputs, so Unix records
/// the owner-only modes promised by the storage boundary.
#[cfg(unix)]
#[test]
fn store_creates_private_root_cassette_side_and_lock() {
    use std::os::unix::fs::PermissionsExt as _;

    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path().join("new").join("vcr");
    let store = VcrStore::new(&root);
    let kind = ArtifactKind::new("shell-output").expect("kind");
    store
        .put_with_side("call", &kind, &json!({}), b"side", ByteLimit::new(4))
        .expect("publish private bundle");

    for (path, expected_mode) in [
        (root, 0o700),
        (tempdir.path().join("new/vcr/call.yaml"), 0o600),
        (tempdir.path().join("new/vcr/call.shell-output"), 0o600),
        (tempdir.path().join("new/vcr/call.bundle.lock"), 0o600),
    ] {
        let mode = std::fs::metadata(&path)
            .expect("private path metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, expected_mode, "{} mode", path.display());
    }
}

/// A retry reclaims regular crash-stage files before publication, while a
/// non-regular stage fails closed instead of being removed as if it were ours.
#[test]
fn bundled_side_artifact_recovers_regular_stages_and_rejects_nonregular_stage() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");
    std::fs::write(
        tempdir.path().join(".call.yaml.stage"),
        b"interrupted cassette",
    )
    .expect("cassette stage");
    std::fs::write(
        tempdir.path().join(".call.shell-output.stage"),
        b"interrupted side",
    )
    .expect("side stage");
    store
        .put_with_side(
            "call",
            &kind,
            &json!({}),
            b"replacement",
            ByteLimit::new(16),
        )
        .expect("retry bundle");
    assert!(!tempdir.path().join(".call.yaml.stage").exists());
    assert!(!tempdir.path().join(".call.shell-output.stage").exists());

    std::fs::create_dir(tempdir.path().join(".blocked.shell-output.stage"))
        .expect("nonregular side stage");
    let error = store
        .put_with_side("blocked", &kind, &json!({}), b"side", ByteLimit::new(16))
        .expect_err("nonregular stage must fail closed");
    assert!(matches!(error, VcrError::UnsafePath { .. }));
    assert!(!tempdir.path().join("blocked.yaml").exists());
    assert!(!tempdir.path().join("blocked.shell-output").exists());
}

/// Directory final paths are not cassettes or side artifacts and must fail
/// closed instead of being treated as missing or read as arbitrary directory
/// data.
#[test]
fn store_rejects_nonregular_final_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::create_dir(tempdir.path().join("cassette.yaml")).expect("cassette directory");
    std::fs::create_dir(tempdir.path().join("side.shell-output")).expect("side directory");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");

    assert!(matches!(
        store.get::<serde_json::Value>("cassette"),
        Err(VcrError::UnsafePath { .. })
    ));
    assert!(matches!(
        store.get_side("side", &kind, ByteLimit::new(16)),
        Err(VcrError::UnsafePath { .. })
    ));
}

fn yaml_string_at_limit(limit: usize) -> String {
    let one_byte = serde_yaml_ng::to_string("x").expect("serialize one-byte YAML string");
    let overhead = one_byte.len() - 1;
    let value = "x".repeat(limit - overhead);
    assert_eq!(
        serde_yaml_ng::to_string(&value)
            .expect("serialize YAML string")
            .len(),
        limit
    );
    value
}

/// Cassette publication failure removes the side published by that bundle.
#[test]
fn bundled_side_artifact_cleans_side_when_cassette_publication_fails() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::write(tempdir.path().join("call.yaml"), b"existing").expect("existing cassette");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");
    assert!(
        store
            .put_with_side("call", &kind, &json!({}), b"payload", ByteLimit::new(16))
            .is_err()
    );
    assert!(!tempdir.path().join("call.shell-output").exists());
}

/// A complete side orphan left before cassette publication is reclaimed under
/// the per-key lock so a retry can publish a consistent bundle.
#[test]
fn bundled_side_artifact_reclaims_crash_orphan() {
    let tempdir = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tempdir.path()).expect("dir");
    std::fs::write(tempdir.path().join("call.shell-output"), b"orphan").expect("orphan");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");
    store
        .put_with_side(
            "call",
            &kind,
            &json!({}),
            b"replacement",
            ByteLimit::new(16),
        )
        .expect("retry bundle");
    assert_eq!(
        store
            .get_side("call", &kind, ByteLimit::new(16))
            .expect("side"),
        b"replacement"
    );
}

/// The maximum accepted limit saturates its extra-byte read probe, so a
/// nonempty side still replays instead of panicking or appearing empty.
#[test]
fn bundled_side_artifact_reads_nonempty_at_max_byte_limit() {
    let tempdir = TempDir::new().expect("tempdir");
    let store = VcrStore::new(tempdir.path());
    let kind = ArtifactKind::new("shell-output").expect("kind");
    store
        .put_with_side(
            "maximum",
            &kind,
            &json!({}),
            b"nonempty",
            ByteLimit::new(u64::MAX),
        )
        .expect("maximum limit accepts side");

    assert_eq!(
        store
            .get_side("maximum", &kind, ByteLimit::new(u64::MAX))
            .expect("maximum limit reads side"),
        b"nonempty"
    );
}
