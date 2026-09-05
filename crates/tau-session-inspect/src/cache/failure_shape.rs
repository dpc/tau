//! Exact required shapes of the two existing Codex failure envelopes.
//!
//! These predicates validate private evidence without retaining or projecting
//! its strings. They are revision-coupled readers, not a historical schema
//! registry.

use serde_json::Value;

/// One required producer field and its allocation-free structural predicate.
type FieldCheck<'a> = (&'a str, fn(&Value) -> bool);

/// Validates required current compact-HTTP schema-zero fields recursively.
pub(super) fn compact(value: &Value) -> bool {
    fields(
        value,
        &[
            ("operation", |v| literal(v, &["compact"])),
            ("backend", |v| {
                fields(
                    v,
                    &[
                        ("kind", |v| literal(v, &["responses"])),
                        ("transport", |v| literal(v, &["unary_http"])),
                    ],
                )
            }),
            ("http", |v| {
                fields(
                    v,
                    &[
                        ("status", u16_value),
                        ("headers", |v| {
                            fields(
                                v,
                                &[
                                    ("content_type", nullable_bounded_bytes),
                                    ("retry_after", nullable_bounded_bytes),
                                    ("request_id", nullable_bounded_bytes),
                                    ("openai_request_id", nullable_bounded_bytes),
                                    ("x_request_id", nullable_bounded_bytes),
                                ],
                            )
                        }),
                    ],
                )
            }),
            ("body", |v| {
                fields(
                    v,
                    &[
                        ("decoded_bytes_received", Value::is_u64),
                        ("retained_bytes", Value::is_u64),
                        ("complete", Value::is_boolean),
                        ("truncated", Value::is_boolean),
                        ("redacted_prefix_truncated", Value::is_boolean),
                        ("sha256_decoded_received", |v| {
                            v.as_str().is_some_and(|s| {
                                s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
                            })
                        }),
                        ("sha256_coverage", |v| {
                            literal(v, &["complete_decoded_body", "decoded_bytes_received"])
                        }),
                        ("redacted_decoded_prefix_base64", Value::is_string),
                    ],
                ) && optional(v, "parsed_error", parsed_error)
            }),
        ],
    )
}

/// Validates required current finite-attempt schema-one fields recursively.
pub(super) fn attempt(value: &Value) -> bool {
    fields(
        value,
        &[
            ("operation", |v| literal(v, &["inference", "compact"])),
            ("logical_attempt", |v| v.as_u64().is_some_and(|n| n != 0)),
            ("wire_dispatch_index", nullable_u64),
            ("backend", |v| {
                fields(
                    v,
                    &[
                        ("kind", |v| literal(v, &["responses"])),
                        ("transport_intent", |v| literal(v, &["websocket"])),
                        ("transport_established", Value::is_boolean),
                    ],
                )
            }),
            ("outcome", |v| literal(v, &["retry_scheduled"])),
            ("classification", |v| {
                fields(
                    v,
                    &[
                        ("category", |v| {
                            literal(
                                v,
                                &[
                                    "transport",
                                    "overload",
                                    "throttle",
                                    "usage_window",
                                    "account",
                                    "auth",
                                    "unknown",
                                ],
                            )
                        }),
                        ("retry_after_secs", nullable_u64),
                    ],
                )
            }),
            ("wire", |v| {
                fields(
                    v,
                    &[
                        ("wire_dispatches", Value::is_u64),
                        ("repair_used", Value::is_boolean),
                        ("response_bytes_received", Value::is_u64),
                        ("semantic_progress", |v| literal(v, &["none", "parsed"])),
                    ],
                )
            }),
            ("provider", |v| {
                v.is_null()
                    || fields(
                        v,
                        &[
                            ("terminal_event_type", nullable_string),
                            ("canonical_error_code", nullable_string),
                            ("provider_request_id", nullable_string),
                            ("provider_response_id", nullable_string),
                            ("message", lengths),
                            // This deliberately opaque structural value has no scalar meaning.
                            ("terminal_event_shape", |_| true),
                        ],
                    )
            }),
            ("transport", |v| {
                v.is_null()
                    || fields(
                        v,
                        &[
                            ("phase", |v| {
                                literal(v, &["pre_upgrade", "send", "response_stream"])
                            }),
                            ("kind", |v| {
                                literal(
                                    v,
                                    &[
                                        "clean_eof",
                                        "websocket_close",
                                        "malformed_text",
                                        "binary_frame",
                                        "websocket_read",
                                        "websocket_send",
                                        "websocket_control_ping",
                                        "response_idle_timeout",
                                        "websocket_upgrade",
                                        "outbound",
                                    ],
                                )
                            }),
                            ("ws_close_code", |v| v.is_null() || u16_value(v)),
                            ("ws_close_reason", lengths),
                            ("clean_eof", Value::is_boolean),
                            ("frame_bytes", nullable_u64),
                        ],
                    )
            }),
            ("truncation", |v| {
                fields(
                    v,
                    &[
                        ("total", Value::is_boolean),
                        ("shape", Value::is_boolean),
                        ("identifiers", Value::is_boolean),
                    ],
                )
            }),
        ],
    )
}

/// Requires every named field, including nullable fields, with its exact type.
fn fields(value: &Value, required: &[FieldCheck<'_>]) -> bool {
    value.is_object()
        && required
            .iter()
            .all(|(name, check)| value.get(*name).is_some_and(check))
}

/// Validates an omitted-or-present field that the producer skips when absent.
fn optional(value: &Value, name: &str, check: fn(&Value) -> bool) -> bool {
    value.get(name).is_none_or(check)
}

/// Checks a closed producer-owned literal without exporting it.
fn literal(value: &Value, allowed: &[&str]) -> bool {
    value.as_str().is_some_and(|s| allowed.contains(&s))
}

/// Checks one typed HTTP/WebSocket status or close code.
fn u16_value(value: &Value) -> bool {
    value.as_u64().is_some_and(|v| u16::try_from(v).is_ok())
}

/// Requires an unsigned number or explicit null, not an absent field.
fn nullable_u64(value: &Value) -> bool {
    value.is_null() || value.is_u64()
}

/// Requires a string or explicit null, never a scalar coerced into text.
fn nullable_string(value: &Value) -> bool {
    value.is_null() || value.is_string()
}

/// Validates scalar length observations emitted for redacted transport details.
fn lengths(value: &Value) -> bool {
    fields(
        value,
        &[
            ("present", Value::is_boolean),
            ("utf8_bytes", Value::is_u64),
            ("unicode_scalars", Value::is_u64),
        ],
    )
}

/// Validates the explicit-null header representation.
fn nullable_bounded_bytes(value: &Value) -> bool {
    value.is_null() || bounded_bytes(value)
}

/// Validates required byte evidence and the producer's omitted optional fields.
fn bounded_bytes(value: &Value) -> bool {
    fields(
        value,
        &[
            ("original_bytes", Value::is_u64),
            ("retained_bytes", Value::is_u64),
            ("truncated", Value::is_boolean),
            ("base64", Value::is_string),
        ],
    ) && optional(value, "original_unicode_scalars", Value::is_u64)
        && optional(value, "retained_unicode_scalars", Value::is_u64)
        && optional(value, "utf8", Value::is_string)
}

/// Validates each emitted allowlisted provider field without retaining its
/// value.
fn parsed_error(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.is_empty()
            && object.iter().all(|(name, value)| {
                ["code", "type", "param", "message"].contains(&name.as_str())
                    && bounded_bytes(value)
            })
    })
}
