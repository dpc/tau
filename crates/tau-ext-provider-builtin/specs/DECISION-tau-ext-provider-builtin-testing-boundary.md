# DECISION-tau-ext-provider-builtin-testing-boundary: Test integration, not backend protocols

Authority: inferred

This crate tests provider registry, runtime, and scheduler integration, while
backend wire formats, parsers, and transport pools remain owned by their provider
crates. Injected executors and monotonic clocks provide deterministic retry and
cooldown coverage. Temporary auth files and injected endpoint outcomes cover OAuth
and credential generations without live credentials, Internet access, or
wall-clock sleeps.

This separation avoids duplicating protocol matrices while still testing the
integration that only the built-in provider extension owns. The ChatGPT backend
boundary is recorded in
[`DECISION-tau-provider-chatgpt-backend-testing-boundary`](../../tau-provider-chatgpt/specs/DECISION-tau-provider-chatgpt-backend-testing-boundary.md).
The evolving integration catalog lives in
[`docs/testing.md`](../../../docs/testing.md).
