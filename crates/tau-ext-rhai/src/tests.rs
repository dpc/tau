use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tau_proto::{
    CborValue, Configure, Event, EventSelector, HarnessInputMessage, HarnessInputReader,
    HarnessOutputMessage, HarnessOutputWriter, InterceptAction, InterceptRequest,
    InterceptionPriority, UnixMicros,
};

use super::*;

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("writer mutex").clone()
    }

    fn into_bytes(self) -> Vec<u8> {
        Arc::try_unwrap(self.0)
            .expect("single writer reference")
            .into_inner()
            .expect("writer mutex")
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("writer mutex").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_script(dir: &tempfile::TempDir, source: &str) -> std::path::PathBuf {
    let path = dir.path().join("hook.rhai");
    std::fs::write(&path, source).expect("write script");
    path
}

fn configure_with_script(path: &Path) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        instance_name: None,
        config: CborValue::Map(vec![(
            CborValue::Text("script".to_owned()),
            CborValue::Text(path.display().to_string()),
        )]),
        state_dir: None,
        secrets: BTreeMap::new(),
    })
}

fn empty_configure() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        instance_name: None,
        config: CborValue::Map(Vec::new()),
        state_dir: None,
        secrets: BTreeMap::new(),
    })
}

fn configure_with_script_and_extra(
    path: &Path,
    mut extra: Vec<(CborValue, CborValue)>,
) -> HarnessOutputMessage {
    let mut config = vec![(
        CborValue::Text("script".to_owned()),
        CborValue::Text(path.display().to_string()),
    )];
    config.append(&mut extra);
    HarnessOutputMessage::Configure(Configure {
        instance_name: None,
        config: CborValue::Map(config),
        state_dir: None,
        secrets: BTreeMap::new(),
    })
}

fn prompt_event(text: &str) -> Event {
    Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: text.to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        display_name: None,
        ctx_id: None,
    })
}

fn tool_started(tool_name: &str, args: CborValue) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("call_1"),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments: args,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}
fn run_frames(input_frames: &[HarnessOutputMessage]) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    for frame in input_frames {
        writer.write_message(frame).expect("write input frame");
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    run(Cursor::new(input), output.clone()).expect("run rhai extension");

    let mut reader = HarnessInputReader::new(Cursor::new(output.into_bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output frame") {
        frames.push(frame);
    }
    frames
}

fn frames_from_bytes_lossy(bytes: Vec<u8>) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(Cursor::new(bytes));
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = reader.read_message() {
        frames.push(frame);
    }
    frames
}

fn emitted_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn emitted_transient(message: &HarnessInputMessage) -> Option<bool> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.transient),
        _ => None,
    }
}

fn tool_result_output(frames: &[HarnessInputMessage]) -> &str {
    for frame in frames {
        let Some(Event::ToolResult(result)) = emitted_event(frame) else {
            continue;
        };
        let CborValue::Map(fields) = &result.result else {
            continue;
        };
        for (key, value) in fields {
            if let (CborValue::Text(key), CborValue::Text(output)) = (key, value)
                && key == "output"
            {
                return output;
            }
        }
    }
    panic!("tool result output");
}

fn tool_result_has_output(frame: &HarnessInputMessage, expected: &str) -> bool {
    let Some(Event::ToolResult(result)) = emitted_event(frame) else {
        return false;
    };
    let CborValue::Map(fields) = &result.result else {
        return false;
    };
    fields.iter().any(|(key, value)| {
        matches!(
            (key, value),
            (CborValue::Text(key), CborValue::Text(output))
                if key == "output" && output == expected
        )
    })
}

