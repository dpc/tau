# tau-ext-test-dummy architecture

`tau-ext-test-dummy` is a disabled-by-default fixture extension for Tau harness
tests. It is not user-facing functionality.

The extension is intentionally small:

- it performs the standard stdio extension handshake;
- it registers `restart_test_dummy` in the `test` tool group;
- it subscribes to `tool.started` and ignores replay-marked deliveries before
  performing restart/tool-result side effects;
- it intercepts `agent.prompt_submitted` and rewrites whole-word ASCII `tao` to
  `tau`.

The prompt interceptor must preserve prompt identity and routing fields
(`agent_id`, `message_class`, `originator`, display metadata, and context id)
and only change `text`.

The `restart_mode` config exists to make harness tests deterministic:
`random` preserves the historical random exit-or-error behavior, while
`success`, `error`, and `exit` force the corresponding outcome.

Restart replies (`success` `ToolResult` and `error` `ToolError`) must echo the
incoming `ToolStarted.originator`; they must not synthesize
`PromptOriginator::User`, because extension-originated invocations rely on the
originator for correct routing/classification.
