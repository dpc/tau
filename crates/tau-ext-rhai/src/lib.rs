//! Rhai scripting extension for trusted local Tau event hooks.
//!
//! The extension keeps Tau protocol handling in Rust and exposes delivered
//! events to Rhai scripts as JSON-shaped maps matching Serde's JSON form.

mod shell;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rhai::{Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, Map, Scope};
use serde::Deserialize;
use tau_proto::{
    CborValue, ClientKind, ConfigError, Configure, Event, EventSelector, HarnessInputMessage,
    HarnessNotice, HarnessOutputMessage, Hello, Intercept, InterceptAction, InterceptReply,
    InterceptionPriority, NoticeLevel, PROTOCOL_VERSION, PeerInputReader, PeerOutputWriter,
    PromptOriginator, Ready, Subscribe, ToolError, ToolGroup, ToolGroupName, ToolName,
    ToolRegister, ToolResult, ToolResultKind, ToolSpec, ToolStarted, ToolType, UnixMicros,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "rhai";

/// Maximum simultaneously pending Rhai-spawned shell jobs per extension.
const MAX_PENDING_SHELL_JOBS: usize = 32;

/// Maximum shell timeout accepted from scripts.
const MAX_SHELL_TIMEOUT_SECS: i64 = 24 * 60 * 60;

/// Maximum time to wait for a canceled shell worker to finish shutdown.
const SHELL_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Rhai script path. Absolute paths are preferred; relative paths are
    /// resolved by the extension process current working directory.
    script: Option<PathBuf>,
    /// JSON-compatible user variables passed to `init(config)`.
    vars: serde_json::Value,
    /// Script execution limits.
    limits: Limits,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Limits {
    /// Maximum Rhai operations before aborting a callback.
    max_operations: Option<u64>,
    /// Maximum nested Rhai function calls.
    max_call_levels: Option<usize>,
    /// Maximum expression nesting depth during parsing. Zero disables the
    /// limit.
    max_expr_depth: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct InitOutput {
    subscribe: Vec<EventSelector>,
    intercept: Vec<InitIntercept>,
    ready_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitIntercept {
    selectors: Vec<EventSelector>,
    priority: InterceptionPriority,
}

/// Runtime events consumed by the single-threaded Rhai interpreter loop.
enum RuntimeInput {
    /// A message read from the Tau harness connection.
    Harness(HarnessOutputMessage),
    /// Completion of an asynchronously spawned shell job.
    ShellComplete(ShellCompletion),
    /// The harness input stream reached EOF without an explicit disconnect.
    ReaderClosed,
}

/// Phase-sensitive host state shared with Rhai host-function closures.
#[derive(Default)]
struct HostState {
    /// True only while `init(config)` is running.
    init_active: bool,
    /// Tool groups staged by `register_tool_group` during init.
    groups: BTreeMap<String, StagedToolGroup>,
    /// Tool registrations staged by `register_tool` during init.
    tools: Vec<StagedTool>,
    /// Next host-local shell job id.
    next_shell_job_id: i64,
    /// Shell jobs known to the runtime but not yet completed.
    shell_jobs: HashMap<i64, PendingShellJob>,
}

/// A tool group staged by a script during init.
#[derive(Clone)]
struct StagedToolGroup {
    /// Validated Tau tool group name.
    name: ToolGroupName,
    /// Optional group prompt fragment, reserved for future Rhai API growth.
    prompt_fragment: Option<tau_proto::PromptFragment>,
}

/// A tool registration staged by a script during init.
#[derive(Clone)]
struct StagedTool {
    /// Metadata exposed to Tau's tool registry.
    spec: ToolSpec,
    /// Optional group name referenced by the tool spec map.
    group: Option<ToolGroupName>,
    /// Rhai function pointer invoked for live owned `tool.started` events.
    handler: FnPtr,
}

/// Tool-call context saved while an async shell job defers completion.
#[derive(Clone)]
struct PendingToolCall {
    /// Stable Tau tool call id.
    call_id: tau_proto::ToolCallId,
    /// Registered tool name.
    tool_name: ToolName,
    /// Protocol tool kind.
    tool_type: ToolType,
    /// Prompt originator echoed in terminal tool events.
    originator: PromptOriginator,
}

/// Shell job state kept until the worker thread reports completion.
struct PendingShellJob {
    /// Command string passed to the host shell.
    command: String,
    /// Optional Rhai callback invoked with `(result, job)` on completion.
    on_complete: Option<FnPtr>,
    /// Optional tool call waiting for this job's result.
    tool_call: Option<PendingToolCall>,
    /// Whether this job token has already been returned from a tool handler.
    tool_claimed: bool,
    /// JSON-compatible user metadata copied into the job map.
    tag: Option<Dynamic>,
    /// Cancellation and process-group handle shared with the shell worker.
    cancel: shell::ShellCancel,
    /// Worker thread running the shell command.
    join_handle: std::thread::JoinHandle<()>,
}

/// Rhai-visible token identifying an asynchronous host shell job.
#[derive(Clone, Debug)]
struct ShellJob {
    /// Host-local shell job id.
    id: i64,
}

/// Completion message sent from shell worker threads to the interpreter loop.
struct ShellCompletion {
    /// Completed job id.
    job_id: i64,
    /// Structured shell outcome.
    result: serde_json::Value,
}

type HostStateRef = Rc<RefCell<HostState>>;

struct ScriptRuntime {
    engine: Engine,
    ast: rhai::AST,
    scope: Scope<'static>,
    host_state: HostStateRef,
    tools: HashMap<ToolName, FnPtr>,
}

/// Run the extension over stdio.
///
/// # Errors
///
/// Returns protocol I/O and worker-thread failures from the extension run.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over the supplied reader/writer pair.
///
/// # Errors
///
/// Returns protocol I/O failures, initial output failures, or reader/writer
/// worker failures encountered while serving the supplied streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));

    writer.write_message(&HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::new("tau-ext-rhai"),
        client_kind: ClientKind::Tool,
    }))?;
    writer.flush()?;

    let Some(configure) = read_initial_config(&mut reader)? else {
        return Ok(());
    };

    let (tx, rx) = mpsc::channel::<HarnessInputMessage>();
    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for message in rx {
            writer
                .write_message(&message)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    let (runtime_tx, runtime_rx) = mpsc::channel::<RuntimeInput>();
    let reader_runtime_tx = runtime_tx.clone();
    let reader_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        while let Some(message) = reader
            .read_message()
            .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?
        {
            let disconnected = matches!(message, HarnessOutputMessage::Disconnect(_));
            if reader_runtime_tx
                .send(RuntimeInput::Harness(message))
                .is_err()
            {
                return Ok(());
            }
            if disconnected {
                return Ok(());
            }
        }
        let _ = reader_runtime_tx.send(RuntimeInput::ReaderClosed);
        Ok(())
    });

    let mut runtime = match load_runtime(&configure, tx.clone(), runtime_tx.clone()) {
        Ok((mut runtime, init, config_json)) => {
            send_init_messages(&tx, &runtime, init)?;
            runtime.start(config_json, &tx);
            Some(runtime)
        }
        Err(message) => {
            tracing::warn!(target: LOG_TARGET, error = %message, "rhai disabled");
            send_config_error_ready(&tx, message)?;
            None
        }
    };

    let mut reader_closed = false;

    while let Ok(input) = runtime_rx.recv() {
        match input {
            RuntimeInput::Harness(HarnessOutputMessage::Deliver(delivery)) => {
                let (event, replay, recorded_at) = delivery.into_parts();
                if let Some(runtime) = runtime.as_mut() {
                    runtime.on_delivered_event(event, replay, recorded_at, &tx);
                }
            }
            RuntimeInput::Harness(HarnessOutputMessage::InterceptRequest(req)) => {
                let action = runtime
                    .as_mut()
                    .map(|runtime| runtime.on_intercept(*req.event, req.transient, &tx))
                    .unwrap_or_else(|| InterceptAction::Pass(None));
                let _ = tx.send(HarnessInputMessage::InterceptReply(InterceptReply {
                    action,
                }));
            }
            RuntimeInput::Harness(HarnessOutputMessage::Disconnect(_)) => {
                if let Some(runtime) = runtime.as_mut() {
                    runtime.cancel_and_join_pending_shell_jobs();
                }
                break;
            }
            RuntimeInput::ReaderClosed => {
                reader_closed = true;
                if runtime
                    .as_ref()
                    .is_none_or(|runtime| !runtime.has_pending_shell_jobs())
                {
                    break;
                }
            }
            RuntimeInput::Harness(HarnessOutputMessage::Configure(_)) => {}
            RuntimeInput::Harness(_) => {}
            RuntimeInput::ShellComplete(completion) => {
                if let Some(runtime) = runtime.as_mut() {
                    runtime.on_shell_complete(completion, &tx);
                    if reader_closed && !runtime.has_pending_shell_jobs() {
                        break;
                    }
                }
            }
        }
    }

    drop(runtime);
    drop(runtime_tx);
    drop(tx);
    reader_handle
        .join()
        .map_err(|e| -> Box<dyn Error> { format!("reader thread panicked: {e:?}").into() })?
        .map_err(|e| -> Box<dyn Error> { e })?;
    writer_handle
        .join()
        .map_err(|e| -> Box<dyn Error> { format!("writer thread panicked: {e:?}").into() })?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

