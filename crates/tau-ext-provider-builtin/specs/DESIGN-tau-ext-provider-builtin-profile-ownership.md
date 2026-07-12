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
