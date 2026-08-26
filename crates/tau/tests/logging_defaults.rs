//! Subprocess coverage for bundled component logging-filter ownership.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use tau_proto::{HarnessOutputMessage, HarnessOutputWriter};

/// Run one configured dummy component with an exact TAU_LOG environment state.
fn dummy_stderr(filter: Option<&str>) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tau"));
    command
        .arg("component")
        .arg("ext-test-dummy")
        .env_remove("TAU_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(filter) = filter {
        command.env("TAU_LOG", filter);
    }
    let mut child = command.spawn().expect("spawn bundled dummy component");
    let mut input = HarnessOutputWriter::new(child.stdin.take().expect("component stdin"));
    input
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({})),
            instance_name: tau_proto::ExtensionName::parse("test-dummy").expect("extension name"),
            state_dir: None,
            secrets: BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure dummy component");
    drop(input);
    let output = child.wait_with_output().expect("wait for dummy component");
    assert!(output.status.success());
    String::from_utf8(output.stderr).expect("component stderr is UTF-8")
}

/// Proves absent and malformed filters use scoped info, explicit warn replaces
/// it, and an explicit component target can opt back into private diagnostics.
#[test]
fn bundled_component_filter_fallback_and_replacement_are_exact() {
    let absent = dummy_stderr(None);
    let invalid = dummy_stderr(Some("[invalid directive"));
    let warn = dummy_stderr(Some("warn"));
    let off = dummy_stderr(Some(""));
    let debug = dummy_stderr(Some("tau_ext_test_dummy=debug,warn"));

    assert_eq!(absent.matches("test dummy configured").count(), 1);
    assert_eq!(invalid.matches("test dummy configured").count(), 1);
    assert!(!warn.contains("test dummy configured"));
    assert!(off.is_empty());
    assert_eq!(debug.matches("test dummy configured").count(), 1);
}
