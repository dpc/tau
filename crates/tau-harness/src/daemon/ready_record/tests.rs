use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use super::*;

/// A readiness line must preserve path bytes exactly or reject the identity,
/// and its newline-inclusive limit must hold even for heavily escaped paths.
#[test]
fn readiness_rejects_unrepresentable_and_oversized_paths() {
    let id = SessionId::parse("project-abc123").expect("valid session id");
    let non_utf8 = Path::new(OsStr::from_bytes(b"/project-\xff"));
    assert!(ReadyRecord::encode(&id, non_utf8).is_err());
    let escaped = format!("/{}", "\n".repeat(MAX_RECORD_BYTES / 2));
    assert!(ReadyRecord::encode(&id, Path::new(&escaped)).is_err());
    let record =
        ReadyRecord::encode(&id, Path::new("/project\n\"")).expect("encode escaped identity");
    let mut output = Vec::new();
    record.write_to(&mut output).expect("write readiness");
    assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    let decoded: serde_json::Value = serde_json::from_slice(&output).expect("decode readiness");
    assert_eq!(decoded["project_root"], "/project\n\"");
    assert_eq!(decoded["session_id"], id.as_str());
}
