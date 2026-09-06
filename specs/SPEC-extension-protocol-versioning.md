# SPEC-extension-protocol-versioning: Extension protocol skew admission

## Record justification

The contract is necessarily distributed across the shared protocol DTO, every
Hello producer, harness admission, extension startup sequencing, and
extension-visible event behavior.

## Revision scope

The explicit `{major, minor}` protocol revision covers the shared harness-peer
wire contract and extension-visible event schemas and behavior. It is independent
of Cargo, package, release, journal physical-format, and every other version.
Implementation-only changes do not bump it. Boundary changes bump the minor
revision only when best-effort continuation is deliberate; when in doubt, they
bump the major revision and reset the minor revision to zero.

The initial revision is `1.0`. Its object-shaped wire value deliberately breaks
the former scalar-zero Hello field. Missing, malformed, and legacy scalar values
fail decoding; Tau provides no bootstrap legacy decoder or default.

## Admission

Every producer of the shared Hello message advertises the revision compiled into
`tau-proto`. The harness alone checks it during admission and before extension
configuration:

- equal revisions continue without a warning;
- equal majors with different minors emit one visible warning for that
  connection and continue best-effort in either direction;
- different majors reject the connection before configuration, declarations,
  subscriptions, or extension state initialization.

The diagnostic identifies the peer and both revisions and recommends rebuilding
or updating the peer. It is live-only, remains replayable to late UI subscribers
for the current process, and does not enter a journal. Reconnection may warn
again. Admission adds no negotiation round trip, and Configure remains the first
harness response to an admitted configured extension.

This policy changes neither session-target validation nor capability, cleanup,
security, and connection-ownership semantics. It makes no compatibility
guarantee after a minor-skew admission and does not version or migrate journals.
