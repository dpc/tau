# SPEC-tau-ext-xmpp-allowlist-and-default-recipient: Mandatory allowlist and default recipient

`allowed_jids` is mandatory and nonempty, and `default_recipient` must match it. Bare
allowlist entries match any sender resource for that account; full entries match
exactly. The default is therefore authorized for notices and invites. Admission succeeds
before routing or message-fact publication. A bare entry does not authorize an arbitrary
MUC occupant without a current matching real-JID mapping or the explicit
trusted-membership opt-in.
