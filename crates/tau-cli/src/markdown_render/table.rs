//! Semantic tables retained until terminal layout knows the pane's width.

use super::{
    MarkdownRun, RenderRuns, TABLE_MAX_OUTPUT_BYTES, TableAlignment, styled_block_from_runs,
};

/// Width-independent display table retained by static and streaming
/// projections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MarkdownTable {
    /// Header and body cells, excluding the generated delimiter row.
    pub(super) rows: Vec<Vec<Vec<MarkdownRun>>>,
    /// Indentation of header, delimiter, and body rows.
    pub(super) indents: Vec<String>,
    /// Delimiter alignment in column order.
    pub(super) alignments: Vec<TableAlignment>,
    /// Natural delimiter widths before pane allocation.
    pub(super) delimiter_widths: Vec<usize>,
    /// Unpadded inline-styled source used for impossible widths or output
    /// bounds.
    pub(super) fallback: Vec<MarkdownRun>,
    /// Whether the source table's last row ended with a newline.
    pub(super) trailing_newline: bool,
}

impl MarkdownTable {
    /// Resolves semantic cells once; the terminal owns all width-dependent
    /// work.
    pub(super) fn resolve(
        &self,
        theme: &tau_themes::Theme,
        base_style_name: &str,
        osc8_links: bool,
        style: tau_cli_term::Style,
    ) -> tau_term_screen::StyledTable {
        let resolve_runs = |runs: &[MarkdownRun]| {
            styled_block_from_runs(
                theme,
                base_style_name,
                &[],
                RenderRuns {
                    stable: runs,
                    live: &[],
                    incomplete: "",
                },
                false,
                osc8_links,
            )
            .content
        };
        tau_term_screen::StyledTable {
            rows: self
                .rows
                .iter()
                .map(|row| row.iter().map(|cell| resolve_runs(cell)).collect())
                .collect(),
            indents: self.indents.clone(),
            alignments: self
                .alignments
                .iter()
                .map(|alignment| match alignment {
                    TableAlignment::Left => tau_term_screen::TableColumnAlignment::Left,
                    TableAlignment::LeftMarked => tau_term_screen::TableColumnAlignment::LeftMarked,
                    TableAlignment::Right => tau_term_screen::TableColumnAlignment::Right,
                    TableAlignment::Center => tau_term_screen::TableColumnAlignment::Center,
                })
                .collect(),
            delimiter_widths: self.delimiter_widths.clone(),
            fallback: resolve_runs(&self.fallback),
            style,
            trailing_newline: self.trailing_newline,
            max_output_bytes: TABLE_MAX_OUTPUT_BYTES,
        }
    }
}
