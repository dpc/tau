# SPEC-tau-provider-codex-retry-classification: Mutable configuration retry classification

WebSocket URL, credential, account, and header construction failures derived from mutable provider profiles return `LlmError::ReloadableConfig`. The outer scheduler retries them at auth/config cadence and reloads profile state before the next attempt. Provider HTTP status/body strings, including 499 and cancellation-looking prose, never impersonate typed local cancellation.
