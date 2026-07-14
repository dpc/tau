# DESIGN-tau-provider-chatgpt-responses-surface-selection: GPT-5.6 Responses surface selection

Status: confirmed, 2026-07-14, dpc

GPT-5.6 Sol, Terra, and Luna use the standard ChatGPT/Codex Responses contract by default. Responses Lite is available only through an explicit per-ChatGPT-profile compatibility opt-in; Tau never selects Lite from a model name, upstream catalog hint, retry, or fallback.

The current upstream Codex catalog pairs these Lite routes with code-mode-only operation, and the Lite request contract disables ordinary parallel function/custom tool calls. Tau does not support the associated programmatic/code-mode tool-call workflow and has no plan to add it. Making Lite a default in the future requires an explicit replacement design decision after Tau supports the required programmatic tool calls and the agent-workflow capability and efficiency tradeoffs are approved.
