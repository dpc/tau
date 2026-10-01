use std::sync::Arc;

use super::*;
use crate::{StyledBlock, layout_block};

/// Creates a small grid with caller-owned bounds and an identifiable fallback.
fn table() -> StyledTable {
    StyledTable {
        rows: vec![
            vec!["Name".into(), "Explanation".into(), "Result".into()],
            vec![
                "one".into(),
                "a longer explanation with words".into(),
                "yes".into(),
            ],
        ],
        indents: vec![String::new(); 3],
        alignments: vec![
            TableColumnAlignment::Left,
            TableColumnAlignment::Right,
            TableColumnAlignment::Center,
        ],
        delimiter_widths: vec![3, 4, 5],
        fallback: "raw fallback".into(),
        style: Style::default().fg(crate::Color::Blue),
        trailing_newline: true,
        max_output_bytes: 8192,
    }
}

/// Flattens projected text without soft-wrapping it again or dropping newlines.
fn text(content: &StyledText) -> String {
    content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect()
}

/// Allocation shrinks widest prose first, breaks ties deterministically,
/// respects unequal minima, and never expands intrinsically small columns.
#[test]
fn widest_first_allocation_respects_minima_and_does_not_expand() {
    let mut widths = [4, 20, 8];
    shrink_widths(&mut widths, &[3, 4, 5], 22).expect("valid width allocation");
    assert_eq!(widths, [4, 10, 8]);
    shrink_widths(&mut widths, &[3, 4, 5], 12).expect("minimum grid fits");
    assert_eq!(widths, [3, 4, 5]);
    shrink_widths(&mut widths, &[3, 4, 5], 200).expect("natural grid fits");
    assert_eq!(widths, [3, 4, 5]);
    let mut widths = [1_000_000_000, 1_000_000_000];
    shrink_widths(&mut widths, &[3, 3], 13).expect("large intrinsic widths shrink");
    assert_eq!(widths, [6, 7], "ties shrink the earlier column first");
    let mut widths = [20, 20, 20];
    shrink_widths(&mut widths, &[3, 3, 5], 12).expect("unequal minima fit");
    assert_eq!(
        widths,
        [3, 4, 5],
        "a minimum above the cap must not receive spare columns"
    );
}

/// Exhaustive small cases compare the optimized common-cap allocator against
/// literal widest-first shrinking, including uneven minima and remainder ties.
#[test]
fn optimized_allocation_matches_widest_first_oracle() {
    for minimum_case in 0..27usize {
        let minima: [usize; 3] =
            std::array::from_fn(|index| 3 + (minimum_case / 3usize.pow(index as u32)) % 3);
        for width_case in 0..216usize {
            let original: [usize; 3] = std::array::from_fn(|index| {
                minima[index] + (width_case / 6usize.pow(index as u32)) % 6
            });
            for budget in minima.iter().sum::<usize>()..=original.iter().sum::<usize>() {
                let mut expected = original;
                while budget < expected.iter().sum() {
                    let index = (0..3)
                        .filter(|index| minima[*index] < expected[*index])
                        .max_by_key(|index| (expected[*index], std::cmp::Reverse(*index)))
                        .expect("minimum grid fits");
                    expected[index] -= 1;
                }
                let mut actual = original;
                shrink_widths(&mut actual, &minima, budget).expect("minimum grid fits");
                assert_eq!(
                    actual, expected,
                    "original={original:?} minima={minima:?} budget={budget}"
                );
            }
        }
    }
}

/// Whitespace at a wrap boundary becomes a newline rather than an empty row,
/// and oversized tokens break only at complete Unicode graphemes.
#[test]
fn cell_wrap_uses_words_then_graphemes() {
    let lines = wrap_cell(&"hello world".into(), 5).expect("word wrapping succeeds");
    assert_eq!(
        lines.iter().map(text).collect::<Vec<_>>(),
        ["hello", "world"]
    );
    let lines = wrap_cell(&"a  b".into(), 8).expect("interior whitespace fits");
    assert_eq!(text(&lines[0]), "a  b");
    let source = "中👨‍👩‍👧‍👦e\u{301}abcdef";
    let lines = wrap_cell(&source.into(), 3).expect("Unicode graphemes fit");
    assert_eq!(lines.iter().map(text).collect::<String>(), source);
    assert!(lines.iter().all(|line| line.char_count() <= 3));
    assert!(lines.iter().map(text).any(|line| line.contains("👨‍👩‍👧‍👦")));
}

