# tau-ext-test-dummy security

This crate is a local test fixture and should remain disabled in normal user
configuration.

Security-relevant boundaries:

- Communication is limited to the trusted local Tau extension stdio protocol.
- The crate does not read or write files, persist state, access secrets, open
  network connections, or spawn subprocesses.
- Config parsing rejects unknown fields and invalid `restart_mode` values.
- Replayed `tool.started` deliveries are ignored so historical events cannot
  retrigger tool replies or extension exits.

Prompt interception is deliberately narrow. It changes only prompt text and
must preserve routing and identity fields so hidden/internal or extension
originated prompts do not become user-visible prompts by accident.
