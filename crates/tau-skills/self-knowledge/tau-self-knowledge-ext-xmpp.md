---
name: tau-self-knowledge-ext-xmpp
description: Use for the separately installed XMPP bridge std-xmpp, its disabled default, trusted extension boundary, and owning configuration documentation.
advertise: false
---

# XMPP bridge

`std-xmpp` is disabled by default. Install the flake's `tau-ext-xmpp` package on
`PATH` and enable the instance, for example `TAU_ENABLE_EXTENSIONS=std-xmpp tau`.
Tau supervises it over stdio but bundles neither executable nor source.
Its empty Tau default config supplies no account, room or gateway settings.

For Tau protocol 23.1, rebuild with registry `dpc-tau-client`/`dpc-tau-proto`
0.29.0. Update the extension with the harness before separately authorized activation.
Older-protocol binaries fail before configuration.

Tau owns secret delivery, instance state, publisher identity, prefixes, role policy
and supervision. The standalone project owns XMPP configuration and lifecycle;
consult it rather than guessing settings. The extension is trusted local code,
not a sandbox; external messages remain untrusted content.

## Reply context

The producer may attach a bounded descriptive XEP-0461 relation beside the exact
selected body, leaving sender-written fallback quotes unchanged. `to` identifies
the parent author, not a destination; a MUC occupant need not equal the room.
`xmpp-reply-description:` is an opaque conversation/author/message locator, not
tool authority. Optional resource/nickname labels remain untrusted.

The renderer puts relation metadata in `message`, without an empty
`quoted_message`. It neither receives separate parent content nor fetches it.
Malformed/oversized optional context is omitted or reduced rather than invalidating
the admitted body. The combined cap is `min(configured max_message_bytes, 131072)`,
including at most 4,096 context bytes and 2,048 encoded bytes per quoted value.

Parent changes do not refresh published context. This adds no edit/delete
ingestion, ACK-backed delivery queue, routes or permissions. Older producers may
append textual context instead.

See `docs/extensions.md#xmpp` for the owning-project pointer and
`tau-self-knowledge-debugging-extensions` for startup/log diagnosis.