fn read_initial_config<R: Read>(
    reader: &mut PeerInputReader<BufReader<R>>,
) -> Result<Option<Configure>, Box<dyn Error>> {
    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::Configure(configure) => return Ok(Some(configure)),
            HarnessOutputMessage::Disconnect(_) => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}

fn load_runtime(
    configure: &Configure,
    tx: mpsc::Sender<HarnessInputMessage>,
    runtime_tx: mpsc::Sender<RuntimeInput>,
) -> Result<(ScriptRuntime, InitOutput, serde_json::Value), String> {
    let cfg = tau_extension::parse_config::<ExtConfig>(&configure.config)?;
    let script = cfg
        .script
        .ok_or_else(|| "rhai script config field is required".to_owned())?;
    let source = fs::read_to_string(&script)
        .map_err(|e| format!("reading Rhai script {}: {e}", script.display()))?;

    let mut engine = Engine::new();
    let host_state = Rc::new(RefCell::new(HostState::default()));
    register_host_functions(&mut engine, tx, runtime_tx, host_state.clone());
    let max_expr_depth = cfg.limits.max_expr_depth.unwrap_or(64);
    engine.set_max_expr_depths(max_expr_depth, max_expr_depth);
    if let Some(max) = cfg.limits.max_operations {
        engine.set_max_operations(max);
    }
    if let Some(max) = cfg.limits.max_call_levels {
        engine.set_max_call_levels(max);
    }
    let ast = engine
        .compile(&source)
        .map_err(|e| format!("compiling Rhai script {}: {e}", script.display()))?;
    let mut runtime = ScriptRuntime {
        engine,
        ast,
        scope: Scope::new(),
        host_state,
        tools: HashMap::new(),
    };
    let config_json = init_config_json(&cfg.vars, configure.state_dir.as_ref());
    let init = runtime.init(config_json.clone())?;
    runtime.finish_tool_registration()?;
    Ok((runtime, init, config_json))
}

