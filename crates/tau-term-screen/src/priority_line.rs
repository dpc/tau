//! Priority-based single-line layout.

use crate::style::{is_line_break_grapheme, push_grapheme_cells, visit_styled_graphemes};
use crate::{Cell, Style, StyledText};

/// Display-column bounds for middle truncation of one [`PriorityLine`] item.
///
/// Both bounds include the one-column `┄` marker. Content wider than
/// `max_width` is always truncated, even when the line has spare room. A
/// retained item may shrink as far as `min_width` while competing for space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriorityLineTruncation {
    /// Smallest permitted retained representation, including `┄`.
    min_width: usize,
    /// Largest permitted retained representation, including `┄`.
    max_width: usize,
}

impl PriorityLineTruncation {
    /// Creates valid inclusive display-column bounds.
    ///
    /// # Panics
    ///
    /// Panics when `min_width` is zero or exceeds `max_width`.
    #[must_use]
    pub const fn new(min_width: usize, max_width: usize) -> Self {
        assert!(0 < min_width, "minimum truncation width must be positive");
        assert!(
            min_width <= max_width,
            "minimum truncation width must not exceed maximum"
        );
        Self {
            min_width,
            max_width,
        }
    }

    /// Returns the smallest permitted retained display width.
    #[must_use]
    pub const fn min_width(self) -> usize {
        self.min_width
    }

    /// Returns the largest permitted retained display width.
    #[must_use]
    pub const fn max_width(self) -> usize {
        self.max_width
    }
}

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
    /// Optional inclusive bounds for middle truncation.
    truncation: Option<PriorityLineTruncation>,
    /// Whether a retained predecessor in the same group receives a separator.
    separated: bool,
}

/// Sanitized cells plus exact source grapheme boundaries for one item.
struct PriorityLineItemCells {
    /// Sanitized styled character cells.
    cells: Vec<Cell>,
    /// Cell ranges for complete non-line-break grapheme clusters.
    graphemes: Vec<std::ops::Range<usize>>,
}

/// One internal layout pass and its essential-band outcome.
pub(crate) struct PriorityLineLayout {
    /// Exactly one terminal row.
    pub(crate) row: Vec<Cell>,
    /// Whether every configured essential item survived.
    pub(crate) required_items_fit: bool,
}

/// A single line that progressively hides lower-importance styled items.
///
/// Items retain insertion order and styling within each alignment group; all
/// retained left items render before all retained right items. Normally,
/// adjacent items within a group receive one separator cell; explicitly
/// attached fragments do not. Retained left and right groups receive at least
/// one padding cell between them. Layout hides larger priorities first until
/// the line fits. For equal priorities, later inserted items hide first. Empty
/// content passed to any push method is ignored.
///
/// After every lower-importance item is discarded, a highest-importance
/// survivor that cannot fit is also hidden. The result stays empty rather than
/// resurrecting a less-important smaller item, wrapping, or clipping content.
/// Attach the line through [`crate::StyledBlock::priority_line`] so block
/// layout can recompute retention for the current terminal width.
///
/// Truncatable items first reserve their configured minimum representation.
/// If all item minima do not fit, whole items disappear by the same priority
/// rule as non-truncatable items. Remaining columns then grow retained
/// truncatable items toward their configured maxima in ascending priority and
/// insertion order. This makes competition deterministic while retaining as
/// many useful elements as their minima permit. Truncation preserves complete
/// Unicode grapheme clusters and terminal display width, using exactly `┄`
/// between the retained prefix and suffix.
///
/// Callers may mark an essential priority band with [`Self::require_through`].
/// If any accepted item in that band cannot survive, the line becomes empty
/// instead of presenting only part of the essential meaning.
#[derive(Clone, Debug, Default)]
pub struct PriorityLine {
    /// Items in stable visual and equal-priority order.
    items: Vec<PriorityLineItem>,
    /// Optional strongest-to-threshold band that must survive as a unit.
    required_through: Option<PriorityLinePriority>,
    /// Style used for one-column separators within edge groups.
    separator_style: Style,
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

    /// Requires every accepted item at or above `priority` to survive layout.
    ///
    /// If the terminal cannot fit those items at their minimum
    /// representations, layout returns a fill-only row instead of presenting
    /// an incomplete essential set. Items with larger numeric priorities keep
    /// their normal independent truncation and hiding behavior.
    pub fn require_through(&mut self, priority: PriorityLinePriority) {
        self.required_through = Some(priority);
    }

    /// Sets the style for one-column separators between ordinary items.
    pub fn set_separator_style(&mut self, style: Style) {
        self.separator_style = style;
    }

    /// Appends one independently hideable item to an edge group.
    pub fn push(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: impl Into<StyledText>,
    ) {
        self.push_item(priority, alignment, content.into(), None, true);
    }

    /// Appends one middle-truncatable item to an edge group.
    ///
    /// The item stays whole when it fits within `truncation.max_width()`;
    /// otherwise layout retains a prefix and suffix around the exact `┄`
    /// marker. It may disappear by priority when even all retained items'
    /// minimum representations and separators cannot fit.
    pub fn push_truncated(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: impl Into<StyledText>,
        truncation: PriorityLineTruncation,
    ) {
        self.push_item(priority, alignment, content.into(), Some(truncation), true);
    }

