# DECISION-two-second-default-tool-backgrounding: Two-second default tool backgrounding

Authority: confirmed, 2026-07-25, dpc

## Decision

Tool registrations that omit `background_support` must use
`MinForegroundSeconds(2)`. Explicit per-tool background policies remain
authoritative.

This choice is governed by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).

## Rationale

Two seconds reduces the foreground delay for slow tool calls while preserving a
brief opportunity for fast calls to return inline.
