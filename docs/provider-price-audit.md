# Shipped API estimate audit — September 29, 2026

These are **comparison estimates, not billing facts**. The audit covers every
hard-coded production rate in `tau-provider-codex::estimated_api_prices`,
`tau-ext-provider-builtin::chat_completions::builtin_estimated_prices`, and
`tau-proto::ESTIMATED_API_COST_FALLBACK`. Values are USD per million tokens.

## OpenAI standard short-context rates

Each model link is the provider-owned source for that exact ID. No older
OpenAI base rates needed changing; Sol 6.1 adds a new row.

| Model | Ordinary input | Cached input | Cache write | Output |
| --- | ---: | ---: | ---: | ---: |
| [gpt-6.1-sol](https://developers.openai.com/api/docs/models/gpt-6.1-sol) | 2 | 0.10 | 2.50 | 10 |
| [gpt-6-astra](https://developers.openai.com/api/docs/models/gpt-6-astra) | 10 | 1 | 12.50 | 50 |
| [gpt-6-sol](https://developers.openai.com/api/docs/models/gpt-6-sol) | 2 | 0.20 | 2.50 | 10 |
| [gpt-6-luna](https://developers.openai.com/api/docs/models/gpt-6-luna) | 0.10 | 0.01 | 0.125 | 0.50 |
| [gpt-5.6-sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol) | 4 | 0.40 | 5 | 20 |
| [gpt-5.6-terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra) | 2 | 0.20 | 2.50 | 12 |
| [gpt-5.6-luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna) | 0.20 | 0.02 | 0.25 | 1.20 |
| [gpt-5.5](https://developers.openai.com/api/docs/models/gpt-5.5) | 5 | 0.50 | — | 30 |
| [gpt-5.4](https://developers.openai.com/api/docs/models/gpt-5.4) | 2.50 | 0.25 | — | 15 |
| [gpt-5.4-mini](https://developers.openai.com/api/docs/models/gpt-5.4-mini) | 0.75 | 0.075 | — | 4.50 |
| [gpt-5.3-codex](https://developers.openai.com/api/docs/models/gpt-5.3-codex) | 1.75 | 0.175 | — | 14 |

The private ChatGPT catalog publishes all four categories for GPT-6 models.
Other private models intentionally publish only ordinary input and output;
their omitted read/write categories use the existing fallback rules, not the
API table's cache prices. Public API caching does not prove private
subscription billing. The GPT-5.6 Sol base price is promotional through at
least November 21, 2026 and needs another audit when that offer changes.

Sol 6.1 input above 272,000 tokens uses 2× ordinary/read/write and 1.5× output
rates for the **entire request**. Fast uses 2×, Batch/Flex 0.5×, and regional
processing adds 10%. These conditional rates are not represented by the
current scalar estimate fields. Other models can also have conditional tiers.
The private subscription route's actual accounting is not established by
public API prices. Missing cache-write counts are not inferred: unmatched
input is priced as ordinary input, which can underestimate write-heavy use.

## DeepSeek

[DeepSeek's live pricing table](https://api-docs.deepseek.com/quick_start/pricing/)
now says the retired `deepseek-v4-flash` alias routes to V4.1 Flash.
Its prior shipped `0.14 / 0.0028 / 0.28` rates were stale. Tau now uses the
**conservative peak** `0.30 / 0.006 / 1.20` input/read/output rates.
Off-peak is `0.15 / 0.003 / 0.60`; peak windows are weekdays
01:00–04:00 and 06:00–10:00 UTC. Tau does not choose rates from request time.
Explicit profile overrides retain precedence per category. A model alias
can change routing again, so this estimate needs periodic re-auditing.

## Unknown and dynamic models

The central unknown-model fallback remains `5 / 0.50 / 30`, no write rate.
Its former “GPT-5.6-equivalent” label was wrong: these are GPT-5.5-equivalent
comparison rates, not a verified tariff for an unknown model. This audit
changes the label, not the fallback policy or accumulated-cost semantics.

Grok uses provider discovery metadata, suppressing scalar prices when a
long-context threshold is present. OpenRouter publishes no discovered rates
and therefore uses the fallback unless explicitly configured. Generic public
Responses and Chat Completions profiles use operator-supplied prices (apart
from the documented DeepSeek default). Arbitrary overrides and changing
third-party catalogs are outside this fixed-table audit.

Hosted tool fees, image-specific charges, service tiers, regional uplifts,
subscriptions, and storage without both a rate and usage are outside these
token estimates. Accurate conditional accounting would require a separate
approved design for selecting and preserving response-local tariff evidence;
this change does not alter persistence or pretend a base rate is an invoice.
