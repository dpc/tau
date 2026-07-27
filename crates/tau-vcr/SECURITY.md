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

Recording is create-only, bounded, private, and exclusively publishes one new
key without overwriting reviewed evidence. A caller may bundle one separately
bounded, key-derived side artifact; VCR publishes it privately before the
cassette via private staging and exclusive hard-link publication. It makes a
best-effort removal if cassette publication fails. A persistent per-key
advisory lock serializes cooperating publishers; after a crash releases that
lock, a retry safely removes a complete side artifact that has no published
cassette. A crash may also leave a harmless key-specific staging file, but
never a partial final side artifact. Replay confines and
no-follow reads that side artifact exactly like the cassette. The configured root itself must not
be a symbolic link; pre-existing platform path ancestors are part of the local
filesystem trust boundary. Revisit this contract when changing
platform support, path handling, publication primitives, resource limits,
diagnostics, environment modes, or concurrent recording.