fn init_config_json(vars: &serde_json::Value, state_dir: Option<&PathBuf>) -> serde_json::Value {
    serde_json::json!({
        "vars": vars,
        "state_dir": state_dir.map(|p| p.display().to_string()),
    })
}

fn send_init_messages(
    tx: &mpsc::Sender<HarnessInputMessage>,
    runtime: &ScriptRuntime,
    init: InitOutput,
) -> Result<(), Box<mpsc::SendError<HarnessInputMessage>>> {
    if !init.subscribe.is_empty() {
        tx.send(HarnessInputMessage::Subscribe(Subscribe {
            selectors: init.subscribe,
        }))
        .map_err(Box::new)?;
    }
    for intercept in init.intercept {
        tx.send(HarnessInputMessage::Intercept(Intercept {
            selectors: intercept.selectors,
            priority: intercept.priority,
        }))
        .map_err(Box::new)?;
    }
    for registration in runtime.tool_register_events() {
        tx.send(HarnessInputMessage::emit(Event::ToolRegister(registration)))
            .map_err(Box::new)?;
    }
    tx.send(HarnessInputMessage::Ready(Ready {
        message: Some(
            init.ready_message
                .unwrap_or_else(|| "rhai ready".to_owned()),
        ),
    }))
    .map_err(Box::new)?;
    Ok(())
}

fn normalize_init_output(mut init: InitOutput) -> Result<InitOutput, String> {
    let Some(first) = init.intercept.first() else {
        return Ok(init);
    };
    let priority = first.priority;
    let mut selectors = Vec::new();
    for intercept in std::mem::take(&mut init.intercept) {
        if intercept.priority != priority {
            return Err("init intercept entries must all use the same priority".to_owned());
        }
        selectors.extend(intercept.selectors);
    }
    init.intercept = vec![InitIntercept {
        selectors,
        priority,
    }];
    Ok(init)
}
fn send_config_error_ready(
    tx: &mpsc::Sender<HarnessInputMessage>,
    message: String,
) -> Result<(), Box<mpsc::SendError<HarnessInputMessage>>> {
    tx.send(HarnessInputMessage::ConfigError(ConfigError {
        message: message.clone(),
    }))
    .map_err(Box::new)?;
    tx.send(HarnessInputMessage::Ready(Ready {
        message: Some(format!("rhai disabled: {message}")),
    }))
    .map_err(Box::new)?;
    Ok(())
}

fn register_host_functions(
    engine: &mut Engine,
    tx: mpsc::Sender<HarnessInputMessage>,
    runtime_tx: mpsc::Sender<RuntimeInput>,
    host_state: HostStateRef,
) {
    engine.register_type_with_name::<ShellJob>("ShellJob");

    let register_state = host_state.clone();
    engine.register_fn(
        "register_tool_group",
        move |name: ImmutableString, spec: Map| -> Result<(), Box<EvalAltResult>> {
            stage_tool_group(&register_state, name.as_str(), spec).map_err(eval_error)
        },
    );

    let register_state = host_state.clone();
    engine.register_fn(
        "register_tool",
        move |name: ImmutableString, spec: Map, handler: FnPtr| -> Result<(), Box<EvalAltResult>> {
            stage_tool(&register_state, name.as_str(), spec, handler).map_err(eval_error)
        },
    );

    let emit_tx = tx.clone();
    let emit_state = host_state.clone();
    engine.register_fn(
        "tau_emit",
        move |event: Dynamic| -> Result<(), Box<EvalAltResult>> {
            ensure_not_init(&emit_state, "tau_emit")?;
            enqueue_event(&emit_tx, event, false);
            Ok(())
        },
    );

    let emit_tx = tx.clone();
    let emit_state = host_state.clone();
    engine.register_fn(
        "tau_emit_transient",
        move |event: Dynamic| -> Result<(), Box<EvalAltResult>> {
            ensure_not_init(&emit_state, "tau_emit_transient")?;
            enqueue_event(&emit_tx, event, true);
            Ok(())
        },
    );

    let info_tx = tx.clone();
    let info_state = host_state.clone();
    engine.register_fn(
        "tau_info",
        move |message: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            ensure_not_init(&info_state, "tau_info")?;
            enqueue_info(&info_tx, message.as_str(), NoticeLevel::Info, true);
            Ok(())
        },
    );

    let info_tx = tx.clone();
    let info_state = host_state.clone();
    engine.register_fn(
        "tau_info",
        move |message: ImmutableString, level: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            ensure_not_init(&info_state, "tau_info")?;
            enqueue_info(
                &info_tx,
                message.as_str(),
                parse_info_level(level.as_str()),
                true,
            );
            Ok(())
        },
    );

    let shell_state = host_state.clone();
    let shell_runtime_tx = runtime_tx.clone();
    engine.register_fn(
        "shell_spawn",
        move |command: ImmutableString, opts: Map| -> Result<ShellJob, Box<EvalAltResult>> {
            ensure_not_init(&shell_state, "shell_spawn")?;
            shell_spawn(&shell_state, &shell_runtime_tx, command.to_string(), opts)
                .map_err(eval_error)
        },
    );

    let shell_state = host_state.clone();
    let shell_runtime_tx = runtime_tx;
    engine.register_fn(
        "shell_spawn",
        move |command: ImmutableString| -> Result<ShellJob, Box<EvalAltResult>> {
            ensure_not_init(&shell_state, "shell_spawn")?;
            shell_spawn(
                &shell_state,
                &shell_runtime_tx,
                command.to_string(),
                Map::new(),
            )
            .map_err(eval_error)
        },
    );

    engine.register_fn(
        "tau_log",
        move |level: ImmutableString, message: ImmutableString| match level.as_str() {
            "trace" => tracing::trace!(target: LOG_TARGET, message = %message, "rhai script log"),
            "debug" => tracing::debug!(target: LOG_TARGET, message = %message, "rhai script log"),
            "warn" => tracing::warn!(target: LOG_TARGET, message = %message, "rhai script log"),
            "error" => tracing::error!(target: LOG_TARGET, message = %message, "rhai script log"),
            _ => tracing::info!(target: LOG_TARGET, message = %message, "rhai script log"),
        },
    );
}

