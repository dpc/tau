//! Structural equality tests over synthetic private request values.

use serde_json::json;

use super::*;

/// Canonical hashing ignores object member order while preserving array order,
/// scalar type, and domain separation.
#[test]
fn canonical_fingerprints_preserve_the_approved_structural_boundaries() {
    let key = FingerprintKey([7; 32]);
    assert_eq!(
        fingerprint(&key, b"body", &json!({"b":[1,true],"a":"x"})),
        fingerprint(&key, b"body", &json!({"a":"x","b":[1,true]}))
    );
    assert_ne!(
        fingerprint(&key, b"body", &json!({"a":[1,2]})),
        fingerprint(&key, b"body", &json!({"a":[2,1]}))
    );
    assert_ne!(
        fingerprint(&key, b"body", &json!({"a":1})),
        fingerprint(&key, b"body", &json!({"a":"1"}))
    );
    assert_ne!(
        fingerprint(&key, b"body", &json!({"a":1})),
        fingerprint(&key, b"controls", &json!({"a":1}))
    );
}

/// Adapter extraction hashes every unknown member through the complete body and
/// other-fields category without retaining its key or value.
#[test]
fn request_extraction_retains_unknown_structure_only_as_keyed_evidence() {
    let key = FingerprintKey([9; 32]);
    let base = json!({
        "session_id":"session","agent_prompt_id":"prompt",
        "backend":"responses","transport":"http-sse","model":"model",
        "attempt_id":"0123456789abcdef0123456789abcdef",
        "wire_dispatch_index":1,
        "body":{"input":[{"role":"user","content":"private"}],"tools":[],
            "reasoning":{"effort":"high"},"unknown_private":{"secret":"value"}}
    });
    let mut changed = base.clone();
    changed["body"]["unknown_private"]["secret"] = "changed".into();
    let first = request(&key, "instance", &base).expect("request evidence");
    let second = request(&key, "instance", &changed).expect("request evidence");
    assert_ne!(first.body, second.body);
    assert_ne!(first.other, second.other);
    let serialized = serde_json::to_string(&first).expect("stored evidence");
    assert!(!serialized.contains("private"));
    assert!(!serialized.contains("unknown_private"));
    assert!(!serialized.contains("secret"));
    assert!(!serialized.contains("value"));
}
