//! Text-diff helpers for file mutation tools.

/// Number of unchanged lines to keep around each hunk's edits.
const DIFF_CONTEXT_LINES: usize = 3;

/// Compute a [`tau_proto::DiffSummary`] from two file contents using
/// the `similar` crate. Opcode-level replacements that are exactly one
/// Remove paired with one Add collapse into a single
/// [`tau_proto::DiffLine::Modify`] with intra-line word-level segments; larger
/// replacements render all removals before additions so the displayed order
/// stays unified-diff-like.
pub(crate) fn compute_diff(old: &str, new: &str) -> tau_proto::DiffSummary {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut summary = tau_proto::DiffSummary::default();

    for group in diff.grouped_ops(DIFF_CONTEXT_LINES) {
        if group.is_empty() {
            continue;
        }

        // Hunk header (1-based line numbers like unified-diff).
        let first = &group[0];
        let last = &group[group.len() - 1];
        let old_start = first.old_range().start as u32 + 1;
        let new_start = first.new_range().start as u32 + 1;
        let old_count = (last.old_range().end - first.old_range().start) as u32;
        let new_count = (last.new_range().end - first.new_range().start) as u32;

        let mut lines: Vec<tau_proto::DiffLine> = Vec::new();
        for op in &group {
            let mut removed = Vec::new();
            let mut added = Vec::new();
            let mut equal = Vec::new();

            for change in diff.iter_changes(op) {
                let text = strip_eol(change.value()).to_owned();
                match change.tag() {
                    ChangeTag::Equal => equal.push(text),
                    ChangeTag::Delete => {
                        summary.removed += 1;
                        removed.push(text);
                    }
                    ChangeTag::Insert => {
                        summary.added += 1;
                        added.push(text);
                    }
                }
            }

            if !equal.is_empty() {
                lines.extend(
                    equal
                        .into_iter()
                        .map(|text| tau_proto::DiffLine::Equal { text }),
                );
                continue;
            }

            if removed.len() == 1 && added.len() == 1 {
                lines.push(make_modify(&removed[0], &added[0]));
                continue;
            }

            lines.extend(
                removed
                    .into_iter()
                    .map(|text| tau_proto::DiffLine::Remove { text }),
            );
            lines.extend(
                added
                    .into_iter()
                    .map(|text| tau_proto::DiffLine::Add { text }),
            );
        }

        summary.hunks.push(tau_proto::DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        });
    }

    summary
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

fn make_modify(old: &str, new: &str) -> tau_proto::DiffLine {
    use similar::{ChangeTag, TextDiff};
    let inline = TextDiff::from_words(old, new);
    let mut old_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    let mut new_segs: Vec<tau_proto::DiffSegment> = Vec::new();
    for change in inline.iter_all_changes() {
        let text = change.value().to_owned();
        match change.tag() {
            ChangeTag::Equal => {
                old_segs.push(tau_proto::DiffSegment::Equal { text: text.clone() });
                new_segs.push(tau_proto::DiffSegment::Equal { text });
            }
            ChangeTag::Delete => {
                old_segs.push(tau_proto::DiffSegment::Remove { text });
            }
            ChangeTag::Insert => {
                new_segs.push(tau_proto::DiffSegment::Add { text });
            }
        }
    }
    tau_proto::DiffLine::Modify {
        old: old_segs,
        new: new_segs,
    }
}

#[cfg(test)]
mod tests;
