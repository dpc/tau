# DESIGN-tau-ext-xmpp-muc-identity: MUC conversation identity

Status: unconfirmed

Recommended routing is one MUC room per globally unique Tau agent id. `muc.room_template` renders the complete localpart in Handlebars strict mode. Its default is `{{agent_id}}-{{agent_hash}}`, combining the full agent id with a 40-bit compact-base32, domain-separated BLAKE3 label over that id. Templates can instead use agent/session/role/role-group/instance identity or explicit randomness and may omit collision protection as trusted operator policy. If a normalized rendered room is already active or pending for a different agent, the worker rejects registration before join/routing insertion instead of overwriting `room_to_agent`.