fn eval_error(message: String) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message),
        rhai::Position::NONE,
    ))
}

fn ensure_not_init(state: &HostStateRef, function_name: &str) -> Result<(), Box<EvalAltResult>> {
    if state.borrow().init_active {
        return Err(eval_error(format!(
            "{function_name} is not available during init"
        )));
    }
    Ok(())
}

fn stage_tool_group(state: &HostStateRef, name: &str, spec: Map) -> Result<(), String> {
    if !spec.is_empty() {
        return Err("register_tool_group spec must be empty in this version".to_owned());
    }
    let group_name = ToolGroupName::try_new(name.to_owned())
        .ok_or_else(|| format!("invalid tool group name `{name}`"))?;
    let mut state = state.borrow_mut();
    if !state.init_active {
        return Err("register_tool_group is only available during init".to_owned());
    }
    state.groups.insert(
        group_name.as_str().to_owned(),
        StagedToolGroup {
            name: group_name,
            prompt_fragment: None,
        },
    );
    Ok(())
}

fn stage_tool(state: &HostStateRef, name: &str, spec: Map, handler: FnPtr) -> Result<(), String> {
    let tool_name =
        ToolName::try_new(name.to_owned()).ok_or_else(|| format!("invalid tool name `{name}`"))?;
    let description = optional_string_field(&spec, "description")?;
    let model_visible_name = optional_string_field(&spec, "model_visible_name")?
        .map(|name| {
            ToolName::try_new(name.clone())
                .ok_or_else(|| format!("invalid model_visible_name `{name}`"))
        })
        .transpose()?;
    let enabled_by_default = optional_bool_field(&spec, "enabled_by_default")?.unwrap_or(true);
    let parameters = match spec.get("parameters") {
        Some(value) => Some(dynamic_to_json(value)?),
        None => None,
    };
    let group = optional_string_field(&spec, "group")?
        .map(|name| {
            ToolGroupName::try_new(name.clone())
                .ok_or_else(|| format!("invalid tool group name `{name}`"))
        })
        .transpose()?;
    let mut state = state.borrow_mut();
    if !state.init_active {
        return Err("register_tool is only available during init".to_owned());
    }
    state.tools.push(StagedTool {
        spec: ToolSpec {
            name: tool_name,
            model_visible_name,
            description,
            tool_type: ToolType::Function,
            parameters,
            format: None,
            tags: Vec::new(),
            enabled_by_default,
            background_support: None,
            examples: Vec::new(),
        },
        group,
        handler,
    });
    Ok(())
}

fn optional_string_field(map: &Map, key: &str) -> Result<Option<String>, String> {
    map.get(key)
        .map(|value| {
            value
                .clone()
                .try_cast::<ImmutableString>()
                .map(|s| s.to_string())
                .ok_or_else(|| format!("field `{key}` must be a string"))
        })
        .transpose()
}

fn optional_bool_field(map: &Map, key: &str) -> Result<Option<bool>, String> {
    map.get(key)
        .map(|value| {
            value
                .clone()
                .try_cast::<bool>()
                .ok_or_else(|| format!("field `{key}` must be a bool"))
        })
        .transpose()
}

fn shell_spawn(
    state: &HostStateRef,
    runtime_tx: &mpsc::Sender<RuntimeInput>,
    command: String,
    opts: Map,
) -> Result<ShellJob, String> {
    let timeout_secs = optional_int_field(&opts, "timeout")?.unwrap_or(120);
    if timeout_secs < 0 {
        return Err("shell_spawn timeout must be non-negative".to_owned());
    }
    if timeout_secs > MAX_SHELL_TIMEOUT_SECS {
        return Err(format!(
            "shell_spawn timeout must be at most {MAX_SHELL_TIMEOUT_SECS} seconds"
        ));
    }
    let cwd = optional_string_field(&opts, "cwd")?;
    let on_complete = opts
        .get("on_complete")
        .map(|value| {
            value
                .clone()
                .try_cast::<FnPtr>()
                .ok_or_else(|| "shell_spawn on_complete must be a function pointer".to_owned())
        })
        .transpose()?;
    let tag = opts.get("tag").cloned();

    let mut state_guard = state.borrow_mut();
    if MAX_PENDING_SHELL_JOBS <= state_guard.shell_jobs.len() {
        return Err(format!(
            "too many pending shell jobs (limit {MAX_PENDING_SHELL_JOBS})"
        ));
    }
    state_guard.next_shell_job_id += 1;
    let id = state_guard.next_shell_job_id;
    let cancel = shell::ShellCancel::default();
    let worker_cancel = cancel.clone();
    let tx = runtime_tx.clone();
    let worker_command = command.clone();
    let join_handle = std::thread::spawn(move || {
        let result = shell::run_shell_command(
            worker_command,
            cwd,
            Duration::from_secs(timeout_secs as u64),
            worker_cancel,
        );
        let _ = tx.send(RuntimeInput::ShellComplete(ShellCompletion {
            job_id: id,
            result,
        }));
    });
    state_guard.shell_jobs.insert(
        id,
        PendingShellJob {
            command,
            on_complete,
            tool_call: None,
            tool_claimed: false,
            tag,
            cancel,
            join_handle,
        },
    );
    Ok(ShellJob { id })
}

