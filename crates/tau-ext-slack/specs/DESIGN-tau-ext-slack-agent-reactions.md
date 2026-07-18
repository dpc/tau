# DESIGN-tau-ext-slack-agent-reactions: Agent-invoked Slack emoji reactions

Status: confirmed, 2026-07-14, dpc

The Slack-local reaction policy remains current. Its former harness transport
registration/completion integration is superseded by
[SPEC-extension-published-message-facts](../../../specs/SPEC-extension-published-message-facts.md):
targets are keyed to locally written message-fact IDs and successful sends publish
`message.sent` before their ordinary tool result.

## Recommendation

Add one fourth, separately authorized Slack tool:

```text
slack_react({
  "message_ref": "<Tau/extension-issued message fact ID>",
  "emoji": "eyes",
  "action": "add" | "remove"
})
```

It is a bounded source-targeted mutation tool, not general Slack lookup. It accepts no Slack conversation ID, timestamp, thread, destination alias, user, or message text. Implement explicit add/remove only; do **not** implement toggle or list in v1. Use Slack's bot-token `reactions.add` / `reactions.remove` methods and a new `reactions:write` scope. Keep all reference, route, and reaction ownership state bounded and runtime-only.

Key defaults/decisions:

- Logical name `slack_react`, tag `slack:react`, existing logical group `slack`, `enabled_by_default: false`.
- A role may receive/register/send without react, or react without send. `slack_register` is not the authorization for this tool; current role/tool policy is. Whole-group grants intentionally gain this new surface and must be documented.
- Incoming locally written create/edit message-fact IDs and references returned by successful `slack_send` are eligible. Human-reaction occurrence IDs, bridge help/control posts, unpublished events, and arbitrary Slack items are not.
- The reference is a Tau-issued opaque fact selector in the documented
  `slack-message:<opaque-digest>` format, not a secret or bearer capability.
  Every use must resolve an exact retained local target. Revalidate exact
  extension instance, live agent, active session, current source/config
  authority, native target, and reaction ownership on every call.
- Adding establishes runtime ownership only after an unambiguous Slack success. Removing is permitted only for a reaction that the same Tau agent owns in this runtime. Never adopt/remove a pre-existing bot reaction merely because Slack returns `already_reacted`.
- No new config key. The extension, tool, and `reactions:write` installation are the operator opt-ins. No automatic retries, no local rate-limit queue, no persistence, and no crash-safe exactly-once claim.

This fits the existing Slack architecture: source-bound Tau fact selectors,
current routes, per-instance prefixing, fact-keyed local authority, runtime-only
ownership, and no separately selectable native Slack route arguments.

## Exact model API

### Tool declaration

Constants:

```rust
REACT_TOOL_NAME = "slack_react"
REACT_TOOL_TAG = "slack:react"
```

Register it through `scoped_tool`, in `slack_tool_group()`. `tool_prefix: work` therefore exposes `work_slack_react` in group `work_slack`; the semantic tag remains `slack:react`. Prefixing must not rewrite `action`, schema fields, examples, tags, or prose literals, consistent with `DECISION-extension-tool-prefixes`.

Recommended schema:

```json
{
  "type": "object",
  "properties": {
    "message_ref": {
      "type": "string",
      "minLength": 1,
      "maxLength": 128,
      "description": "Tau-issued Slack message fact ID from a written fact or successful slack_send result"
    },
    "emoji": {
      "type": "string",
      "minLength": 1,
      "maxLength": 77,
      "pattern": "^[a-z0-9_+-]{1,64}(::skin-tone-[2-6])?$",
      "description": "Slack emoji name without surrounding colons"
    },
    "action": { "type": "string", "enum": ["add", "remove"] }
  },
  "required": ["message_ref", "emoji", "action"],
  "additionalProperties": false
}
```

Runtime validation must repeat the schema constraints. Do not trim, lowercase, normalize, or accept Unicode emoji/`:colon_wrappers:`. A base is 1-64 lowercase ASCII letters/digits/`_+-`; the only colon syntax is one exact `::skin-tone-[2-6]` suffix. The current inbound `validate_reaction_name` is close but admits uppercase; use a stricter outbound validator (or tighten shared validation only with ingress regressions). Slack remains authoritative for whether a standard/custom name exists and whether it is reactable.

