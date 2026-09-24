//! Request-wide typed image admission using the established adapter bounds.

#[cfg(test)]
mod tests;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tau_proto::{ToolResultContentPart, ToolResultItem, ToolResultStatus};

/// Shared request budget, not a separate allowance for each tool result.
pub(super) struct ImageBudget {
    /// Audited native image support for the selected route.
    supported: bool,
    /// Admitted original encoded image bytes.
    image_bytes: usize,
    /// Admitted expanded data URL bytes.
    data_url_bytes: usize,
}

impl ImageBudget {
    /// Start the same 24 MiB raw / 32 MiB expanded budget as other adapters.
    pub(super) fn new(supported: bool) -> Self {
        Self {
            supported,
            image_bytes: 0,
            data_url_bytes: 0,
        }
    }

    /// Keep images inside their function result, never a synthetic user
    /// message.
    pub(super) fn output(&mut self, result: &ToolResultItem) -> Value {
        let text = result.render_provider_text();
        if !matches!(result.status, ToolResultStatus::Success) || result.provider_content.is_empty()
        {
            return Value::String(text);
        }
        if !self.supported {
            return Value::String(format!(
                "{text}\n[image omitted: this provider route does not support native image tool output]"
            ));
        }
        let mut content = vec![json!({"type": "input_text", "text": text})];
        for part in &result.provider_content {
            let ToolResultContentPart::Image(image) = part;
            let encoded_len = image.data.len().div_ceil(3).saturating_mul(4);
            let url_len = "data:;base64,"
                .len()
                .saturating_add(image.media_type.mime_type().len())
                .saturating_add(encoded_len);
            let raw_total = self.image_bytes.saturating_add(image.data.len());
            let url_total = self.data_url_bytes.saturating_add(url_len);
            if 24 * 1024 * 1024 < raw_total || 32 * 1024 * 1024 < url_total {
                content.push(json!({
                    "type": "input_text",
                    "text": "[image omitted: aggregate provider image request limit exceeded]",
                }));
                continue;
            }
            self.image_bytes = raw_total;
            self.data_url_bytes = url_total;
            let encoded = STANDARD.encode(&image.data);
            content.push(json!({
                "type": "input_image",
                "image_url": format!("data:{};base64,{encoded}", image.media_type.mime_type()),
                "detail": image.detail,
            }));
        }
        Value::Array(content)
    }
}
