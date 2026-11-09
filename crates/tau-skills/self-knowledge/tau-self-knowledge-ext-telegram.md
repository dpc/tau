---
name: tau-self-knowledge-ext-telegram
description: Use when configuring, operating, or troubleshooting Tau std-telegram, including routing, tools, gateway-client mode, or security.
---

# Tau std-telegram extension self-knowledge

`std-telegram` is disabled-by-default configuration for the separately maintained
`tau-ext-telegram`. Install the Tau flake's package on `PATH` before enabling it;
Tau supervises it over stdio. The
[standalone project](https://radicle.network/nodes/radicle.dpc.pw/rad%3Az3sPdSePnxtBvP9pTLwUwVgpMU68r)
owns setup and operational documentation. Its Cargo README installation
instructions depend on future registry publication.

For Tau protocol 23.1, rebuild the bridge and gateway with registry
`dpc-tau-client`/`dpc-tau-proto` 0.29.0 and update them with a compatible harness
before separately authorized activation. Older-protocol bridges fail before
configuration; changing the advertised version is not migration.
The authenticated gateway protocol remains 0.

## Local polling and routing

Configure a named bot-token secret, nonempty numeric `allowed_user_ids`, and
optional exact `chat_id`. Without `chat_id`, an allowlisted user can link one
private chat with `/start`; groups/supergroups require explicit configuration.
Agents become routes after `telegram_register` enables them. Desired registrations
persist in Session-scope instance state, but restart restores active routes only
for agents still loaded after replay.

`telegram_send` can use only the configured/linked chat, not a model-chosen native
destination. Give each bot instance a distinct generic `tool_prefix`.

## Reply context

Local-poll and gateway-client delivery may include bounded same-event/chat/thread
`reply_to_message` context as a descriptive text suffix after the normalized outer
request. `quote.text` is a sender-selected unverified excerpt.
The producer does not fetch parents, include `external_reply` or parent
attachments, or recurse. Missing content does not prove deletion.

Parent identities are descriptive, not canonical references or `telegram_send`
capabilities. Only the outer message controls admission, `/to`, routes and mentions.
Context remains frozen through retries/ACKs; parent changes do not refresh it.
Optional context may shrink or disappear to preserve the outer body.

The SDK 0.29.0 migration preserves this text suffix; the producer does not use
typed `quoted_message` parts.
This adds no native per-message send routing.

## Gateway modes

The flake exports `tau-telegram-gateway`; operators install and supervise it
separately. Gateway-client mode uses an exact local socket path and named
per-instance client secret. The gateway, not the sidecar, owns the bot token,
polling, admission, durable checkpoints and outbound routing.
Mutual authentication does not contain malicious same-UID processes.
External Telegram content remains untrusted.

A trusted instance can set `mode: gateway_fixed_chat_send` to send summaries
without ingress. Configure only `gateway_socket_path`, `gateway_client_secret`,
and nonempty exact `allowed_agent_ids`; declare the secret on that instance and
configure the gateway's fixed `--chat-id` first.

This mode exposes only `telegram_send`, never registers/polls, and rejects
bot-token, destination, Telegram allowlist and polling fields. It sends verbatim
text without the normal agent prefix and shares the gateway's aggregate rate
limit. The shared key also authorizes normal route operations: fixed-send mode
is trusted extension policy, not containment of compromised same-key code.

Consult the standalone project's README, security notes and linked specifications
for command, retry, durability, gateway and troubleshooting contracts.
