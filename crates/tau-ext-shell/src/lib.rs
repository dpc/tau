//! Filesystem and shell tool extension.
//!
//! Provides `read`, `edit`, `replace`, `apply_patch`, `dir_lock`, `grep`,
//! `find`, `ls`, `workdir`, `shell`, and `gpt_shell` tools.
//!
//! The `echo` tool is available under `cfg(test)` or the
//! `echo-agent` cargo feature for harness-side echo-agent tests.

use std::collections::HashMap;
use std::error::Error;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{
    ActionError, ActionInvoke, ActionOutput, ActionResult, AgentContextKey, AgentContextValue,
    CborValue, DiscoveryAgentsFile, DiscoveryModifiedMicros, DiscoverySkillCandidate, Event,
    ExtAgentContextPublish, ExtensionAgentDiscoverySnapshotDeclared, ExtensionContextReady,
    ExtensionSessionContextReady, ExtensionSessionDiscoverySnapshotDeclared, HarnessInputMessage,
    PromptContent, PromptFragment, PromptPriority, SessionAgentLoaded, SessionStarted,
    ToolCancelled, ToolExample, ToolExampleSelector, ToolResult, ToolResultKind, ToolSpec, ToolTag,
};
use tracing::{debug, trace};

#[cfg(test)]
static DETACHED_OUTPUT_OVERLOAD_NOTIFY: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);

use crate::tools::{shell as path_crate_tools_shell, world as path_crate_tools_world};
use crate::{
    dir_lock as path_crate_dir_lock, display as path_crate_display, tools as path_crate_tools,
};

mod agents;
mod argument;
mod config;
mod cwd_state;
mod diff;
mod dir_lock;
mod display;
mod isolation;
#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
mod pty_stdio;
mod runtime;
mod scheduler;
mod shell_output_spool;
mod shell_process;
mod tool_lifecycle;
mod tools;
mod truncate;

#[cfg(test)]
mod tests;

use crate::agents::{ancestor_dirs, discover_session_agents_files};
use crate::config::{ExtConfig, ShellConfig};
use crate::cwd_state::{CwdState, WorkdirSnapshot};
use crate::dir_lock::{DIR_LOCK_TOOL_NAME, DirLockManager};
use crate::runtime::ShellRuntime;
use crate::scheduler::{WorkMeta, WorkPriority, WorkScheduler};
use crate::tool_lifecycle::{ToolCancellationState, ToolLifecycle};
#[cfg(any(test, feature = "echo-agent"))]
use crate::tools::ECHO_TOOL_NAME;
use crate::tools::shell::{ShellAccessMode, ShellCommandMode};
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME, GREP_TOOL_NAME,
    LS_TOOL_NAME, READ_IMAGE_TOOL_NAME, READ_TOOL_NAME, REPLACE_TOOL_NAME, SHELL_TOOL_NAME,
    WORKDIR_TOOL_NAME, execute_tool,
};

/// Cloneable shell output adapter.
///
/// Production optional worker, progress, and diagnostic output uses
/// tau-client's detached enqueue path so shell workers do not block on protocol
/// flush. Sole terminals, user-shell completion, discovery, context,
/// prerequisite metadata, and readiness use checked writes instead. The shared
/// sticky failure state wakes the manual loop and prevents worker cleanup from
/// releasing ownership after a failed terminal. Tests can use an mpsc-backed
/// adapter for direct state-machine coverage.
#[derive(Clone)]
pub(crate) struct Output {
    /// Production client or test channel receiving output frames.
    inner: OutputInner,
    /// Optional local-to-wire tool-name mapping for one scoped invocation.
    tool_name_scope: Option<(tau_proto::ToolName, tau_proto::ToolName)>,
    /// First mandatory-output failure shared with the manual policy loop.
    failure: Arc<Mutex<MandatoryOutputFailure>>,
}

/// Cross-thread notification that a checked protocol write failed.
#[derive(Default)]
struct MandatoryOutputFailure {
    /// First failure retained until the policy loop observes it.
    message: Option<String>,
    /// Sticky marker preserving worker ownership after loop observation.
    failed: bool,
    /// Manual-loop wake handle installed after tau-client startup.
    waker: Option<tau_client::ManualRuntimeWaker>,
}

/// Backend used by [`Output`] to publish protocol frames.
#[derive(Clone)]
enum OutputInner {
    /// Production tau-client writer handle.
    Client(tau_client::ClientHandle),
    #[cfg(test)]
    /// Direct unit-test channel.
    Channel(mpsc::Sender<HarnessInputMessage>),
}

impl Output {
    fn client(handle: tau_client::ClientHandle) -> Self {
        Self {
            inner: OutputInner::Client(handle),
            tool_name_scope: None,
            failure: Arc::default(),
        }
    }

    #[cfg(test)]
    fn channel(tx: mpsc::Sender<HarnessInputMessage>) -> Self {
        Self {
            inner: OutputInner::Channel(tx),
            tool_name_scope: None,
            failure: Arc::default(),
        }
    }

    fn scoped_tool(&self, local: tau_proto::ToolName, wire: tau_proto::ToolName) -> Self {
        Self {
            inner: self.inner.clone(),
            tool_name_scope: Some((local, wire)),
            failure: Arc::clone(&self.failure),
        }
    }

    fn scope_tool_name(&self, tool_name: &mut tau_proto::ToolName) {
        if let Some((local, wire)) = &self.tool_name_scope
            && tool_name == local
        {
            *tool_name = wire.clone();
        }
    }

    fn send(&self, mut message: HarnessInputMessage) -> tau_client::ClientResult<()> {
        self.scope_message(&mut message);
        let result = match &self.inner {
            OutputInner::Client(handle) => handle.send_detached(message),
            #[cfg(test)]
            OutputInner::Channel(tx) => tx
                .send(message)
                .map_err(|_| tau_client::ClientError::WriterClosed),
        };
        #[cfg(test)]
        if matches!(result, Err(tau_client::ClientError::Overloaded))
            && let Some(notify) = DETACHED_OUTPUT_OVERLOAD_NOTIFY
                .lock()
                .expect("detached overload notification")
                .as_ref()
        {
            let _ = notify.send(());
        }
        result
    }

    /// Sends mandatory lifecycle traffic in order and waits for writer flush.
    fn send_checked(&self, mut message: HarnessInputMessage) -> tau_client::ClientResult<()> {
        self.scope_message(&mut message);
        let result = match &self.inner {
            OutputInner::Client(handle) => handle.send(message),
            #[cfg(test)]
            OutputInner::Channel(tx) => tx
                .send(message)
                .map_err(|_| tau_client::ClientError::WriterClosed),
        };
        self.retain_mandatory_failure(result)
    }

    fn scope_message(&self, message: &mut HarnessInputMessage) {
        if let HarnessInputMessage::Emit(emit) = message {
            let tool_name = match emit.event.as_mut() {
                Event::ToolProgressReported(event) => Some(&mut event.tool_name),
                Event::ToolResultReported(event) => Some(&mut event.tool_name),
                Event::ToolResult(event) => Some(&mut event.tool_name),
                Event::ToolErrorReported(event) => Some(&mut event.tool_name),
                Event::ToolError(event) => Some(&mut event.tool_name),
                Event::ToolCancelledReported(event) => Some(&mut event.tool_name),
                Event::ToolCancelled(event) => Some(&mut event.tool_name),
                _ => None,
            };
            if let Some(tool_name) = tool_name {
                self.scope_tool_name(tool_name);
            }
        }
    }

    /// Submit one transient tool progress observation.
    fn report_tool_progress(
        &self,
        progress: tau_proto::ToolProgress,
    ) -> tau_client::ClientResult<()> {
        self.send(HarnessInputMessage::emit_with_persist(
            Event::ToolProgressReported(progress),
            false,
        ))
    }

    /// Submit one terminal tool outcome through the typed client report helper.
    fn report_tool_terminal(&self, event: Event) -> tau_client::ClientResult<()> {
        let mut outcome = tau_client::ToolTerminalOutcome::try_from(event).map_err(|event| {
            tau_client::ClientError::handler(format!(
                "terminal report helper received {}",
                event.name()
            ))
        })?;
        self.scope_tool_name(outcome.tool_name_mut());
        let result = match &self.inner {
            OutputInner::Client(handle) => handle.report_tool_terminal(outcome),
            #[cfg(test)]
            OutputInner::Channel(tx) => tx
                .send(HarnessInputMessage::emit_with_persist(
                    outcome.into_reported_event(),
                    false,
                ))
                .map_err(|_| tau_client::ClientError::WriterClosed),
        };
        self.retain_mandatory_failure(result)
    }

    /// Installs the wake handle used when a worker observes checked-output
    /// failure.
    fn install_waker(&self, waker: tau_client::ManualRuntimeWaker) {
        self.failure
            .lock()
            .expect("mandatory output failure lock poisoned")
            .waker = Some(waker);
    }

    /// Returns the first worker-side mandatory-output failure.
    fn take_mandatory_failure(&self) -> tau_client::ClientResult<()> {
        let message = self
            .failure
            .lock()
            .expect("mandatory output failure lock poisoned")
            .message
            .take();
        match message {
            Some(message) => Err(tau_client::ClientError::handler(message)),
            None => Ok(()),
        }
    }

    /// Returns whether any checked output has failed before loop teardown.
    fn mandatory_output_failed(&self) -> bool {
        self.failure
            .lock()
            .expect("mandatory output failure lock poisoned")
            .failed
    }

    /// Records a checked-output failure and wakes the policy loop.
    fn retain_mandatory_failure(
        &self,
        result: tau_client::ClientResult<()>,
    ) -> tau_client::ClientResult<()> {
        if let Err(error) = &result {
            let waker = {
                let mut failure = self
                    .failure
                    .lock()
                    .expect("mandatory output failure lock poisoned");
                failure.message.get_or_insert_with(|| error.to_string());
                failure.failed = true;
                failure.waker.clone()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        result
    }

    fn register_local_tool(
        &self,
        registration: tau_proto::ToolRegistrationDeclared,
    ) -> tau_client::ClientResult<()> {
        match &self.inner {
            OutputInner::Client(handle) => handle.register_local_tool(registration),
            #[cfg(test)]
            OutputInner::Channel(tx) => tx
                .send(HarnessInputMessage::emit_with_persist(
                    Event::ToolRegistrationDeclared(registration),
                    false,
                ))
                .map_err(|_| tau_client::ClientError::WriterClosed),
        }
    }
}

fn tool_tags(tags: &[&str]) -> Vec<ToolTag> {
    tags.iter().map(|tag| ToolTag::new(*tag)).collect()
}

fn example_field(name: &str, value: CborValue) -> (CborValue, CborValue) {
    (CborValue::Text(name.to_owned()), value)
}

fn example_text(value: &str) -> CborValue {
    CborValue::Text(value.to_owned())
}

fn example_int(value: i64) -> CborValue {
    CborValue::Integer(value.into())
}

const SHELL_DIR_FORCE_UNLOCK_ACTION_ID: &str = "shell.dir.force_unlock";

const SLOW_LOCK_WAIT_THRESHOLD_SECS: u64 = 5;
const LOCK_WAIT_DURATION_SECONDS_HEADER: &str = "lock_wait_duration_seconds";
const XDG_USER_SKILL_SOURCE_PRECEDENCE: u32 = 0;
const LEGACY_USER_SKILL_SOURCE_PRECEDENCE: u32 = 1;

#[derive(Clone, Copy)]
enum DiscoverySourcePolicy {
    Environment,
    #[cfg(any(test, feature = "echo-agent"))]
    EmptyFixture,
}

impl DiscoverySourcePolicy {
    const fn reads_environment(self) -> bool {
        matches!(self, Self::Environment)
    }
}

enum RuntimeCwdSource {
    Process,
    #[cfg(any(test, feature = "echo-agent"))]
    Fixture(PathBuf),
}

/// One discovery pass split into mandatory state and optional session-only
/// diagnostics.
struct DiscoveryScan {
    /// Complete discovery state published for the session or one agent.
    snapshot: ExtensionSessionDiscoverySnapshotDeclared,
    /// Best-effort diagnostic notices published only during session discovery.
    diagnostics: Vec<HarnessInputMessage>,
}

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run_impl(
        std::io::stdin(),
        std::io::stdout(),
        DiscoverySourcePolicy::Environment,
        RuntimeCwdSource::Process,
    )
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The test-only `echo` tool is registered when built with
/// `cfg(test)` or the `echo-agent` cargo feature.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_impl(
        reader,
        writer,
        DiscoverySourcePolicy::Environment,
        RuntimeCwdSource::Process,
    )
}

