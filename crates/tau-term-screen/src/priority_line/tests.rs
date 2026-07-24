use super::*;

fn text(cells: &[Cell]) -> String {
    cells.iter().map(|cell| cell.ch).collect()
}

fn layout(line: &PriorityLine, width: usize) -> Vec<Cell> {
    line.layout(width, Cell::plain(' '))
}

fn priority(value: u16) -> PriorityLinePriority {
    PriorityLinePriority::new(value)
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
