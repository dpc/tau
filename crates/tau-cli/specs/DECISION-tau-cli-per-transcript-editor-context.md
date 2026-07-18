# DECISION-tau-cli-per-transcript-editor-context: Per-transcript editor context

Authority: confirmed, 2026-06-25, dpc

Assistant response and editor context follow the viewed transcript boundary.
Folding a hidden transcript must not publish its response context.

This preserves conversation continuity without leaking context across transcript
navigation. Exact behavior, including initial no-agent adoption, is specified by
[SPEC-tau-cli-transcript-context](SPEC-tau-cli-transcript-context.md).
