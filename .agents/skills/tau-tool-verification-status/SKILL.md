---
name: tau-tool-verification-status
description: Verify Tau status state and title transitions, validation, acknowledgement persistence, and activation steering around tools and watched agents.
advertise: false
---

# Tau Tool Verification: Status

Load `tau-tool-verification` first for shared verification and reporting rules.
Use this focused plan for the `status` tool. Pair it with
`tau-tool-verification-agent-coordination` when testing watched agents or direct
messages.

Keep durable expectations below separate from observed quirks. Run live probes
through the provider and harness; do not infer activation behavior from source
or from receipt of a queued notification alone. Give every concurrent probe a
distinct nonce.

## Durable expectations

* `status` accepts `working`, `waiting`, `blocked`, and `done`. `waiting` means
  progress is paused pending an expected self-resolving event; `blocked` means
  progress needs external intervention. It trims the title, then
  requires a non-empty, single-line canonical title of at most 160 UTF-8 bytes
  with no control characters, U+2028, or U+2029.
* A successful call clearly reports the accepted state and title. A rejected
  call must not imply that the requested state became current.
* Ordinary transitions work in both directions: initial Working, Working title
  update, Waiting, recovery from Waiting to Working, Blocked, recovery from
  Blocked to Working, and Done. Watched status projection preserves Waiting as
  distinct from Blocked.
* One accepted Working acknowledgement survives routine tool-result rounds.
  Routine results must not repeatedly steer the agent to acknowledge the same
  work.
* Watched-child progress and status updates remain deliverable but do not steer
  the watcher into `status(working)`.
* Genuine activating inputs still activate appropriately: a visible user
  prompt, a direct agent message, and a watched child's final response. Each
  activation should cause at most one acknowledgement request for that work,
  not a reminder loop.
* Steering and watched progress use concise, generic shapes:
  `Reminder: when working on a task use \`status\` tool to acknowledge it.`,
  `Your \`status\` is set to \`working\` on "<title>". Set it to \`done\`,
  \`waiting\`, or \`blocked\` to finish or call \`wait\` when waiting for external events.`,
  and `Watched agent <agent-id> status: <state> on <title>`.

## Efficient live plan

Use several short-lived delegates when possible so event streams and status
state do not contaminate one another. Tell delegates not to change the
repository and not to comply with a status reminder that would invalidate the
probe.

### Transitions and validation

Use a nonce-tagged title and record exact results for:

1. `working`, a second `working` title, `waiting`, recovery to `working`,
   `blocked`, another recovery to `working`, and `done`; verify watched
   projections preserve both paused phases exactly;
2. invalid state and omitted title, which may be rejected by tool-schema
   validation before invocation;
3. empty, whitespace-only, and surrounding-whitespace titles; verify that
   accepted output uses the trimmed canonical title;
4. LF, CR, tab, another ASCII control, U+2028, and U+2029 in a title;
5. titles of exactly 160 and 161 UTF-8 bytes after trimming;
6. a multibyte boundary case to confirm that the limit counts UTF-8 bytes, not
   characters.

Do not claim rejected calls preserved or changed state unless a separate
observable proves it; the tool has no status-query operation. Flag an error
that echoes the requested `state` and `title` as ambiguous if readers could
mistake them for current state.

### Working persistence across routine rounds

Call `status(working)` once, then perform at least five ordinary tool rounds
with no genuine activating input. Mix quick read-only calls rather than using a
single batch. Record every acknowledgement directive and status call. Finish
with one `done` transition, remain alive only long enough to detect a bounded
post-Done repeat (for example, two further turns), then stop the delegate.

Pass when no routine round requests or causes another Working acknowledgement.
Report post-Done directives separately. With no new substantive tool call,
there must be no spontaneous reminder. A post-Done `self_info`, skill, shell,
or other substantive tool admission correctly requests one fresh Working
acknowledgement; `status` and `wait` are exempt. A repeated reminder without
another substantive admission is a defect.

### Watched progress isolation

Start one auto-watched child and instruct it to emit three nonce-tagged Working
titles separated by harmless tool rounds, then Done and one final response.
Correlate each received event with its nonce and classify it as:

* watched status/progress;
* watched final response;
* direct message;
* status acknowledgement directive.

Do not infer causality merely because two events are adjacent. Strong evidence
is an acknowledgement directive source-linked to the watched event in a trace,
or a controlled comparison where the directive appears only after that event.
Verify that all progress arrives without watcher status steering, while the
final response remains a distinct activating input. Require the same generic
`status: <state> on <title>` shape for initial reports and title updates; do not
expect sequencing language such as “started” or “updated”.

### Genuine activation paths

Probe each path independently with a nonce. Begin with the receiving agent
settled in `done` and no outstanding tools or watched events. For each path,
require either an activation-source trace naming that input or exactly one
provider turn that emits the expected nonce-bearing acknowledgement/status
effect. Observe at least two further bounded turns or events and require zero
duplicate activations.

1. Send a direct parent-to-child agent message that requests one exact
   nonce-bearing acknowledgement.
2. Have one watched child produce a nonce-bearing final response separately
   from its progress; require the watcher’s resulting provider turn to
   acknowledge that nonce once.
3. Ask the external user or controlling session to send a nonce-bearing prompt
   that requests one exact acknowledgement. If no external sender is available,
   mark this path unavailable rather than simulating or inferring it.

Receipt proves delivery, not necessarily activation classification. Prefer
event traces that name the activation source. Record duplicate deliveries,
repeated acknowledgement directives, provider retries, and extra turns.

## Current quirks to check, not product contracts

These observations can regress or disappear; verify them rather than assuming
them:

* Implementation-level empty and overlong-title errors may echo the requested
  state and title, which looks like current-state output even though no
  acceptance occurred.
* A delegate that reports Done can receive repeated directives to return to
  Working after subsequent parent/delegate messages or lifecycle events. Bound
  the observation window and report the exact triggering sequence; do not keep
  a delegate alive merely to count an unbounded repeat loop.

## Report

Provide a compact causal transcript with exact nonces and tool/error wording.
Separate:

* passed durable expectations;
* failures with the strongest available causal evidence;
* inconclusive or unavailable activation paths;
* current quirks, confusing wording, duplicate prompts or notifications, and
  wasted calls/turns.
