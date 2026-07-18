# DECISION-tau-ext-provider-builtin-profile-ownership: Built-in provider profile ownership

Authority: inferred

`auth.d/<namespace>.json` profiles own built-in provider namespaces, backend
selection, and model publication. ChatGPT Responses mode is startup-captured profile
policy shared by publication, prompt, prewarm, retry, and quota paths; mutable
credential refresh cannot silently change it.

This permits independently configured namespaces while keeping route identity
stable for one provider process. Resolution and cache ownership are documented in
[ARCH-tau-ext-provider-builtin](ARCH-tau-ext-provider-builtin.md).
