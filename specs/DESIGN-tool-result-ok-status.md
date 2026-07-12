# DESIGN-tool-result-ok-status: Successful tool-result displays use `ok`

Status: confirmed, 2026-07-09, user

Successful tool-result display metadata uses the standard short `ok` status
consistently. Tool-specific success synonyms must not replace `ok` when they add no
distinct lifecycle information. A different status is appropriate only when it
represents a documented non-success lifecycle state.
