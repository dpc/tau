# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Tool argument validation diagnostics are bounded prompt surface

Status: unconfirmed

`tool_registry.rs` validates model-produced tool arguments against Tau's
supported JSON Schema subset before a provider receives a call. Validation
errors are model-visible, so diagnostics must be actionable, deterministic, and
bounded. Prefer reporting the exact schema path, expected shape, actual value
class, and small allowed/missing/unknown field sets over returning generic
provider-style schema errors.

Extension-provided schemas and model-provided values must not determine
unbounded diagnostic size or suggestion work. Lists should be capped, long values
and path segments truncated, and near-name suggestions should use the shared
tie-safe helper from `tau-proto`.

Testing strategy: keep tool-registry validation tests in
`src/tool_registry/tests.rs`. Cover each model-visible diagnostic class and add
regressions for bounds whenever a new diagnostic includes schema-provided,
filesystem-provided, or model-provided strings.

## Schema-guided argument repair is narrow and revalidated

Status: unconfirmed

Tool argument repair is allowed only after normal schema validation fails. The
repair helper is intentionally conservative: it parses JSON object/array strings
only when the schema demands that container, removes invalid `null` values only
from optional known fields, wraps scalars only when the schema demands an array,
and parses integer/boolean strings only for exact integer/boolean schema fields.
It does not split strings, infer subcommands, or rewrite already-valid calls.

Callers must revalidate repaired arguments before dispatch; failed or ambiguous
repair attempts fall back to the normal validation diagnostic path.

Testing strategy: cover every allowed repair, valid-argument no-ops, ambiguous
non-repairs, and cases where a local repair still requires final schema
revalidation.

## Semantic stores can be durable or memory-only

Status: unconfirmed

`AgentStore` and `SessionStore` both support normal durable event streams and
selected memory-only streams used by ephemeral agents/sessions. The memory-only
path must fold the same semantic facts for live replay while avoiding creation of
reserved state directories, sidecars, locks, and event files.

Testing strategy: every new semantic write path should cover both persistence
modes, including a negative filesystem assertion for memory-only records and a
positive replay/folding assertion that the in-memory state still behaves like the
durable equivalent while the process lives.

Durable `events.cbor` replay is fail-closed. Stores verify record framing,
monotonic durable sequence numbers, path-safe store ids, and the same semantic
event/parent invariants enforced for live appends before folding replayed state.
Corrupt, truncated, spliced, or semantically invalid durable records must return a
typed store error instead of being ignored or panicking during fold.

Only one real background completion is valid for a globally unique tool call id:
once either `ToolBackgroundResult` or `ToolBackgroundError` has been recorded,
later background completion events for that id are rejected during both live
append and durable replay. Duplicate detection is global by `ToolCallId`; the
known-call check remains branch-relative and must resolve the event's explicit
fold parent instead of using the mutable global tree head.

Durable sequence numbers count only records actually written to the corresponding
`events.cbor` stream. Memory-only session membership facts update live folded
state but do not advance the durable sequence cursor, so later durable records
remain contiguous on disk.

Durable session ids are store path components. The path-component grammar is
shared by CLI session-id minting, store validation, metadata listing, lock
probes, and cleanup: minted ids must be bounded and must not contain path
separators, NUL, or the reserved `.`/`..` names.

## Tool examples are registration-validated repair metadata

Status: unconfirmed

Tool providers may attach compact examples to `ToolSpec`, including optional
declarative subcommand selectors. These examples are not part of provider-visible
tool definitions. They are registration metadata used only after a failed call,
so good calls pay no prompt-token overhead.

The registry validates examples before accepting a registration: ids and text are
bounded, selector paths must match the example arguments exactly, and function
tool example arguments must satisfy the same schema validator used for model
calls. Bad examples reject the registration clearly instead of being kept as
latent prompt-surface failures.

Testing strategy: cover schema rejection, selector path/value rejection,
compactness budgets, deterministic generic/subcommand fallback, bounded rendering,
and allowed-value diagnostics for missing or invalid selectors.
## Durable standalone-compaction replay

Status: confirmed, 2026-07-11, user

New-format compaction boundaries carry one all-present metadata group:
transaction id, cut, suffix end, compact prompt id, provider-qualified model,
and operation. Replay resolves the transaction's Started fact; requires exact
cut/prompt/model/operation correlation and standalone operation; requires
`suffix_end` to equal the boundary parent with cut as its ancestor; and rejects
partial groups, unknown transactions, mismatches, and duplicate outcomes.
Live validation and replay use the same fail-closed rules. Historical
all-six-absent boundaries remain valid hard boundaries but cannot participate
in new transaction recovery.
## Manual compaction projection

Status: unconfirmed

The agent-tree fold validates unique bounded manual request ids, immutable
caller/target/model/tool-call correlation, and exactly one pre-start outcome.
It projects waiting, started (including transaction outcome), and failed state
so the harness can repair every crash window without resending ambiguous
provider work or duplicating a background completion.
