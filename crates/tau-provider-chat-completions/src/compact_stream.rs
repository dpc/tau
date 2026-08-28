use serde_json::Value;

use super::{LlmError, OutputItemAccumulator, StreamState};

/// Compact-only validator for the exact Chat Completions summary event
/// language.
///
/// Ordinary inference deliberately accepts a wider compatibility language. This
/// validator observes original provider events before that lossy parser and
/// releases no final output until the complete compact response has passed
/// validation.
pub(super) struct CompactStreamValidator {
    /// Independent maximum for narrative and reasoning bytes.
    max_output_bytes: tau_proto::ByteCount,
    /// Whether the one required `stop` terminal has been observed.
    completed: bool,
}

impl CompactStreamValidator {
    /// Start validation with the selected local-summary byte limit.
    pub(super) fn new(max_output_bytes: tau_proto::ByteCount) -> Self {
        Self {
            max_output_bytes,
            completed: false,
        }
    }

    /// Reject a narrative or reasoning delta that would cross the selected
    /// per-channel limit before the ordinary parser appends it.
    pub(super) fn check_append(
        &self,
        current_bytes: usize,
        delta_bytes: usize,
    ) -> Result<(), LlmError> {
        let next_bytes = current_bytes.saturating_add(delta_bytes);
        let next_bytes = tau_proto::ByteCount::new(next_bytes.try_into().unwrap_or(u64::MAX));
        if self.max_output_bytes < next_bytes {
            return Err(output_limit_error());
        }
        Ok(())
    }

    /// Validate one original streamed provider event before compatibility
    /// parsing.
    pub(super) fn observe(&mut self, event: &Value) -> Result<(), LlmError> {
        let Some(object) = event.as_object() else {
            return Err(invalid("summary compactor returned a non-object event"));
        };
        const TOP_LEVEL_FIELDS: &[&str] = &[
            "choices",
            "created",
            "error",
            "id",
            "model",
            "object",
            "obfuscation",
            "provider",
            "service_tier",
            "system_fingerprint",
            "usage",
        ];
        if object
            .keys()
            .any(|key| !TOP_LEVEL_FIELDS.contains(&key.as_str()))
        {
            return Err(invalid(
                "summary compactor returned unsupported provider output",
            ));
        }
        if object.get("error").is_some_and(|error| !error.is_null()) {
            if !error_choices_are_content_free(object.get("choices")) {
                return Err(invalid(
                    "summary compactor returned mixed error and semantic output",
                ));
            }
            return Ok(());
        }
        let Some(choices) = object.get("choices") else {
            return Ok(());
        };
        let Some(choices) = choices.as_array() else {
            return Err(invalid("summary compactor returned invalid choices"));
        };
        if choices.is_empty() {
            return Ok(());
        }
        if choices.len() != 1 || self.completed {
            return Err(invalid(
                "summary compactor returned mixed or post-terminal output",
            ));
        }
        let Some(choice) = choices[0].as_object() else {
            return Err(invalid("summary compactor returned an invalid choice"));
        };
        const CHOICE_FIELDS: &[&str] = &[
            "delta",
            "finish_reason",
            "index",
            "logprobs",
            "native_finish_reason",
        ];
        if choice
            .keys()
            .any(|key| !CHOICE_FIELDS.contains(&key.as_str()))
        {
            return Err(invalid(
                "summary compactor returned unsupported semantic output",
            ));
        }
        if choice.get("index").and_then(Value::as_u64) != Some(0) {
            return Err(invalid(
                "summary compactor returned an invalid choice index",
            ));
        }
        if choice.get("logprobs").is_some_and(|value| !value.is_null()) {
            return Err(invalid(
                "summary compactor returned unsupported semantic output",
            ));
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return Err(invalid("summary compactor returned an invalid delta"));
        };
        const DELTA_FIELDS: &[&str] = &[
            "content",
            "reasoning",
            "reasoning_content",
            "role",
            "thinking",
        ];
        if delta
            .keys()
            .any(|key| !DELTA_FIELDS.contains(&key.as_str()))
        {
            return Err(invalid(
                "summary compactor returned unsupported semantic output",
            ));
        }
        if delta
            .get("role")
            .is_some_and(|role| role.as_str() != Some("assistant"))
        {
            return Err(invalid("summary compactor returned an invalid role"));
        }
        for field in ["content", "reasoning", "reasoning_content", "thinking"] {
            if delta
                .get(field)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                return Err(invalid(
                    "summary compactor returned invalid narrative or reasoning",
                ));
            }
        }
        let finish_reason = choice.get("finish_reason");
        if choice
            .get("native_finish_reason")
            .is_some_and(|native| !native.is_null() && !native.is_string())
        {
            return Err(invalid(
                "summary compactor returned invalid native terminal metadata",
            ));
        }
        match finish_reason {
            None | Some(Value::Null) => {}
            Some(Value::String(reason)) if reason == "stop" => self.completed = true,
            Some(_) => {
                return Err(invalid(
                    "summary compactor did not produce the required stop terminal",
                ));
            }
        }
        Ok(())
    }

    /// Validate the complete parsed projection and release it to the attempt
    /// result.
    pub(super) fn finish(self, state: &StreamState) -> Result<(), LlmError> {
        if !self.completed {
            return Err(invalid(
                "summary compactor did not produce the required stop terminal",
            ));
        }
        let mut narrative_count = 0_usize;
        let mut narrative_bytes = tau_proto::ByteCount::ZERO;
        let mut reasoning_bytes = tau_proto::ByteCount::ZERO;
        for item in &state.output_items {
            let (bytes, total) = match item {
                OutputItemAccumulator::Message(text) => {
                    narrative_count = narrative_count.saturating_add(1);
                    (text.len(), &mut narrative_bytes)
                }
                OutputItemAccumulator::Reasoning(text) => (text.len(), &mut reasoning_bytes),
                OutputItemAccumulator::ToolCall(_) => {
                    return Err(invalid("summary compactor returned a tool call"));
                }
            };
            *total = total.saturating_add(tau_proto::ByteCount::new(
                bytes.try_into().unwrap_or(u64::MAX),
            ));
            if self.max_output_bytes < *total {
                return Err(output_limit_error());
            }
        }
        if narrative_count != 1 || state.text.trim().is_empty() {
            return Err(invalid(
                "summary compactor did not return exactly one nonempty message",
            ));
        }
        Ok(())
    }
}