fn setsid_available() -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn no_configure_exits_after_hello_only() {
    // Rhai uses tau-client deferred startup: before the first Configure, it must
    // send only Hello and must not leak script-dependent startup declarations or
    // inert Ready frames.
    let frames = run_frames(&[]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert_eq!(frames.len(), 1);
}

#[test]
fn bootstrap_waits_for_configure_then_uses_init_plan() {
    // The Rhai extension must not send subscriptions until it has the
    // configured script, because the script decides its own event interest.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                    ready_message: "demo ready",
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Ready(_)));
    let HarnessInputMessage::Ready(ready) = &frames[2] else {
        panic!("expected ready");
    };
    assert_eq!(ready.message.as_deref(), Some("demo ready"));
    assert_eq!(frames.len(), 3);
}

#[test]
fn no_op_init_uses_default_ready_message() {
    // A script can define init for future use without returning a map;
    // unit means the same as an absent init hook.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(&dir, "fn init(config) {}\n");

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    let ready = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Ready(ready) => Some(ready),
            _ => None,
        })
        .expect("ready frame");
    assert_eq!(ready.message.as_deref(), Some("rhai ready"));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
}

#[test]
fn init_host_emit_failure_is_inert() {
    // Host emit helpers are intentionally unavailable during init so a
    // script that fails init cannot leak pre-Ready side effects.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                tau_info("should not leak");
                fail;
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("should not leak")
    )));
}

#[test]
fn shell_spawn_is_unavailable_during_init_and_has_no_side_effect() {
    // Init must remain an inert staging phase: a script that tries to spawn a
    // trusted shell command during init gets ConfigError and cannot leak host
    // side effects before failing configuration.
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("marker");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{
                    shell_spawn("touch {}", #{{ timeout: 5 }});
                }}
            "#,
            marker.display()
        ),
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolRegister(_))))
    );
    assert!(!marker.exists());
}
#[test]
fn start_runs_after_ready_with_host_functions() {
    // `init` remains a pure planning phase, but `start` is an explicit
    // side-effect phase that runs after host functions are registered.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ ready_message: "demo ready" };
            }
            fn start(config) {
                tau_info(`started with ${config.vars.greeting}`);
            }
        "#,
    );
    let configure = HarnessOutputMessage::Configure(Configure {
        instance_name: None,
        config: CborValue::Map(vec![
            (
                CborValue::Text("script".to_owned()),
                CborValue::Text(script.display().to_string()),
            ),
            (
                CborValue::Text("vars".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("greeting".to_owned()),
                    CborValue::Text("honk".to_owned()),
                )]),
            ),
        ]),
        state_dir: None,
        secrets: BTreeMap::new(),
    });

    let frames = run_frames(&[configure]);

    let ready_pos = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready frame");
    let info_pos = frames
        .iter()
        .position(|frame| {
            matches!(
                emitted_event(frame),
                Some(Event::HarnessNotice(info)) if info.message == "started with honk"
            )
        })
        .expect("start info");
    assert!(ready_pos < info_pos);
}

#[test]
fn start_error_reports_but_keeps_extension_ready() {
    // A broken start hook is isolated like on_event/on_intercept failures: the
    // script is already configured, so report the callback error instead of
    // disabling the extension.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn start() {
                unknown_function();
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("rhai start failed")
    )));
}

#[test]
fn tau_emit_respects_transient_flag_and_reports_invalid_events() {
    // The two event-emission host functions differ only in durability metadata,
    // and invalid script-shaped events must become diagnostics instead of being
    // silently dropped.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn start(config) {
                let event = #{
                    event: "harness.notice",
                    payload: #{
                        kind: "extension.notice",
                        message: "from rhai",
                        level: "info",
                        always_show: false,
                    },
                };
                tau_emit(event);
                tau_emit_transient(event);
                tau_emit(#{ event: "not.a.real.event", payload: #{} });
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let notice_emits: Vec<_> = frames
        .iter()
        .filter(|frame| {
            matches!(
                emitted_event(frame),
                Some(Event::HarnessNotice(info)) if info.message == "from rhai"
            )
        })
        .collect();
    assert_eq!(notice_emits.len(), 2);
    assert_eq!(emitted_transient(notice_emits[0]), Some(false));
    assert_eq!(emitted_transient(notice_emits[1]), Some(true));
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("rhai invalid event")
    )));
}

