# Testing tau-ext-utils

This document owns the evolving test catalog for utility tools. Behavioral
authority remains in the applicable Linked Specs.

## Artifact image inspection

Deterministic PNG/JPEG/WebP fixtures cover format sniffing, animation rejection,
allocation and workspace budgets, geometry, EXIF orientation before crop, crop
failures, high/overview profiles, and typed provider content separated from safe
display metadata. Protocol coverage drives correlated Artifact Open/Read/Close
responses through the real extension loop and verifies the artifact-key-only
schema and foreground-only image declaration.

Lifecycle tests cover max-eight read admission, stale response rejection,
descriptor rejection above the separate 8 MiB image limit before any Read,
cancellation immediately behind a completed download, cancellation and shutdown
Close, and complete-terminal budgeting that converts an oversized typed image
into a byte-free error. The runtime additionally bounds ready-input batches and
tracks queued and running decoder cancellation separately. Cross-crate provider
tests own Responses wire shape, Lite detail omission, fail-closed route gating,
request-wide raw and data-URL budgets, and digest-preserving data-URL redaction.

The opt-in real-provider oracle is documented in
[`read_image` visual-fidelity oracle](../../docs/read-image-fidelity-oracle.md).
