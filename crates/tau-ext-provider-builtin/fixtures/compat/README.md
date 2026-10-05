# Provider compatibility fixtures

`profiles/chatgpt.json`, `profiles/responses.json`, and
`profiles/chat_completions_legacy_prompt_cache_key.json` freeze the provider
boundary as it existed at production commit `04637ed2`. The legacy Chat
Completions profile is intentionally a rejection fixture: ticket 446c removed
its boolean `prompt_cache_key` field.

`profiles/chat_completions.json`, `profiles/openrouter.json`, and
`snapshots/models-routing-events.json` are the current approved profile and
routing baseline. They retain ticket 446c's `compat.openai_prompt_cache` break
and incorporate the September 5, 2026 reasoning-mapping migration from native
`efforts` arrays to explicit portable cut-point bands. The matching OpenRouter
discovery cache schema is version 2, so version 1 rows cannot cross that profile
schema change. Profile JSON is canonical `BuiltinProviderProfile` output. The
routing snapshot excludes credentials, normalizes loopback ports and elapsed
microseconds, and otherwise records complete published models, resolved
controls, and ordered events emitted through the production Responses and Chat
Completions seams.
The September 4, 2026 user-approved `gpt-6-astra` catalog addition refreshed
that routing snapshot with Astra's initial conservative fallback cost metadata.
The September 5, 2026 pricing correction replaces it with Astra's explicit
standard short-context API-equivalent ordinary-input, cached-read, cache-write,
and output rates while leaving private subscription billing unspecified. The
September 5 mapping migration additionally records Astra's exact supported
reasoning levels and changes the portable cut points published for affected
models.
The September 23, 2026 catalog refresh adds
[`gpt-6-sol`](https://developers.openai.com/api/docs/models/gpt-6-sol) and
[`gpt-6-luna`](https://developers.openai.com/api/docs/models/gpt-6-luna) to
the built-in ChatGPT/Codex list. OpenAI's
[Standard short-context pricing table](https://developers.openai.com/api/docs/pricing),
checked that day, gives per-million-token input/cached-input/cache-write/output
rates of `$2`/`$0.20`/`$2.50`/`$10` for Sol and
`$0.10`/`$0.01`/`$0.125`/`$0.50` for Luna. It also changes GPT-5.6 Sol's
comparison rates from `$5`/`$0.50`/`$6.25`/`$30` to the current
`$4`/`$0.40`/`$5`/`$20`. The checked table leaves the existing Astra,
GPT-5.6 Terra/Luna, GPT-5.5, GPT-5.4, GPT-5.4 Mini, and GPT-5.3 Codex
ordinary-input, cached-input, and output rates unchanged. Tau records only the
standard short-context tier: the current estimator cannot represent the separate
long-context or processing-tier rates, and these API-equivalent comparisons do
not claim private subscription billing.
The October 5, 2026 image-capability correction records text/image input and
native tool-result modalities for all eleven published ChatGPT/Codex models.
The exact-model source audit and pinned upstream image-tool evidence live in
[`ARCH-tau-provider-codex`](../../../tau-provider-codex/specs/ARCH-tau-provider-codex.md#typed-image-tool-output);
the snapshot does not claim live account verification or grant unknown model IDs
image support.
The zzd2 cache-control migration uses
`options: { mode: implicit, ttl: "30m" }`; the retired legacy
`prompt_cache_retention` contract is deliberately absent because its old `24h`
retention is not the new 30-minute TTL.
The approved runtime-cache-contract change 590b extends the routing snapshot
with the private ChatGPT/Codex model's conservative, content-free response-chain
contract; generic profiles remain absent unless explicitly configured.
The approved configured-compaction change 4hk9 publishes standalone support for
every Codex model, with native capability selecting native versus local-summary
lowering. The routing snapshot changes only those four formerly native-absent
model capability bits; it does not invent provider-default thresholds.

Each `*.events.cbor` file is a length-prefixed, pre-`recorded_at`,
pre-`observation_id` `PersistedAgentEvent` journal. Its matching JSON file is the
readable source event. The in-place observation schema break deliberately rejects
the old binary journal, while the JSON payload still verifies historical provider
field defaults. The pre-transport fixture omits `backend.transport`,
`backend.stale_chain_fallback`, and originator fields.

Treat changes as compatibility decisions, not automatic snapshot updates.
Regenerate only from a named production baseline or an approved interface change,
inspect the semantic diff, update this provenance, and obtain provider-boundary
review.
