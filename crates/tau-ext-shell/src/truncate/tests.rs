use super::*;

/// Ensures simultaneous line and byte truncation preserves honest totals, a
/// bounded head/tail rendering, and the stable marker for an oversized line.
#[test]
fn combined_line_and_byte_truncation_stops_within_budget_without_popping_prefix() {
    let lines = (1..=MAX_OUTPUT_LINES + 1)
        .map(|line| {
            if line == 1 {
                format!("{line} {}", "x".repeat(MAX_OUTPUT_BYTES))
            } else if line == MAX_OUTPUT_LINES + 1 {
                format!("{line} retained tail")
            } else {
                format!("{line} {}", "x".repeat(120))
            }
        })
        .collect::<Vec<_>>();
    let total_bytes = lines.iter().map(String::len).sum::<usize>() + lines.len() - 1;

    let truncated =
        truncate_line_oriented_lines(lines.iter().map(String::as_str), lines.len(), total_bytes);

    assert_eq!(truncated.total_lines, MAX_OUTPUT_LINES + 1);
    assert_eq!(truncated.total_bytes, total_bytes);
    assert_eq!(
        truncated
            .content
            .lines()
            .filter(|line| *line == "...")
            .count(),
        1
    );
    assert!(truncated.content.starts_with("1(truncated)\n"));
    assert!(truncated.content.ends_with("2001 retained tail"));
    assert!(truncated.content.len() <= MAX_OUTPUT_BYTES);
    assert!(truncated.was_truncated);
}