#[test]
fn missing_script_config_reports_error_and_stays_inert() {
    // Missing scripts are configuration errors, but the process stays
    // alive long enough to avoid a harness restart loop.
    let frames = run_frames(&[empty_configure()]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::ConfigError(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Ready(_)));
    let HarnessInputMessage::Ready(ready) = &frames[2] else {
        panic!("expected ready");
    };
    assert!(
        ready
            .message
            .as_deref()
            .is_some_and(|m| m.contains("disabled"))
    );
}

#[test]
fn unknown_config_field_reports_config_error() {
    // Extension config uses deny_unknown_fields so misspelled options fail
    // closed and do not silently disable intended limits or script settings.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(&dir, "");
    let configure = configure_with_script_and_extra(
        &script,
        vec![(CborValue::Text("unknown".to_owned()), CborValue::Bool(true))],
    );

    let frames = run_frames(&[configure]);

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("unknown field")
    )));
}

#[test]
fn max_operations_limit_aborts_runaway_callback() {
    // Script operation limits are a key guardrail for callbacks that accidentally
    // spin forever while handling harness events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                while true {}
            }
        "#,
    );
    let configure = configure_with_script_and_extra(
        &script,
        vec![(
            CborValue::Text("limits".to_owned()),
            CborValue::Map(vec![(
                CborValue::Text("max_operations".to_owned()),
                CborValue::Integer(1000.into()),
            )]),
        )],
    );
    let delivered = HarnessOutputMessage::deliver_live(UnixMicros::new(1), prompt_event("loop"));

    let frames = run_frames(&[configure, delivered]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("on_event failed")
    )));
}

#[test]
fn delivered_event_invokes_script_with_replay_meta() {
    // A delivered event is converted to the JSON-shaped Rhai map; the meta
    // map exposes the replay marker and recorded_at timestamp so scripts can
    // distinguish catch-up history from live events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                tau_info(`saw ${meta.replay}/${meta.recorded_at}: ${event.payload.text}`);
            }
        "#,
    );
    let live = HarnessOutputMessage::deliver_live(UnixMicros::new(11), prompt_event("hello"));
    let replayed = HarnessOutputMessage::deliver_replay(UnixMicros::new(7), prompt_event("old"));

    let frames = run_frames(&[configure_with_script(&script), live, replayed]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("saw false/11: hello")
    )));
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("saw true/7: old")
    )));
}

#[test]
fn script_error_during_on_event_reports_and_keeps_running() {
    // Callback errors are isolated to the failing callback so one bad hook
    // cannot wedge delivery of later events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                if event.payload.text == "boom" {
                    unknown_function();
                }
                tau_info(`handled ${event.payload.text}`);
            }
        "#,
    );
    let failing = HarnessOutputMessage::deliver_live(UnixMicros::new(12), prompt_event("boom"));
    let following = HarnessOutputMessage::deliver_live(UnixMicros::new(13), prompt_event("after"));

    let frames = run_frames(&[configure_with_script(&script), failing, following]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("on_event failed")
    )));
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("handled after")
    )));
}

#[test]
fn init_merges_same_priority_intercepts() {
    // The harness stores one interceptor registration per connection, so
    // same-priority init entries are collapsed into one registration.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [
                        #{ selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }], priority: 0 },
                        #{ selectors: [#{ kind: "prefix", value: "tool." }], priority: 0 },
                    ],
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let intercepts: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Intercept(intercept) => Some(intercept),
            _ => None,
        })
        .collect();
    assert_eq!(intercepts.len(), 1);
    assert_eq!(intercepts[0].priority, InterceptionPriority::new(0));
    assert_eq!(intercepts[0].selectors.len(), 2);
    assert!(matches!(
        &intercepts[0].selectors[0],
        EventSelector::Exact(name) if name.to_string() == "agent.prompt_submitted"
    ));
    assert!(matches!(
        &intercepts[0].selectors[1],
        EventSelector::Prefix(prefix) if prefix == "tool."
    ));
}