Use one tool rather than separate add/remove tools because target and scope authority are identical and cleanup must accompany creation. Do not add `toggle`: read-then-write races, shared bot identity, and retries make it ambiguous. Do not add `list`: it expands disclosure to other users/reactions and would require `reactions.get`/`reactions:read`.

Suggested successful result:

```json
{"status":"ok","action":"add","emoji":"eyes"}
```

Do not echo native routing data. Errors are terminal `ToolError`s with bounded safe text.

### `slack_send` result extension

A successful send must make its posted message targetable. Its tool result is:

```json
{"status":"sent","message_ref":"slack-message:<opaque-digest>","delivery_copies":"one"|"one_or_two_possible"}
```

Derive the bounded extension-local reference from the validated Slack
conversation and message timestamp after Slack returns a valid posted identity.
It is a stable fact ID, not an accepted native route argument. Store that same
ref in `PostedMessageOwner` and in the retained send replay record so identical
same-process replay returns the same ref without reposting.

After Slack success, write `message.sent`, key local posted-message authority to
that fact's `MessageFactId`, and then write the ordinary tool result through the
serialized local write-and-flush gate. This is not a harness commit
acknowledgement. Writer failure activates no target. Pending, unknown,
unwritten, evicted, and stale refs all return the same fail-closed error.

## Target eligibility and semantics

Use one bounded map (suggested capacity 1,024, oldest-first among unpinned
entries, as with reply/post ownership):

```text
message_ref -> ReactionTarget {
  agent_id,
  conversation: exact private SlackConversation,
  message_ts: exact validated Slack item timestamp,
  authority: SourceReply { canonical_message_id, source_user }
           | ConfiguredDestination { alias }
}
```

The native coordinates are retained only in private extension state and are
never accepted as separate route fields by the tool.

| Reference source | Eligible? | Required live authority |
|---|---:|---|
| Locally written incoming message create | Yes | Its own reply route, receiving agent, registered receive/source route, current config/dynamic link |
| Locally written incoming edit | Yes | Edit target ref resolves to the original `(channel, ts)`; its source route remains live |
| Incoming human reaction occurrence | No | It is an occurrence about a target, not a message target; do not turn its reply ID into confused-deputy target authority |
| Successful `slack_send` reply | Yes | Locally written sent-fact ref plus the original current source reply route |
| Successful proactive `slack_send` | Yes | Locally written sent-fact ref plus the same current configured proactive alias/route; registration is not required |
| Bridge-local help/start/agents/select/error post | No | It did not pass through `slack_send` and exposes no ref |
| Rejected/failed/unpublished incoming event or send | No | No target activation |
| Arbitrary Slack ID/timestamp, destination alias, or message not routed to Tau | No | Never accepted |

For incoming creates, install the exact reaction target under the locally written
delivered fact's `MessageFactId`. For edits, retain the original message
timestamp and map the edit fact's target reference to that same Slack item.
Reaction operations deliberately establish no eligible target. Incoming target,
edit ownership, and outbound target references remain runtime-only.

### Threads and dynamic DMs

- React to the exact referenced Slack item timestamp, not `conversation.thread_ts`. A thread reply gets a reaction on the child; a thread root gets one on the root.
- Retain and revalidate the authenticated immutable root separately as route authority. Never allow a tool-supplied root, child substitution, or `reply_broadcast` behavior.
- Fixed-thread root creates and child replies are both eligible when the source route is exact.
- An incoming dynamic-DM message and a `slack_send` reply to it are eligible only while the exact runtime D-to-U/W link and source route remain live. Dynamic links never become proactive destinations.
- A proactive static DM post is eligible by its current configured alias just like other proactive posts.

### Agents, roles, sessions, and restart

- Bind each target reference to the exact agent that received the incoming message or authored the outgoing post. Copying it to another agent, another Slack extension instance, or another prefixed instance fails with the same generic unknown/stale/unauthorized error.
- Role changes do not transfer ownership. The same live agent may use the ref only when its **current** role authorizes the concrete react tool. `slack_send` need not still be authorized.
- `slack_register(false)` revokes incoming/source-reply targets because their reply routes are removed. It should not revoke already published proactive-post targets merely because proactive authority never required registration.
- Agent unload removes all that agent's target/reaction/attempt state. Session shutdown, inactive config replacement, and process restart clear all target references and reaction ownership. A later session/instance cannot adopt them.
- A reaction left remotely after unload/restart or an ambiguous unowned add must
  be removed manually or by an independently authorized Slack client. This
  residue is preferable to allowing a new agent/session to remove a reaction
  created by the same shared bot identity.
