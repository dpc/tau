//! Owner-bound cancellation metadata checks with production builtins and host.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tau_e2e_tests::{DeterministicFixture, ScenarioV1};
use tau_harness::{
    AgentToolCall, EmbeddedOptions, HarnessError, InternalToolHandler, InternalToolHost,
    run_embedded_message_with_options_and_internal_tools,
};
use tau_proto::{CborValue, Event, ToolName, ToolSpec, ToolStarted, ToolType, ToolUseStatus};

/// Seeds owner-scoped routing only; the real builtin handles each cancellation
/// and the real host publishes the resulting progress and terminal events.
struct CancelProbe {
    /// Production builtin owner, including its cancellation bookkeeping.
    builtin: Arc<dyn InternalToolHandler>,
    /// Committed events observed through normal handler broadcast.
    events: Mutex<Vec<Event>>,
}

impl InternalToolHandler for CancelProbe {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.builtin.tool_specs()
    }

    fn handles(&self, name: &ToolName) -> bool {
        self.builtin.handles(name)
    }

    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        self.events.lock().expect("events").push(event.clone());
        self.builtin.handle_event(host, event)?;
        let Event::AgentStarted(started) = event else {
            return Ok(());
        };
        let target = AgentToolCall {
            call_ref: None,
            id: "cancel-display-target".into(),
            name: ToolName::new("shell_command"),
            tool_type: ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        };
        host.ensure_internal_tool_tracking(&started.agent_id, &target, &target.name);
        assert!(host.is_running_cancellable_tool_call_for(&started.agent_id, &target.id));
        for (id, target_id) in [
            ("cancel-display-accepted", target.id.as_str()),
            ("cancel-display-rejected", "cancel-display-unknown"),
        ] {
            let call = AgentToolCall {
                call_ref: None,
                id: id.into(),
                name: ToolName::new("cancel"),
                tool_type: ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("tool_call_id".to_owned()),
                    CborValue::Text(target_id.to_owned()),
                )]),
            };
            host.ensure_internal_tool_tracking(&started.agent_id, &call, &call.name);
            self.builtin.handle_event(
                host,
                &Event::ToolStarted(ToolStarted {
                    call_id: call.id,
                    tool_name: call.name,
                    arguments: call.arguments,
                    agent_id: started.agent_id.clone(),
                    originator: Default::default(),
                    invocation_policy: Default::default(),
                }),
            )?;
        }
        // Acknowledgement must leave the owned target running. Settle the
        // seeded target separately so it cannot interfere with the
        // fixture's text turn.
        assert!(host.is_running_cancellable_tool_call_for(&started.agent_id, &target.id));
        host.finish_tool_with_cbor_result(
            &started.agent_id,
            target.id,
            target.name,
            target.tool_type,
            CborValue::Text("target settled separately".to_owned()),
            None,
        );
        Ok(())
    }
}

/// Prevents terminal cancel rows from dropping the target or conflating request
/// acceptance with target termination. Exercises the actual builtin with a real
/// host; only target/start routing is seeded, without a new fake-provider
/// grammar.
#[test]
fn builtin_cancel_terminals_retain_target_without_claiming_termination()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new(
        "builtin_cancel_terminals_retain_target_without_claiming_termination",
        &ScenarioV1::text_v1("check cancellation metadata", "metadata checked"),
        env!("CARGO_BIN_EXE_tau-e2e-fake-provider"),
        None,
    )?;
    let probe = Arc::new(CancelProbe {
        builtin: tau_harness_tools::builtin_handlers().remove(0),
        events: Mutex::new(Vec::new()),
    });
    run_embedded_message_with_options_and_internal_tools(
        fixture.harness_state_dir(),
        "deterministic-e2e-session",
        "check cancellation metadata",
        EmbeddedOptions::builder()
            .dirs(tau_config::settings::TauDirs {
                config_dir: Some(fixture.config_dir().to_owned()),
                state_dir: Some(fixture.state_dir().to_owned()),
            })
            .runtime_dir(fixture.runtime_dir().to_owned())
            .ignore_startup_environment(true)
            .allowed_extensions(BTreeSet::from(["e2e-fake-provider".parse()?]))
            .build(),
        vec![probe.clone()],
    )?;
    let events = probe.events.lock().expect("events");
    let results = events
        .iter()
        .filter_map(|event| match event {
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "cancel-display-accepted" =>
            {
                Some(result)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [result] = results.as_slice() else {
        panic!("expected one acknowledgement: {results:?}");
    };
    assert_eq!(
        result.result,
        CborValue::Map(vec![(
            CborValue::Text("status".to_owned()),
            CborValue::Text("ok".to_owned()),
        )])
    );
    let display = result.display.as_ref().expect("accepted display");
    assert_eq!(display.args, "cancel-display-target");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
    assert!(display.payload.is_none());
    let errors = events
        .iter()
        .filter_map(|event| match event {
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "cancel-display-rejected" =>
            {
                Some(error)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [error] = errors.as_slice() else {
        panic!("expected one rejection: {errors:?}");
    };
    let display = error.display.as_ref().expect("rejected display");
    assert_eq!(display.args, "cancel-display-unknown");
    assert_eq!(display.status, ToolUseStatus::Error);
    assert_eq!(display.status_text, "Unknown tool call id");
    assert!(display.payload.is_none());
    assert_eq!(events.iter().filter(|event| matches!(event,
        Event::ToolCancelRequest(request) if request.target_call_id.as_str() == "cancel-display-target"
    )).count(), 1);
    assert!(!events.iter().any(|event| matches!(event,
        Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancel-display-target"
    )));
    fixture.assert_consumed()?;
    Ok(())
}
