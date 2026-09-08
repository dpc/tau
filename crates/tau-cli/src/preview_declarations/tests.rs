use std::io::Cursor;

use tau_config::settings::HarnessSettings;

use super::*;

/// Build one configured shell fixture through the ordinary launch resolver.
fn extension(script: String) -> tau_harness::ExtensionConfig {
    let mut settings = HarnessSettings::built_in();
    settings.extensions.clear();
    let mut extensions = tau_harness::resolve_extensions_with_environment_and_cli_overrides(
        &settings,
        vec![tau_harness::builtin_extensions().remove(0)],
        &[],
        &[],
    )
    .expect("resolve fixture")
    .extensions;
    let mut extension = extensions.remove(0);
    extension.name = "inspection-fixture".to_owned();
    extension.role = None;
    extension.command = "sh".to_owned();
    extension.args = vec!["-c".to_owned(), script];
    extension.startup_timeout = Duration::from_millis(100);
    extension
}

/// Encode fixed fixture frames using the real protocol, not a CBOR imitation.
fn output_script(messages: &[HarnessInputMessage]) -> String {
    let octal: String = encoded_frames(messages)
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect();
    format!("printf '{octal}'; exec sleep 20")
}

/// Build the exact peer stream for pipe and in-memory admission tests.
fn encoded_frames(messages: &[HarnessInputMessage]) -> Vec<u8> {
    let mut writer = tau_proto::HarnessInputWriter::new(Vec::new());
    for message in messages {
        writer.write_message(message).expect("encode fixture");
    }
    writer.into_inner()
}

/// Supported identity for tool-only fixtures.
fn hello(supported: bool) -> HarnessInputMessage {
    HarnessInputMessage::Hello(tau_proto::Hello {
        declaration_inspection: supported,
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: "inspection-fixture".parse().expect("name"),
        client_kind: tau_proto::ClientKind::Tool,
        expected_session_id: None,
        capabilities: Vec::new(),
    })
}

/// Completion needs no Ready and cleanup must reap a child ignoring EOF.
#[test]
fn supported_inspection_finishes_and_reaps() {
    let extension = extension(output_script(&[
        hello(true),
        HarnessInputMessage::InspectionComplete(Default::default()),
    ]));
    let mut budget = MAX_COLLECTION_BYTES;
    let before = Instant::now();
    let result = collect(&extension, None, &mut budget);
    assert_eq!(result.outcome, Outcome::CompleteDeclarations);
    assert!(result.inventory.is_some());
    assert!(budget < MAX_COLLECTION_BYTES);
    assert!(before.elapsed() < Duration::from_secs(3));
}

/// Unsupported peers and incompatible majors cannot enter ordinary startup.
#[test]
fn inspection_rejects_unsupported_and_major_skew() {
    for (mut greeting, expected) in [
        (hello(false), Outcome::Unsupported),
        (hello(true), Outcome::ProtocolMismatch),
    ] {
        if expected == Outcome::ProtocolMismatch {
            let HarnessInputMessage::Hello(hello) = &mut greeting else {
                unreachable!();
            };
            hello.protocol_version.major += 1;
        }
        let mut budget = MAX_COLLECTION_BYTES;
        let result = collect(&extension(output_script(&[greeting])), None, &mut budget);
        assert_eq!(result.outcome, expected);
        assert!(result.inventory.is_none());
    }
}

/// A peer stalled before Hello cannot hold the caller in blocking decode.
#[test]
fn inspection_stalled_peer_has_finite_deadline() {
    let before = Instant::now();
    let mut budget = MAX_COLLECTION_BYTES;
    let result = collect(&extension("exec sleep 20".to_owned()), None, &mut budget);
    assert_eq!(result.outcome, Outcome::Deadline);
    assert!(before.elapsed() < Duration::from_secs(3));
}

/// Runtime messages cannot be interpreted as a declaration terminal.
#[test]
fn inspection_rejects_runtime_ready() {
    let mut budget = MAX_COLLECTION_BYTES;
    let result = collect(
        &extension(output_script(&[
            hello(true),
            HarnessInputMessage::Ready(Default::default()),
        ])),
        None,
        &mut budget,
    );
    assert_eq!(result.outcome, Outcome::InvalidProtocol);
}

