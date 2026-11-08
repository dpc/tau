---
name: multipart-review-tau-cli-ui
description: Use when reviewing Tau changes involving CLI UI elements.
advertise: false
---

# Tau CLI UI Review

Before starting, read the `multipart-review` skill.

- **Informative, concise tools:** Check that tool headers identify the operation
  and outcome using nearby tools' wording and compact fields: `…` while running,
  `ok` on success, concise errors, and relevant counts or chips.
- **Dense Unix-terminal UI:** Check that rows, padding, and repeated information
  don't waste precious terminal space. Ensure results and errors remain clear,
  including at narrow widths.
- **Consistent styling:** Ensure changed elements use existing semantic theme
  tokens and rendering helpers. Check that colors, spacing, and layout match
  nearby UI conventions.

For precedents, see `crates/tau-harness-tools/src/lib.rs` (tool wording),
`crates/tau-cli/src/tool_render.rs` (compact headers and narrow-width layout),
and `crates/tau-themes/src/names.rs` (semantic styles).