- Socket reconnect within the same process/session does not clear state. Bounds
  may evict only unowned/unpinned refs. Live ownership is never silently evicted;
  new unowned adds fail before Slack I/O at ownership capacity.

## Reaction ownership and idempotency

Slack `reactions.remove` removes only the authenticated bot user's reaction, but
the bot identity is shared across Tau agents, sessions, instances, and possible
external uses. Add a separate bounded ownership map keyed by the private semantic
tuple `(channel, message_ts, canonical_emoji)` and valued by `agent_id` plus the
ref that established ownership. Do not key ownership only by the presented ref,
because create/edit or other legitimate refs may alias one Slack item.

Rules:

1. Reject `add` before I/O when another Tau agent owns the tuple.
2. `add` + Slack `ok:true`: record/refresh ownership for the calling agent.
3. `add` + `already_reacted`:
   - if the exact tuple is already locally owned by this same agent, treat as idempotent success;
   - otherwise return an ownership error and do **not** adopt it.
4. `remove`: make no Slack call unless the exact tuple is locally owned by this same agent.
5. `remove` + `ok:true`: clear ownership.
6. `remove` + `no_reaction` with local ownership: treat as idempotent success and clear stale ownership.
7. Any other or ambiguous failure preserves existing ownership and never creates
   new ownership. In particular, an ambiguous add made from an unowned state does
   **not** establish provisional ownership; it may have left an orphan reaction,
   but claiming it could adopt a pre-existing reaction whose error response was
   lost.
8. Different agents/instances cannot remove or adopt one another's reaction, even if they share the same Slack bot token.

Reserve an exact tuple while one reaction call is in flight so concurrent add/remove operations cannot race through the local checks. Capture the config/session generation; a late response after teardown must not recreate target or ownership state.

Do not silently evict live reaction ownership: that would leave a remote bot
reaction unremovable through Tau. Count in-flight unowned adds against a fixed
ownership capacity and reject a new unowned add before I/O when full. Pin the
specific target ref that established ownership until the last locally owned emoji
on that target is removed; target-cache insertion may evict only unpinned refs. If
all target entries are pinned, a new ref fails to activate rather than evicting an
owned target. Explicit unload/session/restart clearing can still leave remote
residue, as documented above.

Keep a bounded same-process attempt cache keyed by `ToolCallId`, storing exact agent, arguments, and terminal disposition (suggested 256, matching sends). An identical replay returns/resubmits the same result without another Slack call; a conflicting reuse errors. This does not claim crash-safe idempotency.

## Slack Web API, scopes, and failures

Add to `SlackClient` a reaction operation returning a typed result/error rather than parsing error strings:

```text
POST {api_base}/reactions.add
POST {api_base}/reactions.remove
Authorization: Bearer <bot token>
Content-Type: application/json
{"channel":"<cached exact C/G/D>","timestamp":"<cached exact ts>","name":"eyes"}
```

Do not send file/file_comment/thread/text fields. Use the existing 30-second HTTP bound and production HTTPS rule.

Slack's official method documentation states:

- both methods require bot-token `reactions:write`;
- `reactions.add` is Tier 3 (50+ per minute);
- `reactions.remove` is Tier 2 (20+ per minute);
- skin tones use `name::skin-tone-[2-6]`;
- normal idempotency errors are `already_reacted` and `no_reaction`.

Documentation setup changes:

- Add optional outbound-reaction row: no event subscription, bot scope `reactions:write`.
- Keep `reactions:read` plus `reaction_added`/`reaction_removed` only for the existing inbound owned-post human-reaction feature. Outbound add/remove does not require `reactions:read`, `reactions.get`, or reaction event subscriptions.
- Tell operators to reinstall/refresh the Slack app after adding the scope and retain the membership requirement. No user token and no `chat:write.public`.

Failure policy:

