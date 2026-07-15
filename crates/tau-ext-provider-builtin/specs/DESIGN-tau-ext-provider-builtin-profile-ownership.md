# DESIGN-tau-ext-provider-builtin-profile-ownership: Provider profiles own built-in provider namespaces

Status: inferred

`tau-ext-provider-builtin` treats Tau state `auth.d/<provider>.json` files as the
source of truth for built-in provider registration. The filename supplies the
provider namespace, while the serialized profile `kind` selects the backend
family.

Model publication follows profile ownership: `chatgpt` profiles publish the
ChatGPT/Codex model matrix owned by `tau-provider-chatgpt`; `chat_completions`
profiles publish their configured model list; and `openrouter` profiles convert
to Chat Completions configuration and publish their configured or fetched model
list. Prompt dispatch resolves exact configured model IDs for the selected
provider namespace. Missing or invalid mutable profile/model/auth state remains
visibly pending and is re-resolved before later attempts.

ChatGPT profiles also own `responses_lite_compatibility`. The extension captures
that route choice at startup for model publication and every later prompt,
prewarm, retry, and quota resolution. Mutable credential reload and OAuth refresh
preserve it, but an on-disk mode edit takes effect only after restart. Different
ChatGPT profile namespaces may select different modes.

Within one provider process, a permanent refresh rejection is remembered for
the exact locked credential/mode generation and shared by every profile
consumer. A different credential or mode clears that negative result. The
locked profile generation, not a stale pre-lock snapshot, owns valid-only
fallback. The negative cache is intentionally not persisted; a cold restart may
probe the unchanged generation once before resuming the normal slow Auth retry
cadence.

The default and compatibility policy is defined by
[DESIGN-tau-provider-chatgpt-responses-surface-selection](../../tau-provider-chatgpt/specs/DESIGN-tau-provider-chatgpt-responses-surface-selection.md).
