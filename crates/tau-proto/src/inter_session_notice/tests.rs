use super::*;

/// Transparent serde preserves empty text and rejects values above the shared
/// UTF-8 byte limit before they can enter wire or journal DTOs.
#[test]
fn serde_preserves_empty_notice_and_rejects_oversized_text() {
    let notice: InterSessionNotice =
        serde_json::from_str("\"\"").expect("empty notice remains configured");
    assert_eq!(notice.as_str(), "");
    assert_eq!(
        serde_json::to_string(&notice).expect("serialize notice"),
        "\"\""
    );

    let oversized = "x".repeat(INTER_SESSION_NOTICE_MAX_BYTES + 1);
    let error = serde_json::from_value::<InterSessionNotice>(serde_json::json!(oversized))
        .expect_err("oversized notice must fail");
    assert!(error.to_string().contains("64 KiB"), "{error}");
}