/// Unsupported and skewed peers receive zero harness bytes, not even Configure.
#[test]
fn inspection_admission_never_falls_back_to_runtime_configure() {
    for supported in [false, true] {
        let mut greeting = hello(supported);
        if supported {
            let HarnessInputMessage::Hello(hello) = &mut greeting else {
                unreachable!();
            };
            hello.protocol_version.major += 1;
        }
        let mut output = Vec::new();
        let mut budget = MAX_COLLECTION_BYTES;
        let result = inspect_connection(
            Cursor::new(encoded_frames(&[greeting])),
            &mut output,
            &extension(String::new()),
            None,
            &mut budget,
            Instant::now() + MAX_EXTENSION_TIME,
        );
        assert!(result.is_err());
        assert!(output.is_empty());
    }
}

/// Supplied config and prefix survive lowering; state and secrets never do.
#[test]
fn inspection_configure_contains_only_permitted_inputs() {
    let mut extension = extension(String::new());
    extension.config = serde_json::json!({"enabled": true});
    extension.tool_prefix = Some(tau_proto::ToolNamePrefix::parse("fixture").expect("prefix"));
    let mut output = Vec::new();
    let mut budget = MAX_COLLECTION_BYTES;
    inspect_connection(
        Cursor::new(encoded_frames(&[
            hello(true),
            HarnessInputMessage::InspectionComplete(Default::default()),
        ])),
        &mut output,
        &extension,
        None,
        &mut budget,
        Instant::now() + MAX_EXTENSION_TIME,
    )
    .expect("inspection");
    let mut reader = tau_proto::HarnessOutputReader::new(output.as_slice());
    let Some(HarnessOutputMessage::Configure(configure)) =
        reader.read_message().expect("configure")
    else {
        panic!("exact inspection purpose required");
    };
    assert_eq!(
        configure.purpose,
        tau_proto::ConfigurePurpose::DeclarationInspection
    );
    assert_eq!(configure.tool_prefix, extension.tool_prefix);
    assert_eq!(
        configure
            .config
            .deserialized::<serde_json::Value>()
            .expect("config"),
        extension.config,
    );
    assert!(configure.state_dir.is_none());
    assert!(configure.secrets.is_empty());
    assert!(configure.settings_files.is_empty());
    assert!(reader.read_message().expect("no lifecycle").is_none());
}

/// Malformed and oversized input consumes the shared budget even on failure.
#[test]
fn inspection_invalid_input_is_bounded_and_charged() {
    let mut budget = 32;
    let result = inspect_connection(
        Cursor::new(vec![0xff; 1024]),
        Vec::new(),
        &extension(String::new()),
        None,
        &mut budget,
        Instant::now() + MAX_EXTENSION_TIME,
    );
    assert!(result.is_err());
    assert_eq!(budget, 0);
}

/// Child-selected kind cannot grant profile input or evade configured omission.
#[test]
fn inspection_role_mismatch_sends_no_configuration() {
    for provider in [false, true] {
        let mut extension = extension(String::new());
        extension.role = provider.then(|| "provider".to_owned());
        let mut greeting = hello(true);
        let HarnessInputMessage::Hello(hello) = &mut greeting else {
            unreachable!();
        };
        hello.client_kind = if provider {
            tau_proto::ClientKind::Tool
        } else {
            tau_proto::ClientKind::Provider
        };
        let mut output = Vec::new();
        let mut budget = MAX_COLLECTION_BYTES;
        let result = inspect_connection(
            Cursor::new(encoded_frames(&[greeting])),
            &mut output,
            &extension,
            None,
            &mut budget,
            Instant::now() + MAX_EXTENSION_TIME,
        );
        assert!(matches!(result, Err(Outcome::InvalidProtocol)));
        assert!(output.is_empty());
        assert_eq!(
            empty_preview(&extension, Outcome::InvalidProtocol).state_settings_omitted,
            provider
        );
    }
}

/// Non-provider does not mean Tool: other participant kinds are also rejected.
#[test]
fn inspection_rejects_non_tool_participant_kinds() {
    for kind in [
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ] {
        let mut greeting = hello(true);
        let HarnessInputMessage::Hello(hello) = &mut greeting else {
            unreachable!();
        };
        hello.client_kind = kind;
        let mut output = Vec::new();
        let mut budget = MAX_COLLECTION_BYTES;
        let result = inspect_connection(
            Cursor::new(encoded_frames(&[greeting])),
            &mut output,
            &extension(String::new()),
            None,
            &mut budget,
            Instant::now() + MAX_EXTENSION_TIME,
        );
        assert!(matches!(result, Err(Outcome::InvalidProtocol)));
        assert!(output.is_empty());
    }
}

