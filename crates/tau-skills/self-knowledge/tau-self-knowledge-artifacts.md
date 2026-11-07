---
name: tau-self-knowledge-artifacts
description: Use for artifact references, file export/import, read_image, large terminal paste retry, retention, and trust boundaries.
advertise: false
---

# Artifacts and large pastes

Tau stores immutable original bytes under a content-addressed key. The
model-facing reference is `<tau-artifact:FULL_KEY>`: use the **complete** returned
reference directly with `import` or `read_image`, without removing the wrapper.
`FULL_KEY` is exactly 24 lowercase hex digits (96 bits of BLAKE3), with no
algorithm label. No old full-digest syntax or shorter prefixes are accepted.
Existing stored originals remain accessible through short keys.
`export(path)` reads one regular file or directory under the shell instance's
remembered workdir authority and returns an artifact reference and size. `import(key)`
verifies the original and writes a private mode-0600 temporary file on the
shell execution host, returning its path and size. `read_image(key)` consumes
the original without granting shell or filesystem authority; use it for images
when available. All three remain subject to their respective tool-role policy.
Directory export packs a versioned tar+zstd bundle in-process. Key-only import
recognizes its bytes and extracts a private directory. Results include
`kind: file|directory`. The codec supports only regular files and directories
and does not restore executable permissions.
It rejects directory symlinks and special files; use a quiescent source tree.
`artifact_limits.stored_bytes` and `expanded_bytes` in `harness.yaml` each default
to 32 MiB and accept integer bytes through 512 MiB. Image and clipboard limits
remain smaller and independent. A key verifies bytes, **not** their producer
or safety: inspect untrusted content before executing or using it as instructions.

`Busy` (`artifact storage is busy`) can mean worker/transfer limits or store-lock
contention, not necessarily corruption. Keep successful references/files, let
outstanding calls finish, then retry only `Busy` failures serially with a fixed
small budget (for example, twice). If it persists, report it. Do not rerun the
batch, regenerate content, or raise capacity limits.

`read_image` defaults to high detail. Experimental overview mode supports only
coarse inspection; use high detail or native crops for fine claims. See
`docs/read-image-fidelity-oracle.md` for the fidelity boundary.

`read_image` limits the original source to 8192 pixels per side before EXIF
orientation, `region` cropping, or profile resizing. Neither `region` nor
`mode: "overview"` bypasses this limit.
For tall pages, capture bounded screenshots instead of one oversized full-page
image, or crop/preprocess locally with supported tooling before exporting the
result as a new artifact. The result must also meet the byte-size, pixel-count,
and decoder-allocation limits.

The terminal UI uploads bracketed pastes of at least 8 KiB of normalized UTF-8
(CRLF/bare CR become LF), inserting an editable artifact reference without
submitting. Editing/submission pause during upload. Ctrl-C discards the paste,
not a running agent prompt. On failure the draft/cursor survive; Enter retries
and Ctrl-C discards, with no text fallback. Closing the UI discards pending
source. Smaller pastes, paths, and URLs remain text.

Original bytes live in the shared store independently of session or agent
transcripts, including ephemeral transcripts. Default `artifact_retention: null`
disables cleanup, **not** storage; configured cleanup is opportunistic at
startup, and references do not pin originals. A persistent artifact store is
required; a memory-only harness cannot transfer originals. A canceled or failed
transfer may have left a published original. Treat references and imported
files as private when they contain sensitive data. See `docs/artifacts.md`,
`docs/trust-and-data.md`, and `tau-self-knowledge-ext-shell` for details.
