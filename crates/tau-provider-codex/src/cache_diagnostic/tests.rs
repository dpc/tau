use std::cell::RefCell;

use super::*;

thread_local! {
    /// Enabled only by one scoped test; normal tests retain no metadata rows.
    static RECORDS: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
}

/// Capture a bounded set of production scalar projections on this test thread.
pub(crate) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<Value>) {
    RECORDS.with(|records| {
        assert!(records.replace(Some(Vec::new())).is_none());
    });
    let result = run();
    let records = RECORDS.with(|records| records.take().expect("active test sink"));
    (result, records)
}

/// Intercept a production record only while an explicit local test sink exists.
pub(super) fn observe(record: &Value) -> bool {
    RECORDS.with(|records| {
        let mut records = records.borrow_mut();
        let Some(records) = records.as_mut() else {
            return false;
        };
        assert!(records.len() < 64, "test sink bound");
        records.push(record.clone());
        true
    })
}

/// Malformed fields cannot become zeros or contaminate independently valid
/// counters; read evidence never synthesizes eligibility or reported misses.
#[test]
fn raw_usage_preserves_absence_zero_and_malformed_fields() {
    let mut malformed = Vec::new();
    let usage = reported_usage(
        Some(&json!({
            "input_tokens": 0, "output_tokens": -1,
            "input_tokens_details": {"cached_tokens": "secret", "cache_write_tokens": 2},
            "arbitrary": "PRIVATE"
        })),
        &mut malformed,
    );
    assert_eq!(usage["input_tokens"], 0);
    assert!(usage["read_tokens"].is_null());
    assert!(usage["output_tokens"].is_null());
    assert_eq!(usage["write_tokens"], 2);
    assert!(usage["miss_tokens"].is_null());
    assert_eq!(malformed, ["read_tokens", "output_tokens"]);
    assert!(!usage.to_string().contains("secret"));
    assert!(!usage.to_string().contains("PRIVATE"));
    assert!(reported_usage(None, &mut Vec::new())["input_tokens"].is_null());
}

/// Identity strings are omitted whole at the byte boundary and when reflecting
/// a configured credential; generic Debug never contains diagnostic IDs.
#[test]
fn identities_are_bounded_and_credentials_never_project() {
    let config = crate::tests::test_config("https://private.invalid".to_owned());
    let mut omitted = Vec::new();
    let boundary = "é".repeat(64);
    assert_eq!(
        identity(Some(&boundary), "actual_model", &config, &mut omitted),
        Some(boundary.as_str())
    );
    assert!(
        identity(
            Some(&format!("{boundary}x")),
            "actual_model",
            &config,
            &mut omitted
        )
        .is_none()
    );
    assert!(identity(Some(&config.api_key), "actual_model", &config, &mut omitted).is_none());
    assert_eq!(omitted, ["actual_model", "actual_model"]);
    let id = DiagnosticId::random().expect("entropy");
    let serialized = serde_json::to_value(id).expect("diagnostic ID serialization");
    assert!(!format!("{id:?}").contains(serialized.as_str().expect("hex string")));
}
