# DESIGN-tau-ext-provider-builtin-durable-session-diagnostics: Provider diagnostics require an existing durable session directory

Status: unconfirmed

Provider debug request/response captures may include full prompt text, tool
results, and model output. The extension derives an explicit diagnostics policy
from `harness.session_dir` current-state events and passes that policy into
backend debug writers. Shared backend helpers only return debug paths when that
explicit durable-session signal is true and the durable session directory already
exists; provider diagnostics must not infer durability from filesystem shape or
create per-session roots on their own. This preserves ephemeral-session
persistence boundaries even when an ephemeral run reuses a session id with older
durable state.
