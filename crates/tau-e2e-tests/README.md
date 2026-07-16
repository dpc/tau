# tau-e2e-tests

Hermetic deterministic end-to-end tests and separate opt-in VCR tests.

## Deterministic provider tests

The default workspace test run executes a test-only fake provider as a real
supervised subprocess. No environment opt-in, provider credential, network
access, shell, or VCR cassette is involved:

```sh
cargo nextest run -p tau-e2e-tests --test deterministic_provider
```

The acceptance cases cover streaming/final text, a successful tool round
through `tau-ext-test-dummy`, and startup rejection of invalid scenario config.
The fixture retains its private artifact root on panic, including generated
config/scenario, durable events, extension stderr, and the bounded semantic
provider trace. See
[`DESIGN-tau-e2e-deterministic-provider`](specs/DESIGN-tau-e2e-deterministic-provider.md)
for the coverage ceiling.

## VCR tests

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
