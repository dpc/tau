# Security policy

Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. Delivery is best-effort
at-least-once: an ambiguous crash or retry can duplicate prompts, agents, model
work, and spend.

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.
