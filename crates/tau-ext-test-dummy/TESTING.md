# tau-ext-test-dummy testing strategy

Run lightweight validation with:

```sh
cargo test -p tau-ext-test-dummy
cargo clippy -p tau-ext-test-dummy --all-targets -- -D warnings
```

Regression coverage should include:

- random restart outcomes: tool error and extension exit without reply;
- deterministic `restart_mode` outcomes: `success`, `error`, and `exit`;
- `exit_once_then_success` rejecting missing, relative, and unrelated marker
  configuration; first live marker claim exiting after correlated progress;
  second regular-marker use returning exactly one success; and replayed starts
  neither claiming the marker nor exiting;
- `typed_image: true` enabling `typed_image_test_dummy` with its one fixed
  native image terminal, `provider-content:image` tag, and foreground-only
  declaration; the owning
  `deterministic_typed_image_tool_result_replays_after_clean_restart` E2E
  proves its durable live/replay continuation;
- `hold_no_side_effect` readiness followed by exact correlated cancellation;
- wrong-id cancellation leaving the hold active, concurrent-call rejection, and
  exact cancellation of the original call;
- an injected short deadline producing the fixed timeout terminal;
- hold-mode disconnect joining the worker without a terminal result, error, or
  cancellation report;
- `hold_until_success_release` rejecting malformed and nonce-mismatched frames,
  accepting only the exact typed call-id/nonce frame, and returning the exact
  normal success result;
- release-mode disconnect joining its worker, removing the Unix socket, and
  producing no synthetic success;
- invalid config emitting `ConfigError`;
- replayed `tool.started` deliveries producing no tool result/error and no
  forced exit behavior;
- preserving the incoming `tool.started` originator on deterministic success and
  error restart replies, including extension-originated invocations;
- prompt interception rewriting whole-word ASCII `tao` to `tau`;
- preserving replacement case;
- ignoring substrings inside ASCII words;
- passing non-matching prompts through unchanged;
- preserving prompt identity/routing fields when text is replaced.

Production output lifecycle regressions must block the real writer and exhaust
the 64-frame detached FIFO before no-side-effect cancellation/deadline and
release success terminals. They require exactly one terminal after admission
resumes; forced mandatory-write failure must exit the extension loop while the
hold remains owned for disconnect cleanup.
