# GATE-tau-provider-codex-responses-surface-selection: Select the GPT-5.6 Responses surface explicitly

## Gate

GPT-5.6 Sol, Terra, and Luna profiles use standard ChatGPT/Codex Responses by
default. Responses Lite remains an explicit per-profile compatibility mode and
must not be inferred from model names, catalog hints, retries, or fallback.

## Justification

The user wants Tau's implemented client-tool workflow by default; Tau does not
implement Responses Lite's programmatic code-mode tool-call workflow, so an
implicit switch could silently remove required agent capabilities.
