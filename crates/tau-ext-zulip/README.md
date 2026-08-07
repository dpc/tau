# tau-ext-zulip

`std-zulip` is Tau's disabled-by-default first-party Zulip message bridge. It uses HTTP Basic bot authentication and Zulip's native `POST /api/v1/register` plus long-poll `GET /api/v1/events`; it does not expose a webhook listener or use a third-party Zulip client.

It exposes disabled-by-default, prefix-aware `zulip_register`, `zulip_conversations`, `zulip_send`, and separately tagged `zulip_react` tools. Use one configured instance per receiving Tau agent when exact routing matters. An instance fails closed while zero or multiple agents are registered.

## Configuration

```yaml
extensions:
  std-zulip:
    enable: true
    secrets:
      zulip_bot_email: {}
      zulip_api_key: {}
      zulip_identity_key: {}
    config:
      site: https://chat.example.com
      bot_email_secret: zulip_bot_email
      api_key_secret: zulip_api_key
      identity_key_secret: zulip_identity_key
      offline_message_catch_up: false
      allowed_user_ids: [42, 77]
      sender_aliases:
        - { user_id: 42, alias: dpc }
      direct_messages: { receive: all_messages }
      proactive_direct_messages:
        - { alias: operator, recipient: 1180954, description: "Operator escalation" }
      conversations:
        - name: Operations
          topic: deploy
          receive: mentions_only
          proactive_send: true
          description: Deployment operations
        - name: Engineering
          receive: all_messages
        - name: Announcements
          proactive_send: true
          agent_chosen_topic: true
          description: Choose a topic, including general chat
      max_message_bytes: 16384
```

The sender allowlist is mandatory and uses stable numeric Zulip user IDs. It authorizes only inbound senders; it never grants outbound authority. `sender_aliases` are presentation-only. A configured stream `name` forms the model-callable destination namespace and resolves to its private native ID before each queue registration; a route covers one exact channel and either one exact topic or all topics. `mentions_only` checks Zulip's `mentioned` flag; `all_messages` is explicit operator authority and automatically subscribes the bot before queue registration. Reconnection repeats the idempotent subscription, while route removal never unsubscribes. Proactive stream sends require an exact configured topic unless the operator explicitly sets `agent_chosen_topic: true` and omits `topic`. A topicless agent-chosen route may also receive all topics in that channel, but cannot overlap another receive route; a send-only agent-chosen route may coexist with an exact-topic receive route. `proactive_direct_messages` separately grants one fixed recipient to a destination alias; the recipient ID is configuration-only and never enters tool arguments or discovery output. Every non-bot inbound group-DM participant must be allowlisted. The queue does not request all-public-stream access.
Keep `zulip_identity_key` stable across API-key rotation and restart. It keys
non-reversible publisher-domain sender, conversation, and message identifiers.
Rotating it deliberately starts a new opaque identity namespace; existing facts
remain unchanged, while new events no longer correlate to their old opaque IDs.

`offline_message_catch_up` defaults to `false`, preserving live-only queue
behavior. When enabled, the bridge registers a new live queue first, fetches
newly created messages after its durable identity-scoped position in bounded
pages, and deduplicates the history/live overlap. On the first enabled startup,
it records the current position without replaying older messages. Later allowed
and routed messages are delivered at least once; offline edits, deletes, and
reactions are not recovered. The bridge advances only after observing its own
canonical `message.delivered` fact on the post-persistence subscription downpath.
A crash or failed checkpoint write can therefore duplicate a message but cannot
advance past an uncommitted admitted message.

The checkpoint filename uses a domain-separated keyed digest of the identity
key, never the raw key. Atomic replacement protects against torn writes, and an
identity-scoped process lock rejects concurrent owners. Changing filters or
routes does not replay messages older than the stored position.

Sender aliases and proactive-DM aliases each match
`^[a-z][a-z0-9_-]{0,63}$`; channel names are visible and at most 256 bytes.
Unknown configuration and tool fields fail closed.
At most 64 sender aliases and 64 combined stream/proactive-DM destinations are
accepted. HTTP is accepted only for loopback test servers; production sites
require HTTPS.

## Routing and tools

Call `zulip_register {"enabled":true}` before receiving, discovering, sending, or reacting. Registration resolves configured channel names, subscribes every `all_messages` channel, and creates a live event queue before returning success. `zulip_conversations {}` returns only proactive stream names/proactive-DM aliases, kinds, configured topics, explicit `agent_chosen_topic` authority, and trusted operator descriptions; it never returns stream IDs, participant IDs, queue IDs, credentials, or runtime authority.

`zulip_send` requires exactly one selector:

```json
{"message":"On it.","reply_to":"zulip-message:<opaque-digest>"}
{"message":"Deployment complete.","destination":"Operations"}
{"message":"Hello general chat.","destination":"Announcements","topic":""}
```

A reply selector resolves only in bounded extension-local state and returns to the exact source DM participants or stream/topic. A destination resolves only a current configured stream name with `proactive_send:true` or a configured proactive-DM alias. The latter always sends to its one fixed private recipient and rejects a caller `topic`. By default a stream name has one configured exact topic and rejects a caller `topic`. An operator may instead set `agent_chosen_topic: true` on a proactive route that omits `topic`; only that discovered destination accepts a caller topic, while retaining its resolved private stream ID. The empty string (`"topic":""`) is Zulip's canonical general-chat topic. Replies and exact-topic names always reject caller topics. Successful sends emit `message.sent_reported` before the tool result and return an opaque `message_ref`; native Zulip IDs never become model authority.

`zulip_react` accepts a same-agent, live Tau-issued `message_ref`, a bounded Zulip emoji name, and `add` or `remove`. It never accepts a numeric message ID or route. Inbound edits, deletes, and reactions become immutable `message.*_reported` occurrences referencing a locally owned base message. Delete removes local reply/reaction authority.

Messages use Zulip Markdown with queue registration requesting `apply_markdown=false` and the `empty_topic_name` client capability; rendered HTML is never submitted to the model, and empty general-chat topics retain their stable canonical form. An admitted message retains its exact inbound Markdown, including a leading `@**Bot Name**` address, as untrusted external content.

## Delivery and reconnect behavior

The queue cursor, duplicate cache, reply routes, message ownership, and registrations are bounded process-local state. Transient long-poll failures retry the same cursor with bounded backoff. Queue expiry registers a fresh live queue. With catch-up disabled, Tau emits a content-free warning and does not fetch missed backlog. With catch-up enabled, it resumes bounded created-message recovery from the durable position. Cache eviction, crash, or restart may duplicate delivery; there is no cross-process exactly-once transaction.

Outbound calls make one bounded provider request and do not retry ambiguous sends or mutations, preventing an automatic duplicate remote effect. A timeout can still leave an unknown remote outcome. Configuration, registration, unload, and shutdown generations reject stale local completions; a remote effect may have completed before authority changed.

When the initial `users_me` lookup or `register` request is rejected, the
diagnostic identifies only that operation, HTTP status, and a bounded uppercase
ASCII Zulip error code matching `[A-Z0-9_]` in 1–64 bytes. Missing, malformed,
or oversized codes become `unknown`; remote response messages and bodies,
request data, URLs, headers, and credentials never appear in diagnostics. The
same bound applies when live queue re-registration fails.

The bridge intentionally does not upload or download files. Slack's analogous safe surface is text-only and requests no file scopes, so Zulip Markdown links remain inert text/links rather than creating a local-file capability.
