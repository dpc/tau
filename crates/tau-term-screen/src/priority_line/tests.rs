use super::*;

fn text(cells: &[Cell]) -> String {
    cells.iter().map(|cell| cell.ch).collect()
}

fn layout(line: &PriorityLine, width: usize) -> Vec<Cell> {
    line.layout(width)
}

fn priority(value: u16) -> PriorityLinePriority {
    PriorityLinePriority::new(value)
}

fn truncation(min_width: usize, max_width: usize) -> PriorityLineTruncation {
    PriorityLineTruncation::new(min_width, max_width)
}

/// Exact-fit boundaries must hide only the least-important items needed to fit.
#[test]
fn exact_width_boundaries_drop_larger_priorities_first() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "A");
    line.push(priority(10), PriorityLineAlignment::Left, "B");
    line.push(priority(20), PriorityLineAlignment::Left, "C");

    assert_eq!(text(&layout(&line, 5)), "A B C");
    assert_eq!(text(&layout(&line, 4)), "A B ");
    assert_eq!(text(&layout(&line, 3)), "A B");
    assert_eq!(text(&layout(&line, 2)), "A ");
    assert_eq!(text(&layout(&line, 1)), "A");
    assert_eq!(text(&layout(&line, 0)), "");
}

/// Wide Unicode graphemes must consume terminal columns rather than scalar or
/// byte counts when deciding whether another item fits.
#[test]
fn unicode_display_width_controls_retention() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "界");
    line.push(priority(10), PriorityLineAlignment::Left, "Q");

    assert_eq!(text(&layout(&line, 4)), "界 Q");
    assert_eq!(text(&layout(&line, 3)), "界 ");
    assert_eq!(cells_width(&layout(&line, 3)), 3);
}

/// Equal priorities use reverse insertion order for hiding so repeated layout
/// and resize passes cannot flicker between equally important items.
#[test]
fn equal_priority_items_hide_later_items_first() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "A");
    line.push(priority(10), PriorityLineAlignment::Left, "B");
    line.push(priority(10), PriorityLineAlignment::Left, "C");

    assert_eq!(text(&layout(&line, 3)), "A B");
}

/// Equal-priority truncatable items grow in insertion order, mirroring the
/// stable reverse-insertion rule used when equal-priority items must hide.
#[test]
fn equal_priority_items_grow_earlier_items_first() {
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(10),
        PriorityLineAlignment::Left,
        "abcdef",
        truncation(3, 6),
    );
    line.push_truncated(
        priority(10),
        PriorityLineAlignment::Left,
        "123456789",
        truncation(3, 6),
    );

    assert_eq!(text(&layout(&line, 10)), "abcdef 1┄9");
    assert_eq!(text(&layout(&line, 13)), "abcdef 123┄89");
}

/// Separators belong to retained neighbors, preventing leading, trailing, or
/// doubled separators when an item between them disappears.
#[test]
fn hidden_items_leave_no_dangling_separators() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "left");
    line.push(priority(5), PriorityLineAlignment::Left, "");
    line.push(priority(30), PriorityLineAlignment::Left, "detail");
    line.push(priority(10), PriorityLineAlignment::Right, "right");

    assert_eq!(text(&layout(&line, 10)), "left right");
    assert_eq!(text(&layout(&line, 5)), "left ");
}

/// Re-laying out one immutable line at narrower and wider sizes must restore
/// the same retained items deterministically after a terminal resize.
#[test]
fn resizing_recomputes_from_all_items() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "id");
    line.push(priority(20), PriorityLineAlignment::Right, "tools");

    assert_eq!(text(&layout(&line, 8)), "id tools");
    assert_eq!(text(&layout(&line, 2)), "id");
    assert_eq!(text(&layout(&line, 8)), "id tools");
}

/// If the most-important item itself cannot fit, the line stays single-row and
/// empty instead of resurrecting a smaller discarded item, wrapping, or
/// clipping.
#[test]
fn too_narrow_for_highest_priority_item_renders_empty() {
    let mut line = PriorityLine::new();
    line.push(priority(0), PriorityLineAlignment::Left, "wide");
    line.push(priority(10), PriorityLineAlignment::Left, "x");

    assert_eq!(text(&layout(&line, 3)), "   ");
}

/// Multiple truncatable items reserve their minima, grow in priority order,
/// and disappear only when all retained minimum representations cannot fit.
#[test]
fn multiple_truncatable_items_allocate_deterministically() {
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        "abcdef",
        truncation(3, 6),
    );
    line.push_truncated(
        priority(30),
        PriorityLineAlignment::Left,
        "123456789",
        truncation(3, 7),
    );
    line.push_truncated(
        priority(40),
        PriorityLineAlignment::Left,
        "@abcdef",
        truncation(3, 6),
    );

    assert_eq!(text(&layout(&line, 21)), "abcdef 123┄789 @ab┄ef");
    assert_eq!(text(&layout(&line, 17)), "abcdef 123┄89 @┄f");
    assert_eq!(text(&layout(&line, 14)), "abcdef 1┄9 @┄f");
    assert_eq!(text(&layout(&line, 11)), "a┄f 1┄9 @┄f");
    assert_eq!(text(&layout(&line, 10)), "abcdef 1┄9");
}

