use std::io::Cursor;

use tau_proto::{Configure, ConfigurePurpose, HarnessInputMessage, HarnessOutputMessage};

use super::*;

/// Minimal extension makes runtime construction/Configure observable.
struct Runtime;

impl TauExtension for Runtime {
    type State = usize;

    fn name(&self) -> &'static str {
        "inspection-test"
    }

    fn register(self, builder: &mut crate::ExtensionBuilder<usize>) {
        builder.configure_raw(|cx| {
            *cx.state += 1;
            Ok(())
        });
    }
}

/// Build non-secret startup inputs without touching a filesystem.
fn configure(purpose: ConfigurePurpose) -> Configure {
    Configure {
        purpose,
        config: tau_proto::CborValue::Null,
        instance_name: "inspection-test"
            .parse()
            .expect("valid inspection test fixture"),
        tool_prefix: None,
        state_dir: None,
        secrets: Default::default(),
        settings_files: Default::default(),
    }
}

/// Encode a complete harness conversation to exercise buffered continuation.
fn input(configure: Configure) -> Cursor<Vec<u8>> {
    let mut writer = tau_proto::HarnessOutputWriter::new(Vec::new());
    writer
        .write_message(&HarnessOutputMessage::Configure(configure))
        .expect("valid inspection test fixture");
    writer
        .write_message(&HarnessOutputMessage::Disconnect(Default::default()))
        .expect("valid inspection test fixture");
    Cursor::new(writer.into_inner())
}

/// Construct the opt-in peer's ordinary identity.
fn hello() -> tau_proto::Hello {
    tau_proto::Hello {
        declaration_inspection: false,
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: "inspection-test"
            .parse()
            .expect("valid inspection test fixture"),
        client_kind: tau_proto::ClientKind::Tool,
        expected_session_id: None,
        capabilities: Vec::new(),
    }
}

/// Read collected frames without launching any operational extension.
fn frames(bytes: Vec<u8>) -> Vec<HarnessInputMessage> {
    let mut reader = tau_proto::HarnessInputReader::new(Cursor::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader
        .read_message()
        .expect("valid inspection test fixture")
    {
        frames.push(frame);
    }
    frames
}

/// Inspection must terminate before any runtime closure, declaration builder,
/// worker, ordinary handler or runtime Ready can run.
#[test]
fn inspection_finishes_before_runtime_construction() {
    let mut output = Vec::new();
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::DeclarationInspection)),
        &mut output,
        hello(),
        |_| Ok(Default::default()),
    )
    .expect("valid inspection test fixture");
    assert!(prepared.is_none());
    assert!(matches!(
        frames(output).as_slice(),
        [
            HarnessInputMessage::Hello(tau_proto::Hello {
                declaration_inspection: true,
                ..
            }),
            HarnessInputMessage::InspectionComplete(_),
        ]
    ));
}

/// Ordinary continuation consumes Configure once and preserves a following
/// buffered Disconnect instead of losing it or writing a duplicate Hello.
#[test]
fn ordinary_continuation_preserves_single_handshake() {
    let mut output = Vec::new();
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::Runtime)),
        &mut output,
        hello(),
        |_| panic!("ordinary startup must not invoke inspection"),
    )
    .expect("valid inspection test fixture")
    .expect("valid inspection test fixture");
    let state = prepared
        .run(Runtime, |runner, reader, writer| {
            runner.run(reader, writer, 10)
        })
        .expect("identity remains bound")
        .expect("ordinary runtime succeeds");
    assert_eq!(state, 11);
    assert!(matches!(
        frames(output).as_slice(),
        [HarnessInputMessage::Hello(_), HarnessInputMessage::Ready(_),]
    ));
}

/// Pure declaration failure must not echo secret-bearing configuration errors
/// or substitute operational startup for incomplete inspection.
#[test]
fn inspection_rejection_is_sanitized_and_terminal() {
    let mut output = Vec::new();
    prepare_inspection(
        input(configure(ConfigurePurpose::DeclarationInspection)),
        &mut output,
        hello(),
        |_| Err(ClientError::handler("secret-canary")),
    )
    .expect("valid inspection test fixture");
    let output = frames(output);
    let HarnessInputMessage::InspectionComplete(result) = &output[1] else {
        panic!()
    };
    assert_eq!(
        result.gaps,
        [tau_proto::InspectionGap::InvalidConfiguration]
    );
    assert!(!format!("{output:?}").contains("secret-canary"));
}

