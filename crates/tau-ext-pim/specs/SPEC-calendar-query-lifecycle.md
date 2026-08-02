# SPEC-calendar-query-lifecycle: Calendar query lifecycle visibility

## Record justification

`calendar_search` and `calendar_free_busy` span the model-visible tool interface, runtime result shaping, and Google and ICS provider pagination, so no one local artifact can coherently own this contract.

## Contract

`calendar_search` returns active events by default. Active means that `status`
is absent or is not `cancelled` under ASCII case-insensitive comparison.
`include_cancelled: false` is the default; `include_cancelled: true` deliberately
requests active events plus cancellation records currently exposed by the
provider. It is discovery, not revision history.

The normal provider path suppresses cancellation records before pagination.
Google explicitly requests `showDeleted=false` normally and `showDeleted=true`
for discovery. ICS suppresses standalone cancelled events, cancelled recurring
masters, and cancelled overrides before sorting and page slicing. The runtime
defensively suppresses recognizable cancelled records on a normal search.

`calendar_get(event_id)` remains targeted inspection and may return a cancelled
event when the provider can read it. `calendar_free_busy` includes only active,
blocking events: cancelled events, Google transparent events, and events whose
self response is declined do not block time. Tentative events remain busy.
Provider-neutral filtering happens while filling a semantic page, before that
page is returned. The runtime consumes provider continuations until it has the
requested number of visible or blocking rows or reaches provider exhaustion.
Its continuation cursor starts after every provider row consumed to build the
page, so excluded rows cannot create an empty or short page while later matches
remain. Each semantic page permits at most 100 sequential provider requests and
10,000 consumed provider rows. Exceeding either cap or observing any repeated
cursor fails visibly rather than misreporting a short page or exhaustion.

Range reads default to 20 rows and accept at most 100. A returned cursor
encodes the original command, calendar, normalized absolute range, visibility,
filter, and limit; continuation calls contain the cursor and no other query
field. Normal empty results remain successful and use the normal empty list
shape.