fn optional_int_field(map: &Map, key: &str) -> Result<Option<i64>, String> {
    map.get(key)
        .map(|value| {
            value
                .clone()
                .try_cast::<rhai::INT>()
                .ok_or_else(|| format!("field `{key}` must be an integer"))
        })
        .transpose()
}

fn enqueue_event(tx: &mpsc::Sender<HarnessInputMessage>, event: Dynamic, transient: bool) {
    match dynamic_to_json(&event)
        .and_then(|value| serde_json::from_value::<Event>(value).map_err(|e| e.to_string()))
    {
        Ok(event) => {
            let _ = tx.send(HarnessInputMessage::emit_with_transient(event, transient));
        }
        Err(message) => {
            tracing::warn!(target: LOG_TARGET, error = %message, "script emitted invalid event");
            enqueue_info(
                tx,
                &format!("rhai invalid event: {message}"),
                NoticeLevel::Warning,
                true,
            );
        }
    }
}

fn enqueue_info(
    tx: &mpsc::Sender<HarnessInputMessage>,
    message: &str,
    level: NoticeLevel,
    transient: bool,
) {
    let _ = tx.send(HarnessInputMessage::emit_with_transient(
        Event::HarnessNotice(HarnessNotice {
            kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
            message: message.to_owned(),
            level,
            always_show: false,
        }),
        transient,
    ));
}

fn parse_info_level(level: &str) -> NoticeLevel {
    match level {
        "critical" | "warning" | "important" => NoticeLevel::Warning,
        "debug" => NoticeLevel::Debug,
        "trace" => NoticeLevel::Trace,
        _ => NoticeLevel::Info,
    }
}

impl ScriptRuntime {
    fn init(&mut self, config: serde_json::Value) -> Result<InitOutput, String> {
        if !self.has_function("init", 1) {
            return Ok(InitOutput::default());
        }
        self.host_state.borrow_mut().init_active = true;
        let result = match self.engine.call_fn::<Dynamic>(
            &mut self.scope,
            &self.ast,
            "init",
            (json_to_dynamic(&config)?,),
        ) {
            Ok(value) if value.is_unit() => Ok(InitOutput::default()),
            Ok(value) => dynamic_to_json(&value)
                .and_then(|value| serde_json::from_value(value).map_err(|e| e.to_string()))
                .and_then(normalize_init_output),
            Err(err) => Err(format!("running init: {err}")),
        };
        self.host_state.borrow_mut().init_active = false;
        result
    }

    fn finish_tool_registration(&mut self) -> Result<(), String> {
        let state = self.host_state.borrow();
        for tool in &state.tools {
            if !self.has_function(tool.handler.fn_name(), 2) {
                return Err(format!(
                    "tool handler `{}` must name a function with 2 parameters",
                    tool.handler.fn_name()
                ));
            }
        }
        self.tools = state
            .tools
            .iter()
            .map(|tool| (tool.spec.name.clone(), tool.handler.clone()))
            .collect();
        Ok(())
    }

    fn tool_register_events(&self) -> Vec<ToolRegister> {
        let state = self.host_state.borrow();
        state
            .tools
            .iter()
            .map(|tool| ToolRegister {
                tool: tool.spec.clone(),
                tool_group: tool.group.as_ref().map(|group_name| {
                    state
                        .groups
                        .get(group_name.as_str())
                        .map(|group| ToolGroup {
                            name: group.name.clone(),
                            prompt_fragment: group.prompt_fragment.clone(),
                        })
                        .unwrap_or_else(|| ToolGroup {
                            name: group_name.clone(),
                            prompt_fragment: None,
                        })
                }),
                prompt_fragment: None,
            })
            .collect()
    }

    fn start(&mut self, config: serde_json::Value, tx: &mpsc::Sender<HarnessInputMessage>) {
        if self.has_function("start", 1) {
            let config = match json_to_dynamic(&config) {
                Ok(config) => config,
                Err(message) => {
                    report_callback_error(tx, format!("preparing start config: {message}"));
                    return;
                }
            };
            match self
                .engine
                .call_fn::<Dynamic>(&mut self.scope, &self.ast, "start", (config,))
            {
                Ok(_) => {}
                Err(err) => report_callback_error(tx, format!("rhai start failed: {err}")),
            }
            return;
        }

        if !self.has_function("start", 0) {
            return;
        }
        match self
            .engine
            .call_fn::<Dynamic>(&mut self.scope, &self.ast, "start", ())
        {
            Ok(_) => {}
            Err(err) => report_callback_error(tx, format!("rhai start failed: {err}")),
        }
    }

