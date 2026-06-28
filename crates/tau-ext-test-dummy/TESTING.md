# tau-ext-test-dummy testing strategy

Run lightweight validation with:

```sh
cargo test -p tau-ext-test-dummy
cargo clippy -p tau-ext-test-dummy --all-targets -- -D warnings
```

Regression coverage should include:

- random restart outcomes: tool error and extension exit without reply;
- deterministic `restart_mode` outcomes: `success`, `error`, and `exit`;
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