/// A peer which announces support but never reads Configure cannot block
/// writes.
#[test]
fn inspection_backpressured_configure_has_finite_deadline() {
    let mut extension = extension(output_script(&[hello(true)]));
    extension.config = serde_json::json!({"large": "x".repeat(1024 * 1024)});
    let mut budget = MAX_COLLECTION_BYTES;
    let before = Instant::now();
    let result = collect(&extension, None, &mut budget);
    assert_eq!(result.outcome, Outcome::Deadline);
    assert!(before.elapsed() < Duration::from_secs(3));
}

/// Small typed declaration fixture, independent from runtime registration.
fn tool(name: &str, alias: Option<&str>) -> tau_proto::ToolRegistrationDeclared {
    serde_json::from_value(serde_json::json!({
        "tool": {
            "name": name,
            "model_visible_name": alias,
            "tool_type": "function"
        }
    }))
    .expect("tool")
}

/// Exact route fixture keeps collision tests about names, not capabilities.
fn model(id: &str) -> tau_proto::ProviderModelInfo {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "context_window": 1000,
        "verbosities": [],
        "thinking_summaries": []
    }))
    .expect("model")
}

/// Keep origins stamped independently from the peer's names.
fn declared(instance: &str, tools: Vec<tau_proto::ToolRegistrationDeclared>) -> ExtensionPreview {
    ExtensionPreview {
        instance: instance.to_owned(),
        outcome: Outcome::CompleteDeclarations,
        inventory: Some(tau_proto::InspectionComplete {
            tools,
            ..Default::default()
        }),
        state_settings_omitted: false,
    }
}

/// Tool internals and aliases share a namespace, but model routes do not.
#[test]
fn inspection_collisions_are_typed_sorted_and_attributed() {
    let mut preview = Preview::new(false);
    preview.extensions = vec![
        declared(
            "z-origin",
            vec![tool("z", Some("alias")), tool("same", Some("same"))],
        ),
        declared(
            "a-origin",
            vec![tool("same", None), tool("alias", None), tool("z", None)],
        ),
    ];
    for entry in &mut preview.extensions {
        entry.inventory.as_mut().expect("inventory").providers =
            vec![tau_proto::InspectionProviderModels {
                models: vec![model("route/shared")],
            }];
    }
    preview.finalize();
    let value = serde_json::to_value(&preview).expect("report");
    assert_eq!(value["extensions"][0]["instance"], "a-origin");
    assert_eq!(value["extensions"][1]["instance"], "z-origin");
    assert_eq!(
        value["extensions"][1]["inventory"]["tools"][0]["tool"]["name"],
        "z"
    );
    assert_eq!(
        value["collisions"],
        serde_json::json!([
            {"kind": "tool", "name": "alias"},
            {"kind": "tool", "name": "same"},
            {"kind": "tool", "name": "z"},
            {"kind": "model", "name": "route/shared"}
        ])
    );
    assert!(!preview.complete());
}

/// Same-slot aliases and distinct routes are clean; every closed failure blocks
/// success even when all surviving inventories are empty.
#[test]
fn inspection_complete_decision_preserves_empty_and_incomplete_controls() {
    let mut preview = Preview::new(false);
    assert!(preview.complete());
    preview
        .extensions
        .push(declared("origin", vec![tool("same", Some("same"))]));
    preview.finalize();
    assert!(preview.collisions.is_empty());
    assert!(preview.complete());
    preview.resolution_incomplete = true;
    assert!(!preview.complete());
    preview.resolution_incomplete = false;
    for outcome in [
        Outcome::Partial,
        Outcome::Unsupported,
        Outcome::ProtocolMismatch,
        Outcome::InvalidProtocol,
        Outcome::Unavailable,
        Outcome::Deadline,
        Outcome::CleanupFailed,
        Outcome::Limit,
    ] {
        preview.extensions[0].outcome = outcome;
        assert!(!preview.complete(), "{outcome:?}");
    }
}

/// Visible aliases collide even with distinct internal routes; model names stay
/// independent from the tool namespace.
#[test]
fn inspection_alias_and_model_namespace_controls() {
    let mut preview = Preview::new(false);
    let mut entry = declared(
        "origin",
        vec![tool("one", Some("alias")), tool("two", Some("alias"))],
    );
    entry.inventory.as_mut().expect("inventory").providers =
        vec![tau_proto::InspectionProviderModels {
            models: vec![model("route/one"), model("route/two")],
        }];
    preview.extensions.push(entry);
    preview.finalize();
    assert_eq!(
        preview.collisions,
        vec![Collision {
            kind: CollisionKind::Tool,
            name: "alias".to_owned()
        }]
    );
    preview.extensions[0]
        .inventory
        .as_mut()
        .expect("inventory")
        .tools
        .pop();
    preview.finalize();
    assert!(preview.complete());
}