/// Inspection cannot receive state paths or authorized secret values even when
/// a caller constructs an invalid inspection Configure by hand.
#[test]
fn inspection_rejects_operational_inputs_before_callback() {
    for state_path in [true, false] {
        let mut configure = configure(ConfigurePurpose::DeclarationInspection);
        if state_path {
            configure.state_dir = Some("must-not-open".into());
        } else {
            configure.secrets.insert(
                "credential".to_owned(),
                tau_proto::SecretValue::new("secret-canary"),
            );
        }
        let result = prepare_inspection(input(configure), Vec::new(), hello(), |_| {
            panic!("invalid inputs must not reach declaration callback")
        });
        assert!(result.is_err());
    }
}

/// EOF before purpose selection must be a clean exit without invoking any
/// declaration callback or constructing a normal connection.
#[test]
fn early_eof_never_initializes() {
    assert!(
        prepare_inspection(Cursor::new(Vec::new()), Vec::new(), hello(), |_| {
            panic!("no purpose selected")
        })
        .expect("valid inspection test fixture")
        .is_none()
    );
}

/// Factory-based detached startup must reuse the selected ordinary Configure
/// rather than hang waiting for a second handshake.
#[test]
fn detached_factory_resumes_ordinary_startup() {
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::Runtime)),
        Vec::new(),
        hello(),
        |_| panic!("runtime purpose"),
    )
    .expect("valid inspection test fixture")
    .expect("valid inspection test fixture");
    assert_eq!(
        prepared
            .run(Runtime, |runner, reader, writer| {
                runner.run_detached_writer_with_state(reader, writer, |_| 20)
            })
            .expect("identity remains bound")
            .expect("detached runtime succeeds"),
        21
    );
}

/// Manual state factories and extension-data clients remain unreachable on
/// inspection, but ordinary manual startup still configures state once.
#[test]
fn manual_factory_resumes_ordinary_startup() {
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::Runtime)),
        Vec::new(),
        hello(),
        |_| panic!("runtime purpose"),
    )
    .expect("valid inspection test fixture")
    .expect("valid inspection test fixture");
    let state = prepared
        .run(Runtime, |runner, reader, writer| {
            let mut runtime =
                runner.start_manual_loop_with_extension_data_state(reader, writer, |_, _| 30)?;
            assert!(matches!(
                runtime.recv()?,
                crate::ManualRuntimeInput::Message(HarnessOutputMessage::Disconnect(_))
            ));
            runtime.finish()
        })
        .expect("identity remains bound")
        .expect("manual runtime succeeds");
    assert_eq!(state, 31);
}

/// Deferred manual loops must receive the selected Configure before buffered
/// runtime input, preserving caller ownership of ordinary Ready emission.
#[test]
fn deferred_manual_startup_preserves_configure_order() {
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::Runtime)),
        Vec::new(),
        hello(),
        |_| panic!("runtime purpose"),
    )
    .expect("valid inspection test fixture")
    .expect("valid inspection test fixture");
    let state = prepared
        .run(Runtime, |runner, reader, writer| {
            let mut runtime =
                runner.start_manual_loop_deferred_startup_with_state(reader, writer, |_| 40)?;
            let crate::ManualRuntimeInput::Message(message) = runtime.recv()? else {
                panic!()
            };
            assert!(matches!(message, HarnessOutputMessage::Configure(_)));
            runtime.dispatch_one(message)?;
            runtime.startup_ready(None)?;
            assert!(matches!(
                runtime.recv()?,
                crate::ManualRuntimeInput::Message(HarnessOutputMessage::Disconnect(_))
            ));
            runtime.finish()
        })
        .expect("identity remains bound")
        .expect("deferred runtime succeeds");
    assert_eq!(state, 41);
}