    /// Appends one item without a separator from its retained predecessor.
    ///
    /// This is for independently prioritized fragments that form one visual
    /// token, such as a status label followed by optional details beginning
    /// with punctuation.
    pub fn push_attached(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: impl Into<StyledText>,
    ) {
        self.push_item(priority, alignment, content.into(), None, false);
    }

    /// Appends one middle-truncatable item without a preceding separator.
    ///
    /// The truncation and disappearance rules match [`Self::push_truncated`].
    pub fn push_truncated_attached(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: impl Into<StyledText>,
        truncation: PriorityLineTruncation,
    ) {
        self.push_item(priority, alignment, content.into(), Some(truncation), false);
    }

    /// Stores one nonempty item with its optional truncation policy.
    fn push_item(
        &mut self,
        priority: PriorityLinePriority,
        alignment: PriorityLineAlignment,
        content: StyledText,
        truncation: Option<PriorityLineTruncation>,
        separated: bool,
    ) {
        if content.is_empty() {
            return;
        }
        self.items.push(PriorityLineItem {
            content,
            priority,
            alignment,
            truncation,
            separated,
        });
    }

    /// Lays out retained items in exactly one plain-filled row for `width`
    /// terminal columns.
    #[must_use]
    pub fn layout(&self, width: usize) -> Vec<Cell> {
        self.layout_with_fill(width, Cell::plain(' ')).row
    }

    /// Lays out one row using `fill` for unused columns.
    pub(crate) fn layout_with_fill(&self, width: usize, fill: Cell) -> PriorityLineLayout {
        let cells: Vec<PriorityLineItemCells> = self
            .items
            .iter()
            .map(|item| item_cells(&item.content))
            .collect();
        let mut allocations: Vec<usize> = self
            .items
            .iter()
            .zip(&cells)
            .map(|(item, cells)| minimum_item_width(item, cells))
            .collect();
        let retained = minimum_retention(&self.items, &allocations, width);
        if !required_items_retained(&self.items, &retained, self.required_through) {
            return PriorityLineLayout {
                row: std::iter::repeat_n(fill, width).collect(),
                required_items_fit: false,
            };
        }

        let used = allocated_width(&self.items, &cells, &retained, &allocations);
        let mut remaining = width.saturating_sub(used);
        let mut growth_order: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(index, item)| retained[*index] && item.truncation.is_some())
            .map(|(index, _)| index)
            .collect();
        growth_order.sort_by_key(|index| (self.items[*index].priority, *index));
        for index in growth_order {
            let maximum = maximum_item_width(&self.items[index], &cells[index]);
            let current_width = rendered_item_width(&cells[index], allocations[index]);
            let mut selected = allocations[index];
            let mut selected_width = current_width;
            let affordable_maximum = maximum.min(allocations[index].saturating_add(remaining));
            for candidate in (allocations[index]..=affordable_maximum).rev() {
                let candidate_width = rendered_item_width(&cells[index], candidate);
                if candidate_width.saturating_sub(current_width) <= remaining {
                    selected = candidate;
                    selected_width = candidate_width;
                    break;
                }
            }
            allocations[index] = selected;
            remaining = remaining.saturating_sub(selected_width.saturating_sub(current_width));
        }

        let left = group_cells(
            &self.items,
            &cells,
            &retained,
            &allocations,
            PriorityLineAlignment::Left,
            self.separator_style,
        );
        let right = group_cells(
            &self.items,
            &cells,
            &retained,
            &allocations,
            PriorityLineAlignment::Right,
            self.separator_style,
        );
        let left_width = cells_width(&left);
        let right_width = cells_width(&right);
        let padding = width.saturating_sub(left_width + right_width);

        let mut row = Vec::new();
        row.extend(left);
        row.extend(std::iter::repeat_n(fill, padding));
        row.extend(right);
        PriorityLineLayout {
            row,
            required_items_fit: true,
        }
    }
}

