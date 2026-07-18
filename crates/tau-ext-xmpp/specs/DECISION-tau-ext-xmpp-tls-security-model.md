# DECISION-tau-ext-xmpp-tls-security-model: Plaintext-over-TLS security model

Authority: unconfirmed

The MVP sends ordinary XMPP text protected by TLS certificate validation. It does not
implement OMEMO or any other E2EE, so XMPP servers and room occupants can read message
content. Accepted incoming text is published as `message.delivered` with
transport-neutral sender/conversation metadata and the original stanza body. Actionable
full-resource routes, real-JID proof, room membership evidence, session ids, and agent
routing remain local to the bridge.