#[test]
fn init_rejects_mixed_priority_intercepts() {
    // Multiple priority levels would require multiple harness
    // registrations, so the prototype rejects that script contract.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [
                        #{ selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }], priority: 0 },
                        #{ selectors: [#{ kind: "prefix", value: "tool." }], priority: 1 },
                    ],
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("same priority")
    )));
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Ready(ready) if ready.message.as_deref().is_some_and(|m| m.contains("disabled"))
    )));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::Intercept(_)))
    );
}
#[test]
fn intercept_callback_can_drop_event() {
    // Intercept callbacks must return exactly one InterceptReply. This
    // covers the simplest script-controlled policy: dropping an event.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, transient) { return "drop"; }
        "#,
    );
    let req = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("hello")),
        transient: false,
    });

    let frames = run_frames(&[configure_with_script(&script), req]);
    let replies = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0].action, InterceptAction::Drop));
}

#[test]
fn intercept_callback_can_return_replacement_event() {
    // A script can mutate the JSON-shaped event map and pass the
    // replacement back through Rust deserialization.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, transient) {
                event.payload.text = "changed";
                return #{ kind: "pass", event: event };
            }
        "#,
    );
    let req = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("hello")),
        transient: false,
    });

    let frames = run_frames(&[configure_with_script(&script), req]);

    let replies = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    let replacement = match &replies[0].action {
        InterceptAction::Pass(Some(event)) => Some(event.as_ref()),
        _ => None,
    };
    assert!(matches!(
        replacement,
        Some(Event::AgentPromptSubmitted(prompt)) if prompt.text == "changed"
    ));
}

#[test]
fn register_tool_emits_registration_before_ready() {
    // Tool registrations are staged during init and emitted before Ready so
    // the harness can route later calls only after the script is configured.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                register_tool_group("host", #{});
                register_tool("project_status", #{
                    group: "host",
                    description: "Get project status",
                    parameters: #{ type: "object", additionalProperties: false },
                }, Fn("project_status"));
            }
            fn project_status(args, c) { return "ok"; }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let register_pos = frames
        .iter()
        .position(|frame| matches!(emitted_event(frame), Some(Event::ToolRegister(_))))
        .expect("tool.register");
    let ready_pos = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready");
    assert!(register_pos < ready_pos);
    let Some(Event::ToolRegister(register)) = emitted_event(&frames[register_pos]) else {
        panic!("expected tool.register");
    };
    assert_eq!(register.tool.name.as_str(), "project_status");
    assert_eq!(
        register.tool_group.as_ref().map(|g| g.name.as_str()),
        Some("host")
    );
}

#[test]
fn live_owned_tool_started_invokes_handler_and_replay_is_ignored() {
    // Owned live tool.started events are consumed by the tool dispatcher and
    // produce terminal tool results, while replayed history is ignored to avoid
    // re-running script side effects.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                register_tool("echo_args", #{ description: "Echo args" }, Fn("echo_args"));
            }
            fn echo_args(args, c) { return `saw ${args.text} via ${c.tool_name}`; }
            fn on_event(event, meta) { tau_info("raw should not see owned tool"); }
        "#,
    );
    let live = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "echo_args",
            CborValue::Map(vec![(
                CborValue::Text("text".to_owned()),
                CborValue::Text("hello".to_owned()),
            )]),
        ),
    );
    let replay = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(2),
        tool_started("echo_args", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), live, replay]);

    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].result,
        CborValue::Text("saw hello via echo_args".to_owned())
    );
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::HarnessNotice(info)) if info.message.contains("raw should not see")
    )));
}

