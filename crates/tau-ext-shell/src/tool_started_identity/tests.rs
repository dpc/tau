use tau_proto::{CborValue, PromptOriginator, ToolStarted};

use super::{ToolStartedIdentity, ownership_probe};

/// Executes the former clone-first ownership shape as a differential
/// oracle.
fn legacy_clone_first(
    started: ToolStarted,
    local_name: tau_proto::ToolName,
) -> (ToolStarted, usize) {
    let _wire = started.clone();
    let mut local = started;
    local.tool_name = local_name;
    let _lock_wait = local.clone();
    (local, 2)
}

/// The executable former scheduler shape performs its wire and lock-wait
/// clones, while split/reassembly preserves the same local invocation.
#[test]
fn split_reassembly_matches_executable_clone_first_scheduler_shape() {
    let call_id = "ownership-differential";
    let started = ToolStarted {
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new("prefixed_shell"),
        arguments: CborValue::Map(vec![(
            CborValue::Text("command".to_owned()),
            CborValue::Text("x".repeat(1024 * 1024)),
        )]),
        agent_id: tau_proto::AgentId::parse("ownership-agent").expect("agent id"),
        originator: PromptOriginator::User,
        invocation_policy: Default::default(),
    };
    let (legacy, legacy_argument_clones) =
        legacy_clone_first(started.clone(), tau_proto::ToolName::new("shell"));
    ownership_probe::start(call_id);
    let (identity, arguments) =
        ToolStartedIdentity::split(started, tau_proto::ToolName::new("shell"));
    let current = identity.clone().into_local_started(arguments);
    let work = ownership_probe::finish(call_id);

    assert_eq!(current, legacy);
    assert_eq!(legacy_argument_clones, 2);
    assert_eq!(work.argument_clones, 0);
    assert_eq!(work.identity_clones, 1);
    assert_eq!(work.ingress_text_ptr, work.execution_text_ptr);
}

/// Error and deferred-workdir ownership retain terminal correlation without
/// retaining the discarded argument tree.
#[test]
fn identity_retains_wire_terminal_fields_without_arguments() {
    let payload = "z".repeat(8 * 1024 * 1024);
    let started = ToolStarted {
        call_id: tau_proto::ToolCallId::new("identity-terminal"),
        tool_name: tau_proto::ToolName::new("replace"),
        arguments: CborValue::Text(payload),
        agent_id: tau_proto::AgentId::parse("identity-agent").expect("agent id"),
        originator: PromptOriginator::User,
        invocation_policy: Default::default(),
    };
    let (identity, arguments) =
        ToolStartedIdentity::split(started, tau_proto::ToolName::new("edit"));
    drop(arguments);

    assert_eq!(identity.call_id.as_str(), "identity-terminal");
    assert_eq!(identity.wire_tool_name.as_str(), "replace");
    assert_eq!(identity.local_tool_name.as_str(), "edit");
}