/// Runs an in-process harness test extension without discovering caller-owned
/// skills or `AGENTS.md` files.
///
/// Harness unit tests share one process, so they cannot safely rewrite
/// process-wide HOME, XDG, or working-directory state. This entrypoint keeps
/// their synthetic extension from reading those ambient discovery inputs while
/// preserving the ordinary protocol and tool behavior.
#[cfg(any(test, feature = "echo-agent"))]
pub fn run_for_test_harness<R, W>(
    reader: R,
    writer: W,
    fixture_cwd: PathBuf,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_impl(
        reader,
        writer,
        DiscoverySourcePolicy::EmptyFixture,
        RuntimeCwdSource::Fixture(fixture_cwd),
    )
}

fn registered_tool_specs(dir_lock_enabled: bool) -> Vec<ToolSpec> {
    #[cfg(any(test, feature = "echo-agent"))]
    let echo_tool = Some(ToolSpec {
        name: tau_proto::ToolName::new(ECHO_TOOL_NAME),
        model_visible_name: None,
        description: Some("Echo the provided payload unchanged".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: None,
        format: None,
        tags: tool_tags(&["test:echo"]),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    });
    #[cfg(not(any(test, feature = "echo-agent")))]
    let echo_tool: Option<ToolSpec> = None;
    let mut tools = Vec::new();
    if let Some(echo_tool) = echo_tool {
        tools.push(echo_tool);
    }
    let read_tool = ToolSpec {
        name: tau_proto::ToolName::new(READ_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Reads a file. Defaults to reading the whole file in one call — \
             output is capped at 2000 lines / 10 KiB. Truncated output keeps \
             the first 1000 and last 1000 lines separated by a literal `...` line. \
             Files over 10 MiB are rejected by an input safety cap before output truncation. \
             Prefer one full read. Pass inclusive `start_line`/`end_line` only to \
             fetch one specific known slice, or `ranges` for up to 100 slices; \
             range chunks are separated by one empty line and may overlap, but large overlapping \
             multi-range expansions can be rejected before rendering to keep memory bounded. `start_line` past EOF errors, \
             while `end_line` past EOF returns available lines. Returned content lines are prefixed \
             by their 1-based line number and a space; \
             CRLF, CR, and missing final line endings are marked after the number, e.g. \
             `2(crlf)`, `3(cr)`, or `4(no_nl)`. Invalid UTF-8 is shown with \
             Unicode replacement characters and an `invalid-utf8` line flag. Lines that would exceed \
             the 10 KiB visible output budget are marker-only, e.g. `1(truncated)`. Truncated results include `truncated: true`, `total_lines`, \
             and `total_bytes`, plus a private path to bounded saved output (or `saved_output_unavailable: true` when private storage fails); `valid_utf8: false` is included only when applicable."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional, 1-based inclusive. Omit to start at line 1 (the default)."
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional, 1-based inclusive. Omit to read to end of file (the default and preferred mode). Set this only to continue past a previous truncation, or to fetch a known specific slice of a large file — do NOT pre-slice an ordinary file you haven't already established is large."
                },
                "ranges": {
                    "type": "array",
                    "description": "Optional list of inclusive line ranges to read. Cannot be combined with top-level start_line or end_line. Each chunk is separated by one empty line in the output, and overlapping ranges are returned redundantly. Requests whose overlapping ranges would expand into too much rendered content are rejected before rendering.",
                    "minItems": 1,
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-based inclusive start line to read."
                            },
                            "end_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-based inclusive end line to read."
                            }
                        },
                        "required": ["start_line", "end_line"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&["shell:read", tau_proto::TURN_DATA_FETCH_TOOL_TAG]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "read-file".to_owned(),
            title: Some("Read a file".to_owned()),
            arguments: CborValue::Map(vec![example_field("path", example_text("src/main.rs"))]),
            note: Some("Use only the path field for a full-file read.".to_owned()),
            subcommand: None,
        }],
    };
    let read_image_tool = ToolSpec {
        name: tau_proto::ToolName::new(READ_IMAGE_TOOL_NAME),
        model_visible_name: None,
        description: Some("Read one local image for visual inspection.".to_owned()),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to one local PNG, JPEG, or WebP image"
                },
                "mode": {
                    "type": "string",
                    "enum": ["high", "overview"],
                    "default": "high",
                    "description": "Local preparation profile. `high` (the default) preserves the existing 2048-side/2500-patch bounds. `overview` is experimental, intended only for coarse inspection, and is bounded to 1024 pixels on a side and 600 32px patches."
                },
                "region": {
                    "type": "object",
                    "description": "Optional crop in pixels of the EXIF-oriented source raster, before mode resizing. Uses a top-left origin and half-open extents.",
                    "properties": {
                        "x": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": u32::MAX
                        },
                        "y": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": u32::MAX
                        },
                        "width": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": u32::MAX
                        },
                        "height": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": u32::MAX
                        }
                    },
                    "required": ["x", "y", "width", "height"],
                    "additionalProperties": false
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:read",
            "shell:read:image",
            "provider-content:image",
            tau_proto::TURN_DATA_FETCH_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Never),
        examples: vec![ToolExample {
            id: "read-image".to_owned(),
            title: Some("Inspect a screenshot".to_owned()),
            arguments: CborValue::Map(vec![example_field("path", example_text("screenshot.png"))]),
            note: None,
            subcommand: None,
        }],
    };
    let edit_tool = ToolSpec {
        name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Edit a file using line-oriented replacements. Each edit fully replaces \
             the 1-based half-open `start_line`..`end_line_exclusive` range \
             with `newText`. `start_line` is included and `end_line_exclusive` \
             is excluded. Empty insertion ranges use \
             `start_line == end_line_exclusive`; for example, `1..<1` inserts \
             at the start of the file and `total_lines + 1 ..< total_lines + 1` \
             appends at EOF. All ranges use the original file numbering as if \
             applied simultaneously. Non-empty replacements are kept as whole \
             lines. Ranges must be non-overlapping. Missing files are treated as \
             empty and missing parent directories are created. Per-edit `context_line` \
             must exactly match the original content of `start_line`. Use an empty \
             context_line when `start_line` is the append slot past the end of the \
             file."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                },
                "edits": {
                    "type": "array",
                    "description": "One or more line ranges to replace in the original file",
                    "minItems": 1,
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "start_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-based included start line or insertion slot. Use 1 for the start of the file. To append at EOF, use total_lines + 1. Use together with end_line_exclusive."
                            },
                            "end_line_exclusive": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-based excluded end line or insertion slot. Empty insertion ranges have end_line_exclusive == start_line. To replace read output lines A through B, use start_line A and end_line_exclusive B + 1. Use together with start_line."
                            },
                            "newText": {
                                "type": "string",
                                "description": "Replacement text. Non-empty replacements stay whole-line."
                            },
                            "context_line": {
                                "type": "string",
                                "description": "Exact expected content of the original start_line, including spaces and tabs. Use an empty context_line when start_line is the append slot past the end of the file. If it does not match, the edit fails and returns current line-numbered context around the expected context line."
                            }
                        },
                        "required": ["start_line", "end_line_exclusive", "newText", "context_line"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:edit",
            "shell:edit:line",
            "shell:mutates-files",
            tau_proto::TURN_MANIPULATOR_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "replace-lines".to_owned(),
            title: Some("Replace one line".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("path", example_text("src/main.rs")),
                (
                    CborValue::Text("edits".to_owned()),
                    CborValue::Array(vec![CborValue::Map(vec![
                        example_field("start_line", example_int(10)),
                        example_field("end_line_exclusive", example_int(11)),
                        example_field("newText", example_text("replacement line")),
                        example_field("context_line", example_text("line being replaced")),
                    ])]),
                ),
            ]),
            note: Some("end_line_exclusive is one past the last line replaced.".to_owned()),
            subcommand: None,
        }],
    };
    let apply_patch_tool = ToolSpec {
        name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
        model_visible_name: None,
        description: Some("Use the `apply_patch` tool to edit files.".to_owned()),
        tool_type: tau_proto::ToolType::Custom,
        parameters: None,
        format: Some(tau_proto::ToolFormat::Text),
        tags: tool_tags(&[
            "shell:edit",
            "shell:edit:apply_patch",
            "shell:mutates-files",
            tau_proto::TURN_MANIPULATOR_TOOL_TAG,
        ]),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    };
    let replace_tool = ToolSpec {
        name: tau_proto::ToolName::new(REPLACE_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(EDIT_TOOL_NAME)),
        description: Some(
            "Replace exact text in one existing UTF-8 file. Each oldText must occur exactly \
             once in the same original file snapshot; all edits apply atomically. Matching \
             ignores only an initial UTF-8 BOM and normalizes CRLF/CR to LF. Use newText \
             as an empty string to delete text."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "minLength": 1 },
                            "newText": { "type": "string" }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:edit",
            "shell:edit:replace",
            "shell:mutates-files",
            tau_proto::TURN_MANIPULATOR_TOOL_TAG,
        ]),
        enabled_by_default: false,
        background_support: None,
        examples: Vec::new(),
    };
    let dir_lock_tool = dir_lock_tool_spec(dir_lock_enabled);
    let grep_tool = ToolSpec {
        name: tau_proto::ToolName::new(GREP_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Search file contents for a pattern using ripgrep. Patterns are literal by default; \
             regex metacharacters like `|` require `regex: true`. Returns matching lines \
             with file paths and line numbers. Respects .gitignore. Output is truncated at \
             `limit` matches or 10 KiB of visible output. Visible-cap truncation provides a private saved-output path, or explicit unavailable metadata when storage fails; limit-only and per-line truncation retain native metadata. Long lines are truncated to 500 chars."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern. Treated as a literal string by default. Set `regex: true` to interpret as a regex."
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.rs'"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default: false)"
                },
                "regex": {
                    "type": "boolean",
                    "description": "Interpret `pattern` as a regex instead of a literal string (default: false)"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of lines to show before and after each match (default: 0, max: 20)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default: 100, max: 2000)"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:read",
            "shell:search",
            tau_proto::TURN_DATA_FETCH_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "search-literal".to_owned(),
            title: Some("Search literal text".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("pattern", example_text("TODO")),
                example_field("path", example_text("src")),
                example_field("glob", example_text("**/*.rs")),
            ]),
            note: Some("Set regex=true only when pattern is a regular expression.".to_owned()),
            subcommand: None,
        }],
    };
    let find_tool = ToolSpec {
        name: tau_proto::ToolName::new(FIND_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Search for files by glob pattern. Returns only file paths (directories are \
             never included, even with '**/*') relative to the search directory. Respects \
             .gitignore. Output is truncated at `limit` results or 10 KiB of visible output. Visible-cap truncation provides a private saved-output path, or explicit unavailable metadata when storage fails; limit-only truncation retains native metadata. Use the ls tool \
             if you want to see directory entries."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern matched against file paths relative to `path`. `**` matches any number of intermediate directories, including zero — so `**/*.rs` finds both top-level `a.rs` and nested `src/a.rs`. Directories are not returned, even with `**/*`."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 1000, max: 2000)"
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:read",
            "shell:search",
            tau_proto::TURN_DATA_FETCH_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "find-rust-files".to_owned(),
            title: Some("Find files by glob".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("pattern", example_text("**/*.rs")),
                example_field("path", example_text("crates")),
            ]),
            note: None,
            subcommand: None,
        }],
    };
    let ls_tool = ToolSpec {
        name: tau_proto::ToolName::new(LS_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix \
             for directories. Includes dotfiles. Output lines are prefixed with 1-based \
             entry numbers plus flags such as `escaped`, `invalid-utf8`, or `truncated`; \
             output is capped at `limit` entries, 2000 lines, or 10 KiB of visible output with saved-output metadata and standard truncation headers. \
             When `limit_reached` is true, entries are a bounded filesystem-order sample sorted \
             for display, not a complete alphabetic prefix."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of entries to return (default: 500, max: 2001)"
                }
            },
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:read",
            "shell:list",
            tau_proto::TURN_DATA_FETCH_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "list-directory".to_owned(),
            title: Some("List a directory".to_owned()),
            arguments: CborValue::Map(vec![example_field("path", example_text("src"))]),
            note: None,
            subcommand: None,
        }],
    };
    let workdir_tool = ToolSpec {
        name: tau_proto::ToolName::new(WORKDIR_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Read or change your durable workdir. Omit `path` \
             to read the current path and availability. A provided path is resolved from the \
             last committed workdir, validated, canonicalized, and persisted. Do not combine a \
             workdir change with shell or filesystem calls that rely on the new directory."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string", "minLength": 1, "description": "Optional directory to persist as this instance's workdir" } },
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&["shell:workdir", tau_proto::TURN_MANIPULATOR_TOOL_TAG]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "change-directory".to_owned(),
            title: Some("Change directory".to_owned()),
            arguments: CborValue::Map(vec![example_field("path", example_text("crates/tau"))]),
            note: None,
            subcommand: None,
        }],
    };
    let shell_tool = ToolSpec {
        name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Execute a shell command via `sh -c`. When directory locking is enabled, commands \
             are inferred read-write only while the agent holds a matching `dir_lock`; otherwise \
             they are read-only. When directory locking is disabled, shell commands run read-write. \
             Non-zero exits and timeouts are returned as structured command results with output details. \
             Model-visible output is capped at 2000 lines / \
             15 KiB; truncated output keeps the first 1000 and last 1000 lines \
             separated by a literal `...` line. Output lines are prefixed with `out ` \
             for stdout or `err ` for stderr; missing trailing newlines are marked, e.g. \
             `out(no_nl)`; CRLF and CR line endings are marked as `out(crlf)` \
             or `out(cr)`. Invalid UTF-8 is shown with Unicode replacement characters and \
             an `invalid-utf8` line flag. Lines that would exceed the 15 KiB output budget \
             are marker-only, e.g. `err(truncated)`. Truncated results include complete totals, a warning, and normally an exact temporary path to up to 16 MiB of rendered output; output beyond that saved cap is explicitly marked incomplete, while platforms or filesystems that cannot enforce private storage report `saved_output_unavailable: true`. \
             Stdin is closed and commands cannot receive interactive input. Stdout and stderr may be TTY-backed even though no controlling terminal exists. Use explicit noninteractive flags/messages; do not launch prompts, pagers, or editors. \
             Commands taking longer than 5 seconds include duration metadata. Prefer dedicated \
             tools like `read`, `grep`, and `find` when they fit."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 300"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for this invocation only. Relative paths resolve from this shell instance's remembered workdir; omission uses the remembered workdir. This does not change later calls; use workdir in an earlier turn to change later calls."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:exec",
            "shell:exec:generic",
            tau_proto::TURN_MANIPULATOR_TOOL_TAG,
        ]),
        enabled_by_default: true,
        background_support: None,
        examples: vec![ToolExample {
            id: "run-command".to_owned(),
            title: Some("Run a command".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("command", example_text("cargo test -p tau-core")),
                example_field("timeout", example_int(300)),
            ]),
            note: Some("For file edits, prefer apply_patch when available.".to_owned()),
            subcommand: None,
        }],
    };
    let gpt_shell_tool = ToolSpec {
        name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new("shell_command")),
        description: Some(
            "Run a shell command. Model-visible output is capped at 2000 lines / 15 KiB; \
             truncated results normally provide an exact temporary path to up to 16 MiB of rendered output and mark an incomplete saved artifact honestly; private-storage failures instead report `saved_output_unavailable: true`. \
             Output lines are prefixed with `out ` for stdout or `err ` for stderr; missing \
             trailing newlines are marked with `(no_nl)`. Stdin is closed and commands cannot receive interactive input. Stdout and stderr may be TTY-backed even though no controlling terminal exists. Use explicit noninteractive flags/messages; do not launch prompts, pagers, or editors. For file changes, prefer apply_patch."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 300"
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional working directory for this shell_command invocation only. Relative paths resolve from this shell instance's remembered persistent workdir; omission uses that remembered workdir. This does not change later calls; use the separate top-level workdir(path) tool in an earlier turn to change later calls."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })),
        format: None,
        tags: tool_tags(&[
            "shell:exec",
            "shell:exec:shell_command",
            tau_proto::TURN_MANIPULATOR_TOOL_TAG,
        ]),
        enabled_by_default: false,
        background_support: None,
        examples: vec![ToolExample {
            id: "run-command".to_owned(),
            title: Some("Run a command".to_owned()),
            arguments: CborValue::Map(vec![
                example_field("command", example_text("cargo test -p tau-core")),
                example_field("timeout", example_int(300)),
            ]),
            note: Some("For file edits, prefer apply_patch when available.".to_owned()),
            subcommand: None,
        }],
    };
    let builtin_tools = [
        read_tool,
        read_image_tool,
        edit_tool,
        replace_tool,
        apply_patch_tool,
        dir_lock_tool,
        grep_tool,
        find_tool,
        ls_tool,
        workdir_tool,
        shell_tool,
        gpt_shell_tool,
    ];
    tools.extend(builtin_tools);
    tools
}

