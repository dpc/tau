# DESIGN-extension-tool-prefixes: Per-instance structural tool prefixes

Status: confirmed, 2026-07-13, dpc

Each extension instance may set an optional `tool_prefix`. With prefix `work`,
logical tool `slack_send` is exposed as `work_slack_send`. The same additive
mapping applies to a tool's internal name, model-visible alias, and group; tags,
actions, prompt prose, schemas, grammars, and examples are not rewritten. With
no prefix, structural values are unchanged.

The initial `Configure` establishes an immutable name scope before any
declarations or `Ready`. `tau-client` maps logical builder declarations and
offers explicit scoped factories and dynamic registration helpers. Raw emit is
wire-level. Changing the prefix requires an extension restart.

The harness validates that registrations remain inside an assigned prefix
envelope; it never rewrites extension events. Final internal names have one live
owner: same-connection refresh is allowed, cross-connection registration is
rejected, and harness-internal owners win. One global initial startup preflight
is independent of Ready arrival order: required/required conflicts fail startup,
required/optional keeps the required instance and disables the optional one, and
optional/optional disables every claimant. Internal conflicts fail required
startup or disable optional claimants. After that barrier, respawns and runtime
registrations are ordinary newcomers and cannot evict an incumbent. Duplicate
model-visible aliases remain legal in the registry when policy keeps them
exclusive, but an effective prompt snapshot containing two identical visible
names is rejected.

Exact tool and group policy refers to final names. Semantic tag policy continues
to span instances. Dispatch, completion provenance, replay, persisted history,
and UI use the final name already present in protocol events; historical names
are never retroactively rewritten.

## Rationale

The harness owns instance configuration, while the extension owns declarations,
dynamic registration, dispatch, and tool-specific prose. Establishing one scope
before declarations therefore avoids split ownership and registration races.
Structural mapping is deliberately narrow: rewriting arbitrary schemas, examples,
tags, or prose would be ambiguous and could silently change extension semantics.
Rejecting collisions rather than multiplexing providers preserves deterministic
dispatch and durable final-name provenance.