    fn on_delivered_event(
        &mut self,
        event: Event,
        replay: bool,
        recorded_at: Option<UnixMicros>,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) {
        // ToolStarted currently carries no provider/extension identity; the
        // harness-routed globally visible tool name is the only ownership
        // signal available to extensions, so Rhai dispatch is necessarily
        // name-based until the protocol grows an owner field.
        if let Event::ToolStarted(started) = &event
            && self.tools.contains_key(&started.tool_name)
        {
            if !replay {
                self.on_tool_started(started.clone(), tx);
            }
            return;
        }
        let event = match serde_json::to_value(event)
            .map_err(|e| e.to_string())
            .and_then(|v| json_to_dynamic(&v))
        {
            Ok(event) => event,
            Err(message) => {
                report_callback_error(tx, format!("preparing on_event: {message}"));
                return;
            }
        };
        let meta = match json_to_dynamic(&meta_json(replay, recorded_at)) {
            Ok(meta) => meta,
            Err(message) => {
                report_callback_error(tx, format!("preparing on_event metadata: {message}"));
                return;
            }
        };
        if !self.has_function("on_event", 2) {
            return;
        }
        match self
            .engine
            .call_fn::<Dynamic>(&mut self.scope, &self.ast, "on_event", (event, meta))
        {
            Ok(_) => {}
            Err(err) => report_callback_error(tx, format!("rhai on_event failed: {err}")),
        }
    }

    fn on_intercept(
        &mut self,
        event: Event,
        transient: bool,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) -> InterceptAction {
        let event = match serde_json::to_value(event)
            .map_err(|e| e.to_string())
            .and_then(|v| json_to_dynamic(&v))
        {
            Ok(event) => event,
            Err(message) => {
                report_callback_error(tx, format!("preparing on_intercept: {message}"));
                return InterceptAction::Pass(None);
            }
        };
        if !self.has_function("on_intercept", 2) {
            return InterceptAction::Pass(None);
        }
        match self.engine.call_fn::<Dynamic>(
            &mut self.scope,
            &self.ast,
            "on_intercept",
            (event, transient),
        ) {
            Ok(value) => parse_intercept_action(value).unwrap_or_else(|message| {
                report_callback_error(tx, format!("invalid on_intercept result: {message}"));
                InterceptAction::Pass(None)
            }),
            Err(err) => {
                report_callback_error(tx, format!("rhai on_intercept failed: {err}"));
                InterceptAction::Pass(None)
            }
        }
    }

    fn on_tool_started(&mut self, started: ToolStarted, tx: &mpsc::Sender<HarnessInputMessage>) {
        let Some(handler) = self.tools.get(&started.tool_name).cloned() else {
            return;
        };
        let args = match cbor_to_json(&started.arguments).and_then(|json| json_to_dynamic(&json)) {
            Ok(args) => args,
            Err(message) => {
                self.emit_tool_error(&started, format!("preparing tool arguments: {message}"), tx);
                return;
            }
        };
        let call = match json_to_dynamic(&tool_call_json(&started)) {
            Ok(call) => call,
            Err(message) => {
                self.emit_tool_error(
                    &started,
                    format!("preparing tool call metadata: {message}"),
                    tx,
                );
                return;
            }
        };
        match handler.call::<Dynamic>(&self.engine, &self.ast, (args, call)) {
            Ok(value) => self.handle_tool_value(value, started, tx),
            Err(err) => {
                self.emit_tool_error(&started, format!("rhai tool handler failed: {err}"), tx)
            }
        }
    }

    fn handle_tool_value(
        &mut self,
        value: Dynamic,
        started: ToolStarted,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) {
        if let Some(job) = value.clone().try_cast::<ShellJob>() {
            let mut state = self.host_state.borrow_mut();
            if let Some(pending) = state.shell_jobs.get_mut(&job.id) {
                if pending.tool_claimed {
                    drop(state);
                    self.emit_tool_error(
                        &started,
                        format!("shell job {} was already returned by a tool call", job.id),
                        tx,
                    );
                    return;
                }
                pending.tool_claimed = true;
                pending.tool_call = Some(PendingToolCall {
                    call_id: started.call_id,
                    tool_name: started.tool_name,
                    tool_type: ToolType::Function,
                    originator: started.originator,
                });
                return;
            }
            drop(state);
            self.emit_tool_error(&started, format!("unknown shell job {}", job.id), tx);
            return;
        }
        match dynamic_to_json(&value) {
            Ok(json) => self.emit_tool_result(&started, tau_proto::json_to_cbor(&json), tx),
            Err(message) => {
                self.emit_tool_error(&started, format!("invalid tool result: {message}"), tx)
            }
        }
    }

