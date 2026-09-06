use std::cell::RefCell;

use super::*;

thread_local! {
    /// Thread-local scalar sink without initializing process-global IPC.
    static RECORDS: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
}

/// Intercept admitted scalar records while checking their production bound.
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

/// Collect scalar records on this attempt's current-thread runtime.
pub(crate) fn collect<T>(run: impl FnOnce() -> T) -> (T, Vec<Value>) {
    /// Restore the sink on normal return or test unwind.
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

/// Exact-route capability controls raw cache counters; values larger than input
/// remain raw, and unknown provider fields cannot enter scalar metadata.
#[test]
fn raw_usage_obeys_exact_route_and_preserves_unclamped_counters() {
    let usage = json!({
        "prompt_tokens": 10, "completion_tokens": 2,
        "completion_tokens_details": {"reasoning_tokens": 1},
        "prompt_tokens_details": {"cached_tokens": 99, "cache_write_tokens": 88},
        "prompt_cache_hit_tokens": 77, "prompt_cache_miss_tokens": 66,
        "unknown": "secret-canary",
    });
    let openai = reported_usage(Some(&usage), CacheUsageCompat::OpenAi);
    assert_eq!(openai["reported_usage"]["read_tokens"], 99);
    assert_eq!(openai["reported_usage"]["write_tokens"], 88);
    assert_eq!(openai["reported_usage"]["reasoning_output_tokens"], 1);
    assert!(openai["reported_usage"]["miss_tokens"].is_null());
    assert!(!openai.to_string().contains("secret-canary"));
    let deepseek = reported_usage(Some(&usage), CacheUsageCompat::DeepSeek);
    assert_eq!(deepseek["reported_usage"]["read_tokens"], 77);
    assert_eq!(deepseek["reported_usage"]["miss_tokens"], 66);
    assert!(deepseek["reported_usage"]["write_tokens"].is_null());
    let none = reported_usage(Some(&usage), CacheUsageCompat::None);
    assert!(none["reported_usage"]["read_tokens"].is_null());
    assert!(none["reported_usage"]["write_tokens"].is_null());
    assert!(none["reported_usage"]["miss_tokens"].is_null());
    assert_eq!(none["reported_usage"]["input_tokens"], 10);
}

/// Missing/null/malformed counters never become invented zeros or copied prose.
#[test]
fn raw_usage_marks_malformed_without_retaining_values() {
    let bad = reported_usage(
        Some(&json!({
            "prompt_tokens": "secret-canary", "completion_tokens": -1,
            "prompt_tokens_details": {"cached_tokens": false, "cache_write_tokens": []},
            "cache_write_tokens": 7, "completion_tokens_details": [],
        })),
        CacheUsageCompat::OpenAi,
    );
    assert_eq!(bad["reported_usage"]["write_tokens"], 7);
    assert!(bad["reported_usage"]["read_tokens"].is_null());
    assert_eq!(
        bad["malformed_fields"],
        json!([
            "completion_tokens_details",
            "input_tokens",
            "output_tokens",
            "read_tokens",
            "write_tokens",
        ])
    );
    assert!(!bad.to_string().contains("secret-canary"));
    assert_eq!(
        reported_usage(Some(&json!([])), CacheUsageCompat::None)["malformed_fields"],
        json!(["reported_usage"])
    );
    assert!(
        reported_usage(None, CacheUsageCompat::OpenAi)["reported_usage"]["read_tokens"].is_null()
    );
    assert_eq!(
        reported_usage(Some(&Value::Null), CacheUsageCompat::OpenAi)["malformed_fields"],
        json!([])
    );
}

/// Model identities stay bounded and cannot reflect the exact credential.
#[test]
fn model_identity_omits_oversized_and_credential_bearing_values() {
    assert_eq!(bounded_model("model", " secret "), Some("model"));
    assert_eq!(bounded_model("model secret suffix", " secret "), None);
    assert_eq!(bounded_model(&"x".repeat(129), ""), None);
    assert_eq!(bounded_model("short", "s"), None);
}
