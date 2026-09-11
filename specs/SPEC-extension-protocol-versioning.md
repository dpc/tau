# SPEC-extension-protocol-versioning: Extension protocol skew admission

## Record justification

The contract is necessarily distributed across the shared protocol DTO, every
Hello producer, harness admission, extension startup sequencing, and
extension-visible event behavior.

## Revision scope

Protocol 7.0 adds the optional typed `MessageParty.sender_trust` qualification
to external-message reports and canonical facts. An older harness would
otherwise accept the additive field and silently discard an `untrusted` marker
before persistence and model projection, so configured extensions must rebuild
together and major-skew admission rejects before Configure/Ready. Stored
records without the optional field retain the normal/default treatment.
See [SPEC-external-message-reports-and-facts](SPEC-external-message-reports-and-facts.md).

Protocol 6.0 adds the directed shared Artifact RPC. Configured peers must rebuild
together: a 5.x harness cannot answer a 6.0 client's new operations, and no
best-effort silent-ignore fallback is provided. Major-skew admission rejects
before Configure/Ready; the existing cooperative cross-harness messaging
exception below remains unchanged. See [SPEC-shared-artifacts](SPEC-shared-artifacts.md).

The explicit `{major, minor}` protocol revision covers the shared harness-peer
wire contract and extension-visible event schemas and behavior. It is independent
of Cargo, package, release, journal physical-format, and every other version.
Implementation-only changes do not bump it. Boundary changes bump the minor
revision only when best-effort continuation is deliberate; when in doubt, they
bump the major revision and reset the minor revision to zero.

Protocol 5.0 removes the obsolete `tool.delegate_progress` event schema and adds
the closed `provider_attempt_timing` private capture class. Major skew rejection
prevents a 4.x peer from sending the formerly valid event or receiving the new
capture class through a decoder that cannot represent the compiled contract.
Configured extensions must be rebuilt or updated with the harness before
activation. UI and dedicated cross-harness message connections instead continue
best-effort with a visible warning because partial interactive access and simple
message delivery are preferable to deliberate rejection.

The prior Protocol 5.0 contract also rejected obsolete standalone-compaction
event shapes without another revision bump. Canonical compaction boundaries,
inference checkpoints, and standalone starts are harness-authored facts that
external peers cannot publish, and that revision's harness output remained decodable by
the matched Protocol 5.0 sidecars: complete ownership fields are additive to
their prior optional fields and every emitted trigger variant is already known.
This deliberate historical matched-version compatibility did not restore obsolete journal
decoding or promise that arbitrary older Protocol 5.0 payloads remain valid.

Protocol 4.2 deliberately permits ordinary minor-skew continuation: declaration
inspection support is an additive Hello field and runtime-default Configure
purpose is omitted. Only a collector that first verifies explicit support sends
the new inspection purpose and expects its distinct completion message.
See [SPEC-extension-declaration-inspection](SPEC-extension-declaration-inspection.md).

The initial revision is `1.0`. Its object-shaped wire value deliberately breaks
the former scalar-zero Hello field. Missing, malformed, and legacy scalar values
fail decoding; Tau provides no bootstrap legacy decoder or default.

## Admission

Every producer of the shared Hello message advertises the revision compiled into
`tau-proto`. The harness alone checks it during admission and before extension
configuration:

- equal revisions continue without a warning;
- equal majors with different minors continue best-effort in either direction.
  A harness-launched configured extension emits one concise visible warning for
  that connection;
- different majors reject configured extensions before configuration,
  declarations, subscriptions, or extension state initialization;
- UI connections continue across any revision skew and receive a directed visible
  warning after exact-session admission when applicable;
- dedicated cross-harness message connections continue across major skew. The
  initiating message tool reports a warning header while preserving the actual
  delivery success or failure.

The configured-extension diagnostic concisely identifies the peer and both
revisions. A major-skew diagnostic also says that the peer was rejected;
minor-skew admission is implied by its warning severity. The configured-extension
warning is live-only, remains replayable to late UI subscribers for the current
process, and does not enter a journal. Extension reconnection may warn again.
Admission adds no negotiation round trip, and Configure remains the first harness
response to an admitted configured extension.

This policy changes neither session-target validation nor capability, cleanup,
security, and connection-ownership semantics. Best-effort admission makes no
compatibility guarantee: actual wire decoding, transport, authentication, and
delivery failures remain failures. It does not version or migrate journals.

Socket UI admission additionally returns the harness revision in the existing
`SessionAccepted` acknowledgement. Protocol 4.0 acknowledgements omit the
optional field; newer UIs treat absence as lacking later UI controls. This
allows a newer UI to withhold an additive request from an older harness while
UIs continue best-effort against a differently versioned harness without another
negotiation round trip. The harness sends a directed warning after the
acknowledgement for version-skewed UI connections. Configured extensions still
receive Configure as their first harness response.