    fn on_shell_complete(
        &mut self,
        completion: ShellCompletion,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) {
        let Some(job) = self
            .host_state
            .borrow_mut()
            .shell_jobs
            .remove(&completion.job_id)
        else {
            return;
        };
        let result_dynamic = match json_to_dynamic(&completion.result) {
            Ok(value) => value,
            Err(message) => {
                if let Some(call) = job.tool_call {
                    emit_pending_tool_error(
                        tx,
                        &call,
                        format!("preparing shell result: {message}"),
                    );
                }
                return;
            }
        };
        let job_dynamic = match json_to_dynamic(&shell_job_json(completion.job_id, &job)) {
            Ok(value) => value,
            Err(message) => {
                if let Some(call) = job.tool_call {
                    emit_pending_tool_error(tx, &call, format!("preparing shell job: {message}"));
                }
                return;
            }
        };
        let had_callback = job.on_complete.is_some();
        let outcome = if let Some(callback) = job.on_complete {
            callback.call::<Dynamic>(&self.engine, &self.ast, (result_dynamic, job_dynamic))
        } else {
            Ok(Dynamic::UNIT)
        };
        match (outcome, job.tool_call) {
            (Ok(value), Some(call)) if value.clone().try_cast::<ShellJob>().is_some() => {
                let chained = value.cast::<ShellJob>();
                if let Some(pending) = self.host_state.borrow_mut().shell_jobs.get_mut(&chained.id)
                {
                    if pending.tool_claimed {
                        emit_pending_tool_error(
                            tx,
                            &call,
                            format!(
                                "chained shell job {} was already returned by a tool call",
                                chained.id
                            ),
                        );
                    } else {
                        pending.tool_claimed = true;
                        pending.tool_call = Some(call);
                    }
                } else {
                    emit_pending_tool_error(
                        tx,
                        &call,
                        format!("unknown chained shell job {}", chained.id),
                    );
                }
            }
            (Ok(value), Some(call)) => {
                let json = if value.is_unit() && !had_callback {
                    completion.result
                } else {
                    match dynamic_to_json(&value) {
                        Ok(json) => json,
                        Err(message) => {
                            emit_pending_tool_error(
                                tx,
                                &call,
                                format!("invalid shell callback result: {message}"),
                            );
                            return;
                        }
                    }
                };
                emit_pending_tool_result(tx, &call, tau_proto::json_to_cbor(&json));
            }
            (Ok(_), None) => {}
            (Err(err), Some(call)) => {
                emit_pending_tool_error(tx, &call, format!("rhai shell callback failed: {err}"))
            }
            (Err(err), None) => {
                report_callback_error(tx, format!("rhai shell callback failed: {err}"))
            }
        }
    }

    fn emit_tool_result(
        &self,
        started: &ToolStarted,
        result: CborValue,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) {
        let call = pending_call_from_started(started);
        emit_pending_tool_result(tx, &call, result);
    }

    fn emit_tool_error(
        &self,
        started: &ToolStarted,
        message: String,
        tx: &mpsc::Sender<HarnessInputMessage>,
    ) {
        let call = pending_call_from_started(started);
        emit_pending_tool_error(tx, &call, message);
    }

    fn has_function(&self, name: &str, params: usize) -> bool {
        self.ast
            .iter_functions()
            .any(|f| f.name == name && f.params.len() == params)
    }

    fn has_pending_shell_jobs(&self) -> bool {
        !self.host_state.borrow().shell_jobs.is_empty()
    }

    fn cancel_and_join_pending_shell_jobs(&self) {
        let jobs: Vec<_> = self
            .host_state
            .borrow_mut()
            .shell_jobs
            .drain()
            .map(|(_, job)| job)
            .collect();
        for job in &jobs {
            job.cancel.cancel();
        }
        for job in jobs {
            join_shell_worker_bounded(job);
        }
    }
}

impl Drop for ScriptRuntime {
    fn drop(&mut self) {
        self.cancel_and_join_pending_shell_jobs();
    }
}

fn join_shell_worker_bounded(job: PendingShellJob) {
    let deadline = Instant::now() + SHELL_SHUTDOWN_JOIN_TIMEOUT;
    while !job.join_handle.is_finished() && Instant::now() < deadline {
        job.cancel.cancel();
        std::thread::sleep(Duration::from_millis(10));
    }
    if job.join_handle.is_finished() {
        if let Err(err) = job.join_handle.join() {
            tracing::warn!(target: LOG_TARGET, ?err, "shell worker panicked during shutdown");
        }
    } else {
        tracing::warn!(
            target: LOG_TARGET,
            command = %job.command,
            "shell worker did not finish within shutdown timeout after cancellation"
        );
    }
}

fn report_callback_error(tx: &mpsc::Sender<HarnessInputMessage>, message: String) {
    tracing::warn!(target: LOG_TARGET, error = %message, "rhai callback failed");
    enqueue_info(tx, &message, NoticeLevel::Warning, true);
}

fn meta_json(replay: bool, recorded_at: Option<UnixMicros>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("replay".to_owned(), serde_json::Value::Bool(replay));
    if let Some(recorded_at) = recorded_at {
        map.insert("recorded_at".to_owned(), u64_meta_value(recorded_at.get()));
    }
    serde_json::Value::Object(map)
}

fn u64_meta_value(value: u64) -> serde_json::Value {
    if let Ok(value) = i64::try_from(value) {
        serde_json::Value::Number(value.into())
    } else {
        serde_json::Value::String(value.to_string())
    }
}

fn pending_call_from_started(started: &ToolStarted) -> PendingToolCall {
    PendingToolCall {
        call_id: started.call_id.clone(),
        tool_name: started.tool_name.clone(),
        tool_type: ToolType::Function,
        originator: started.originator.clone(),
    }
}

fn emit_pending_tool_result(
    tx: &mpsc::Sender<HarnessInputMessage>,
    call: &PendingToolCall,
    result: CborValue,
) {
    let _ = tx.send(HarnessInputMessage::emit(Event::ToolResult(ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        tool_type: call.tool_type,
        result,
        kind: ToolResultKind::Final,
        display: None,
        originator: call.originator.clone(),
    })));
}

