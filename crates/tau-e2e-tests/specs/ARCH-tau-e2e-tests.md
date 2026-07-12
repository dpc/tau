# ARCH-tau-e2e-tests: tau-e2e-tests architecture

`tau-e2e-tests` contains opt-in fixtures for real end-to-end Tau runs. These
tests are not sandboxed: they execute a trusted local `tau` binary, use the
user's normal provider authentication store, and allow the shell extension to
run commands with the user's permissions.

Only run these tests with an active VCR mode (`TAU_VCR=record-if-missing` or
`TAU_VCR=replay-only`) plus `TAU_VCR_DIR` and `TAU_E2E_MODEL`. Cassette files
can contain provider traffic, prompts, tool calls, shell output, and other local
test data, so store and share them accordingly.
