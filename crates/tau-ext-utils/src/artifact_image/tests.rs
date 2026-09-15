//! Regression coverage for artifact image lifecycle helpers.

use std::sync::Arc;

use tau_proto::{
    CborValue, Event, ImageContent, ImageDetail, ImageMediaType, ToolResult, ToolResultContentPart,
    ToolResultKind, ToolType,
};

use super::{IMAGE_TOO_LARGE_MESSAGE, budget_prepared_result};

/// Ensures an image whose complete transient terminal exceeds the shared frame
/// cap becomes one byte-free error instead of failing the extension writer.
#[test]
fn oversized_prepared_image_becomes_byte_free_error() {
    let result = ToolResult {
        presentation: Default::default(),
        call_id: tau_proto::ToolCallId::new("oversized"),
        tool_name: tau_proto::ToolName::new("read_image"),
        tool_type: ToolType::Function,
        result: CborValue::Text("metadata".to_owned()),
        provider_content: vec![ToolResultContentPart::Image(ImageContent {
            media_type: ImageMediaType::Png,
            data: Arc::from(vec![0; tau_client::MAX_OUTBOUND_FRAME_BYTES as usize]),
            width: 1,
            height: 1,
            detail: ImageDetail::High,
        })],
        kind: ToolResultKind::Final,
        display: None,
        originator: Default::default(),
    };
    let error = budget_prepared_result(result)
        .expect("measure terminal")
        .expect_err("oversized image becomes error");
    assert_eq!(error.message, IMAGE_TOO_LARGE_MESSAGE);
    assert!(error.details.is_none());
    let message =
        tau_proto::HarnessInputMessage::emit_with_persist(Event::ToolErrorReported(error), false);
    assert!(
        tau_client::encoded_outbound_frame_bytes(&message).expect("measure error")
            <= tau_client::MAX_OUTBOUND_FRAME_BYTES
    );
}
