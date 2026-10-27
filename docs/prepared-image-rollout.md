# Prepared-image references: source and rollout checklist

The source endpoint stores prepared images as durable references for eligible
persistent invocations and resolves them before image-consuming provider work.
This checklist does **not** authorize publication, deployment, activation, or
opening existing live state with a new binary.

The protocol 14.0 / SDK 0.14.0 source and registry publication is now
[verified and recorded](sdk-releases.md#protocol-140-release-acceptance).
External rebuilds and activation are not implied by that acceptance.

This checklist records the protocol-14 prepared-image introduction. For the
current protocol-15 / SDK 0.15.0 package pins and deployment handoff, use the
[coordinated rollout checklist](protocol-15-rollout.md); the image-storage
requirements below still apply.

## Compatibility boundary

| Surface | Required treatment |
|---|---|
| Harness/extension protocol | **14.0**; rebuild configured peers together. Updating only the advertised version is not compatibility. |
| Rust source SDK | `tau-proto` and `tau-client` **0.14.0**, including the actual new DTO handling and helpers. |
| Journals | Framing is unchanged. New readers accept old inline images; old readers cannot decode reference-bearing records. No downgrade or journal-only backup promise. |
| Shared Artifact store | Ordinary object metadata remains version 1. Protected objects and transcript roots are private same-store state, not new extension pin/enumeration authority. |
| Persistent invocation | Prepared scoped authority permits Artifact; not-yet-prepared authority stays Inline. Once Artifact is captured, upload failure is not permission to substitute inline output. |
| Ephemeral/memory-only invocation | The captured policy remains Inline. Shared-store availability alone cannot promote it. |

### Source consumers to update

- Handle `ToolResultContentPart::ImageReference` and its exact `ImageReference`
  descriptor, MIME, dimensions and detail. Do not interpret opaque provider JSON
  or arbitrary text as references.
- Preserve the harness-owned `ToolInvocationPolicy.prepared_image_storage`.
  Producers must honor the captured value, upload the **prepared/re-encoded**
  bytes with `ArtifactUpload`, verify Finalize, and retain call/cancellation
  ownership. Do not reuse the original-image key as a transformation recipe.
- Image-consuming providers must use their existing input loop to correlate
  `ArtifactClient`/`ArtifactDownload`, enforce exact frame/chunk/size/digest/media
  checks, and finish the affected request safely on missing/corrupt data.
  Unsupported routes must omit images without fetching them.
- Keep caches bounded and byte-based; cache hits do not establish durable-store
  health or model-token savings. Preserve existing upstream image budgets.
- Generic projections and diagnostics must remove fetchable keys as well as
  inline bytes. A small hash is still read authority, not harmless metadata.

No further Tau wire change is required for this endpoint. External exhaustive
enum matches, policy struct construction, typed-image producers/providers, and
custom diagnostic projections still require source review. Ordinary non-image
tools do not need a new storage API.

## External consumers and pending rollout work

The approved handoff identifies these separately maintained consumers. This
source checkpoint does not edit their repositories or claim their rebuilds:

- PIM
- Rostra
- Slack
- Swarm **extension**
- Telegram bridge and gateway
- XMPP
- Zulip
- GitHub service
- Discord

Each owner must confirm actual SDK 0.14 use, successful build/inspection and
handling of any image-producing, provider-consuming or custom projection path.
Audit the paired Telegram gateway as part of its integration rather than
assuming that rebuilding the bridge proves gateway compatibility.
The independent **tau-swarm server is not the Swarm extension**; preserve its pin
unless a concrete compatibility dependency requires changing it.

Release order remains:

1. Pass the source graph gate and publish the approved source/SDK revisions
   through their normal release owners.
2. Confirm registry availability of the exact SDK source, then rebuild all
   configured external consumers against it.
3. Assemble and verify the full downstream/Nix closure; match configured
   executables to their rebuilt owners, not just package version strings.
4. Preserve a complete pre-upgrade backup, including the entire Artifact store,
   roots and journals. Keep the old fallback instance and its state untouched.
5. Obtain separate activation approval. Do not restart or repoint an old binary
   at new reference-bearing journals as a rollback shortcut.

External rebuild confirmations, downstream closure verification and activation
approval remain rollout gates. Registry publication and registry-only SDK
consumer acceptance are complete for this SDK line; they are recorded separately
from local source CI.

## Verification entry points

The full producer-to-provider acceptance requires an exact candidate provider
binary. Without the environment variable, that acceptance family deliberately
skips; a plain unit-test count is not evidence that it ran.

```sh
cargo build -p dpc-tau-ext-provider-builtin
TAU_E2E_PROVIDER_BUILTIN_BIN="$PWD/target/debug/tau-ext-provider-builtin" \
  cargo nextest run -p dpc-tau-e2e-tests --test provider_builtin_retry \
  -E 'test(prepared_images)'
```

`provider_builtin_prepared_images_replay_and_ephemeral_fallback` uses the real
utility entrypoint, real harness admission/rooting, real provider Artifact
resolution and a bounded loopback model endpoint. It proves that the descriptor
hashes the re-encoded prepared bytes, not the original; compares actual upstream
pixels and bytes; checks generic live/replay/notice privacy; cold-restarts both
harness and provider; and exercises the ephemeral inline/no-prepared-root path.
All files and subprocesses belong to its private fixture.

Complementary source oracles cover:

| Boundary | Focused coverage |
|---|---|
| Policy/fallback/custody | `prepared_image_policy_requires_durable_scoped_authority`, `image_admission_nondurable_modes_never_start_artifact_io`, lease replacement and scoped-I/O custody tests |
| Original and prepared transfers | Utility upload/cancellation tests, `ArtifactDownload`/`ArtifactUpload` correlation and integrity suites |
| Canonical admission | `referenced_tool_image_is_rooted_before_canonical_publication`, actual-media rejection, cancellation, backpressure, old-branch completion and worker-panic tests |
| Legacy inline history | `legacy_inline_image_history_remains_cold_readable_without_artifacts` |
| Generic compaction | `compaction_images_*`, including new replacement refs, per-use metadata, quotas, stale ownership, cancellation, shutdown billing and cold replay |
| Provider paths/cache | `provider_reference_resolution_reaches_all_codex_submission_paths`, resolver cache/transfer/deadline/cancellation tests, text-only no-fetch omission |
| Privacy | `referenced_images_never_expose_keys_in_debug_projections`, `corrupt_prepared_object_notice_omits_digest_and_path`, generic projection tests and the full acceptance observer |
| Logical/upstream bounds | Per-occurrence retained-image quota tests and each adapter's independent raw/data-URL budget tests; exact terminal and Artifact frame guards remain enabled |
| Retention/deletion | Root union, cleanup races, shared ownership, incomplete publication, malformed root metadata and protected-object tests |

The standard compaction rejection, live/cold opaque replay and deterministic
restart oracles remain required; typed references do not reinterpret opaque
compaction output or relax ordinary provider ToolResult rejection. Final
workspace tests and the project's full SelfCI gate remain mandatory.

## What the old-GC/write evidence does—and does not—prove

Evidence is **source audit plus reduced fixtures**, not execution of an old
released binary:

- `legacy_cleanup_preserves_hardlink_and_private_read_fallback` models the old
  cleaner's known-subtree unlink behavior.
- `duplicate_put_and_receipt_retry_never_mutate_backing`,
  `legacy_reupload_and_metadata_renewal_preserve_protected_bytes`, and
  `incomplete_receipt_recovery_preserves_protected_inode` exercise immutable-data
  and version-1 metadata behavior.
- The full acceptance unlinks only the ordinary prepared-object catalog subtree
  after clean fixture shutdown, then proves a fresh provider actually receives
  protected bytes through normal Artifact RPC.

The audited writers never truncate published data in place, and old cleanup
does not traverse the protected sibling domain. Revisit this evidence if a
writer changes those invariants. It does not promise safe arbitrary partial
backups, subtree tampering, or old-binary decoding of new journals.
