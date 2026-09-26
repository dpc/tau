//! Response-side cache policy and comparison facts. These are not request
//! controls or evidence of actual cache residence.

use serde_json::{Value, json};

/// Return a closed value and an explicit absence/unknown/malformed distinction.
fn choice(value: Option<&Value>, allowed: &[&'static str]) -> Value {
    match value {
        None | Some(Value::Null) => json!({"status": "absent", "value": null}),
        Some(Value::String(s)) => match allowed.iter().copied().find(|v| *v == s) {
            Some(v) => json!({"status": "recognized", "value": v}),
            None => json!({"status": "unknown", "value": null}),
        },
        _ => json!({"status": "malformed", "value": null}),
    }
}

/// Preserve a nonnegative numeric fact without interpreting it as billed usage.
fn count(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => json!({"status": "absent", "value": null}),
        Some(v) => match v.as_u64() {
            Some(n) => json!({"status": "recognized", "value": n}),
            None => json!({"status": "malformed", "value": null}),
        },
    }
}

/// Report only whether a bounded, nonempty comparison identifier was returned.
fn comparison(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => json!({"status": "absent", "present": false}),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 128 => {
            json!({"status": "recognized", "present": true})
        }
        _ => json!({"status": "malformed", "present": false}),
    }
}

/// Project only documented closed policy and diagnostic literals from a
/// terminal response; unknown strings and objects are never copied into the
/// record.
pub(super) fn project(response: Option<&Value>) -> Value {
    let retention = response.and_then(|r| r.get("prompt_cache_retention"));
    let options = response.and_then(|r| r.get("prompt_cache_options"));
    let diagnostics = response.and_then(|r| r.get("prompt_cache_diagnostics"));
    let object_status = |value: Option<&Value>| match value {
        None | Some(Value::Null) => "absent",
        Some(Value::Object(_)) => "recognized",
        _ => "malformed",
    };
    let options_status = object_status(options);
    let diagnostics_status = object_status(diagnostics);
    // A malformed parent cannot supply meaningful child facts.
    let options = options.filter(|v| v.is_object());
    let diagnostics = diagnostics.filter(|v| v.is_object());
    let kind = choice(
        diagnostics.and_then(|v| v.get("type")),
        &[
            "cache_hit",
            "cache_miss",
            "comparison_response_not_found",
            "unavailable",
        ],
    );
    // Only cache_miss has a documented classified reason and miss counts.
    let miss = kind["value"] == "cache_miss";
    let reason = miss
        .then(|| diagnostics.and_then(|v| v.get("reason")))
        .flatten();
    let missed = miss
        .then(|| diagnostics.and_then(|v| v.get("cache_missed_tokens")))
        .flatten();
    let reusable = miss
        .then(|| diagnostics.and_then(|v| v.get("comparison_reusable_tokens")))
        .flatten();
    json!({
        "retention": choice(retention, &["in_memory", "24h"]),
        "options_status": options_status,
        "mode": choice(options.and_then(|v| v.get("mode")), &["implicit", "explicit"]),
        "ttl": choice(options.and_then(|v| v.get("ttl")), &["30m"]),
        "comparison_response_id": comparison(options.and_then(|v| v.get("comparison_response_id"))),
        "diagnostics_status": diagnostics_status,
        "type": kind,
        "reason": choice(reason, &[
            "model_changed", "prompt_cache_key_changed", "service_tier_changed",
            "tools_changed", "text_format_changed", "reasoning_effort_changed",
            "verbosity_changed", "context_compacted", "input_changed"
        ]),
        "comparison_reusable_tokens": count(reusable),
        "cache_missed_tokens": count(missed)
    })
}
