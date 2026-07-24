//! Terminal screen layout and styled-cell rendering utilities.
//!
//! Styling is represented structurally instead of as inline ANSI bytes. Caller
//! text may still contain tabs, newlines, and other controls; layout and cell
//! emission sanitize those values before terminal output.

/// Priority-based single-line layout.
mod priority_line;
/// Screen state tracker and terminal renderer.
pub mod screen;
/// Styled text, block, and cell data model.
pub mod style;

pub use priority_line::{PriorityLine, PriorityLineAlignment, PriorityLinePriority};
pub use screen::{Screen, emit_styled_cells, layout_block, layout_lines};
pub use style::{
    Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText, display_width,
    next_grapheme_boundary, previous_grapheme_boundary, truncate_to_width,
};
