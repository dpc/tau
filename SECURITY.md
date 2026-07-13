# Security policy

Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.
Authenticated ChatGPT quota acquisition and its credential-free lifecycle are
documented in
[`tau-provider-chatgpt/SECURITY.md`](crates/tau-provider-chatgpt/SECURITY.md)
and
[`tau-ext-provider-builtin/SECURITY.md`](crates/tau-ext-provider-builtin/SECURITY.md).

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. Delivery is best-effort
at-least-once: an ambiguous crash or retry can duplicate prompts, agents, model
work, and spend.

## Standalone compaction recovery reliability

Standalone compaction and its continuation are harness-owned durable work. Every
new provider cut must be a closed transcript prefix; a tool-calling assistant
response and its complete terminal results node are indivisible. A failed
transaction with a resume watermark remains fail-closed until an explicit
successor preserves same-branch coverage of that watermark. A successor may
retreat its cut to retain more exact suffix, but it must not replace the owed
watermark with an ancestor or sibling selected by later head navigation.
Ordinary input and `/cancel` do not abandon this ownership; if the selected head
no longer descends from the owed watermark, explicit recovery must remain
blocked. Core validation and warm/cold replay regressions enforce these rules.
Revisit them when adding any explicit abandon/rewind operation or changing
compaction replay ownership.

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.
