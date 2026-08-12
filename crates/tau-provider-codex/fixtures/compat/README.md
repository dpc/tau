# Responses request compatibility fixtures

These full request bodies come from `config_for_model_mode` for `gpt-5.6-sol` with
synthetic credentials at production baseline `04637ed2`. They freeze Standard and
Lite lowering, including tools, assistant phase, reasoning effort and summary,
verbosity, encrypted-reasoning inclusion, service tier, and deterministic
prompt-cache identity.

`responses-compact-standard.json` and `responses-compact-lite.json` use the same
complete input for `/codex/responses/compact`. They freeze its smaller documented
schema: tools are `input` `additional_tools`, while ordinary top-level tools,
parallel calls, reasoning, and text are absent.

`responses-compact-output.json` is the canonical opaque response form. Its raw
item JSON must survive parsing and later provider replay unchanged.

Do not refresh these as incidental snapshots. Any change requires inspecting the
wire-level semantic diff and obtaining provider-boundary review.
