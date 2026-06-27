# tau-e2e-tests

Opt-in end-to-end tests for real Tau turns under VCR recording or replay.

## Running

Required for active E2E runs:

- `TAU_VCR=record-if-missing` to record missing cassettes, or
  `TAU_VCR=replay-only` to require existing cassettes.
- `TAU_VCR_DIR=/path/to/cassettes`.
- `TAU_E2E_MODEL=<provider-profile/model>` for the generated harness role.

Optional:

- `TAU_E2E_TAU_BIN=/path/to/tau` to choose the trusted local Tau binary used for
  provider and shell extensions. Defaults to `tau` on `PATH`.
- `TAU_E2E_SESSION_ID=<id>` to override the stable cassette/session id.

Unset, empty, or `off` `TAU_VCR` skips the tests. Active VCR modes fail loudly
when required VCR configuration is invalid or incomplete.

E2E tests should assert the relevant harness or tool progress from
`VcrFixture::run_turn` rather than only checking that the turn completed.
