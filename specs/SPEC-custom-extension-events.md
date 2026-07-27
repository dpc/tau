# SPEC-custom-extension-events: Custom extension-owned event publication

## Record justification

Custom events span protocol name validation, peer authority, extension
activation, harness interception/persistence, and client subscription APIs.
This record keeps that distributed publication contract coherent.

## Event shape and authority

`extension.event` remains the outer wire variant. Its nested dotted name is the
event name used for interception and subscription. The nested name must have
non-empty segments and an extension-owned category; reserved Tau categories
cannot be represented as custom events. The payload is opaque CBOR with optional
session routing metadata.

Every authenticated live configured extension entry kind, including configured
Core, may author a custom event without a capability. An attached
harness-assigned socket UI may also author one. Unconfigured or disconnected
extension connections, non-UI socket peers, and dedicated external-message
socket peers have no authority. Harness-internal direct publication remains
outside peer admission.

## Publication and activation

Admission performs no custom-event semantic work. Accepted events retain the
caller-selected `Emit.persist` value and enter ordinary generic interception,
commit, and broadcast with their authenticated run-local source. Interceptors
may drop an event or replace its opaque payload and session metadata while
retaining the exact nested event name. A replacement with another nested name is
invalid and leaves the original event to continue through publication.

Pre-Ready extension frames are globally ordered operational traffic, not
activation declarations. The complete frame remains in the bounded deferred
queue until source and global activation permit publication. Disconnect drops
unreleased frames.

## Persistence and consumers

Custom events retain runtime sequencing, debug publication, and live broadcast,
but never enter agent, session, or restore semantic stores for either `persist`
value. There is no cold replay or late historical catch-up. A durable custom
fact requires a separately approved typed event contract.

The harness has no semantic consumer for custom events. Exact and prefix
subscribers consume the committed event directly. Internal routed deliveries
retain the run-local connection source. The ordinary wire `EventDelivery` does
not yet expose authenticated publisher identity; completing the general
publisher-envelope migration remains outside this specification.

Configured extensions are trusted local executables. Existing decoded-frame,
pre-activation queue, and protocol-I/O key-cardinality bounds apply. This
specification adds no custom payload limit or hostile-IPC policy.

## Scope

This specification implements only the custom-event row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
It does not change DTOs, client APIs, subscriber matching, first-party event
authority, or any tool, action, shell, UI-command, or publisher-envelope row.