- Validate schema/ref/authority/ownership before I/O; these failures do not freeze config or call Slack.
- Immediately before the first fully authorized API attempt, set the existing monotonic config freeze latch under the state lock. Targets will normally already imply a frozen config, but keep the invariant explicit. Any transport/API outcome after the attempt remains frozen.
- Do not auto-retry or sleep. On HTTP 429 / `ratelimited`, return a bounded retryable error containing only a strictly parsed/clamped `Retry-After` duration. The agent may make a new explicit call later.
- Map `missing_scope` to an actionable `reactions:write` + reinstall diagnostic.
- Map `invalid_name`, `too_many_emoji`, `too_many_reactions`, archived/locked/missing target, membership/permission, and auth failures to bounded stable categories. A safe Slack error code may be retained; never echo response bodies, URLs, channel/timestamps, message bodies, or credentials.
- Treat network/timeout, malformed response, HTTP 5xx, and Slack `fatal_error`, `internal_error`, `request_timeout`, or `service_unavailable` as **outcome unknown** because Slack documents that some failures may occur after an effect. Same-call replay must not call again. A new explicit retry follows the ownership rules above.
- Definitive invalid/stale/unauthorized target failures should use one generic local error to avoid turning the map into a route oracle.

The existing `parse_slack_api_response` only special-cases 429 for `chat.postMessage` and can echo bounded raw non-2xx bodies. Do not reuse that stringly path unchanged for reactions; return a typed reaction API error and ensure raw Slack bodies cannot enter tool errors/logs.

## Config, routing, audit, privacy, and security

### Configuration/routing

No `ExtConfig` field is added. Conversation policy does not gain a broad `react` permission: eligibility is the intersection of (a) a source-bound locally retained exact target, (b) current route authority, and (c) current role authorization for `slack_react`. This allows reactions on receive-only incoming messages without turning aliases into message selectors.

`prefix_agent_id` has no effect on reactions. `tool_prefix` scopes the tool/group structurally. References remain extension-instance scoped even though their text is not prefixed. Add/remove does not change agent selection, registration, reply authority, thread routing, or wake an agent. The bot's own resulting reaction event is rejected by the existing self-user check, preventing prompt loops.

### Audit boundary

For v1, use the existing tool trace as the audit record:
`ToolStarted` records action, bounded emoji name, and fact ref;
`ToolResult`/`ToolError` records the outcome. Native channel/timestamp is never accepted separately from the fact ref. Agent-invoked reactions do not fabricate `message.reaction_added`; that
fact is reserved for externally observed reaction occurrences and could wake an
agent. A future generic typed external-mutation audit fact can be designed across
publishers, but is not required for this bounded Slack-only tool.

### Security/privacy impact

- A reaction is externally visible speech/action by the Slack app. It can notify users or trigger Slack workflows/automations; therefore it must remain separately role-gated and default-off.
- Prompt-injected Slack content can induce a granted agent to react, but only to an exact message the same agent received or posted and whose authority remains live. It cannot select arbitrary Slack IDs.
- Conversation members, Slack Connect participants, workspace administrators, and Slack see the bot identity, emoji, target, and timing. This is not end-to-end private.
- The tool performs no reaction/user listing and reads no message body or reaction roster. It adds only `reactions:write`; no new data-discovery scope.
- Fact refs are not credentials and can appear in tool audit/transcript, but remain agent/session/instance/route checked. Never log additional private route mapping.
- Custom emoji existence remains a server-side fact; error handling should not become a broad enumeration API.

## Implementation map

Primary changes should stay inside `crates/tau-ext-slack`:

1. `src/lib.rs`
   - tool constants/spec/declaration/dispatch and scoped descriptions;
   - strict parsers for action/ref/emoji;
   - `SlackClient` reaction method plus typed error;
   - target/ownership/attempt state and lifecycle clearing;
   - fact-keyed incoming target metadata and post-publication activation;
   - `PostedMessageOwner` ref/authority and sent-fact publication;
   - structured `slack_send` result and `handle_react`;
   - generic safe 429 handling without leaking response bodies.
2. Prefer a focused module analogous to `posted_message_cache.rs` for bounded target/reaction ownership and in-flight state rather than further enlarging `lib.rs`.
3. `src/tests.rs`: extend `FakeClient` with recorded reaction calls and scripted results; add loopback HTTP wire/error tests.
4. Records/docs: add a focused linked design record; update `specs/ARCH-tau-ext-slack.md`, `README.md`, `SECURITY.md`, root `SECURITY.md` Slack paragraph if needed, and `crates/tau-skills/self-knowledge/tau-self-knowledge-ext-slack.md`.
5. No `tau-proto`/harness schema change is needed for v1.

