# DESIGN-tau-core-schema-guided-argument-repair: Schema-guided argument repair is narrow and revalidated

Status: unconfirmed

Tool argument repair is allowed only after normal schema validation fails. The
repair helper is intentionally conservative: it parses JSON object/array strings
only when the schema demands that container, removes invalid `null` values only
from optional known fields, wraps scalars only when the schema demands an array,
and parses integer/boolean strings only for exact integer/boolean schema fields.
It does not split strings, infer subcommands, or rewrite already-valid calls.

Callers must revalidate repaired arguments before dispatch; failed or ambiguous
repair attempts fall back to the normal validation diagnostic path.

Testing strategy: cover every allowed repair, valid-argument no-ops, ambiguous
non-repairs, and cases where a local repair still requires final schema
revalidation.