/// Build one logical registration without runtime handles so normal and
/// inspection code can share declaration construction.
fn registration() -> tau_proto::ToolRegistrationDeclared {
    tau_proto::ToolRegistrationDeclared {
        tool: tau_proto::ToolSpec {
            provider_scope: None,
            name: tau_proto::ToolName::new("lookup"),
            model_visible_name: Some(tau_proto::ToolName::new("lookup_alias")),
            description: Some("literal lookup is not renamed".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![],
            enabled_by_default: true,
            background_support: None,
            examples: vec![],
        },
        tool_group: Some(tau_proto::ToolGroup {
            name: tau_proto::ToolGroupName::new("tools"),
            prompt_fragment: None,
        }),
        prompt_fragment: None,
    }
}

/// Inspection must use the existing structural scope mapping for names,
/// aliases and groups, leaving prose untouched.
#[test]
fn pure_declarations_use_the_normal_structural_scope() {
    let mut config = configure(ConfigurePurpose::DeclarationInspection);
    config.tool_prefix =
        Some(tau_proto::ToolNamePrefix::parse("preview").expect("valid inspection test fixture"));
    let expected = crate::ToolNameScope::from_configure(&config)
        .scope_registration(registration())
        .expect("valid inspection test fixture");
    let mut output = Vec::new();
    prepare_inspection(input(config), &mut output, hello(), |_| {
        Ok(tau_proto::InspectionComplete {
            tools: vec![registration()],
            ..Default::default()
        })
    })
    .expect("valid inspection test fixture");
    let output = frames(output);
    let HarnessInputMessage::InspectionComplete(result) = &output[1] else {
        panic!()
    };
    assert_eq!(result.tools, [expected]);
}

/// A completion frame must be measured before writing so an excessive pure
/// declaration cannot defeat the collector's bounded single-result contract.
#[test]
fn oversized_completion_is_not_partially_written() {
    let mut output = Vec::new();
    let result = prepare_inspection(
        input(configure(ConfigurePurpose::DeclarationInspection)),
        &mut output,
        hello(),
        |_| {
            let mut tool = registration();
            tool.tool.description = Some("x".repeat(crate::MAX_OUTBOUND_FRAME_BYTES as usize));
            Ok(tau_proto::InspectionComplete {
                tools: vec![tool],
                ..Default::default()
            })
        },
    );
    assert!(matches!(result, Err(ClientError::Overloaded)));
    assert!(matches!(
        frames(output).as_slice(),
        [HarnessInputMessage::Hello(_)]
    ));
}

/// An admitted name, kind or capability set cannot be silently replaced by the
/// ordinary runner while suppressing its Hello; reject before runtime creation.
#[test]
fn ordinary_continuation_is_bound_to_advertised_identity() {
    for mismatch in 0..3 {
        let mut hello = hello();
        match mismatch {
            0 => hello.client_name = "other-extension".parse().expect("test name"),
            1 => hello.client_kind = tau_proto::ClientKind::Provider,
            _ => hello
                .capabilities
                .push(tau_proto::PeerCapability::ActionProvider),
        }
        let prepared = prepare_inspection(
            input(configure(ConfigurePurpose::Runtime)),
            Vec::new(),
            hello,
            |_| panic!("runtime purpose"),
        )
        .expect("prepare")
        .expect("ordinary connection");
        let result = prepared.run(Runtime, |_, _, _| {
            panic!("mismatched identity must not construct runtime state")
        });
        assert!(result.is_err());
    }
}

/// Existing production helpers may use their own error type or non-Result
/// return values; bootstrap identity validation must not force error
/// conversion.
#[test]
fn ordinary_continuation_accepts_arbitrary_callback_output() {
    let prepared = prepare_inspection(
        input(configure(ConfigurePurpose::Runtime)),
        Vec::new(),
        hello(),
        |_| panic!("runtime purpose"),
    )
    .expect("prepare")
    .expect("ordinary connection");
    assert_eq!(
        prepared
            .run(Runtime, |_, _, _| "normal-only")
            .expect("bound identity"),
        "normal-only"
    );
}