fn run_impl<R, W>(
    reader: R,
    writer: W,
    discovery_policy: DiscoverySourcePolicy,
    runtime_cwd_source: RuntimeCwdSource,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let initial_config = ExtConfig::default();
    let mut runtime = tau_client::TauExtensionRunner::new(ShellExtension {
        initial_config: initial_config.clone(),
    })
    .start_manual_loop_with_state(reader, writer, |handle| match runtime_cwd_source {
        RuntimeCwdSource::Process => {
            ShellRuntime::new(Output::client(handle), initial_config, discovery_policy)
        }
        #[cfg(any(test, feature = "echo-agent"))]
        RuntimeCwdSource::Fixture(fixture_cwd) => ShellRuntime::new_for_test_harness(
            Output::client(handle),
            initial_config,
            discovery_policy,
            fixture_cwd,
        ),
    })?;

    let waker = runtime.waker();
    runtime.state().install_waker(waker);
    let loop_result = run_shell_manual_loop(&mut runtime);

    // EOF/disconnect/errors may arrive without a committed SessionShutdown event.
    // Wake lock waiters and drop scheduler workers before tau-client shuts down
    // its writer, because active worker jobs may be blocked inside DirLockManager
    // and worker-held output handles must not enqueue after writer shutdown.
    runtime.state_mut().final_shutdown();
    let finish_result = runtime.finish().map(|_| ());
    match (loop_result, finish_result) {
        (Ok(()), Ok(())) => Ok(()),
        (_, Err(error)) => Err(Box::new(error)),
        (Err(error), _) => Err(Box::new(error)),
    }
}

fn run_shell_manual_loop(
    runtime: &mut tau_client::ManualExtensionRuntime<ShellRuntime>,
) -> tau_client::ClientResult<()> {
    loop {
        runtime.state().take_mandatory_output_failure()?;
        match runtime.try_recv()? {
            tau_client::ManualRuntimePoll::Message(message) => {
                match runtime.dispatch_one(message)? {
                    tau_client::DispatchOutcome::Continue => {}
                    tau_client::DispatchOutcome::StopRequested
                    | tau_client::DispatchOutcome::Disconnect(_) => return Ok(()),
                }
            }
            tau_client::ManualRuntimePoll::InputClosed => return Ok(()),
            tau_client::ManualRuntimePoll::Empty => runtime.wait_for_wake(),
        }
    }
}

struct ShellExtension {
    initial_config: ExtConfig,
}

impl tau_client::TauExtension for ShellExtension {
    type State = ShellRuntime;

