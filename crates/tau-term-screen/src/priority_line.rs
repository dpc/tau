//! Priority-based single-line layout.

use crate::{Cell, StyledText};

/// Nonnegative importance assigned to one independently hideable line item.
///
/// Zero is the most important value. Larger values disappear first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PriorityLinePriority(
    /// Stored nonnegative importance value.
    u16,
);

impl PriorityLinePriority {
    /// Creates a priority from its nonnegative integer value.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the underlying integer value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Which edge owns an item in a [`PriorityLine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityLineAlignment {
    /// Keep the item in the left-aligned group.
    Left,
    /// Keep the item in the right-aligned group.
    Right,
}

/// One independently hideable item in a [`PriorityLine`].
#[derive(Clone, Debug)]
struct PriorityLineItem {
    /// Styled content retained or hidden as one unit.
    content: StyledText,
    /// Importance where smaller values are retained before larger values.
    priority: PriorityLinePriority,
    /// Edge group that determines the item's visual placement.
    alignment: PriorityLineAlignment,
}

/// A single line that progressively hides lower-importance styled items.
///
/// Items retain insertion order and styling within each alignment group; all
/// retained left items render before all retained right items. Adjacent items
/// within a group receive one plain separator cell, while retained left and
/// right groups receive at least one padding cell between them. Layout hides
/// larger priorities first until the line fits. For equal priorities, later
/// inserted items hide first. Empty content passed to [`Self::push`] is
/// ignored.
///
/// After every lower-importance item is discarded, a highest-importance
/// survivor that cannot fit is also hidden. The result stays empty rather than
/// resurrecting a less-important smaller item, wrapping, or clipping content.
/// Attach the line through [`crate::StyledBlock::priority_line`] so block
/// layout can recompute retention for the current terminal width.
#[derive(Clone, Debug, Default)]
pub struct PriorityLine {
    /// Items in stable visual and equal-priority order.
    items: Vec<PriorityLineItem>,
}

impl PriorityLine {
    /// Creates an empty priority line.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the line contains no accepted items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Appends one independently hideable item to an edge group.
    pub fn push(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: impl Into<StyledText>,
    ) {
        let content = content.into();
        if !content.is_empty() {
            self.items.push(PriorityLineItem {
                content,
                priority,
                alignment,
            });
        }
    }

    /// Lays out retained items in exactly one row for `width` terminal columns.
    pub(crate) fn layout(&self, width: usize, fill: Cell) -> Vec<Cell> {
        let mut retained = vec![true; self.items.len()];
        while minimum_width(&self.items, &retained) > width {
            let Some(index) = self
                .items
                .iter()
                .enumerate()
                .filter(|(index, _)| retained[*index])
                .max_by_key(|(index, item)| (item.priority, *index))
                .map(|(index, _)| index)
            else {
                break;
            };
            retained[index] = false;
        }

        let left = group_cells(&self.items, &retained, PriorityLineAlignment::Left);
        let right = group_cells(&self.items, &retained, PriorityLineAlignment::Right);
        let left_width = cells_width(&left);
        let right_width = cells_width(&right);
        let padding = width.saturating_sub(left_width + right_width);

        let mut row = Vec::new();
        row.extend(left);
        row.extend(std::iter::repeat_n(fill, padding));
        row.extend(right);
        row
    }
}

fn minimum_width(items: &[PriorityLineItem], retained: &[bool]) -> usize {
    let retained_items = items
        .iter()
        .zip(retained)
        .filter(|(_, retained)| **retained);
    let mut count = 0usize;
    let content_width = retained_items
        .map(|(item, _)| {
            count += 1;
            item.content.char_count()
        })
        .sum::<usize>();
    content_width + count.saturating_sub(1)
}

fn group_cells(
    items: &[PriorityLineItem],
    retained: &[bool],
    alignment: PriorityLineAlignment,
) -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut needs_separator = false;
    for (item, retained) in items.iter().zip(retained) {
        if !retained || item.alignment != alignment {
            continue;
        }
        if needs_separator {
            cells.push(Cell::plain(' '));
        }
        cells.extend(item.content.to_cells());
        needs_separator = true;
    }
    cells
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(Cell::col_width).sum()
}

#[cfg(test)]
#[path = "priority_line/tests.rs"]
mod tests;
