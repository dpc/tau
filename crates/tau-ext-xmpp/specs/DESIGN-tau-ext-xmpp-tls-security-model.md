# DESIGN-tau-ext-xmpp-tls-security-model: Plaintext-over-TLS security model

Status: unconfirmed

The MVP sends ordinary XMPP text protected by TLS certificate validation. It does not implement OMEMO or any other E2EE, so XMPP servers and room occupants can read message content. Incoming text is prefixed with XMPP message/channel/source context and is submitted only via `extension.prompt_submit_request`; the model-visible prefix does not include generated room labels, session ids, or agent ids.
