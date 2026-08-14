//! Shared deterministic fixtures for CLI unit tests.

/// One encoded harness-owned tree result shared by client parity regressions.
pub(crate) const TREE_PREVIEW_PARITY_NOTICE: &str = concat!(
    "    0   before first prompt (root)\n",
    r"    1   before prompt  user: A\u{001B}[2J\u{000D}forged \u{202E}B\\C",
    "\n",
    r"    2 * before prompt  user: é 雪 🦀 سلام \u{200D}",
);
