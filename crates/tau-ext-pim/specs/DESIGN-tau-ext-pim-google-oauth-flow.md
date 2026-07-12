# DESIGN-tau-ext-pim-google-oauth-flow: Google OAuth flow split

Status: confirmed, 2026-06-22, dpc

Gmail IMAP/SMTP OAuth and Google Calendar OAuth intentionally use different
Google OAuth client types and flow families. Gmail requires the restricted
`https://mail.google.com/` scope for IMAP/SMTP XOAUTH2, and Google's device-flow
endpoint rejects that scope, so state-owned Gmail auth uses a Desktop
installed-app authorization-code flow with PKCE and a manually pasted failed
loopback redirect URL. Google Calendar remains on the device flow with a
`TVs and Limited Input devices` client because that UX works for the Calendar
scope and avoids changing working Calendar behavior. Shared helper code may be
centralized, but email and calendar pending auth state stay separate so one
flow's migration or validation rules cannot silently weaken the other.