    fn name(&self) -> &'static str {
        "tau-ext-shell"
    }

    fn register(self, builder: &mut tau_client::ExtensionBuilder<Self::State>) {
        let tools = registered_tool_specs(self.initial_config.dir_lock.enable);

        // Replay policy is declared per handler below: historical cwd metadata
        // is folded, while effectful tools/actions/cancellation/UI commands and
        // session lifecycle publications stay live-only.
        let shell_tool_group = tau_proto::ToolGroup {
            name: tau_proto::ToolGroupName::new("shell"),
            prompt_fragment: None,
        };
        let test_tool_group = tau_proto::ToolGroup {
            name: tau_proto::ToolGroupName::new("test"),
            prompt_fragment: None,
        };

        for tool in tools {
            let tool_group = if tool.name.as_str() == "echo" {
                test_tool_group.clone()
            } else {
                shell_tool_group.clone()
            };
            builder.tool_with_group_and_prompt_fragment(tool, Some(tool_group), None, |cx| {
                let local_tool_name = cx.local_tool_name().clone();
                cx.state
                    .handle_scoped_tool_started(cx.invoke.clone(), &local_tool_name)
            });
        }
        builder
            .register_context_provider()
            .register_session_context_provider()
            .publish_prompt_fragment(shell_workdir_prompt_fragment(&self.initial_config.shell))
            .publish_actions(shell_action_schema())
            .on_live::<tau_proto::ToolCancelRequest>(|cx| {
                cx.state
                    .handle_event(Event::ToolCancelRequest(cx.event.clone()), false)
            })
            .on_raw_live(
                tau_proto::EventSelector::Exact(tau_proto::EventName::ACTION_INVOKE),
                |cx| cx.state.handle_event(cx.event().clone(), false),
            )
            .on_restore::<tau_proto::SessionStarted>(|cx| {
                cx.state
                    .handle_event(Event::SessionStarted(cx.event.clone()), true)
            })
            .on_live::<tau_proto::SessionStarted>(|cx| {
                cx.state
                    .handle_event(Event::SessionStarted(cx.event.clone()), false)
            })
            .on_restore::<tau_proto::SessionAgentLoaded>(|cx| {
                cx.state
                    .handle_event(Event::SessionAgentLoaded(cx.event.clone()), true)
            })
            .on_live::<tau_proto::SessionAgentLoaded>(|cx| {
                cx.state
                    .handle_event(Event::SessionAgentLoaded(cx.event.clone()), false)
            })
            .on_restore::<tau_proto::SessionAgentUnloaded>(|cx| {
                cx.state
                    .handle_event(Event::SessionAgentUnloaded(cx.event.clone()), true)
            })
            .on_live::<tau_proto::SessionAgentUnloaded>(|cx| {
                cx.state
                    .handle_event(Event::SessionAgentUnloaded(cx.event.clone()), false)
            })
            .on_live::<tau_proto::AgentReplayComplete>(|cx| {
                cx.state
                    .handle_event(Event::AgentReplayComplete(cx.event.clone()), false)
            })
            .on_restore::<tau_proto::AgentMetadataSet>(|cx| {
                cx.state
                    .handle_event(Event::AgentMetadataSet(cx.event.clone()), true)
            })
            .on_live::<tau_proto::AgentMetadataSet>(|cx| {
                cx.state
                    .handle_event(Event::AgentMetadataSet(cx.event.clone()), false)
            })
            .on_restore::<tau_proto::AgentMetadataUnset>(|cx| {
                cx.state
                    .handle_event(Event::AgentMetadataUnset(cx.event.clone()), true)
            })
            .on_live::<tau_proto::AgentMetadataUnset>(|cx| {
                cx.state
                    .handle_event(Event::AgentMetadataUnset(cx.event.clone()), false)
            })
            .on_live::<tau_proto::SessionShutdown>(|cx| {
                cx.state
                    .handle_event(Event::SessionShutdown(cx.event.clone()), false)
            })
            .on_live::<tau_proto::StartAgentAccepted>(|cx| {
                cx.state
                    .handle_event(Event::StartAgentAccepted(cx.event.clone()), false)
            })
            .on_live::<tau_proto::StartAgentResult>(|cx| {
                cx.state
                    .handle_event(Event::StartAgentResult(cx.event.clone()), false)
            })
            .on_live::<tau_proto::UiShellCommand>(|cx| {
                cx.state
                    .handle_event(Event::UiShellCommand(cx.event.clone()), false)
            })
            .configure_raw(|cx| {
                let cfg = cx.parse_config::<ExtConfig>()?;
                cx.state.apply_config(
                    cx.configure.instance_name.clone(),
                    cx.configure.tool_prefix.clone(),
                    cfg,
                )
            })
            .ready_message("filesystem and shell tools ready");
    }
}

fn apply_working_directory(
    current: &ExtConfig,
    next: &ExtConfig,
    runtime_started: bool,
) -> Result<(), String> {
    match (&current.working_directory, &next.working_directory) {
        (None, Some(_)) if runtime_started => Err(
            "ext-shell working_directory cannot be set after runtime events have started"
                .to_owned(),
        ),
        (None, Some(working_directory)) => set_process_working_directory(working_directory),
        (Some(current), Some(next)) if current == next => Ok(()),
        (Some(current), Some(next)) => Err(format!(
            "ext-shell working_directory cannot be changed after startup (current: {}, requested: {})",
            current.display(),
            next.display()
        )),
        _ => Ok(()),
    }
}

fn set_process_working_directory(working_directory: &Path) -> Result<(), String> {
    std::env::set_current_dir(working_directory).map_err(|err| {
        format!(
            "failed to set ext-shell working_directory to {}: {err}",
            working_directory.display()
        )
    })
}

fn dir_lock_tool_spec(enabled_by_default: bool) -> ToolSpec {
    let tags = if enabled_by_default {
        tool_tags(&["shell:lock", tau_proto::TURN_WAIT_TOOL_TAG])
    } else {
        tool_tags(&[tau_proto::TURN_WAIT_TOOL_TAG])
    };
    ToolSpec {
        name: tau_proto::ToolName::new(DIR_LOCK_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Lock or unlock a directory and its contents for updates. Waits for the lock when \
             necessary."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["update", "unlock"],
                    "description": "Lock or unlock the directory for updates"
                },
                "directory": {
                    "type": "string",
                    "description": "Existing directory to canonicalize before locking"
                },
                "owner_agent_id": {
                    "type": "string",
                    "description": "Optional owner agent id for force-unlocking a manual lock held by another agent"
                }
            },
            "required": ["command", "directory"],
            "additionalProperties": false
        })),
        format: None,
        tags,
        enabled_by_default,
        background_support: None,
        examples: vec![
            ToolExample {
                id: "update-lock".to_owned(),
                title: Some("Acquire update lock".to_owned()),
                arguments: CborValue::Map(vec![
                    example_field("command", example_text("update")),
                    example_field("directory", example_text(".")),
                ]),
                note: Some(
                    "Acquire before making file changes when directory locking is enabled."
                        .to_owned(),
                ),
                subcommand: Some(ToolExampleSelector {
                    path: vec!["command".to_owned()],
                    value: example_text("update"),
                }),
            },
            ToolExample {
                id: "unlock".to_owned(),
                title: Some("Release update lock".to_owned()),
                arguments: CborValue::Map(vec![
                    example_field("command", example_text("unlock")),
                    example_field("directory", example_text(".")),
                ]),
                note: None,
                subcommand: Some(ToolExampleSelector {
                    path: vec!["command".to_owned()],
                    value: example_text("unlock"),
                }),
            },
        ],
    }
}

fn shell_action_schema() -> tau_actions::ActionSchema {
    tau_actions::ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![tau_actions::ActionCommand {
            name: ":shell-dir-force-unlock".to_owned(),
            description: "Force-release ext-shell manual directory locks overlapping a directory"
                .to_owned(),
            action_id: Some(SHELL_DIR_FORCE_UNLOCK_ACTION_ID.to_owned()),
            args: vec![tau_actions::ActionArg {
                name: "directory".to_owned(),
                description: "Existing directory whose overlapping manual locks should be released"
                    .to_owned(),
                required: true,
                suggestions: Vec::new(),
                kind: tau_actions::ActionArgKind::RestString,
            }],
            children: Vec::new(),
        }],
    }
}

fn dispatch_action_invoke(invoke: ActionInvoke, lock_manager: &DirLockManager) -> Event {
    if invoke.action_id != SHELL_DIR_FORCE_UNLOCK_ACTION_ID {
        return action_error(invoke, "unknown shell action".to_owned());
    }
    let Some(directory) = invoke.argv.first().map(String::as_str) else {
        return action_error(invoke, "missing directory argument".to_owned());
    };
    let dir = match crate::dir_lock::canonical_existing_dir(Path::new(directory)) {
        Ok(dir) => dir,
        Err(message) => return action_error(invoke, message),
    };
    let removed = match lock_manager.force_unlock_overlapping(&dir) {
        Ok(removed) => removed,
        Err(message) => {
            return action_error(invoke, format!("dir_lock backend error: {message}"));
        }
    };
    if removed.is_empty() {
        return action_error(
            invoke,
            format!("no manual directory locks overlap {}", dir.display()),
        );
    }

    let mut lines = vec![format!(
        "Force-unlocked {} manual directory lock(s) overlapping {}.",
        removed.len(),
        dir.display()
    )];
    for entry in removed {
        lines.push(format!("{} owner={}", entry.dir.display(), entry.owner));
    }
    Event::ActionResultReported(ActionResult {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        output: ActionOutput::Text {
            text: lines.join("\n"),
        },
    })
}

fn action_error(invoke: ActionInvoke, message: String) -> Event {
    Event::ActionErrorReported(ActionError {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        message,
        details: None,
    })
}

fn rewrite_invoke_for_cwd(
    mut invoke: tau_proto::ToolStarted,
    base: &Path,
) -> tau_proto::ToolStarted {
    if invoke.tool_name == WORKDIR_TOOL_NAME {
        return invoke;
    }
    let field = match invoke.tool_name.as_str() {
        SHELL_TOOL_NAME => path_crate_tools::ShellSurface::Generic.directory_argument(),
        GPT_SHELL_TOOL_NAME => path_crate_tools::ShellSurface::ChatGpt.directory_argument(),
        READ_TOOL_NAME | READ_IMAGE_TOOL_NAME | EDIT_TOOL_NAME | REPLACE_TOOL_NAME
        | FIND_TOOL_NAME | GREP_TOOL_NAME | LS_TOOL_NAME => "path",
        DIR_LOCK_TOOL_NAME => "directory",
        _ => return invoke,
    };
    let explicit_path = cbor_optional_text(&invoke.arguments, field);
    if explicit_path.is_none() && cbor_has_field(&invoke.arguments, field) {
        // Preserve malformed present values for the surface parser to reject.
        return invoke;
    }
    let Some(path) = explicit_path
        .clone()
        .or_else(|| matches!(field, "path").then(|| ".".to_owned()))
        .or_else(|| {
            matches!(
                invoke.tool_name.as_str(),
                SHELL_TOOL_NAME | GPT_SHELL_TOOL_NAME
            )
            .then(|| base.display().to_string())
        })
    else {
        return invoke;
    };
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    if let Some(canonical) = canonicalize_existing_dir_for_cwd_field(&absolute, field) {
        set_cbor_text_field(
            &mut invoke.arguments,
            field,
            canonical.display().to_string(),
        );
    } else {
        set_cbor_text_field(&mut invoke.arguments, field, absolute.display().to_string());
    }
    invoke
}

fn canonicalize_existing_dir_for_cwd_field(path: &Path, field: &str) -> Option<PathBuf> {
    (field == "cwd" || field == "workdir" || field == "directory" || field == "path")
        .then(|| path.canonicalize().ok())
        .flatten()
        .filter(|path| path.is_dir())
}

fn cbor_optional_text(arguments: &CborValue, field: &str) -> Option<String> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match (key, value) {
        (CborValue::Text(key), CborValue::Text(value)) if key == field => Some(value.clone()),
        _ => None,
    })
}

fn cbor_has_field(arguments: &CborValue, field: &str) -> bool {
    let CborValue::Map(entries) = arguments else {
        return false;
    };
    entries
        .iter()
        .any(|(key, _)| matches!(key, CborValue::Text(key) if key == field))
}

fn set_cbor_text_field(arguments: &mut CborValue, field: &str, value: String) {
    let CborValue::Map(entries) = arguments else {
        return;
    };
    if let Some((_, existing)) = entries
        .iter_mut()
        .find(|(key, _)| matches!(key, CborValue::Text(key) if key == field))
    {
        *existing = CborValue::Text(value);
    } else {
        entries.push((CborValue::Text(field.to_owned()), CborValue::Text(value)));
    }
}

