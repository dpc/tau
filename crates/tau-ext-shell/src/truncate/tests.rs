use super::*;

#[test]
fn combined_line_and_byte_truncation_stops_within_budget_without_popping_prefix() {
    let lines = (1..=MAX_OUTPUT_LINES + 1)
        .map(|line| format!("{line} {}", "x".repeat(120)))
        .collect::<Vec<_>>();
    let total_bytes = lines.iter().map(String::len).sum::<usize>() + lines.len() - 1;

    let truncated =
        truncate_line_oriented_lines(lines.iter().map(String::as_str), lines.len(), total_bytes);

    assert!(truncated.was_truncated);
    assert!(truncated.content.len() <= MAX_OUTPUT_BYTES);
    assert!(truncated.content.starts_with("1 "));
}