/// Accept only the documented content-free choice envelope on provider errors.
fn error_choices_are_content_free(choices: Option<&Value>) -> bool {
    let Some(choices) = choices else {
        return true;
    };
    let Some(choices) = choices.as_array() else {
        return false;
    };
    if choices.is_empty() {
        return true;
    }
    if choices.len() != 1 {
        return false;
    }
    let Some(choice) = choices[0].as_object() else {
        return false;
    };
    const ERROR_CHOICE_FIELDS: &[&str] = &[
        "delta",
        "error",
        "finish_reason",
        "index",
        "native_finish_reason",
    ];
    if choice
        .keys()
        .any(|field| !ERROR_CHOICE_FIELDS.contains(&field.as_str()))
        || choice.get("index").and_then(Value::as_u64) != Some(0)
        || ["finish_reason", "native_finish_reason"]
            .iter()
            .any(|field| {
                choice
                    .get(*field)
                    .is_some_and(|value| !value.is_null() && !value.is_string())
            })
        || choice
            .get("error")
            .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return false;
    }
    let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
        return false;
    };
    delta.iter().all(|(field, value)| match field.as_str() {
        "content" => value.is_null() || value.as_str() == Some(""),
        "role" => value.as_str() == Some("assistant"),
        _ => false,
    })
}

/// Construct a deterministic compact-shape rejection.
fn invalid(message: &str) -> LlmError {
    LlmError::InvalidCompaction(message.to_owned())
}

/// Construct the shared incremental and final per-channel limit rejection.
fn output_limit_error() -> LlmError {
    invalid("summary compactor narrative or reasoning exceeds its byte limit")
}
