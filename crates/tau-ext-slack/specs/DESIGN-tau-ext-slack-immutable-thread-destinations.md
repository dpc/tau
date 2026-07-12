# DESIGN-tau-ext-slack-immutable-thread-destinations: Thread destinations are immutable authenticated routes

Status: unconfirmed

Reply thread roots are validated event metadata and travel inside the same
pending, accepted, and active conversation state as the configured channel or
linked DM. `slack_send` exposes no thread argument. Top-level origins store no
thread root, while threaded origins supply their root to `chat.postMessage`;
successful completion must repeat the exact typed route metadata or fail closed.
Configured proactive aliases may instead bind one fixed validated thread root.
