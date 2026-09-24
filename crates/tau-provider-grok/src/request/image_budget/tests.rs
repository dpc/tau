//! Aggregate image admission oracles without oversized encoded allocations.

use super::*;
use crate::request::tests::image_result;

/// No bytes escape failed results; each independent byte bound is enforced.
#[test]
fn image_budget_omits_failed_unsupported_and_oversized_images() {
    let mut result = image_result();
    let mut budget = ImageBudget::new(true);
    result.status = ToolResultStatus::Error {
        message: "failed".into(),
    };
    assert!(budget.output(&result).is_string());
    result.status = ToolResultStatus::Success;
    budget.image_bytes = 24 * 1024 * 1024;
    let output = budget.output(&result);
    assert!(
        output[1]["text"]
            .as_str()
            .expect("omission")
            .contains("aggregate")
    );
    assert_eq!(budget.data_url_bytes, 0);
    budget.image_bytes = 0;
    budget.data_url_bytes = 32 * 1024 * 1024;
    assert!(!budget.output(&result).to_string().contains("base64"));
    assert_eq!(budget.image_bytes, 0);
}

/// Admitting a result consumes the allowance for the next result in the
/// request.
#[test]
fn image_budget_accumulates_across_tool_results() {
    let result = image_result();
    let mut budget = ImageBudget::new(true);
    budget.image_bytes = 24 * 1024 * 1024 - 3;
    let accepted = budget.output(&result);
    assert_eq!(accepted[1]["image_url"], "data:image/png;base64,YWJj");
    assert_eq!(budget.image_bytes, 24 * 1024 * 1024);
    assert_eq!(budget.data_url_bytes, "data:image/png;base64,YWJj".len());
    let rejected = budget.output(&result);
    assert!(
        rejected[1]["text"]
            .as_str()
            .expect("omission")
            .contains("aggregate")
    );
    assert_eq!(budget.data_url_bytes, "data:image/png;base64,YWJj".len());
}
