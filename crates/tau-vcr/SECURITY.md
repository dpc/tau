# VCR storage security and reliability boundary

`tau-vcr` is a schema-agnostic local test/support library. Cassette schemas and
live-versus-replay behavior belong to callers. Configured cassette roots are
local filesystem inputs; cassette contents may contain sensitive prompts, tool
data, identifiers, provider output, or host paths and are private by default.

Replay reads are bounded, reject unsafe or non-regular paths, and distinguish
true absence from read failure so record-if-missing cannot silently fall
through to live work. Replay-only missing, malformed, mismatched, or unsupported
cassettes fail closed. Safe filesystem cassette operations are Unix-only;
other platforms fail closed rather than following reparse points.

Recording is create-only, bounded, private, and atomically publishes one new
key without overwriting reviewed evidence. The configured root itself must not
be a symbolic link; pre-existing platform path ancestors are part of the local
filesystem trust boundary. Revisit this contract when changing
platform support, path handling, publication primitives, resource limits,
diagnostics, environment modes, or concurrent recording.
