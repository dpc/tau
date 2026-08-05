# Provider compatibility fixtures

`profiles/chatgpt.json`, `profiles/openrouter.json`, `profiles/responses.json`, and
`profiles/chat_completions_legacy_prompt_cache_key.json` freeze the provider
boundary as it existed at production commit `04637ed2`. The legacy Chat
Completions profile is intentionally a rejection fixture: ticket 446c removed its
boolean `prompt_cache_key` field.

`profiles/chat_completions.json` and `snapshots/models-routing-events.json` are
the approved ticket 446c profile and routing baseline. They use
`compat.openai_prompt_cache` and demonstrate the intentional schema break. Profile
JSON is canonical `BuiltinProviderProfile` output. The routing snapshot excludes
credentials, normalizes loopback ports and elapsed microseconds, and otherwise
records complete published models, resolved controls, and ordered events emitted
through the production Responses and Chat Completions seams.

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