/// Graphemes split across style spans use the first scalar's style, while
/// hyperlink metadata survives every hard-wrapped fragment.
#[test]
fn styled_graphemes_and_links_survive_cell_wrap() {
    let style = Style::default().fg(crate::Color::Red).bold();
    let cell = StyledText::from(vec![
        Span::new("e", style).hyperlink("https://example.test"),
        Span::plain("\u{301}abcdefghijkl").hyperlink("https://example.test"),
    ]);
    let lines = wrap_cell(&cell, 3).expect("styled graphemes fit");
    assert_eq!(
        lines.iter().map(text).collect::<String>(),
        "e\u{301}abcdefghijkl"
    );
    assert_eq!(lines[0].to_cells()[0].style, style);
    assert!(
        lines
            .iter()
            .flat_map(StyledText::to_cells)
            .all(|cell| cell.hyperlink.as_deref() == Some("https://example.test"))
    );
}

/// Table borders fit actual content width, with delimiter colons retained and
/// alignment applied independently to each continuation fragment.
#[test]
fn grid_wraps_headers_and_body_at_content_width() {
    let table = table();
    for width in [22, 26, 40, 80] {
        let projected = text(&table.project(width, 0).expect("minimum grid fits"));
        assert!(projected.lines().all(|line| display_width(line) <= width));
        let rows = projected.lines().collect::<Vec<_>>();
        let delimiter = rows
            .iter()
            .find(|line| line.contains("---:"))
            .expect("delimiter row is present");
        assert!(delimiter.contains(":---"));
        assert!(rows.iter().any(|row| row.contains(" yes ")), "{rows:?}");
        assert!(rows.len() > 3 || width == 80);
    }
    assert_eq!(table.project(80, 0), table.project(200, 0));
}

/// Impossible grids, malformed caller data, newline cells, oversized graphemes,
/// and generated-output exhaustion select fallback without truncating.
#[test]
fn invalid_or_over_budget_tables_fall_back() {
    let mut table = table();
    assert!(table.project(21, 0).is_none());
    assert!(table.project(30, usize::MAX).is_none());
    table.max_output_bytes = 24;
    assert!(table.project(24, 0).is_none());
    table.max_output_bytes = 8192;
    table.rows[1][0] = "line\nbreak".into();
    assert!(table.project(24, 0).is_none());
    assert!(wrap_cell(&"中".into(), 1).is_none());
    table.rows[1][0] = "".into();
    assert!(table.project(24, 0).is_some());
    table.delimiter_widths[0] = usize::MAX;
    assert!(table.project(80, 0).is_none());
    table.delimiter_widths.clear();
    assert!(table.project(80, 0).is_none());
    table.delimiter_widths = vec![3, 4, 5];
    table.rows[1].pop();
    assert!(table.project(80, 0).is_none());
}

/// Responsive spans reflow after margins and a prefix, survive cloning, and
/// preserve their plain-text fallback for width-independent consumers.
#[test]
fn span_layout_reserves_markers_and_reflows_after_clone() {
    let table = Arc::new(table());
    let mut span = Span::plain("raw fallback");
    span.table = Some(table);
    let content = StyledText::from(vec![Span::plain("◆ "), span]);
    let block = StyledBlock::new(content).margin_left(2).margin_right(3);
    let restored = block.clone();
    let wide = layout_block(&restored, 80);
    let narrow = layout_block(&restored, 32);
    assert!(wide.len() < narrow.len());
    assert_eq!(wide, layout_block(&restored, 80));
    assert_eq!(text(&block.content), "◆ raw fallback");
    for row in narrow {
        let pipes = row
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.ch == '|')
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(pipes[0], 4);
    }
    let fallback = layout_block(&restored, 8);
    assert!(fallback.iter().flatten().any(|cell| cell.ch == 'r'));
}
