# Shared provider security boundary

## Environment-aware HTTP helper

The shared `ureq` agent applies platform certificate verification and
environment proxy/`NO_PROXY` selection. Provider-specific OAuth endpoints,
response parsing, and error classification belong to the owning backend crate.

Workspace `ureq` currently disables gzip, brotli, and charset decoding. Enabling
or feature-unifying any response decoding requires revisiting the pre/post
decode bounds here and adding compressed-expansion coverage before preserving
the same safety claim in each backend that consumes it.
