//! Output-truncation helpers shared by every tool.

/// Maximum lines before truncation kicks in.
pub(crate) const MAX_OUTPUT_LINES: usize = 2000;
/// Number of leading lines kept when line-count truncation kicks in.
pub(crate) const TRUNCATED_OUTPUT_HEAD_LINES: usize = MAX_OUTPUT_LINES / 2;
/// Number of trailing lines kept when line-count truncation kicks in.
pub(crate) const TRUNCATED_OUTPUT_TAIL_LINES: usize = MAX_OUTPUT_LINES / 2;
/// Maximum bytes before truncation kicks in.
pub(crate) const MAX_OUTPUT_BYTES: usize = 10 * 1024;

/// Result of a truncation operation.
pub(crate) struct Truncated {
    pub(crate) content: String,
    pub(crate) was_truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
}

/// Truncate line-oriented output without adding prose notices.
///
/// When the line count is too high, up to 1000 lines from each end are selected
/// within the byte budget with a literal `...` separator. Individually
/// oversized lines become marker-only records such as `out(truncated)`.
pub(crate) fn truncate_line_oriented(input: &str) -> Truncated {
    let lines: Vec<&str> = input.lines().collect();
    truncate_line_oriented_lines(lines.iter().copied(), lines.len(), input.len())
}

/// Truncate already-rendered line-oriented output with known original totals.
pub(crate) fn truncate_line_oriented_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    total_lines: usize,
    total_bytes: usize,
) -> Truncated {
    truncate_line_oriented_lines_with_byte_limit(lines, total_lines, total_bytes, MAX_OUTPUT_BYTES)
}

/// Truncate already-rendered output with a caller-specific byte budget.
pub(crate) fn truncate_line_oriented_lines_with_byte_limit<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    total_lines: usize,
    total_bytes: usize,
    max_output_bytes: usize,
) -> Truncated {
    let all_lines: Vec<&str> = lines.into_iter().collect();
    let line_count_truncated = MAX_OUTPUT_LINES < total_lines;
    if line_count_truncated {
        let mut head = Vec::new();
        let mut head_bytes = 0usize;
        for line in all_lines.iter().take(TRUNCATED_OUTPUT_HEAD_LINES) {
            let rendered = if max_output_bytes < line.len() {
                mark_line(line, "truncated")
            } else {
                (*line).to_owned()
            };
            if max_output_bytes / 2
                < head_bytes.saturating_add(usize::from(!head.is_empty()) + rendered.len())
            {
                if head.is_empty() {
                    let marker = mark_line(line, "truncated");
                    let _ =
                        push_budgeted_line(&mut head, &mut head_bytes, &marker, max_output_bytes);
                }
                break;
            }
            let _ = push_budgeted_line(&mut head, &mut head_bytes, &rendered, max_output_bytes);
        }
        let _ = push_budgeted_line(&mut head, &mut head_bytes, "...", max_output_bytes);
        let mut tail = Vec::new();
        let mut remaining = max_output_bytes.saturating_sub(head_bytes);
        for line in all_lines.iter().rev().take(TRUNCATED_OUTPUT_TAIL_LINES) {
            let rendered = if max_output_bytes < line.len() {
                mark_line(line, "truncated")
            } else {
                (*line).to_owned()
            };
            let needed = rendered.len() + 1;
            if remaining < needed {
                if tail.is_empty() {
                    let marker = mark_line(line, "truncated");
                    if marker.len() < remaining {
                        tail.push(marker);
                    }
                }
                break;
            }
            remaining -= needed;
            tail.push(rendered);
        }
        tail.reverse();
        head.extend(tail);
        return Truncated {
            content: head.join("\n"),
            was_truncated: true,
            total_lines,
            total_bytes,
        };
    }
    let selected: Vec<Option<&str>> = all_lines.iter().copied().map(Some).collect();

    let mut rendered = Vec::with_capacity(selected.len());
    let mut rendered_bytes = 0usize;
    let mut was_truncated = line_count_truncated || max_output_bytes < total_bytes;
    for line in selected {
        let line = match line {
            Some(line) => line,
            None => {
                if !push_budgeted_line(&mut rendered, &mut rendered_bytes, "...", max_output_bytes)
                {
                    was_truncated = true;
                    break;
                }
                continue;
            }
        };
        let separator_bytes = usize::from(!rendered.is_empty());
        if max_output_bytes < line.len()
            || max_output_bytes < rendered_bytes.saturating_add(separator_bytes + line.len())
        {
            let marker = mark_line(line, "truncated");
            if !push_budgeted_line(
                &mut rendered,
                &mut rendered_bytes,
                &marker,
                max_output_bytes,
            ) {
                break;
            }
            was_truncated = true;
        } else if !push_budgeted_line(&mut rendered, &mut rendered_bytes, line, max_output_bytes) {
            was_truncated = true;
            break;
        }
    }

    Truncated {
        content: rendered.join("\n"),
        was_truncated,
        total_lines,
        total_bytes,
    }
}

fn can_push_budgeted_line(
    rendered: &[String],
    rendered_bytes: usize,
    line: &str,
    max_output_bytes: usize,
) -> bool {
    let separator_bytes = usize::from(!rendered.is_empty());
    rendered_bytes.saturating_add(separator_bytes + line.len()) <= max_output_bytes
}

fn push_budgeted_line(
    rendered: &mut Vec<String>,
    rendered_bytes: &mut usize,
    line: &str,
    max_output_bytes: usize,
) -> bool {
    if !can_push_budgeted_line(rendered, *rendered_bytes, line, max_output_bytes) {
        return false;
    }
    let separator_bytes = usize::from(!rendered.is_empty());
    rendered.push(line.to_owned());
    *rendered_bytes += separator_bytes + line.len();
    true
}

/// Add a marker to a rendered line prefix and skip its content.
pub(crate) fn mark_line(line: &str, marker: &str) -> String {
    let prefix = line.split_once(' ').map_or_else(
        || {
            if line.chars().all(|ch| ch.is_ascii_digit()) {
                line
            } else {
                ""
            }
        },
        |(prefix, _)| prefix,
    );
    if let Some((base, existing)) = prefix.split_once('(')
        && let Some(existing) = existing.strip_suffix(')')
    {
        return format!("{base}({existing},{marker})");
    }
    format!("{prefix}({marker})")
}

/// Truncate from the head (keep first and last lines with a separator).
pub(crate) fn truncate_head(input: &str) -> Truncated {
    truncate_line_oriented(input)
}

/// Truncate from the tail (kept for callers that only need line-oriented
/// truncation).
#[cfg(test)]
pub(crate) fn truncate_tail(input: &str) -> Truncated {
    truncate_line_oriented(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_line_and_byte_truncation_stops_within_budget_without_popping_prefix() {
        let lines = (1..=MAX_OUTPUT_LINES + 1)
            .map(|line| format!("{line} {}", "x".repeat(120)))
            .collect::<Vec<_>>();
        let total_bytes = lines.iter().map(String::len).sum::<usize>() + lines.len() - 1;

        let truncated = truncate_line_oriented_lines(
            lines.iter().map(String::as_str),
            lines.len(),
            total_bytes,
        );

        assert!(truncated.was_truncated);
        assert!(truncated.content.len() <= MAX_OUTPUT_BYTES);
        assert!(truncated.content.starts_with("1 "));
    }
}
