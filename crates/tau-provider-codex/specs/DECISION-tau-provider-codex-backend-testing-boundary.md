# DECISION-tau-provider-codex-backend-testing-boundary: Test backend transports locally

Authority: inferred

ChatGPT wire lowering and parsing, WebSocket inference/pool behavior, and supported
unary HTTPS operations are tested in this crate with focused parsers and bounded
local loopback peers. Built-in-provider tests own scheduler integration; deterministic fake,
curated VCR, and transcript replay evidence do not substitute for live local
transport behavior.

This ownership keeps backend regressions close to the implementation without
creating production resolver or OAuth endpoint seams solely for tests. Detailed
coverage and fixture hygiene live in [`docs/testing.md`](../docs/testing.md).
