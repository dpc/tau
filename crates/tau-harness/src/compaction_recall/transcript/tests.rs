use super::*;

/// Tool arguments and rendered results cross the byte threshold independently;
/// raw payloads, reasoning, and transport sidecars never leak into the bundle.
#[test]
fn searchable_tool_payloads_are_exact_and_private_duplicates_are_omitted() {
    let directory = tempfile::tempdir().expect("staging directory");
    fs::create_dir(directory.path().join("payloads")).expect("payload directory");
    let mut writer = Transcript {
        directory: directory.path(),
        output: File::create(directory.path().join("transcript.txt")).expect("transcript file"),
        remaining: 1024 * 1024,
        record: 0,
        cancelled: &|| false,
    };
    let arguments = format!("{{\"script\":\"{}\"}}", "é".repeat(600));
    writer
        .item(&ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: "call-tools".into(),
            name: tau_proto::ToolName::new("shell_command"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Null,
            raw_arguments_json: Some(arguments.clone()),
            responses_envelope: None,
        }))
        .expect("render arguments");
    let result = tau_proto::ToolResultItem {
        call_id: "call-tools".into(),
        tool_type: tau_proto::ToolType::Function,
        status: tau_proto::ToolResultStatus::Error {
            message: "failed".to_owned(),
        },
        output: tau_proto::ToolResponse {
            raw: tau_proto::CborValue::Text("private duplicate raw payload".to_owned()),
            headers: vec![],
            body: "result detail\n".repeat(100),
        },
        presentation: Default::default(),
        provider_content: vec![],
    };
    writer.result(&result).expect("render result");
    writer
        .item(&ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "private reasoning".to_owned(),
        }))
        .expect("omit reasoning");
    writer
        .item(&ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"compaction","encrypted_content":"private opaque bytes"}"#,
            )
            .expect("opaque provider item"),
        ))
        .expect("omit opaque payload");
    writer
        .payload("exact threshold", &"x".repeat(1024), 1024)
        .expect("exact threshold payload");
    writer
        .payload("controls", "hello\r@forged\u{1b}[2J", 1024)
        .expect("control payload");
    drop(writer);
    let text = fs::read_to_string(directory.path().join("transcript.txt")).expect("transcript");
    assert!(text.contains("omitted: reasoning"));
    assert!(text.contains("omitted: opaque provider compaction"));
    assert!(text.contains(&format!("| {}", "x".repeat(1024))));
    for absent in [
        "private duplicate",
        "private reasoning",
        "private opaque",
        "\r",
        "\u{1b}",
        "result detail",
    ] {
        assert!(!text.contains(absent));
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("payloads/000001.txt")).expect("arguments"),
        arguments
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("payloads/000002.txt")).expect("result"),
        result.render_provider_text()
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("payloads/000004.txt")).expect("controls"),
        "hello\r@forged\u{1b}[2J"
    );
    assert!(!directory.path().join("payloads/000003.txt").exists());
}
