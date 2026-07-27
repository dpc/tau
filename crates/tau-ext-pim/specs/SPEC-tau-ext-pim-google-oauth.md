# SPEC-tau-ext-pim-google-oauth: Google OAuth behavior

## Record justification

Google OAuth spans email and calendar configuration, their separate pending-state and tool flows, the shared OAuth helper, and extension storage, so no one implementation area can own the complete validation and secret-handling contract.

Gmail uses the fixed restricted `https://mail.google.com/` scope and a Desktop
installed-app authorization-code flow with PKCE. Pending state retains redirect
URI, state, and verifier; the pasted failed loopback redirect must match its
host, port, root path, contain no fragment, and contain exactly one matching
state plus exactly one code or error before exchange.

Google's device-flow endpoint rejects the restricted mail scope, while Calendar
works with the TVs/Limited Input device flow. Calendar finish takes no pasted
URL. Email and calendar pending states remain separate so changes to one flow
cannot weaken the other's validation. State-owned
refresh and pending records use private extension storage; manual
refresh-token-secret mode refuses interactive authentication and state writes.
Pending and stored records validate schema and account identity; corrupt or
expired state fails closed and requires a new start.
Extension storage is installed before configuration so config handlers never
race OAuth state access. Gmail pending PKCE/CSRF state expires after 10 minutes.

Interactive auth actions expose only currently enabled, state-owned Google
accounts from their corresponding email or calendar inventory. Account
suggestions and diagnostics may contain safe account IDs, never secret
names/values or native OAuth state.

Model-visible output may contain only the browser
or verification URL, user code, and instructions. It never contains an auth or
device code, PKCE verifier, refresh/access token, client secret, pasted redirect,
or private URL. Errors explicitly never echo the pasted URL, authorization code,
OAuth state, verifier, client secret, refresh/access token, or device code. OAuth
HTTP operations are bounded. Access tokens are cached per account with an expiry
skew; invalid or huge expiry values fail safely.
Authentication may retry only authentication, never a possibly accepted side
effect. Replay-marked action/tool work is ignored.