fn emit_pending_tool_error(
    tx: &mpsc::Sender<HarnessInputMessage>,
    call: &PendingToolCall,
    message: String,
) {
    let _ = tx.send(HarnessInputMessage::emit(Event::ToolError(ToolError {
        call_id: call.call_id.clone(),
        tool_name: call.tool_name.clone(),
        tool_type: call.tool_type,
        message,
        details: None,
        display: None,
        originator: call.originator.clone(),
    })));
}

fn tool_call_json(started: &ToolStarted) -> serde_json::Value {
    serde_json::json!({
        "call_id": started.call_id.as_str(),
        "tool_name": started.tool_name.as_str(),
        "agent_id": started.agent_id.as_str(),
        "originator": started.originator,
        "tool_type": ToolType::Function,
    })
}

fn shell_job_json(id: i64, job: &PendingShellJob) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), serde_json::Value::Number(id.into()));
    map.insert(
        "command".to_owned(),
        serde_json::Value::String(job.command.clone()),
    );
    if let Some(tag) = &job.tag
        && let Ok(tag) = dynamic_to_json(tag)
    {
        map.insert("tag".to_owned(), tag);
    }
    serde_json::Value::Object(map)
}

fn cbor_to_json(value: &CborValue) -> Result<serde_json::Value, String> {
    match value {
        CborValue::Null => Ok(serde_json::Value::Null),
        CborValue::Bool(v) => Ok(serde_json::Value::Bool(*v)),
        CborValue::Integer(v) => i64::try_from(*v)
            .map(|v| serde_json::Value::Number(v.into()))
            .map_err(|_| "CBOR integer is outside JSON integer range".to_owned()),
        CborValue::Float(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "CBOR float must be finite".to_owned()),
        CborValue::Text(v) => Ok(serde_json::Value::String(v.clone())),
        CborValue::Bytes(_) => Err("CBOR bytes are not JSON-compatible".to_owned()),
        CborValue::Array(values) => values
            .iter()
            .map(cbor_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let CborValue::Text(key) = key else {
                    return Err("CBOR map key is not a string".to_owned());
                };
                map.insert(key.clone(), cbor_to_json(value)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => Err("unsupported CBOR value".to_owned()),
    }
}
fn parse_intercept_action(value: Dynamic) -> Result<InterceptAction, String> {
    if value.is_unit() {
        return Ok(InterceptAction::Pass(None));
    }
    if let Some(s) = value.clone().try_cast::<ImmutableString>() {
        return match s.as_str() {
            "pass" => Ok(InterceptAction::Pass(None)),
            "drop" => Ok(InterceptAction::Drop),
            other => Err(format!("unknown action string `{other}`")),
        };
    }
    let json = dynamic_to_json(&value)?;
    let obj = json
        .as_object()
        .ok_or_else(|| "result must be (), string, or map".to_owned())?;
    let kind = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "result map needs string `kind`".to_owned())?;
    match kind {
        "pass" => {
            let event = obj
                .get("event")
                .cloned()
                .map(serde_json::from_value::<Event>)
                .transpose()
                .map_err(|e| e.to_string())?;
            Ok(InterceptAction::Pass(event.map(Box::new)))
        }
        "drop" => Ok(InterceptAction::Drop),
        other => Err(format!("unknown action kind `{other}`")),
    }
}

fn json_to_dynamic(value: &serde_json::Value) -> Result<Dynamic, String> {
    match value {
        serde_json::Value::Null => Ok(Dynamic::UNIT),
        serde_json::Value::Bool(v) => Ok(Dynamic::from_bool(*v)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Dynamic::from_int(i as rhai::INT))
            } else if let Some(u) = n.as_u64() {
                if let Ok(i) = rhai::INT::try_from(u) {
                    Ok(Dynamic::from_int(i))
                } else {
                    Ok(Dynamic::from(u.to_string()))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(Dynamic::from_float(f as rhai::FLOAT))
            } else {
                Err("unsupported JSON number".to_owned())
            }
        }
        serde_json::Value::String(v) => Ok(Dynamic::from(v.clone())),
        serde_json::Value::Array(values) => values
            .iter()
            .map(json_to_dynamic)
            .collect::<Result<Array, _>>()
            .map(Dynamic::from_array),
        serde_json::Value::Object(values) => {
            let mut map = Map::new();
            for (key, value) in values {
                map.insert(key.as_str().into(), json_to_dynamic(value)?);
            }
            Ok(Dynamic::from_map(map))
        }
    }
}

fn dynamic_to_json(value: &Dynamic) -> Result<serde_json::Value, String> {
    if value.is_unit() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(v) = value.clone().try_cast::<bool>() {
        return Ok(serde_json::Value::Bool(v));
    }
    if let Some(v) = value.clone().try_cast::<rhai::INT>() {
        return Ok(serde_json::Value::Number(v.into()));
    }
    if let Some(v) = value.clone().try_cast::<rhai::FLOAT>() {
        return serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "float must be finite".to_owned());
    }
    if let Some(v) = value.clone().try_cast::<ImmutableString>() {
        return Ok(serde_json::Value::String(v.to_string()));
    }
    if let Some(values) = value.clone().try_cast::<Array>() {
        return values
            .iter()
            .map(dynamic_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array);
    }
    if let Some(values) = value.clone().try_cast::<Map>() {
        let mut map = serde_json::Map::new();
        for (key, value) in values {
            map.insert(key.to_string(), dynamic_to_json(&value)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    Err(format!(
        "unsupported Rhai value type `{}`",
        value.type_name()
    ))
}
