# DESIGN-tau-provider-chatgpt-mutable-config-retries: Mutable request configuration retries

Status: unconfirmed

WebSocket URL, credential, account, and header construction failures derived from mutable provider profiles return `LlmError::ReloadableConfig`. The outer scheduler retries them at auth/config cadence and reloads profile state before the next attempt. Provider HTTP status/body strings, including 499 and cancellation-looking prose, never impersonate typed local cancellation.