fn schedule_tool_started(
    (mut invoke, local_tool_name): (tau_proto::ToolStarted, &tau_proto::ToolName),
    scheduler: &WorkScheduler,
    tx: &Output,
    config: ExtConfig,
    lock_manager: DirLockManager,
    cancellation: ToolCancellationState,
    cwd_state: CwdState,
) -> Result<(), Box<(tau_proto::ToolStarted, crate::display::ToolFailure)>> {
    let wire_invoke = invoke.clone();
    let tx = tx.scoped_tool(local_tool_name.clone(), invoke.tool_name.clone());
    invoke.tool_name = local_tool_name.clone();
    let workdir_snapshot = cwd_state.snapshot(&invoke.agent_id).map_err(|message| {
        Box::new((
            wire_invoke.clone(),
            path_crate_display::ToolFailure::new(message),
        ))
    })?;
    if matches!(workdir_snapshot, WorkdirSnapshot::Invalid) && invoke.tool_name != WORKDIR_TOOL_NAME
    {
        return Err(Box::new((
            wire_invoke,
            path_crate_display::ToolFailure::new(
                "remembered workdir metadata is invalid; repair it with an absolute workdir path",
            ),
        )));
    }
    if matches!(workdir_snapshot, WorkdirSnapshot::ReplayFailed) {
        return Err(Box::new((
            wire_invoke,
            path_crate_display::ToolFailure::new(
                "workdir replay failed for this agent; reload the agent before retrying",
            ),
        )));
    }
    if matches!(workdir_snapshot, WorkdirSnapshot::Invalid) {
        let requested = cbor_optional_text(&invoke.arguments, "path");
        if !requested
            .as_deref()
            .is_none_or(|path| Path::new(path).is_absolute())
        {
            return Err(Box::new((
                wire_invoke,
                path_crate_display::ToolFailure::new(
                    "remembered workdir metadata is invalid; repair it with an absolute workdir path",
                ),
            )));
        }
    }
    let invoke = match &workdir_snapshot {
        WorkdirSnapshot::Valid(cwd) => rewrite_invoke_for_cwd(invoke, cwd),
        WorkdirSnapshot::Invalid => invoke,
        WorkdirSnapshot::ReplayFailed => unreachable!("replay failures return above"),
    };
    if invoke.tool_name == WORKDIR_TOOL_NAME
        && cbor_optional_text(&invoke.arguments, "path").is_some()
    {
        let base = match &workdir_snapshot {
            WorkdirSnapshot::Valid(path) => Some(path.as_path()),
            WorkdirSnapshot::Invalid => None,
            WorkdirSnapshot::ReplayFailed => unreachable!("replay failures return above"),
        };
        let path = path_crate_tools::workdir::target_dir(&invoke.arguments, base)
            .map_err(|failure| Box::new((wire_invoke.clone(), failure)))?;
        cwd_state
            .start_pending_workdir_result(
                invoke.agent_id.clone(),
                path,
                wire_invoke.clone(),
                None,
            )
            .map_err(|_| {
                Box::new((
                    wire_invoke.clone(),
                    path_crate_display::ToolFailure::new(
                        "another workdir change is already pending for this agent and shell instance",
                    ),
                ))
            })?;
        cwd_state.mark_pending_workdir_awaiting_echo(&invoke.agent_id, &wire_invoke.call_id);
        let path = cwd_state
            .pending_workdir_target(&invoke.agent_id, &wire_invoke.call_id)
            .expect("newly reserved workdir target");
        let mutation_id =
            cwd_state.pending_workdir_mutation_id(&invoke.agent_id, &wire_invoke.call_id);
        if tx
            .send_checked(HarnessInputMessage::emit_transient(
                Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
                    agent_id: invoke.agent_id,
                    key: cwd_state.key(),
                    value: CborValue::Text(path.display().to_string()),
                    mutation_id,
                    inheritable: true,
                }),
            ))
            .is_err()
        {
            let failure =
                path_crate_display::ToolFailure::new("failed to request workdir metadata commit");
            if send_tool_failure(wire_invoke.clone(), failure, &tx).is_ok() {
                cwd_state.take_pending_workdir_by_call(&wire_invoke.call_id);
            }
            return Ok(());
        }
        return Ok(());
    }
    let priority = priority_for_tool(&invoke, &config);
    let meta = WorkMeta {
        call_id: Some(invoke.call_id.clone()),
        agent_id: Some(invoke.agent_id.clone()),
        queued_bytes: approximate_tool_bytes(&invoke, scheduler.queued_bytes_limit()),
    };
    let tx_for_job = tx.clone();
    let lifecycle = cancellation.lifecycles.admit(
        invoke.call_id.clone(),
        invoke.tool_name.clone(),
        invoke.agent_id.clone(),
        tx_for_job.clone(),
    );
    let lifecycle_for_error = lifecycle.clone();
    let invoke_for_error = wire_invoke;
    let cwd_state_for_error = cwd_state.clone();
    scheduler
        .enqueue(priority, meta, move || {
            #[cfg(test)]
            lifecycle.test_pause_after_dequeue();
            if invoke.tool_name == DIR_LOCK_TOOL_NAME {
                if lifecycle.start_effect() {
                    crate::dir_lock::dispatch_dir_lock_tool(
                        invoke,
                        &lock_manager,
                        config.dir_lock.enable,
                        &tx_for_job,
                        lifecycle.clone(),
                    );
                }
            } else if config.dir_lock.enable && is_dir_lock_update_tool(invoke.tool_name.as_str()) {
                dispatch_locked_tool_invoke(
                    invoke,
                    ToolDispatchContext {
                        shell_config: config.shell,
                        tx: tx_for_job.clone(),
                        running_calls: Arc::clone(&cancellation.running_calls),
                        enforce_ro_bind: config.dir_lock.enforce_ro_bind,
                        cwd_state: cwd_state.clone(),
                        lifecycle: lifecycle.clone(),
                    },
                    &lock_manager,
                    match &workdir_snapshot {
                        WorkdirSnapshot::Valid(cwd) => cwd.clone(),
                        WorkdirSnapshot::Invalid => {
                            unreachable!("only workdir admits invalid state")
                        }
                        WorkdirSnapshot::ReplayFailed => {
                            unreachable!("replay failures return above")
                        }
                    },
                );
            } else {
                if lifecycle.start_effect() {
                    dispatch_tool_invoke(
                        invoke,
                        ToolDispatchContext {
                            shell_config: config.shell,
                            tx: tx_for_job.clone(),
                            running_calls: Arc::clone(&cancellation.running_calls),
                            enforce_ro_bind: config.dir_lock.enforce_ro_bind,
                            cwd_state: cwd_state.clone(),
                            lifecycle: lifecycle.clone(),
                        },
                        None,
                        config
                            .dir_lock
                            .enable
                            .then_some(ShellCommandMode::visible(ShellAccessMode::ReadOnly)),
                        workdir_snapshot.clone(),
                    );
                }
            }
            if !tx_for_job.mandatory_output_failed() {
                lifecycle.finish();
            }
        })
        .map_err(|error| {
            lifecycle_for_error.finish();
            cwd_state_for_error.take_pending_workdir_by_call(&invoke_for_error.call_id);
            Box::new((
                invoke_for_error,
                path_crate_display::ToolFailure::new(error.message),
            ))
        })
}

/// Frozen resources needed to enqueue one UI shell command.
struct UiShellScheduleContext<'a> {
    /// Scheduler that owns the queued command.
    scheduler: &'a WorkScheduler,
    /// Extension output used to publish command events.
    tx: &'a Output,
    /// Shell execution policy captured at admission.
    shell_config: ShellConfig,
    /// Cancellation senders for commands currently executing.
    running_ui_commands: Arc<Mutex<HashMap<tau_proto::ShellCommandId, mpsc::Sender<()>>>>,
    /// Lifecycle generation shared with shutdown handling.
    shutdown_generation: Arc<AtomicU64>,
    /// Lifecycle generation captured when the command was admitted.
    scheduled_generation: u64,
    /// Canonical workdir captured when the command was admitted.
    cwd: PathBuf,
}

fn schedule_ui_shell_command(
    cmd: tau_proto::UiShellCommand,
    context: UiShellScheduleContext<'_>,
) -> Result<(), Box<(tau_proto::UiShellCommand, String)>> {
    let UiShellScheduleContext {
        scheduler,
        tx,
        shell_config,
        running_ui_commands,
        shutdown_generation,
        scheduled_generation,
        cwd,
    } = context;
    let meta = WorkMeta {
        call_id: None,
        agent_id: cmd.target_agent_id.clone(),
        queued_bytes: cmd.command.len(),
    };
    let tx_for_job = tx.clone();
    let cmd_for_error = cmd.clone();
    let command_id = cmd.command_id.clone();
    scheduler
        .enqueue(WorkPriority::User, meta, move || {
            let (cancel_tx, cancel_rx) = mpsc::channel();
            running_ui_commands
                .lock()
                .expect("running ui shell registry lock poisoned")
                .insert(command_id.clone(), cancel_tx.clone());
            if shutdown_generation.load(Ordering::SeqCst) != scheduled_generation {
                let _ = cancel_tx.send(());
            }
            path_crate_tools::shell::dispatch_user_shell_command(
                cmd,
                shell_config,
                &tx_for_job,
                cancel_rx,
                cwd,
            );
            running_ui_commands
                .lock()
                .expect("running ui shell registry lock poisoned")
                .remove(&command_id);
        })
        .map_err(|error| Box::new((cmd_for_error, error.message)))
}

fn priority_for_tool(invoke: &tau_proto::ToolStarted, config: &ExtConfig) -> WorkPriority {
    if invoke.tool_name == DIR_LOCK_TOOL_NAME {
        if is_dir_lock_update_invocation(&invoke.arguments) {
            return WorkPriority::Bulk;
        }
        return WorkPriority::Control;
    }
    if matches!(
        invoke.tool_name.as_str(),
        READ_TOOL_NAME | GREP_TOOL_NAME | FIND_TOOL_NAME | LS_TOOL_NAME
    ) {
        return WorkPriority::Cheap;
    }
    if config.dir_lock.enable && is_dir_lock_update_tool(invoke.tool_name.as_str()) {
        return WorkPriority::Bulk;
    }
    WorkPriority::Bulk
}

fn approximate_tool_bytes(invoke: &tau_proto::ToolStarted, queued_bytes_limit: usize) -> usize {
    let cap = queued_bytes_limit.saturating_add(1);
    let base = invoke
        .call_id
        .as_str()
        .len()
        .saturating_add(invoke.tool_name.as_str().len())
        .saturating_add(invoke.agent_id.as_str().len());
    saturating_add_capped(base, estimate_cbor_bytes(&invoke.arguments, cap), cap)
}

fn estimate_cbor_bytes(value: &CborValue, cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    match value {
        CborValue::Integer(_) | CborValue::Float(_) | CborValue::Bool(_) | CborValue::Null => {
            8.min(cap)
        }
        CborValue::Bytes(bytes) => bytes.len().min(cap),
        CborValue::Text(text) => text.len().min(cap),
        CborValue::Tag(_, inner) => saturating_add_capped(8, estimate_cbor_bytes(inner, cap), cap),
        CborValue::Array(values) => estimate_cbor_sequence(values.iter(), cap),
        CborValue::Map(entries) => {
            let mut total = 1usize;
            for (key, value) in entries {
                total = saturating_add_capped(total, estimate_cbor_bytes(key, cap - total), cap);
                if cap <= total {
                    return cap;
                }
                total = saturating_add_capped(total, estimate_cbor_bytes(value, cap - total), cap);
                if cap <= total {
                    return cap;
                }
            }
            total
        }
        _ => 8.min(cap),
    }
}

fn estimate_cbor_sequence<'a>(values: impl Iterator<Item = &'a CborValue>, cap: usize) -> usize {
    let mut total = 1usize;
    for value in values {
        total = saturating_add_capped(total, estimate_cbor_bytes(value, cap - total), cap);
        if cap <= total {
            return cap;
        }
    }
    total
}

fn saturating_add_capped(lhs: usize, rhs: usize, cap: usize) -> usize {
    lhs.saturating_add(rhs).min(cap)
}

