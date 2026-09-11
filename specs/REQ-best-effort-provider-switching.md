# REQ-best-effort-provider-switching: Best-effort provider switching

## Requirement and source

The user requests useful best-effort continuation when changing providers,
without a collection of provider-specific repair hacks. Preserve portable
messages, tool calls, and tool results. Replay provider-owned opaque material
and wire sidecars only when their producing provider is known compatible with
the destination; otherwise omit them from the request projection.

Compatibility normally means the same configured provider ID, independently of
the model name. The user additionally requests account-only switching between
private ChatGPT/Codex aliases without losing an existing opaque replacement.
Different aliases may therefore attempt unchanged replay when both exact routes
are currently published by the same configured Tau-owned built-in provider
instance, their frozen profiles select the private ChatGPT adapter, and their
exact upstream model and effective Responses mode match. Names, tags, generic
Responses compatibility, or a third-party component cannot establish this
exception. Missing, removed, or ambiguous source routes remain incompatible.

Canonical producing model IDs are interpreted through the currently accepted
configuration, including for old windows after cold replay. Historical profile
settings are not recorded or backfilled. As with the existing same-provider
rule, replacing an endpoint or credentials under an ID is not detected.
Missing authoritative origin is not evidence of compatibility. Account-alias
admission permits an attempt, not a guarantee of remote ciphertext portability;
destination rejection remains explicit and leaves canonical history intact.

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