/// Truncation boundaries must preserve complete wide graphemes, use display
/// columns, and restore the configured maximum after a resize.
#[test]
fn middle_truncation_handles_unicode_and_resize_boundaries() {
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        "ab界cd",
        truncation(4, 5),
    );

    assert_eq!(text(&layout(&line, 5)), "ab┄cd");
    assert_eq!(text(&layout(&line, 4)), "ab┄d");
    assert_eq!(text(&layout(&line, 3)), "   ");
    assert_eq!(text(&layout(&line, 5)), "ab┄cd");
}

/// Configured minima reserve terminal columns even when a wide grapheme cannot
/// use every reserved column around the one-column marker.
#[test]
fn wide_grapheme_truncation_cannot_render_below_minimum() {
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        "界界",
        truncation(2, 4),
    );

    assert_eq!(text(&layout(&line, 1)), " ");
    let at_minimum = layout(&line, 2);
    assert_eq!(text(&at_minimum), "┄ ");
    assert_eq!(cells_width(&at_minimum), 2);
}

/// Standalone zero-column graphemes remain independently selectable suffix
/// clusters instead of being mistaken for continuation cells of their neighbor.
#[test]
fn middle_truncation_preserves_standalone_zero_width_graphemes() {
    let zero_width_style = crate::Style::default().fg(crate::Color::Blue);
    let content = crate::StyledText::from(vec![
        crate::Span::new("abcd", crate::Style::default()),
        crate::Span::new("\u{200b}e", zero_width_style),
    ]);
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        content,
        truncation(4, 4),
    );

    let cells = layout(&line, 4);
    assert_eq!(text(&cells), "ab┄\u{200b}e");
    assert_eq!(cells_width(&cells), 4);
    assert_eq!(
        cells
            .iter()
            .find(|cell| cell.ch == '\u{200b}')
            .expect("standalone zero-width grapheme")
            .style,
        zero_width_style
    );
}

/// Attached details must consume no separator, retain their own style through
/// middle truncation, and disappear cleanly without leaving punctuation.
#[test]
fn attached_truncated_item_preserves_separator_and_style_semantics() {
    let error_style = crate::Style::default().fg(crate::Color::Red);
    let mut line = PriorityLine::new();
    line.set_separator_style(error_style);
    line.push(priority(10), PriorityLineAlignment::Left, "err");
    line.push_truncated_attached(
        priority(20),
        PriorityLineAlignment::Left,
        crate::Span::new(": abcdef", error_style),
        truncation(3, 5),
    );

    let cells = layout(&line, 8);
    assert_eq!(text(&cells), "err: ┄ef");
    assert_eq!(
        cells
            .iter()
            .find(|cell| cell.ch == '┄')
            .expect("truncation marker")
            .style,
        error_style
    );
    assert_eq!(text(&layout(&line, 5)), "err  ");
}

/// Configured separator styling survives ordinary spacing while attached
/// fragments continue to omit that separator.
#[test]
fn configured_separator_style_is_preserved() {
    let separator_style = crate::Style::default().bg(crate::Color::DarkBlue);
    let mut line = PriorityLine::new();
    line.set_separator_style(separator_style);
    line.push(priority(0), PriorityLineAlignment::Left, "A");
    line.push(priority(10), PriorityLineAlignment::Left, "B");

    let cells = layout(&line, 3);
    assert_eq!(text(&cells), "A B");
    assert_eq!(cells[1].style, separator_style);
}

/// A terminal narrower than every configured minimum must render only fill
/// cells rather than a marker-only fragment or a lower-priority survivor.
#[test]
fn too_narrow_for_truncated_representation_renders_empty() {
    let mut line = PriorityLine::new();
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        "abcdef",
        truncation(3, 6),
    );
    line.push(priority(10), PriorityLineAlignment::Left, "x");

    assert_eq!(text(&layout(&line, 2)), "  ");
}

/// A required priority band must fail closed as one semantic set when the
/// terminal cannot retain every accepted item in that band.
#[test]
fn required_priority_band_never_renders_partially() {
    let mut line = PriorityLine::new();
    line.require_through(priority(10));
    line.push_truncated(
        priority(0),
        PriorityLineAlignment::Left,
        "identity",
        truncation(4, 8),
    );
    line.push(priority(10), PriorityLineAlignment::Left, "err");
    line.push(priority(20), PriorityLineAlignment::Left, "details");

    assert_eq!(text(&layout(&line, 8)), "id┄y err");
    assert_eq!(text(&layout(&line, 7)), "       ");
}