/// Frozen resources shared by one model tool dispatch.
struct ToolDispatchContext {
    /// Shell execution policy captured at admission.
    shell_config: ShellConfig,
    /// Extension output used to publish tool events.
    tx: Output,
    /// Cancellation senders for tool calls currently executing.
    running_calls: Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    /// Whether read-only shell workdirs must be bind-mounted.
    enforce_ro_bind: bool,
    /// Per-instance workdir state used by the persistent workdir tool.
    cwd_state: CwdState,
    /// Shared effect-start and cancellation authority for this admitted call.
    lifecycle: ToolLifecycle,
}

fn dispatch_locked_tool_invoke(
    invoke: tau_proto::ToolStarted,
    context: ToolDispatchContext,
    lock_manager: &DirLockManager,
    cwd: PathBuf,
) {
    let ToolDispatchContext {
        shell_config,
        tx,
        running_calls,
        enforce_ro_bind,
        cwd_state,
        lifecycle,
    } = context;
    let dirs = match crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
        invoke.tool_name.as_str(),
        &invoke.arguments,
        &cwd,
    ) {
        Ok(dirs) => crate::dir_lock::normalize_lock_dirs(dirs),
        Err(error) => {
            if lifecycle.claim_terminal_before_effect() {
                let _ = send_tool_failure(invoke, error, &tx);
            }
            return;
        }
    };
    let shell_command_mode = is_shell_command_tool(invoke.tool_name.as_str())
        .then_some(ShellCommandMode::visible(ShellAccessMode::ReadWrite));

    let lock_wait_started = Instant::now();
    let wait_invoke = invoke.clone();
    let wait_dirs = dirs.clone();
    let wait_shell_command_mode = shell_command_mode;
    let wait_tx = tx.clone();
    let on_wait = move || {
        let _ = wait_tx.report_tool_progress(crate::dir_lock::waiting_progress(
            &wait_invoke,
            &wait_dirs,
            wait_shell_command_mode,
        ));
    };
    let guard = match if shell_command_mode.is_some() {
        lock_manager.acquire_auto_if_manual_covers(
            invoke.call_id.clone(),
            invoke.agent_id.clone(),
            dirs,
            on_wait,
        )
    } else {
        lock_manager.acquire_auto(
            invoke.call_id.clone(),
            invoke.agent_id.clone(),
            dirs,
            on_wait,
        )
    } {
        Ok(guard) => guard,
        Err(path_crate_dir_lock::LockAcquireError::NotCovered) => {
            if lifecycle.start_effect() {
                dispatch_tool_invoke(
                    invoke,
                    ToolDispatchContext {
                        shell_config,
                        tx,
                        running_calls,
                        enforce_ro_bind,
                        cwd_state,
                        lifecycle: lifecycle.clone(),
                    },
                    None,
                    Some(ShellCommandMode::visible(ShellAccessMode::ReadOnly)),
                    WorkdirSnapshot::Valid(cwd),
                );
            }
            return;
        }
        Err(path_crate_dir_lock::LockAcquireError::Cancelled) => {
            lifecycle.report_cancelled_before_effect();
            return;
        }
        Err(path_crate_dir_lock::LockAcquireError::Abandoned(lock)) => {
            if lifecycle.claim_terminal_before_effect() {
                let _ = send_tool_failure(invoke, lock.tool_failure(), &tx);
            }
            return;
        }
        Err(path_crate_dir_lock::LockAcquireError::SelfConflict { dir }) => {
            if lifecycle.claim_terminal_before_effect() {
                let _ = send_tool_failure(
                    invoke,
                    path_crate_display::ToolFailure::new(format!(
                        "automatic directory lock is outside your manual lock coverage: {}",
                        dir.display()
                    )),
                    &tx,
                );
            }
            return;
        }
        Err(path_crate_dir_lock::LockAcquireError::Backend(message)) => {
            if lifecycle.claim_terminal_before_effect() {
                let _ = send_tool_failure(
                    invoke,
                    path_crate_display::ToolFailure::new(format!(
                        "dir_lock backend error: {message}"
                    )),
                    &tx,
                );
            }
            return;
        }
    };

    let lock_wait_duration_seconds =
        reported_lock_wait_duration_seconds(lock_wait_started.elapsed());
    #[cfg(test)]
    lifecycle.test_pause_after_lock();
    if lifecycle.start_effect() {
        dispatch_tool_invoke(
            invoke,
            ToolDispatchContext {
                shell_config,
                tx,
                running_calls,
                enforce_ro_bind,
                cwd_state,
                lifecycle: lifecycle.clone(),
            },
            lock_wait_duration_seconds,
            shell_command_mode,
            WorkdirSnapshot::Valid(cwd),
        );
    }
    drop(guard);
}

fn send_ui_shell_saturated_failure(cmd: tau_proto::UiShellCommand, message: String, tx: &Output) {
    let _ = tx.send_checked(HarnessInputMessage::emit(
        Event::ShellCommandFinishedReported(tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            target_agent_id: cmd.target_agent_id,
            output: message,
            exit_code: None,
            cancelled: false,
        }),
    ));
}

fn send_tool_failure(
    invoke: tau_proto::ToolStarted,
    failure: crate::display::ToolFailure,
    tx: &Output,
) -> tau_client::ClientResult<()> {
    let crate::display::ToolFailure {
        message,
        details,
        display,
    } = failure;
    tx.report_tool_terminal(Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message,
        details: details.map(|details| *details),
        display: Some(*display),
        originator: invoke.originator,
    }))
}

fn reported_lock_wait_duration_seconds(elapsed: Duration) -> Option<u64> {
    if elapsed <= Duration::from_secs(SLOW_LOCK_WAIT_THRESHOLD_SECS) {
        return None;
    }

    let whole_seconds = elapsed.as_secs();
    if Duration::from_secs(whole_seconds) < elapsed {
        Some(whole_seconds.saturating_add(1))
    } else {
        Some(whole_seconds)
    }
}

fn with_lock_wait_duration(event: Event, lock_wait_duration_seconds: Option<u64>) -> Event {
    let Some(seconds) = lock_wait_duration_seconds else {
        return event;
    };

    match event {
        Event::ToolResult(mut result) => {
            result.result = cbor_value_with_lock_wait_duration(result.result, seconds, "output");
            Event::ToolResult(result)
        }
        Event::ToolError(mut error) => {
            error.details = Some(match error.details {
                Some(details) => cbor_value_with_lock_wait_duration(details, seconds, "details"),
                None => CborValue::Map(vec![lock_wait_duration_entry(seconds)]),
            });
            Event::ToolError(error)
        }
        event => event,
    }
}

fn cbor_value_with_lock_wait_duration(
    value: CborValue,
    seconds: u64,
    non_map_payload_key: &str,
) -> CborValue {
    match value {
        CborValue::Map(mut entries) => {
            prepend_lock_wait_duration(&mut entries, seconds);
            CborValue::Map(entries)
        }
        value => CborValue::Map(vec![
            lock_wait_duration_entry(seconds),
            (CborValue::Text(non_map_payload_key.to_owned()), value),
        ]),
    }
}

fn prepend_lock_wait_duration(entries: &mut Vec<(CborValue, CborValue)>, seconds: u64) {
    entries.retain(|(key, _)| match key {
        CborValue::Text(key) => key != LOCK_WAIT_DURATION_SECONDS_HEADER,
        _ => true,
    });
    entries.insert(0, lock_wait_duration_entry(seconds));
}

fn lock_wait_duration_entry(seconds: u64) -> (CborValue, CborValue) {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    (
        CborValue::Text(LOCK_WAIT_DURATION_SECONDS_HEADER.to_owned()),
        CborValue::Integer(seconds.into()),
    )
}

/// Execute a single tool invocation and send the response event(s).
fn dispatch_tool_invoke(
    mut invoke: tau_proto::ToolStarted,
    context: ToolDispatchContext,
    lock_wait_duration_seconds: Option<u64>,
    shell_command_mode: Option<ShellCommandMode>,
    workdir_snapshot: WorkdirSnapshot,
) {
    let ToolDispatchContext {
        shell_config,
        tx,
        running_calls,
        enforce_ro_bind,
        cwd_state,
        lifecycle,
    } = context;
    if invoke.tool_name == WORKDIR_TOOL_NAME {
        if cbor_optional_text(&invoke.arguments, "path").is_none() {
            let output = path_crate_tools::workdir::status_output(match &workdir_snapshot {
                WorkdirSnapshot::Valid(path) => Some(path.as_path()),
                WorkdirSnapshot::Invalid => None,
                WorkdirSnapshot::ReplayFailed => unreachable!("replay failures return above"),
            });
            let _ = tx.report_tool_terminal(Event::ToolResult(ToolResult {
                presentation: Default::default(),
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                tool_type: tau_proto::ToolType::Function,
                result: output.result,
                provider_content: output.provider_content,
                kind: ToolResultKind::Final,
                display: Some(output.display),
                originator: invoke.originator,
            }));
            return;
        }
        let agent_id = invoke.agent_id.clone();
        if let Some(path) = cwd_state.pending_workdir_target(&agent_id, &invoke.call_id) {
            if cwd_state.mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id) {
                let metadata = Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
                    agent_id,
                    key: cwd_state.key(),
                    value: CborValue::Text(path.display().to_string()),
                    mutation_id: None,
                    inheritable: true,
                });
                let _ = tx.send_checked(HarnessInputMessage::emit_transient(metadata));
            }
            return;
        }
        // Every setter is reserved and validated at admission. A missing
        // reservation means cancellation or lifecycle cleanup won the race.
        return;
    }
    let tool_cwd = match workdir_snapshot {
        WorkdirSnapshot::Valid(cwd) => cwd,
        WorkdirSnapshot::Invalid => unreachable!("non-workdir calls reject invalid state"),
        WorkdirSnapshot::ReplayFailed => unreachable!("replay failures return above"),
    };
    if matches!(
        invoke.tool_name.as_str(),
        READ_TOOL_NAME
            | EDIT_TOOL_NAME
            | GREP_TOOL_NAME
            | FIND_TOOL_NAME
            | LS_TOOL_NAME
            | SHELL_TOOL_NAME
            | GPT_SHELL_TOOL_NAME
    ) {
        crate::shell_output_spool::note_call();
    }
    let world = match world_after_shell_authorization(
        &mut invoke,
        &shell_config,
        tau_vcr::VcrConfig::from_env(),
        tool_cwd,
    ) {
        Ok(world) => world,
        Err(crate::display::ToolFailure {
            message,
            details,
            display,
        }) => {
            let event = Event::ToolError(tau_proto::ToolError {
                presentation: Default::default(),
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                message,
                details: details.map(|details| *details),
                display: Some(*display),
                originator: invoke.originator.clone(),
            });
            let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
            let _ = tx.report_tool_terminal(event);
            return;
        }
    };

    if invoke.tool_name == SHELL_TOOL_NAME || invoke.tool_name == GPT_SHELL_TOOL_NAME {
        dispatch_cancellable_shell_tool(CancellableShellDispatch {
            invoke,
            shell_config,
            tx: &tx,
            running_calls: &running_calls,
            lifecycle: &lifecycle,
            lock_wait_duration_seconds,
            shell_command_mode: shell_command_mode.unwrap_or(ShellCommandMode::READ_WRITE_HIDDEN),
            enforce_ro_bind,
            world,
        });
        return;
    }

    if invoke.tool_name == GREP_TOOL_NAME || invoke.tool_name == FIND_TOOL_NAME {
        dispatch_cancellable_non_shell_tool(
            invoke,
            &tx,
            &running_calls,
            &lifecycle,
            lock_wait_duration_seconds,
            world,
        );
        return;
    }

    if let Some(display) = crate::tools::initial_display(&invoke) {
        let _ = tx.report_tool_progress(tau_proto::ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: None,
            progress: None,
            display: Some(display),
        });
    }

    let events = execute_tool(invoke, world);
    for event in events {
        let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
        let _ = tx.report_tool_terminal(event);
    }
}

