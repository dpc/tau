# ChatGPT plan sign-in

Provider-owned implementation of OpenAI's public Sign in with ChatGPT flow.
This is separate from legacy Codex OAuth: registrations, credentials, and
inference use the documented public ChatGPT plan-sharing contract.

The library has no credential filesystem authority. Callers own durable host
identity, account selection, protected storage, and refresh serialization.
Network operations use Tau's fixed outbound policy, finite deadlines and bounded
responses; errors never retain credential-bearing upstream bodies.

Contract checked against OpenAI's Sign in with ChatGPT documentation on
October 2, 2026.