#[test]
fn tool_handler_throw_emits_tool_error_and_keeps_running() {
    // Handler exceptions fail only the current tool call and do not disable the
    // extension, so a subsequent call can still complete.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("maybe", #{}, Fn("maybe")); }
            fn maybe(args, c) {
                if args.fail { throw "boom"; }
                return "ok";
            }
        "#,
    );
    let fail = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "maybe",
            CborValue::Map(vec![(
                CborValue::Text("fail".to_owned()),
                CborValue::Bool(true),
            )]),
        ),
    );
    let ok = HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        tool_started(
            "maybe",
            CborValue::Map(vec![(
                CborValue::Text("fail".to_owned()),
                CborValue::Bool(false),
            )]),
        ),
    );

    let frames = run_frames(&[configure_with_script(&script), fail, ok]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(emitted_event(frame), Some(Event::ToolError(_))))
    );
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolResult(result)) if result.result == CborValue::Text("ok".to_owned())
    )));
}

#[test]
fn shell_job_returned_by_tool_defers_until_completion_callback() {
    // Returning ShellJob from a tool handler defers ToolResult emission until
    // the async command completes; the completion callback's value becomes the
    // final tool result.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("host_echo", #{}, Fn("host_echo")); }
            fn host_echo(args, c) {
                return shell_spawn("printf shell-ok", #{ timeout: 5, on_complete: Fn("done") });
            }
            fn done(result, job) {
                if !result.success { throw result.output; }
                return result.output;
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("host_echo", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolResult(result)) if result.result == CborValue::Text("shell-ok".to_owned())
    )));
}

#[test]
fn shell_completion_wakes_runtime_while_harness_input_stays_open() {
    // A completed shell-backed tool must produce its ToolResult without waiting
    // for another harness frame or input EOF; the shell worker wake is the only
    // stimulus after the tool.started frame.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("host_echo", #{}, Fn("host_echo")); }
            fn host_echo(args, c) {
                return shell_spawn("printf live-open", #{ timeout: 5 });
            }
        "#,
    );

    let (input_reader, input_writer) = UnixStream::pair().expect("unix stream pair");
    let mut harness_writer = HarnessOutputWriter::new(
        input_writer
            .try_clone()
            .expect("clone harness input writer"),
    );
    let output = SharedWriter::default();
    let run_output = output.clone();
    let run_thread = std::thread::spawn(move || {
        run(input_reader, run_output).map_err(|error| error.to_string())
    });

    harness_writer
        .write_message(&configure_with_script(&script))
        .expect("write configure");
    harness_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("host_echo", CborValue::Map(Vec::new())),
        ))
        .expect("write tool start");
    harness_writer.flush().expect("flush harness input");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let frames = frames_from_bytes_lossy(output.bytes());
        if frames
            .iter()
            .any(|frame| tool_result_has_output(frame, "live-open"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for shell completion wake"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    drop(harness_writer);
    drop(input_writer);
    run_thread
        .join()
        .expect("run thread")
        .expect("run rhai extension");
}

#[test]
fn shell_completions_are_not_starved_by_ready_harness_input() {
    // The runtime checks shell completions between harness messages so replay
    // catch-up or another ready burst cannot postpone completed shell callbacks
    // until all queued harness input has drained.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("host_echo", #{}, Fn("host_echo")); }
            fn host_echo(args, c) {
                return shell_spawn("printf fair", #{ timeout: 5 });
            }
            fn on_event(event, meta) {
                let checksum = 0;
                for n in 0..20000 {
                    checksum += n;
                }
                tau_info(event.payload.message);
            }
        "#,
    );
    let mut input = vec![
        configure_with_script(&script),
        HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("host_echo", CborValue::Map(Vec::new())),
        ),
    ];
    for i in 0..200 {
        input.push(HarnessOutputMessage::deliver_live(
            UnixMicros::new(2 + i),
            Event::HarnessNotice(HarnessNotice {
                kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                message: format!("flood-{i}"),
                level: NoticeLevel::Info,
                always_show: false,
            }),
        ));
    }

    let frames = run_frames(&input);
    let result_index = frames
        .iter()
        .position(|frame| tool_result_has_output(frame, "fair"))
        .expect("tool result");
    let last_flood_notice_index = frames
        .iter()
        .rposition(|frame| {
            matches!(
                emitted_event(frame),
                Some(Event::HarnessNotice(notice)) if notice.message.starts_with("flood-")
            )
        })
        .expect("flood notice");
    assert!(
        result_index < last_flood_notice_index,
        "shell completion was emitted only after the harness burst drained"
    );
}