/// Authorize shell invocations before opening VCR state, then construct the
/// execution world shared by shell and non-shell tools.
fn world_after_shell_authorization(
    invoke: &mut tau_proto::ToolStarted,
    shell_config: &ShellConfig,
    vcr_config: Option<tau_vcr::VcrConfig>,
    tool_cwd: PathBuf,
) -> Result<path_crate_tools_world::ShellWorld, crate::display::ToolFailure> {
    if let Some(surface) = path_crate_tools::ShellSurface::for_tool_name(invoke.tool_name.as_str())
        && let Some(canonical_cwd) = path_crate_tools::shell::prepare_tool_invocation(
            surface,
            &invoke.arguments,
            shell_config,
        )?
    {
        set_cbor_text_field(
            &mut invoke.arguments,
            surface.directory_argument(),
            canonical_cwd.display().to_string(),
        );
    }
    path_crate_tools_world::ShellWorld::for_tool_in_dir(
        invoke.tool_name.as_str(),
        invoke.call_id.as_str(),
        &invoke.arguments,
        vcr_config,
        tool_cwd,
    )
}

fn dispatch_cancellable_non_shell_tool(
    invoke: tau_proto::ToolStarted,
    tx: &Output,
    running_calls: &Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    lifecycle: &ToolLifecycle,
    lock_wait_duration_seconds: Option<u64>,
    world: path_crate_tools::world::ShellWorld,
) {
    #[cfg(test)]
    lifecycle.test_pause_before_active_registration();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    running_calls
        .lock()
        .expect("running call registry lock poisoned")
        .insert(invoke.call_id.clone(), cancel_tx.clone());
    if lifecycle.effect_cancel_requested() {
        let _ = cancel_tx.send(());
    }

    if let Some(display) = crate::tools::initial_display(&invoke) {
        let _ = tx.report_tool_progress(tau_proto::ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: None,
            progress: None,
            display: Some(display),
        });
    }

    let call_id = invoke.call_id.clone();
    let tool_name = invoke.tool_name.clone();
    let outcome = crate::tools::execute_cancellable_tool(invoke, world, cancel_rx);

    running_calls
        .lock()
        .expect("running call registry lock poisoned")
        .remove(&call_id);

    match outcome {
        path_crate_tools::CancellableToolOutcome::Finished(events) => {
            for event in events {
                let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
                let _ = tx.report_tool_terminal(event);
            }
        }
        path_crate_tools::CancellableToolOutcome::Cancelled => {
            let event = Event::ToolCancelled(ToolCancelled {
                presentation: Default::default(),
                call_id,
                tool_name,
                tool_type: tau_proto::ToolType::Function,
                display: None,
            });
            let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
            let _ = tx.report_tool_terminal(event);
        }
    }
}

/// Parameters needed to run a cancellable shell-like tool invocation.
struct CancellableShellDispatch<'a> {
    /// Tool invocation emitted by the harness.
    invoke: tau_proto::ToolStarted,
    /// Effective shell execution configuration for this invocation.
    shell_config: ShellConfig,
    /// Channel used to send progress and terminal events back to the harness.
    tx: &'a Output,
    /// Shared registry used by cancel requests to signal running shell
    /// processes.
    running_calls: &'a Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    /// Lifecycle authority that bridges effect start to sender registration.
    lifecycle: &'a ToolLifecycle,
    /// Seconds spent waiting on a directory lock before this invocation ran.
    lock_wait_duration_seconds: Option<u64>,
    /// Display and access mode chosen for the shell command.
    shell_command_mode: ShellCommandMode,
    /// Whether read-only commands should run under the native read-only bind
    /// guard.
    enforce_ro_bind: bool,
    /// Tool execution world carrying the cwd and recorded side effects.
    world: path_crate_tools::world::ShellWorld,
}

fn dispatch_cancellable_shell_tool(params: CancellableShellDispatch<'_>) {
    let CancellableShellDispatch {
        invoke,
        shell_config,
        tx,
        running_calls,
        lifecycle,
        lock_wait_duration_seconds,
        shell_command_mode,
        enforce_ro_bind,
        mut world,
    } = params;
    #[cfg(test)]
    lifecycle.test_pause_before_active_registration();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    debug!(
        call_id = %invoke.call_id,
        tool_name = %invoke.tool_name,
        "registering cancellable shell call"
    );
    running_calls
        .lock()
        .expect("running call registry lock poisoned")
        .insert(invoke.call_id.clone(), cancel_tx.clone());
    if lifecycle.effect_cancel_requested() {
        let _ = cancel_tx.send(());
    }

    let _ = tx.report_tool_progress(tau_proto::ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: None,
        progress: None,
        display: Some(path_crate_tools::shell::initial_display(
            &invoke.arguments,
            shell_command_mode,
        )),
    });
    let result = path_crate_tools::shell::run_command_cancellable_for_tool(
        path_crate_tools::shell::ShellInvocation {
            surface: path_crate_tools::ShellSurface::for_tool_name(invoke.tool_name.as_str())
                .expect("shell dispatch accepts only known shell tools"),
            call_id: invoke.call_id.as_str(),
            arguments: &invoke.arguments,
        },
        &shell_config,
        shell_command_mode,
        enforce_ro_bind,
        Some(cancel_rx),
        &mut world,
    );
    let outcome = match (result, world.finish()) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(failure)) | (Err(failure), Ok(())) | (Err(failure), Err(_)) => Err(failure),
    };
    let event = match outcome {
        Ok(path_crate_tools_shell::CommandOutcome::Finished(output)) => {
            debug!(call_id = %invoke.call_id, tool_name = %invoke.tool_name, "cancellable shell call finished");
            Event::ToolResult(ToolResult {
                presentation: Default::default(),
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                result: output.result,
                provider_content: Vec::new(),
                kind: ToolResultKind::Final,
                display: Some(output.display),
                originator: invoke.originator.clone(),
            })
        }
        Ok(path_crate_tools_shell::CommandOutcome::Cancelled) => {
            debug!(call_id = %invoke.call_id, tool_name = %invoke.tool_name, "cancellable shell call cancelled");
            Event::ToolCancelled(ToolCancelled {
                presentation: Default::default(),
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                display: None,
            })
        }
        Err(crate::display::ToolFailure {
            message,
            details,
            display,
        }) => {
            debug!(
                call_id = %invoke.call_id,
                tool_name = %invoke.tool_name,
                message,
                "cancellable shell call failed"
            );
            Event::ToolError(tau_proto::ToolError {
                presentation: Default::default(),
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                message,
                details: details.map(|details| *details),
                display: Some(*display),
                originator: invoke.originator.clone(),
            })
        }
    };

    running_calls
        .lock()
        .expect("running call registry lock poisoned")
        .remove(&invoke.call_id);
    trace!(call_id = %invoke.call_id, "removed shell call from cancellation registry");
    let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
    if tx.report_tool_terminal(event).is_err() {
        debug!(call_id = %invoke.call_id, "failed to send terminal shell event to harness");
    }
}

fn dispatch_session_started(
    started: SessionStarted,
    tx: &Output,
    discovery_policy: DiscoverySourcePolicy,
) -> tau_client::ClientResult<()> {
    let session_id = started.session_id.clone();
    let scan = build_discovery_snapshot(started, discovery_policy);
    for diagnostic in scan.diagnostics {
        let _ = tx.send(diagnostic);
    }
    dispatch_session_discovery_messages(
        session_id,
        vec![HarnessInputMessage::emit_transient(
            Event::ExtensionSessionDiscoverySnapshotDeclared(scan.snapshot),
        )],
        tx,
    )
}

/// Publish one ordered session-discovery batch followed by its readiness
/// acknowledgement.
fn dispatch_session_discovery_messages(
    session_id: tau_proto::SessionId,
    messages: Vec<HarnessInputMessage>,
    tx: &Output,
) -> tau_client::ClientResult<()> {
    for message in messages {
        tx.send_checked(message)?;
    }
    tx.send_checked(HarnessInputMessage::emit_transient(
        Event::ExtensionSessionContextReady(ExtensionSessionContextReady { session_id }),
    ))
}

fn apply_started_cwd_metadata(
    started: tau_proto::AgentStarted,
    tx: &Output,
    cwd_state: &CwdState,
    is_replay: bool,
) -> tau_client::ClientResult<()> {
    for item in started.metadata {
        if item.key == cwd_state.key() {
            if let CborValue::Text(path) = item.value {
                let cwd = PathBuf::from(path);
                if cwd_state.set_metadata_text(started.agent_id.clone(), cwd.clone())
                    && !is_replay
                    && let Some((session_id, initialization_id)) =
                        cwd_state.initialization(&started.agent_id)
                {
                    tx.send_checked(HarnessInputMessage::emit_transient(cwd_context_event(
                        session_id,
                        started.agent_id.clone(),
                        initialization_id,
                        &cwd,
                        cwd_state,
                    )))?;
                }
            } else {
                cwd_state.set_invalid(started.agent_id.clone());
            }
        }
    }
    Ok(())
}

fn dispatch_session_agent_loaded(
    loaded: SessionAgentLoaded,
    tx: &Output,
    cwd_state: &CwdState,
    defer_default_until_replay_complete: bool,
    discovery_policy: DiscoverySourcePolicy,
) -> tau_client::ClientResult<()> {
    if defer_default_until_replay_complete {
        cwd_state.set_pending_ready(
            loaded.agent_id,
            loaded.session_id,
            loaded.agent_initialization_id,
        );
        return Ok(());
    }
    publish_agent_discovery_snapshot(&loaded, tx, discovery_policy)?;
    if let Some(cwd) = cwd_state.get(&loaded.agent_id) {
        tx.send_checked(HarnessInputMessage::emit_transient(cwd_context_event(
            loaded.session_id.clone(),
            loaded.agent_id.clone(),
            loaded.agent_initialization_id.clone(),
            &cwd,
            cwd_state,
        )))?;
        tx.send_checked(HarnessInputMessage::emit_transient(
            Event::ExtensionContextReady(ExtensionContextReady {
                session_id: loaded.session_id,
                agent_id: loaded.agent_id,
                agent_initialization_id: loaded.agent_initialization_id,
            }),
        ))?;
        return Ok(());
    }

    cwd_state.set_pending_ready(
        loaded.agent_id.clone(),
        loaded.session_id,
        loaded.agent_initialization_id,
    );
    let Ok(cwd) = cwd_state.process_default() else {
        return Ok(());
    };
    tx.send_checked(HarnessInputMessage::emit_transient(
        Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
            agent_id: loaded.agent_id,
            key: cwd_state.key(),
            value: CborValue::Text(cwd.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }),
    ))
}

fn cwd_context_event(
    session_id: tau_proto::SessionId,
    agent_id: tau_proto::AgentId,
    agent_initialization_id: tau_proto::AgentInitializationId,
    cwd: &Path,
    cwd_state: &CwdState,
) -> Event {
    let status = if cwd.is_dir() {
        "available"
    } else {
        "unavailable"
    };
    Event::ExtAgentContextPublish(ExtAgentContextPublish {
        session_id,
        agent_id,
        agent_initialization_id,
        key: AgentContextKey::new("workdir"),
        value: AgentContextValue(serde_json::json!({
            "label": cwd_state.context_label(),
            "path": cwd.display().to_string(),
            "status": status,
        })),
    })
}

