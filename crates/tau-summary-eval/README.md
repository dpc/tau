# Tau summary evaluation

`tau-summary-eval` scores summaries against a small, versioned,
synthetic-public corpus. It makes deterministic summary-quality regressions
easy to compare without turning stochastic model output into behavioral
authority.

The binary has no provider, credential, HTTP, or runtime dependency. Both
commands are offline:

```console
cargo run -p dpc-tau-summary-eval -- validate-corpus \
  crates/tau-summary-eval/fixtures/corpus-v1.json

cargo run -p dpc-tau-summary-eval -- score \
  --corpus crates/tau-summary-eval/fixtures/corpus-v1.json \
  --candidates crates/tau-summary-eval/fixtures/offline-candidates-v1.json \
  --output /tmp/tau-summary-result.json
```

The scorer lowercases text, collapses whitespace, and uses literal substring
alternatives. Coverage is an integer number of basis points. A case passes only
when all required facts match and no forbidden claim matches. Corpus order
defines result order. This intentionally modest oracle measures stable fact
retention; it does not claim to measure prose quality or semantic equivalence.
`scoring_contract` identifies these semantics independently of Tau's package
version and must change whenever identical inputs can produce different scores.
The complete checked-in result fixture pins that contract and the v1 JSON shape.

## Stable records

Corpus, candidate, and result records use strict JSON schemas identified by
`schema_version: 1`. Unknown fields fail parsing. A candidate set targets one
exact `corpus_id` and `corpus_version`; the result also records SHA-256 digests
of both exact input files. Result records omit transcript and summary text but
retain per-case fact IDs, scores, and complete generation provenance. The CLI
creates result files owner-readable/writable (`0600`) and refuses to overwrite.
Provenance validation rejects common secret markers, but operators must still
review metadata and never put credentials in a candidate record.

Offline candidate provenance records the exact generator and configuration.
Live provenance requires all of:

```json
{
  "kind": "live",
  "provider": "provider-name",
  "model": "model-name",
  "model_version": "exact-snapshot-or-reported-alias",
  "configuration": "temperature=0; prompt-revision=abc123; judge=none",
  "date_utc": "2026-08-27",
  "opt_in": "I_REVIEWED_PRIVACY_AND_ACCEPT_PROVIDER_COST"
}
```

The explicit token records operator intent; it does not launch a provider. Tau
deliberately supplies no live runner or implicit fallback. To run a live trial,
an operator must separately choose and invoke a provider, review the exact
synthetic-public inputs being disclosed, accept its price and retention terms,
then construct the candidate file. Keep generated summaries and results outside
the repository unless they have received the same public-data review as the
corpus. Never use live results as semantic authority or a blocking CI gate.

## Corpus handling

Checked-in corpora must use the `synthetic-public` classification and contain
only deliberately authored data. The validator bounds every input and flags
common host-path, credential, token, and personal-email markers as a backstop;
this is not a substitute for human review. Increment `corpus_version` for any
case or assertion change. Review diffs directly and never derive a public
fixture from private sessions, logs, prompts, tool output, or provider captures.