#[test]
fn shell_completion_callback_throw_emits_tool_error() {
    // A shell completion callback exception maps to ToolError for the deferred
    // call instead of silently dropping the failure.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("bad_shell", #{}, Fn("bad_shell")); }
            fn bad_shell(args, c) {
                return shell_spawn("printf shell-ok", #{ timeout: 5, on_complete: Fn("done") });
            }
            fn done(result, job) { throw "callback boom"; }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("bad_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolError(error)) if error.message.contains("callback boom")
    )));
}

#[test]
fn shell_result_includes_cwd_stderr_exit_and_start_error_shape() {
    // The documented shell result map must expose working-directory behavior,
    // stderr appending, nonzero process exits, and start failures in a stable
    // JSON/CBOR-compatible shape for script tools.
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = tempfile::tempdir().expect("tempdir");
    std::fs::write(cwd.path().join("input.txt"), "ok").expect("write cwd input");
    let missing_cwd = dir.path().join("missing");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{
                    register_tool("shell_contract", #{{}}, Fn("shell_contract"));
                }}
                fn shell_contract(args, c) {{
                    if args["case"] == "cwd_stderr" {{
                        return shell_spawn("cat input.txt; printf err >&2; exit 7", #{{
                            cwd: "{}",
                            timeout: 5,
                        }});
                    }}
                    return shell_spawn("printf nope", #{{
                        cwd: "{}",
                        timeout: 5,
                    }});
                }}
            "#,
            cwd.path().display(),
            missing_cwd.display()
        ),
    );
    let ok = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "shell_contract",
            CborValue::Map(vec![(
                CborValue::Text("case".to_owned()),
                CborValue::Text("cwd_stderr".to_owned()),
            )]),
        ),
    );
    let start_error = HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        tool_started(
            "shell_contract",
            CborValue::Map(vec![(
                CborValue::Text("case".to_owned()),
                CborValue::Text("start_error".to_owned()),
            )]),
        ),
    );

    let frames = run_frames(&[configure_with_script(&script), ok, start_error]);

    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(&result.result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    let cwd_result = results
        .iter()
        .find_map(|result| match result {
            CborValue::Map(fields)
                if fields.iter().any(|(key, value)| {
                    matches!(
                        (key, value),
                        (CborValue::Text(key), CborValue::Text(output))
                            if key == "output" && output.contains("ok")
                    )
                }) =>
            {
                Some(fields)
            }
            _ => None,
        })
        .expect("cwd shell result");
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Bool(false)) if key == "success"
    )));
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Integer(status)) if key == "status" && *status == 7.into()
    )));
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(output))
            if key == "output" && output.contains("ok") && output.contains("[stderr]\nerr")
    )));
    let start_error_result = results
        .iter()
        .find_map(|result| match result {
            CborValue::Map(fields)
                if fields.iter().any(|(key, value)| {
                    matches!(
                        (key, value),
                        (CborValue::Text(key), CborValue::Text(reason))
                            if key == "termination_reason" && reason == "start_error"
                    )
                }) =>
            {
                Some(fields)
            }
            _ => None,
        })
        .expect("start error shell result");
    assert!(start_error_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(reason)) if key == "termination_reason" && reason == "start_error"
    )));
}

#[test]
fn oversized_shell_timeout_is_rejected_before_pending_job_is_inserted() {
    // Timeout validation must happen before the job enters the pending map; an
    // overflowing deadline would otherwise panic the worker and wedge tool calls.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("huge_timeout", #{}, Fn("huge_timeout")); }
            fn huge_timeout(args, c) {
                return shell_spawn("printf never", #{ timeout: 999999999999999999 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("huge_timeout", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolError(error)) if error.message.contains("timeout must be at most")
    )));
}

