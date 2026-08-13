# Responses request compatibility fixtures

These full request bodies come from `config_for_model_mode` for `gpt-5.6-sol` with
synthetic credentials at production baseline `04637ed2`. They freeze Standard and
Lite lowering, including tools, assistant phase, reasoning effort and summary,
verbosity, encrypted-reasoning inclusion, service tier, and deterministic
prompt-cache identity.

`responses-compact-standard.json` and `responses-compact-lite.json` preserve
historical compatibility evidence for the retired private unary route. Current
ChatGPT standalone compaction uses the ordinary Responses request shape plus a
final `compaction_trigger`; these legacy fixtures are not its production
contract.

`responses-compact-output.json` is the canonical opaque response form. Its raw
item JSON must survive parsing and later provider replay unchanged.

Do not refresh these as incidental snapshots. Any change requires inspecting the
wire-level semantic diff and obtaining provider-boundary review.