fn minimum_retention(items: &[PriorityLineItem], allocations: &[usize], width: usize) -> Vec<bool> {
    let mut retained = vec![true; items.len()];
    while reserved_width(items, &retained, allocations) > width {
        let Some(index) = items
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
    retained
}

fn reserved_width(items: &[PriorityLineItem], retained: &[bool], allocations: &[usize]) -> usize {
    grouped_width(items, retained, |index| allocations[index])
}

fn required_items_retained(
    items: &[PriorityLineItem],
    retained: &[bool],
    required_through: Option<PriorityLinePriority>,
) -> bool {
    !required_through.is_some_and(|required_through| {
        items
            .iter()
            .enumerate()
            .any(|(index, item)| item.priority <= required_through && !retained[index])
    })
}

fn minimum_item_width(item: &PriorityLineItem, cells: &PriorityLineItemCells) -> usize {
    item.truncation.map_or_else(
        || cells_width(&cells.cells),
        |truncation| truncation.min_width().min(cells_width(&cells.cells)),
    )
}

fn maximum_item_width(item: &PriorityLineItem, cells: &PriorityLineItemCells) -> usize {
    item.truncation.map_or_else(
        || cells_width(&cells.cells),
        |truncation| truncation.max_width().min(cells_width(&cells.cells)),
    )
}

fn allocated_width(
    items: &[PriorityLineItem],
    cells: &[PriorityLineItemCells],
    retained: &[bool],
    allocations: &[usize],
) -> usize {
    grouped_width(items, retained, |index| {
        rendered_item_width(&cells[index], allocations[index])
    })
}

fn grouped_width(
    items: &[PriorityLineItem],
    retained: &[bool],
    mut item_width: impl FnMut(usize) -> usize,
) -> usize {
    let mut group_width = |alignment| {
        let mut width = 0;
        let mut has_item = false;
        for (index, item) in items.iter().enumerate() {
            if !(!retained[index] || item.alignment != alignment) {
                if has_item && item.separated {
                    width += 1;
                }
                width += item_width(index);
                has_item = true;
            }
        }
        (width, has_item)
    };
    let (left_width, has_left) = group_width(PriorityLineAlignment::Left);
    let (right_width, has_right) = group_width(PriorityLineAlignment::Right);
    left_width + right_width + usize::from(has_left && has_right)
}

fn group_cells(
    items: &[PriorityLineItem],
    item_cells: &[PriorityLineItemCells],
    retained: &[bool],
    allocations: &[usize],
    alignment: PriorityLineAlignment,
    separator_style: Style,
) -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut needs_separator = false;
    for (index, item) in items.iter().enumerate() {
        if !(!retained[index] || item.alignment != alignment) {
            if needs_separator && item.separated {
                cells.push(Cell::new(' ', separator_style));
            }
            cells.extend(middle_truncated_cells(
                &item_cells[index],
                allocations[index],
            ));
            needs_separator = true;
        }
    }
    cells
}

fn rendered_item_width(cells: &PriorityLineItemCells, width: usize) -> usize {
    cells_width(&middle_truncated_cells(cells, width))
}

fn middle_truncated_cells(item: &PriorityLineItemCells, width: usize) -> Vec<Cell> {
    let cells = &item.cells;
    if cells_width(cells) <= width {
        return cells.clone();
    }
    if width == 0 {
        return Vec::new();
    }

    let graphemes = &item.graphemes;
    let content_budget = width - 1;
    let prefix_budget = content_budget.div_ceil(2);
    let suffix_budget = content_budget / 2;
    let mut prefix_end = 0;
    let mut prefix_width = 0;
    while prefix_end < graphemes.len() {
        let range = &graphemes[prefix_end];
        let grapheme_width = cells_width(&cells[range.clone()]);
        if prefix_budget < prefix_width + grapheme_width {
            break;
        }
        prefix_width += grapheme_width;
        prefix_end += 1;
    }

    let mut suffix_start = graphemes.len();
    let mut suffix_width = 0;
    while prefix_end < suffix_start {
        let range = &graphemes[suffix_start - 1];
        let grapheme_width = cells_width(&cells[range.clone()]);
        if suffix_budget < suffix_width + grapheme_width {
            break;
        }
        suffix_width += grapheme_width;
        suffix_start -= 1;
    }

    let mut spare = content_budget.saturating_sub(prefix_width + suffix_width);
    while prefix_end < suffix_start {
        let prefix_range = &graphemes[prefix_end];
        let next_prefix_width = cells_width(&cells[prefix_range.clone()]);
        if next_prefix_width <= spare {
            spare -= next_prefix_width;
            prefix_end += 1;
            continue;
        }
        let suffix_range = &graphemes[suffix_start - 1];
        let next_suffix_width = cells_width(&cells[suffix_range.clone()]);
        if next_suffix_width <= spare {
            spare -= next_suffix_width;
            suffix_start -= 1;
            continue;
        }
        break;
    }

    let prefix_cell_end = graphemes
        .get(prefix_end)
        .map_or(cells.len(), |range| range.start);
    let suffix_cell_start = graphemes
        .get(suffix_start)
        .map_or(cells.len(), |range| range.start);
    let marker_style = cells
        .get(prefix_cell_end)
        .or_else(|| cells.get(suffix_cell_start))
        .or_else(|| cells.first())
        .map_or_else(Style::default, |cell| cell.style);
    let mut out = Vec::new();
    out.extend_from_slice(&cells[..prefix_cell_end]);
    out.push(Cell::new('┄', marker_style));
    out.extend_from_slice(&cells[suffix_cell_start..]);
    out
}

fn item_cells(content: &StyledText) -> PriorityLineItemCells {
    let mut cells = Vec::new();
    let mut graphemes = Vec::new();
    visit_styled_graphemes(content.spans(), |grapheme, style, hyperlink| {
        if is_line_break_grapheme(grapheme) {
            return;
        }
        let start = cells.len();
        push_grapheme_cells(&mut cells, grapheme, style, hyperlink);
        graphemes.push(start..cells.len());
    });
    PriorityLineItemCells { cells, graphemes }
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(Cell::col_width).sum()
}

#[cfg(test)]
#[path = "priority_line/tests.rs"]
mod tests;
