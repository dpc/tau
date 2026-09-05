use std::cell::RefCell;

use super::*;

thread_local! {
    /// Test-local scalar sink; no global writer initialization or provider I/O.
    static RECORDS: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
}

/// Intercept admitted rows without bypassing real construction and bounds.
pub(crate) fn observe(value: &Value) -> bool {
    RECORDS.with(|records| {
        if let Some(records) = records.borrow_mut().as_mut() {
            assert!(
                serde_json::to_vec(value).expect("scalar JSON").len()
                    <= tau_provider::cache_diagnostic::MAX_RECORD_BYTES
            );
            records.push(value.clone());
            true
        } else {
            false
        }
    })
}

/// Collect one synchronous attempt's scalar observations.
pub(crate) fn collect<T>(run: impl FnOnce() -> T) -> (T, Vec<Value>) {
    /// Restore the thread-local sink even if an assertion unwinds.
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RECORDS.with(|records| *records.borrow_mut() = None);
        }
    }
    RECORDS.with(|records| {
        assert!(records.borrow().is_none());
        *records.borrow_mut() = Some(Vec::new());
    });
    let _reset = Reset;
    let result = run();
    let records = RECORDS.with(|records| records.borrow_mut().take().expect("installed sink"));
    (result, records)
}

/// Raw observations must preserve counters larger than input and never turn
/// absent cache usage into a fabricated zero.
#[test]
fn raw_usage_preserves_missing_malformed_and_out_of_range_counters() {
    let raw = json!({"input_tokens": 10, "output_tokens": 2,
        "input_tokens_details": {"cached_tokens": 99, "cache_write_tokens": 88}});
    let fields = reported_usage(Some(&raw));
    assert_eq!(fields["reported_usage"]["read_tokens"], 99);
    assert_eq!(fields["reported_usage"]["write_tokens"], 88);
    assert!(reported_usage(None)["reported_usage"]["read_tokens"].is_null());
    let bad = reported_usage(Some(&json!({
        "input_tokens": "credential-canary", "output_tokens": -1,
        "input_tokens_details": {"cached_tokens": false, "cache_write_tokens": []},
        "cache_write_tokens": 7,
    })));
    assert_eq!(bad["reported_usage"]["write_tokens"], 7);
    assert_eq!(
        bad["malformed_fields"],
        json!([
            "input_tokens",
            "read_tokens",
            "write_tokens",
            "output_tokens"
        ])
    );
    assert!(!bad.to_string().contains("credential-canary"));
    assert_eq!(
        reported_usage(Some(&json!([])))["malformed_fields"],
        json!(["reported_usage"])
    );
}

/// Model identities are bounded and the exact untrimmed dispatched credential
/// cannot enter scalar workload metadata.
#[test]
fn model_projection_omits_oversized_and_credential_bearing_identities() {
    assert_eq!(bounded_model("model", " secret "), Some("model"));
    assert_eq!(bounded_model("model secret suffix", " secret "), None);
    assert_eq!(bounded_model(&"x".repeat(129), ""), None);
    assert_eq!(
        bounded_model(&"x".repeat(128), "")
            .expect("model at cap")
            .len(),
        128
    );
}
