# Chat Completions transport boundary

Chat Completions sends prompts, tool definitions, credentials, and conversation
context to configured external endpoints. Every attempt receives the built-in
provider process's immutable `OutboundNetworkPolicy`; reqwest environment proxy
discovery and redirects remain disabled. A selected proxy is the only route
for that attempt and failure never falls back direct.

Target and HTTPS-proxy TLS use platform verification plus the optional
startup-read custom CA. Plain HTTP endpoints expose headers and content to a
selected proxy. Cancellation drops the active async response future/socket;
header, body, and idle waits retain finite bounds. Error bodies remain bounded
and provider content must never enter retry status, ordinary logs, or watcher
events.
