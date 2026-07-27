# GATE-typed-image-tool-results: Keep local images as typed tool results

## Gate

Local image inspection must produce native typed tool-result content rather than
base64 text or synthesized user messages. Image bytes may reach only explicitly
audited provider/model routes that advertise image-input and image-tool-result
capabilities.

## Justification

The user wants images to retain tool-call causality and durable replay authority
without exposing payload bytes to unaudited provider routes or generic
observers.
