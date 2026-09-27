use super::*;

/// A valid snapshot below the file quota may still exceed the decoded
/// protocol frame limit because CBOR encodes bytes as an expanded array.
/// The exact reply measurement must reject it without moving the file.
#[test]
fn large_valid_history_returns_bounded_rpc_error_without_archiving() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    std::fs::create_dir_all(&root).expect("root");
    let line = b"{\"schema\":1,\"agent_id\":\"agent-one\",\"session_id\":\"session-one\",\"timestamp_us\":1000000,\"report\":\"valid\"}\n";
    let contents = line.repeat(92_000);
    assert!(contents.len() as u64 <= crate::EXTENSION_DATA_MAX_FILE_BYTES);
    let active = root.join(tau_proto::PAPERCUT_FILE_NAME);
    std::fs::write(&active, &contents).expect("active fixture");
    let value = run_papercut_read(&root).expect("locked snapshot");
    let result = bounded_papercut_read_response(
        "fixture-request",
        tau_proto::ExtensionDataResultPayload::Ok { value },
    );
    assert!(matches!(
        result,
        tau_proto::ExtensionDataResultPayload::Error {
            kind: tau_proto::ExtensionDataErrorKind::QuotaExceeded,
            ..
        }
    ));
    assert_eq!(std::fs::read(&active).expect("active intact"), contents);
    assert_eq!(std::fs::read_dir(root).expect("root").count(), 1);
}

/// A small snapshot and its request correlation still fit the exact
/// directed reply envelope and are not rejected based on raw byte guesses.
#[test]
fn small_history_reply_preserves_complete_bytes() {
    let result = bounded_papercut_read_response(
        "fixture-request",
        tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::ReadPapercuts {
                contents: Some(b"small\n".to_vec()),
            },
        },
    );
    assert_eq!(
        result,
        tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::ReadPapercuts {
                contents: Some(b"small\n".to_vec()),
            },
        }
    );
}
