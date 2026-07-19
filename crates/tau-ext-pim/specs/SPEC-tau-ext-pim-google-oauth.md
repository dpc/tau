# SPEC-tau-ext-pim-google-oauth: Google OAuth behavior

Gmail uses the fixed restricted `https://mail.google.com/` scope and a Desktop
installed-app authorization-code flow with PKCE. Pending state retains redirect
URI, state, and verifier; the pasted failed loopback redirect must match its
host, port, root path, contain no fragment, and contain exactly one matching
state plus exactly one code or error before exchange. The generated verifier is
64 characters; accepted verifiers are 43–128 characters. Redirect URLs are at
most 8,192 characters and OAuth fields at most 4,096.

Calendar keeps its separate TVs/Limited Input device flow. Calendar finish takes
no pasted URL. Email and calendar pending states remain separate. State-owned
refresh and pending records use private extension storage; manual
refresh-token-secret mode refuses interactive authentication and state writes.
Pending and stored records validate schema and account identity; corrupt or
expired state fails closed and requires a new start.
Extension storage is installed before configuration so config handlers never
race OAuth state access. Gmail pending PKCE/CSRF state expires after 10 minutes.

The extension publishes Google auth account argument suggestions from the
current validated effective configuration. Only enabled accounts accepted by
the corresponding interactive state-owned Google flow and exactly representable
as one safe slash token are available. Such ids are at most 128 bytes and contain
only printable, non-whitespace ASCII. Disabled modules/accounts, other
providers/backends, and accounts with a manual refresh token secret are excluded.
Email and calendar inventories remain separate, sort lexicographically, and
replace the prior action schema after successful reconfiguration. Completion
choices are capped at 64 per inventory to fit the combined schema's shared
budgets, while missing-account diagnostics repeat at most six names and report
any omitted count. An empty inventory offers no account candidates and says
explicitly that no accounts are available for interactive Google authorization.
Usage remains `<account>`. Published choices and diagnostics contain account ids
only, never secret names/values or native OAuth state.

OAuth JSON is capped at 1 MiB. Model-visible output may contain only the browser
or verification URL, user code, and instructions. It never contains an auth or
device code, PKCE verifier, refresh/access token, client secret, pasted redirect,
or private URL. Errors explicitly never echo the pasted URL, authorization code,
OAuth state, verifier, client secret, refresh/access token, or device code. OAuth
HTTP operations have a 20-second request bound. Access tokens are cached per
account with a 60-second expiry skew; invalid or huge expiry values fail safely.
Authentication may retry only authentication, never a possibly accepted side
effect. Replay-marked action/tool work is ignored.

The flow-family choice is
[DECISION-tau-ext-pim-google-oauth-flow](DECISION-tau-ext-pim-google-oauth-flow.md).