All new/changed Rust structs, fields, public methods, helpers, and tests need informative rustdoc/doc comments per the repository style.

## Required test matrix

### Tool declaration and parsing

- `slack_react` disabled by default, `slack:react`, group `slack`; examples schema-valid.
- Generic prefix scopes the tool and group, leaves tag/action/schema unchanged, and scoped descriptions mention the prefixed send/react names.
- Missing/wrong/extra fields, invalid action, empty/oversized ref, colon-wrapped/Unicode/uppercase/oversized/malformed-tone emoji all fail before I/O; `eyes`, `thumbsup`, `+1`, and `wave::skin-tone-3` pass.
- Raw `channel`, `timestamp`, `thread_ts`, alias, or Slack-native IDs never select a target.

### Target creation and routing

- Incoming create target appears only after the fact frame is written locally;
  writer failure does not activate it.
- An edit target ref resolves to the exact original item; a reaction occurrence
  ref is ineligible.
- Successful send returns a stable `slack-message:<opaque-digest>` fact ref after
  writing `message.sent` and the result locally. Writer failure activates
  nothing. Same-call replay preserves the ref and does not repost or rewrite the
  fact.
- Root, fixed-thread root, and child references call Slack with the exact item timestamp and authenticated route root; no child/root substitution.
- Dynamic DM requires the exact live D-to-user source link. Proactive post works without registration; local help/control posts remain ineligible.

### Authorization/lifecycle

- Unknown, evicted, stale, cross-agent, cross-instance/prefix, wrong route/alias, changed dynamic link, inactive lifecycle, unloaded agent, and old session/restart refs fail without network and with non-oracular errors.
- Unregister revokes source/reply targets but not proactive targets or their
  deletion provenance. Current role policy remains enforced by the harness.
- Inactive config replacement and session shutdown clear all new caches; stale late API completion cannot reinstall them.
- Capacity tests prove oldest-first eviction of unpinned refs, refusal to evict
  live ownership/pinned refs, pre-I/O ownership-capacity rejection, bounded
  attempts, and in-flight cleanup.
- Authorized attempt freezes before I/O even on ambiguous failure/429; local validation/authorization failure does not newly freeze.

### Slack and ownership behavior

- Exact JSON endpoint/body/header for add and remove; no thread/file/text fields.
- Add success records owner; remove by same owner succeeds and clears. Remove without owner or by another agent performs no API call.
- `already_reacted` is success only for the locally owning agent; otherwise no adoption. `no_reaction` is success/clear only after local ownership.
- Ambiguous unowned add does not claim ownership; ambiguous remove retains it. Same call-id replay never repeats I/O; conflicting call-id reuse fails.
- Per-target in-flight reservation prevents concurrent contradictory operations.
- 429 exposes safe Retry-After and does not retry; missing scope names `reactions:write`; transient/unknown results are labeled outcome-unknown; raw bodies, tokens, native IDs, and message text do not appear in diagnostics.
- Self-authored reaction Events API echoes remain rejected; existing eligible human owned-post reaction ingress remains unchanged.

### Validation commands

At minimum run the exact candidate tests and normal project gates after implementation:

```text
cargo nextest run -p tau-ext-slack
cargo check --workspace --all-targets
nix develop -c treefmt --fail-on-change
nix develop -c selfci
```

Then request the bead's independent design/implementation review and address findings before final exact-candidate `selfci`.

## Explicitly out of scope for v1

Reaction listing/search, toggle, arbitrary channel/timestamp targets, alias-wide targeting, reacting to files/canvases, Unicode input normalization, emoji discovery, bulk operations, per-emoji ACLs, reaction text/agent prefixing, auto-retry/queueing, durable target/ownership recovery, cross-agent/instance adoption, removal of pre-existing bot/human reactions, and a new cross-transport typed mutation protocol.

## Sources consulted

- Current `tau-ext-slack` code/tests, README, SECURITY, self-knowledge, and linked local architecture/design records (canonical selectors, proactive sends, immutable threads, edit and inbound-reaction ownership, discovery, sender admission, lifecycle).
- Root `ARCH-external-message-boundary` and extension tool-prefix design.
- Official Slack docs: `reactions.add`, `reactions.remove`, `reactions:write`, and `reactions:read` (method arguments, modifier syntax, scopes, error semantics, and Tier 3/Tier 2 rate limits).