#[test]
fn disconnect_cancels_pending_shell_jobs() {
    // On harness shutdown the extension must not leave trusted shell children
    // running after its runtime exits.
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("marker");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{ register_tool("long_shell", #{{}}, Fn("long_shell")); }}
                fn long_shell(args, c) {{
                    return shell_spawn("sleep 2; touch '{}'", #{{ timeout: 10 }});
                }}
            "#,
            marker.display()
        ),
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("long_shell", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let _frames = run_frames(&[
        configure_with_script(&script),
        started,
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect::default()),
    ]);
    assert!(started_at.elapsed() < Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(2500));

    assert!(!marker.exists());
}

#[test]
fn shell_completion_kills_background_descendant_holding_output_pipe() {
    // A shell that exits while a background child inherits stdout used to wedge
    // when joining pipe readers. Completion must kill the remaining process
    // group before waiting for captured output readers.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("background_pipe", #{}, Fn("background_pipe")); }
            fn background_pipe(args, c) {
                return shell_spawn("sleep 60 & printf done", #{ timeout: 10 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("background_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert_eq!(tool_result_output(&frames), "done");
}

#[test]
fn shell_completion_does_not_wait_for_detached_descendant_holding_output_pipe() {
    // A hostile or careless trusted command can detach from the shell process
    // group while keeping inherited stdout open. The extension should still
    // return promptly with already-captured output instead of waiting for pipe
    // EOF from a process group it no longer owns. This regression requires the
    // `setsid` helper to be available in the test environment.
    if !setsid_available() {
        eprintln!("skipping detached setsid regression: setsid not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("detached_pipe", #{}, Fn("detached_pipe")); }
            fn detached_pipe(args, c) {
                return shell_spawn("setsid sh -c 'sleep 2' & printf done", #{ timeout: 10 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("detached_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert_eq!(tool_result_output(&frames), "done");
}

#[test]
fn shell_completion_bounds_detached_descendant_continuing_to_write() {
    // Post-completion pipe draining must have an absolute bound. A detached
    // descendant that keeps writing to inherited stdout after the foreground
    // shell exits should not keep the pipe reader alive indefinitely. This
    // regression requires the `setsid` helper to be available in the test
    // environment.
    if !setsid_available() {
        eprintln!("skipping detached setsid writer regression: setsid not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("writing_pipe", #{}, Fn("writing_pipe")); }
            fn writing_pipe(args, c) {
                return shell_spawn("setsid sh -c 'i=0; while [ $i -lt 200 ]; do printf x; i=$((i+1)); sleep 0.01; done' & printf done", #{ timeout: 10 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("writing_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert!(tool_result_output(&frames).contains("done"));
}

#[test]
fn shell_timeout_kills_process_group_and_returns_result() {
    // Timeout cleanup must kill descendants that inherit stdout/stderr so pipe
    // reader joins cannot wedge the extension after the shell parent exits.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("timeout_shell", #{}, Fn("timeout_shell")); }
            fn timeout_shell(args, c) {
                return shell_spawn("sh -c 'sleep 60 & wait'", #{ timeout: 1 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("timeout_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResult(result)) => Some(&result.result),
            _ => None,
        })
        .expect("tool result");
    let CborValue::Map(fields) = result else {
        panic!("result map");
    };
    assert!(fields.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Bool(true)) if key == "timed_out"
    )));
    assert!(fields.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(reason)) if key == "termination_reason" && reason == "timeout"
    )));
}

#[test]
fn shell_spawn_admission_cap_fails_deterministically() {
    // The pending-job cap should reject excess shell work as a tool error while
    // keeping the extension responsive instead of spawning unbounded threads.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("saturate_shell", #{}, Fn("saturate_shell")); }
            fn saturate_shell(args, c) {
                for n in 0..33 {
                    shell_spawn("sleep 1", #{ timeout: 5 });
                }
                return "unexpected";
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("saturate_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolError(error)) if error.message.contains("too many pending shell jobs")
    )));
}