fn invalid_cwd_context_event(
    session_id: tau_proto::SessionId,
    agent_id: tau_proto::AgentId,
    agent_initialization_id: tau_proto::AgentInitializationId,
    cwd_state: &CwdState,
) -> Event {
    Event::ExtAgentContextPublish(ExtAgentContextPublish {
        session_id,
        agent_id,
        agent_initialization_id,
        key: AgentContextKey::new("workdir"),
        value: AgentContextValue(serde_json::json!({
            "label": cwd_state.context_label(),
            "path": "<invalid>",
            "status": "invalid",
        })),
    })
}

fn cwd_notice_event(agent_id: tau_proto::AgentId, cwd: &Path) -> Event {
    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        inference_activation: false,
        agent_id,
        text: format!("Your working directory changed to {}.", cwd.display()),
        message_class: tau_proto::PromptMessageClass::Internal,
    })
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name,
        READ_TOOL_NAME
            | READ_IMAGE_TOOL_NAME
            | EDIT_TOOL_NAME
            | REPLACE_TOOL_NAME
            | APPLY_PATCH_TOOL_NAME
            | GREP_TOOL_NAME
            | FIND_TOOL_NAME
            | LS_TOOL_NAME
            | WORKDIR_TOOL_NAME
            | SHELL_TOOL_NAME
            | GPT_SHELL_TOOL_NAME
            | DIR_LOCK_TOOL_NAME
    ) || is_echo_tool(name)
}

fn is_dir_lock_update_invocation(arguments: &CborValue) -> bool {
    crate::argument::optional_argument_text(arguments, "command")
        .ok()
        .flatten()
        .as_deref()
        == Some("update")
}

fn is_dir_lock_update_tool(name: &str) -> bool {
    matches!(
        name,
        EDIT_TOOL_NAME
            | REPLACE_TOOL_NAME
            | APPLY_PATCH_TOOL_NAME
            | SHELL_TOOL_NAME
            | GPT_SHELL_TOOL_NAME
    )
}

fn is_shell_command_tool(name: &str) -> bool {
    matches!(name, SHELL_TOOL_NAME | GPT_SHELL_TOOL_NAME)
}

#[cfg(any(test, feature = "echo-agent"))]
fn is_echo_tool(name: &str) -> bool {
    name == ECHO_TOOL_NAME
}

#[cfg(not(any(test, feature = "echo-agent")))]
fn is_echo_tool(_name: &str) -> bool {
    false
}

fn build_discovery_snapshot(
    _started: SessionStarted,
    discovery_policy: DiscoverySourcePolicy,
) -> DiscoveryScan {
    let mut diagnostics = Vec::new();
    let (skills, agents_files) = if discovery_policy.reads_environment() {
        let skill_dirs = session_skill_dirs(std::env::current_dir().ok(), dirs::home_dir());
        let result = tau_skills::load_skills_from_skill_dirs(&skill_dirs);
        push_skill_diagnostic_requests(&mut diagnostics, result.diagnostics);
        let skills = result
            .skills
            .into_iter()
            .map(discovery_skill_candidate)
            .collect();
        let agents_files = discover_session_agents_files()
            .into_iter()
            .map(|file| DiscoveryAgentsFile {
                file_path: file.file_path,
                content: file.content,
            })
            .collect();
        (skills, agents_files)
    } else {
        (Vec::new(), Vec::new())
    };
    DiscoveryScan {
        snapshot: ExtensionSessionDiscoverySnapshotDeclared {
            session_id: _started.session_id,
            skills,
            agents_files,
        },
        diagnostics,
    }
}

fn discovery_skill_candidate(skill: tau_skills::Skill) -> DiscoverySkillCandidate {
    let file_path = skill.file_path.canonicalize().unwrap_or(skill.file_path);
    let sampled_modified = std::fs::metadata(&file_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(system_time_to_discovery_micros);
    DiscoverySkillCandidate {
        name: skill.name.into(),
        description: skill.description,
        file_path,
        add_to_prompt: skill.add_to_prompt,
        user_invocable: skill.user_invocable,
        disable_model_invocation: skill.disable_model_invocation,
        argument_hint: skill.argument_hint,
        sampled_modified,
    }
}

fn system_time_to_discovery_micros(time: std::time::SystemTime) -> Option<DiscoveryModifiedMicros> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_micros())
            .ok()
            .map(DiscoveryModifiedMicros::new),
        Err(error) => i64::try_from(error.duration().as_micros())
            .ok()
            .and_then(i64::checked_neg)
            .map(DiscoveryModifiedMicros::new),
    }
}

fn publish_agent_discovery_snapshot(
    loaded: &SessionAgentLoaded,
    tx: &Output,
    discovery_policy: DiscoverySourcePolicy,
) -> tau_client::ClientResult<()> {
    publish_agent_discovery_snapshot_for(
        loaded.session_id.clone(),
        loaded.agent_id.clone(),
        loaded.agent_initialization_id.clone(),
        tx,
        discovery_policy,
    )
}

fn publish_agent_discovery_snapshot_for(
    session_id: tau_proto::SessionId,
    agent_id: tau_proto::AgentId,
    agent_initialization_id: tau_proto::AgentInitializationId,
    tx: &Output,
    discovery_policy: DiscoverySourcePolicy,
) -> tau_client::ClientResult<()> {
    let session = SessionStarted {
        session_id: session_id.clone(),
        reason: tau_proto::SessionStartReason::Resume,
    };
    publish_agent_discovery_scan(
        build_discovery_snapshot(session, discovery_policy),
        agent_id,
        agent_initialization_id,
        tx,
    )
}

/// Publishes only the mandatory snapshot from one per-agent discovery scan.
fn publish_agent_discovery_scan(
    scan: DiscoveryScan,
    agent_id: tau_proto::AgentId,
    agent_initialization_id: tau_proto::AgentInitializationId,
    tx: &Output,
) -> tau_client::ClientResult<()> {
    tx.send_checked(agent_discovery_message(
        scan.snapshot,
        agent_id,
        agent_initialization_id,
    ))
}

/// Converts session discovery data into one correlated per-agent declaration.
fn agent_discovery_message(
    snapshot: ExtensionSessionDiscoverySnapshotDeclared,
    agent_id: tau_proto::AgentId,
    agent_initialization_id: tau_proto::AgentInitializationId,
) -> HarnessInputMessage {
    HarnessInputMessage::emit_transient(Event::ExtensionAgentDiscoverySnapshotDeclared(
        ExtensionAgentDiscoverySnapshotDeclared {
            session_id: snapshot.session_id,
            agent_id,
            agent_initialization_id,
            skills: snapshot.skills,
            agents_files: snapshot.agents_files,
        },
    ))
}

fn shell_workdir_prompt_fragment(shell: &config::ShellConfig) -> PromptFragment {
    let mut template = String::from(
        "{{#if agent_context.workdir}}### Shell workdirs\n\nEach shell extension instance \
         has its own persistent workdir; there is no global shell cwd.\n\
         {{#each agent_context.workdir}}- {{#if (eq value.label \"default\")}}default shell \
         tools (`workdir`){{else}}`{{value.label}}_*` shell tools \
         (`{{value.label}}_workdir`){{/if}}: `{{value.path}}` \
         [{{value.status}}]\n{{/each}}\nNormally set the matching workdir tool to the project \
         root before project work. It sets the cwd/base for later shell and filesystem calls \
         in that same instance. The cwd can select configured directory-scoped wrappers, \
         notably `direnv exec .`, and affect other cwd-sensitive wrappers/tools. After \
         changing it, make dependent calls only in a later tool turn after success; sibling \
         calls have no workdir-first ordering.{{/if}}",
    );
    if let Some(allowlist) = shell.allowlist_prompt_fragment() {
        template.push_str(&allowlist);
    }
    PromptFragment::new(
        "shell.workdir",
        PromptPriority::new(900),
        PromptContent::new(template),
    )
}

fn push_skill_diagnostic_requests(
    messages: &mut Vec<HarnessInputMessage>,
    diagnostics: Vec<tau_skills::SkillDiagnostic>,
) {
    for diagnostic in diagnostics {
        let (kind, level) = match diagnostic.kind {
            tau_skills::DiagnosticKind::Warning => ("warning", tau_proto::NoticeLevel::Info),
            tau_skills::DiagnosticKind::Collision => ("collision", tau_proto::NoticeLevel::Trace),
            tau_skills::DiagnosticKind::Skipped => ("skipped", tau_proto::NoticeLevel::Warning),
        };
        messages.push(HarnessInputMessage::ExtensionNoticeRequest(
            tau_proto::ExtensionNoticeRequest {
                message: format!(
                    "skill {kind}: {}\n{}",
                    diagnostic.path.display(),
                    diagnostic.message
                ),
                level,
            },
        ));
    }
}

fn session_skill_dirs(
    cwd: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
) -> Vec<tau_skills::SkillDir> {
    let mut skill_dirs = Vec::new();
    if let Some(cwd) = cwd.as_deref() {
        for project_dir in project_skill_ancestor_dirs(cwd, home.as_deref()) {
            push_existing_project_skill_dir(
                &mut skill_dirs,
                project_dir.join(".agents").join("skills"),
            );
            push_existing_project_skill_dir(
                &mut skill_dirs,
                project_dir.join(".agents.local").join("skills"),
            );
        }
    }
    if let Some(home) = home {
        skill_dirs.push(user_skill_dir_precedence(
            home.join(".config").join("agents").join("skills"),
            XDG_USER_SKILL_SOURCE_PRECEDENCE,
        ));
        skill_dirs.push(user_skill_dir_precedence(
            home.join(".config").join("agents.local").join("skills"),
            XDG_USER_SKILL_SOURCE_PRECEDENCE,
        ));
        skill_dirs.push(user_skill_dir_precedence(
            home.join(".agents").join("skills"),
            LEGACY_USER_SKILL_SOURCE_PRECEDENCE,
        ));
        skill_dirs.push(user_skill_dir_precedence(
            home.join(".agents.local").join("skills"),
            LEGACY_USER_SKILL_SOURCE_PRECEDENCE,
        ));
    }
    skill_dirs
}

fn project_skill_ancestor_dirs(
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    ancestor_dirs(cwd)
        .into_iter()
        .filter(|dir| dir.parent().is_some())
        .filter(|dir| {
            let Some(home) = home else {
                return true;
            };
            !cwd.starts_with(home) || (dir.starts_with(home) && dir != home)
        })
        .collect()
}

fn push_existing_project_skill_dir(
    skill_dirs: &mut Vec<tau_skills::SkillDir>,
    path: std::path::PathBuf,
) {
    if path.is_dir() {
        skill_dirs.push(project_skill_dir(path));
    }
}

fn project_skill_dir(path: std::path::PathBuf) -> tau_skills::SkillDir {
    tau_skills::SkillDir {
        path,
        add_to_prompt_by_default: true,
        source_precedence: None,
    }
}

fn user_skill_dir_precedence(
    path: std::path::PathBuf,
    source_precedence: u32,
) -> tau_skills::SkillDir {
    tau_skills::SkillDir {
        path,
        add_to_prompt_by_default: false,
        source_precedence: Some(source_precedence),
    }
}
