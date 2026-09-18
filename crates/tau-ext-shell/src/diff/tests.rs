use tau_proto::{DiffHunk, DiffLine, DiffSegment};

use super::*;

/// Ensures a whole-file addition uses the unified-diff empty-side coordinate
/// and reports the exact added line and aggregate totals.
#[test]
fn whole_file_addition_uses_zero_old_start() {
    let diff = compute_diff("", "hello\n");

    assert_eq!(diff.removed, 0);
    assert_eq!(diff.added, 1);
    assert_eq!(
        diff.hunks,
        vec![DiffHunk {
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Add {
                text: "hello".into(),
            }],
        }]
    );
}

/// Ensures a whole-file deletion uses the unified-diff empty-side coordinate
/// and reports the exact removed line and aggregate totals.
#[test]
fn whole_file_deletion_uses_zero_new_start() {
    let diff = compute_diff("hello\n", "");

    assert_eq!(diff.removed, 1);
    assert_eq!(diff.added, 0);
    assert_eq!(
        diff.hunks,
        vec![DiffHunk {
            old_start: 1,
            old_count: 1,
            new_start: 0,
            new_count: 0,
            lines: vec![DiffLine::Remove {
                text: "hello".into(),
            }],
        }]
    );
}

/// Ensures multi-line replacements retain exact unified-diff coordinates while
/// rendering all removals before additions for consumers that display hunks.
#[test]
fn multi_line_replacement_renders_removals_before_additions() {
    let diff = compute_diff("one\ntwo\nkeep\n", "alpha\nbeta\nkeep\n");

    assert_eq!(diff.removed, 2);
    assert_eq!(diff.added, 2);
    assert_eq!(
        diff.hunks,
        vec![DiffHunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine::Remove { text: "one".into() },
                DiffLine::Remove { text: "two".into() },
                DiffLine::Add {
                    text: "alpha".into(),
                },
                DiffLine::Add {
                    text: "beta".into(),
                },
                DiffLine::Equal {
                    text: "keep".into(),
                },
            ],
        }]
    );
}

/// Ensures paired single-line replacements preserve unified-diff coordinates
/// and exact intra-line segments so renderers can emphasize only changed text.
#[test]
fn single_line_replacement_still_gets_inline_modify() {
    let diff = compute_diff("let count = 1;\n", "let count = 2;\n");

    assert_eq!(diff.removed, 1);
    assert_eq!(diff.added, 1);
    assert_eq!(
        diff.hunks,
        vec![DiffHunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Modify {
                old: vec![
                    DiffSegment::Equal { text: "let".into() },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Equal {
                        text: "count".into(),
                    },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Equal { text: "=".into() },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Remove { text: "1;".into() },
                ],
                new: vec![
                    DiffSegment::Equal { text: "let".into() },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Equal {
                        text: "count".into(),
                    },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Equal { text: "=".into() },
                    DiffSegment::Equal { text: " ".into() },
                    DiffSegment::Add { text: "2;".into() },
                ],
            }],
        }]
    );
}
