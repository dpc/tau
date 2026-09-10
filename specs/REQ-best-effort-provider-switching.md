# REQ-best-effort-provider-switching: Best-effort provider switching

## Requirement and source

The user requests useful best-effort continuation when changing providers,
without a collection of provider-specific repair hacks. Preserve portable
messages, tool calls, and tool results. Replay provider-owned opaque material
and wire sidecars only when their producing provider is known compatible with
the destination; otherwise omit them from the request projection.

Compatibility means the same configured provider ID, independently of the model
name. Different aliases are conservatively incompatible. Replacing an endpoint
or credentials under the same ID is not detected. Missing authoritative origin
is not evidence of compatibility.

## Justification

Providers can share a wire format without accepting each other's private
reasoning handles or replay envelopes. Dropping that unsupported material can
make otherwise portable history useful without claiming lossless reasoning
transfer or guaranteed provider acceptance.

## Acceptance and exceptions

- Keep canonical history unchanged, including exact compatible raw replay, so
  switching back and cold replay retain the original material.
- Use canonical producing-provider facts, never guesses from JSON contents.
- Warn through the existing UI diagnostic channel when material is omitted,
  once per agent destination-provider transition, not every continuation or
  retry. Warn on the first dispatch with omissions too. Suppression is
  process-local and resets on restart.
- Refuse before issuing a provider request when incompatible or unknown-origin
  opaque compaction replaces history: dropping the only retained prefix is not
  an acceptable best-effort conversion.
- Do not automatically migrate or summarize history, edit opaque JSON,
  fingerprint credentials, or retry requests by successively stripping fields.

This governs destination-specific prompt projection under
[SPEC-provider-prompt-materialization-authority](SPEC-provider-prompt-materialization-authority.md)
and preserves the replacement-window authority of
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
