# DESIGN-tau-provider-chatgpt-retry-observability: Structured retry observability

Status: confirmed, 2026-07-11, dpc

Adapters classify attempts and the shared scheduler emits safe structured retry
facts independently of local UI prose. Watch subscriptions, dedupe, and fanout
remain harness-owned.
