//! Filesystem and shell tool extension.
//!
//! Provides `read`, `edit`, `apply_patch`, `dir_lock`, `grep`, `find`, `ls`,
//! `shell`, and `gpt_shell` tools.
//!
//! The `echo` tool is available under `cfg(test)` or the
//! `echo-agent` cargo feature for harness-side echo-agent tests.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{
    ActionError, ActionInvoke, ActionOutput, ActionResult, AgentContextKey, AgentContextValue,
    CborValue, ConfigError, Event, ExtAgentContextPublish, ExtPromptFragmentPublish,
    ExtensionContextReady, HarnessInputMessage, HarnessOutputMessage, PeerInputReader,
    PeerOutputWriter, PromptContent, PromptFragment, PromptPriority, SessionAgentLoaded,
    SessionStarted, ToolCancelled, ToolResult, ToolResultKind, ToolSpec, ToolTag,
};
use tracing::{debug, trace};

mod agents;
mod argument;
mod config;
mod cwd_state;
mod diff;
mod dir_lock;
mod display;
mod isolation;
mod scheduler;
mod tools;
mod truncate;

#[cfg(test)]
mod tests;

use crate::agents::{ancestor_dirs, discover_session_agents_files};
use crate::config::{ExtConfig, ShellConfig};
use crate::cwd_state::CwdState;
use crate::dir_lock::{DIR_LOCK_TOOL_NAME, DirLockManager};
use crate::scheduler::{WorkMeta, WorkPriority, WorkScheduler};
#[cfg(any(test, feature = "echo-agent"))]
use crate::tools::ECHO_TOOL_NAME;
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, CD_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME,
    GREP_TOOL_NAME, LS_TOOL_NAME, READ_TOOL_NAME, SHELL_TOOL_NAME, execute_tool,
};

fn tool_tags(tags: &[&str]) -> Vec<ToolTag> {
    tags.iter().map(|tag| ToolTag::new(*tag)).collect()
}

const SHELL_DIR_FORCE_UNLOCK_ACTION_ID: &str = "shell.dir.force_unlock";

const SLOW_LOCK_WAIT_THRESHOLD_SECS: u64 = 5;
const LOCK_WAIT_DURATION_SECONDS_HEADER: &str = "lock_wait_duration_seconds";

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run_impl(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The test-only `echo` tool is registered when built with
/// `cfg(test)` or the `echo-agent` cargo feature.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_impl(reader, writer)
}

fn run_impl<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));

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
    });
    #[cfg(not(any(test, feature = "echo-agent")))]
    let echo_tool: Option<ToolSpec> = None;
    let mut config = ExtConfig::default();
    let tools = echo_tool.into_iter().chain([
        ToolSpec {
            name: tau_proto::ToolName::new(READ_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Reads a file. Defaults to reading the whole file in one call — \
                 output is capped at 2000 lines / 50 KB. Truncated output keeps \
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
                 the 50 KB output budget are marker-only, e.g. `1(truncated)`. Truncated results include `truncated: true`, `total_lines`, \
                 and `total_bytes`; `valid_utf8: false` is included only when applicable."
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
            tags: tool_tags(&["test:echo"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
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
                 must exactly match the original line immediately before `start_line`; \
                 use an empty context_line when `start_line` is 1. EOF appends use \
                 the original last line as context when the file is non-empty."
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
                                    "description": "Exact expected content of the original line immediately before start_line, including spaces and tabs. Use an empty context_line when start_line is 1. Appends at EOF use the original last line as context when the file is non-empty. If it does not match, the edit fails and returns current line-numbered context around the expected context line."
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
            tags: tool_tags(&["shell:edit", "shell:edit:line", "shell:mutates-files"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Use the `apply_patch` tool to edit files."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Custom,
            parameters: None,
            format: Some(tau_proto::ToolFormat::Text),
            tags: tool_tags(&["shell:edit", "shell:edit:apply_patch", "shell:mutates-files"]),
            enabled_by_default: false,
            background_support: None,
        },
        dir_lock_tool_spec(config.dir_lock.enable),
        ToolSpec {
            name: tau_proto::ToolName::new(GREP_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Search file contents for a pattern using ripgrep. Patterns are literal by default; \
                 regex metacharacters like `|` require `regex: true`. Returns matching lines \
                 with file paths and line numbers. Respects .gitignore. Output is truncated at \
                 `limit` matches or 50KB. Long lines are truncated to 500 chars."
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
            tags: tool_tags(&["shell:read", "shell:search"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(FIND_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Search for files by glob pattern. Returns only file paths (directories are \
                 never included, even with '**/*') relative to the search directory. Respects \
                 .gitignore. Output is truncated at `limit` results or 50KB. Use the ls tool \
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
            tags: tool_tags(&["shell:read", "shell:search"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(LS_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "List directory contents. Returns entries sorted alphabetically, with '/' suffix \
                 for directories. Includes dotfiles. Output lines are prefixed with 1-based \
                 entry numbers plus flags such as `escaped`, `invalid-utf8`, or `truncated`; \
                 output is capped at `limit` entries, 2000 lines, or 50KB with standard truncation headers. \
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
            tags: tool_tags(&["shell:read", "shell:list"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(CD_TOOL_NAME),
            model_visible_name: None,
            description: Some("Change the remembered working directory for this shell extension instance.".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Directory to switch to" } },
                "required": ["path"],
                "additionalProperties": false
            })),
            format: None,
            tags: tool_tags(&["shell:cd"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            model_visible_name: None,
            description: Some(
                "Execute a shell command via `sh -c`. Set `mode` to `rw` for commands \
                 that may modify files, or `ro` for read-only commands. Non-zero exits and timeouts \
                 are returned as structured command results with output details. Output is capped at 2000 lines / \
                 50 KB; truncated output keeps the first 1000 and last 1000 lines \
                 separated by a literal `...` line. Output lines are prefixed with `out ` \
                 for stdout or `err ` for stderr; missing trailing newlines are marked, e.g. \
                 `out(no_nl)`; CRLF and CR line endings are marked as `out(crlf)` \
                 or `out(cr)`. Invalid UTF-8 is shown with Unicode replacement characters and \
                 an `invalid-utf8` line flag. Lines that would exceed the 50 KB output budget \
                 are marker-only, e.g. `err(truncated)`. Truncated results include `truncated: true`, `total_lines`, and `total_bytes`. \
                 Commands taking longer than 5 seconds include duration metadata. Prefer dedicated \
                 tools like `read`, `grep`, and `find` when they fit."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["ro", "rw"],
                        "description": "Filesystem access intent: `ro` for read-only commands, `rw` for commands that may modify files"
                    },
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 120"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    }
                },
                "required": ["mode", "command"],
                "additionalProperties": false
            })),
            format: None,
            tags: tool_tags(&["shell:exec", "shell:exec:generic"]),
            enabled_by_default: true,
            background_support: None,
        },
        ToolSpec {
            name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
            model_visible_name: Some(tau_proto::ToolName::new("shell_command")),
            description: Some(
                "Run a shell command. Output is capped at 2000 lines / 50 KB; \
                 Output lines are prefixed with `out ` for stdout or `err ` for stderr; missing \
                 trailing newlines are marked with `(no_nl)`. For file changes, prefer apply_patch."
                    .to_owned(),
            ),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["ro", "rw"],
                        "description": "Filesystem access intent: `ro` for read-only commands, `rw` for commands that may modify files"
                    },
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds. The command is killed if it exceeds this. Default: 120"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the command"
                    }
                },
                "required": ["mode", "command"],
                "additionalProperties": false
            })),
            format: None,
            tags: tool_tags(&["shell:exec", "shell:exec:shell_command"]),
            enabled_by_default: false,
            background_support: None,
        },
    ]);

    // No past events requested: the shell starts from fresh live state.
    // Replaying old invokes/commands would repeat work; old session starts
    // would duplicate context publication.
    let mut handshake = tau_extension::Handshake::tool("tau-ext-shell").subscribe([
        tau_proto::EventName::TOOL_STARTED,
        tau_proto::EventName::TOOL_CANCEL_REQUEST,
        tau_proto::EventName::ACTION_INVOKE,
        tau_proto::EventName::SESSION_STARTED,
        tau_proto::EventName::SESSION_AGENT_LOADED,
        tau_proto::EventName::SESSION_AGENT_UNLOADED,
        tau_proto::EventName::AGENT_METADATA_SET,
        tau_proto::EventName::AGENT_METADATA_UNSET,
        tau_proto::EventName::SESSION_SHUTDOWN,
        tau_proto::EventName::AGENT_START_ACCEPTED,
        tau_proto::EventName::AGENT_START_RESULT,
        tau_proto::EventName::UI_SHELL_COMMAND,
    ]);
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
        handshake =
            handshake.register_tool_with_group_and_prompt_fragment(tool, Some(tool_group), None);
    }
    handshake = handshake.announce_event(Event::ExtensionContextProviderRegister(
        tau_proto::ExtensionContextProviderRegister {},
    ));
    handshake
        .announce_event(Event::ExtPromptFragmentPublish(ExtPromptFragmentPublish {
            fragment: shell_cwd_prompt_fragment(),
        }))
        .publish_actions(shell_action_schema())
        .ready_message("filesystem and shell tools ready")
        .run(&mut writer)?;

    // Response channel: worker threads send protocol messages here; writer
    // thread drains them onto the wire.
    let (tx, rx) = mpsc::channel::<HarnessInputMessage>();
    let scheduler = WorkScheduler::new(tx.clone(), Default::default());
    let running_shells = Arc::new(Mutex::new(
        HashMap::<tau_proto::ToolCallId, mpsc::Sender<()>>::new(),
    ));
    let lock_manager = DirLockManager::default();
    let cwd_state = CwdState::new();
    let mut start_agent_owners = HashMap::<String, tau_proto::AgentId>::new();

    // Writer thread: drains response messages and writes them to the wire.
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

    // Reader loop: dispatch each owned tool invocation to a worker thread.
    // ToolStarted is a subscribed committed delivery, so it carries an ack
    // sequence that must be acknowledged after processing like other subscribed
    // events.
    let mut runtime_started = false;
    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::Configure(msg) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                    Ok(mut cfg) => {
                        if cfg.working_directory.is_none() {
                            cfg.working_directory = config.working_directory.clone();
                        }
                        if let Some(instance_name) = msg.instance_name.as_ref() {
                            cwd_state.set_instance_name(instance_name.as_str().to_owned());
                        }
                        if let Err(message) =
                            apply_working_directory(&config, &cfg, runtime_started)
                        {
                            tx.send(HarnessInputMessage::ConfigError(ConfigError { message }))?;
                            continue;
                        }
                        let dir_lock_was_enabled = config.dir_lock.enable;
                        let dir_lock_changed = dir_lock_was_enabled != cfg.dir_lock.enable;
                        let dir_lock_disabling = dir_lock_was_enabled && !cfg.dir_lock.enable;
                        config = cfg;
                        if dir_lock_disabling {
                            let _ = lock_manager.disable();
                        }
                        if dir_lock_changed {
                            tx.send(HarnessInputMessage::emit(Event::ToolRegister(
                                tau_proto::ToolRegister {
                                    tool: dir_lock_tool_spec(config.dir_lock.enable),
                                    tool_group: Some(tau_proto::ToolGroup {
                                        name: tau_proto::ToolGroupName::new("shell"),
                                        prompt_fragment: None,
                                    }),
                                    prompt_fragment: None,
                                },
                            )))?;
                        }
                    }
                    Err(message) => {
                        tx.send(HarnessInputMessage::ConfigError(ConfigError { message }))?;
                    }
                }
            }
            HarnessOutputMessage::Deliver(delivery) => {
                runtime_started = true;
                // Replay-marked frames re-send historical facts to late
                // subscribers. Execution triggers are skipped on replay so
                // history does not re-run side effects; metadata-bearing facts
                // are folded so cwd state is restored before live readiness.
                let is_replay = delivery.is_replay();
                match delivery.into_event() {
                    Event::AgentStarted(started) => {
                        apply_started_cwd_metadata(started, &tx, &cwd_state, is_replay);
                    }
                    Event::ToolStarted(invoke) => {
                        if is_replay {
                            continue;
                        }
                        if !is_shell_tool(invoke.tool_name.as_str()) {
                            continue;
                        }
                        if let Err(error) = schedule_tool_started(
                            invoke,
                            &scheduler,
                            &tx,
                            config.clone(),
                            lock_manager.clone(),
                            Arc::clone(&running_shells),
                            cwd_state.clone(),
                        ) {
                            let (invoke, failure) = *error;
                            send_tool_failure(invoke, failure, &tx);
                        }
                    }
                    Event::SessionStarted(started) => {
                        if is_replay {
                            continue;
                        }
                        dispatch_session_started(started, &tx);
                    }
                    Event::SessionAgentLoaded(loaded) => {
                        if is_replay {
                            continue;
                        }
                        dispatch_session_agent_loaded(loaded, &tx, &cwd_state);
                    }
                    Event::SessionAgentUnloaded(unloaded) => {
                        if is_replay {
                            continue;
                        }
                        lock_manager.release_agent(&unloaded.agent_id);
                        scheduler.cancel_agent(&unloaded.agent_id);
                        cwd_state.unset(&unloaded.agent_id);
                        cwd_state.take_pending_ready(&unloaded.agent_id);
                        cwd_state.take_pending_notice(&unloaded.agent_id);
                        cwd_state.take_pending_cd_result(&unloaded.agent_id);
                        start_agent_owners.retain(|_, agent_id| agent_id != &unloaded.agent_id);
                    }
                    Event::AgentMetadataSet(set) => {
                        if set.key == cwd_state.key()
                            && let CborValue::Text(path) = set.value
                        {
                            let cwd = PathBuf::from(path);
                            let agent_id = set.agent_id;
                            cwd_state.set(agent_id.clone(), cwd.clone());
                            if is_replay {
                                continue;
                            }
                            let _ = tx.send(HarnessInputMessage::emit(cwd_context_event(
                                agent_id.clone(),
                                &cwd,
                            )));
                            if cwd_state.take_pending_notice(&agent_id).is_some() {
                                let _ = tx.send(HarnessInputMessage::emit(cwd_notice_event(
                                    agent_id.clone(),
                                    &cwd,
                                )));
                            }
                            if let Some(pending_cd) =
                                cwd_state.take_committed_pending_cd_result(&agent_id, &cwd)
                            {
                                if pending_cd.matched_request {
                                    let output = crate::tools::cd::output(cwd.as_path());
                                    let event = Event::ToolResult(ToolResult {
                                        call_id: pending_cd.invoke.call_id,
                                        tool_name: pending_cd.invoke.tool_name,
                                        tool_type: tau_proto::ToolType::Function,
                                        result: output.result,
                                        kind: ToolResultKind::Final,
                                        display: Some(output.display),
                                        originator: pending_cd.invoke.originator,
                                    });
                                    let _ = tx.send(HarnessInputMessage::emit(
                                        with_lock_wait_duration(
                                            event,
                                            pending_cd.lock_wait_duration_seconds,
                                        ),
                                    ));
                                } else {
                                    let event = Event::ToolError(tau_proto::ToolError {
                                        call_id: pending_cd.invoke.call_id,
                                        tool_name: pending_cd.invoke.tool_name,
                                        tool_type: tau_proto::ToolType::Function,
                                        message: format!(
                                            "committed cwd metadata did not match requested cwd; cwd changed to {}",
                                            cwd.display()
                                        ),
                                        details: None,
                                        display: None,
                                        originator: pending_cd.invoke.originator,
                                    });
                                    let _ = tx.send(HarnessInputMessage::emit(
                                        with_lock_wait_duration(
                                            event,
                                            pending_cd.lock_wait_duration_seconds,
                                        ),
                                    ));
                                }
                            }
                            if let Some(session_id) = cwd_state.take_pending_ready(&agent_id) {
                                let _ = tx.send(HarnessInputMessage::emit(
                                    Event::ExtensionContextReady(ExtensionContextReady {
                                        session_id,
                                        agent_id,
                                    }),
                                ));
                            }
                        } else if set.key == cwd_state.key() {
                            if is_replay {
                                continue;
                            }
                            let agent_id = set.agent_id;
                            let cwd = cwd_state.get_or_default(&agent_id);
                            let _ = tx.send(HarnessInputMessage::emit(cwd_context_event(
                                agent_id.clone(),
                                &cwd,
                            )));
                            cwd_state.take_pending_notice(&agent_id);
                            if let Some(pending_cd) = cwd_state.take_pending_cd_result(&agent_id) {
                                let event = Event::ToolError(tau_proto::ToolError {
                                    call_id: pending_cd.invoke.call_id,
                                    tool_name: pending_cd.invoke.tool_name,
                                    tool_type: tau_proto::ToolType::Function,
                                    message:
                                        "committed cwd metadata value is not text; cwd unchanged"
                                            .to_owned(),
                                    details: None,
                                    display: None,
                                    originator: pending_cd.invoke.originator,
                                });
                                let _ =
                                    tx.send(HarnessInputMessage::emit(with_lock_wait_duration(
                                        event,
                                        pending_cd.lock_wait_duration_seconds,
                                    )));
                            }
                            if let Some(session_id) = cwd_state.take_pending_ready(&agent_id) {
                                let _ = tx.send(HarnessInputMessage::emit(
                                    Event::ExtensionContextReady(ExtensionContextReady {
                                        session_id,
                                        agent_id,
                                    }),
                                ));
                            }
                        }
                    }
                    Event::AgentMetadataUnset(unset) => {
                        if unset.key == cwd_state.key() {
                            cwd_state.unset(&unset.agent_id);
                            if is_replay {
                                continue;
                            }
                            let cwd = cwd_state.get_or_default(&unset.agent_id);
                            let _ = tx.send(HarnessInputMessage::emit(cwd_context_event(
                                unset.agent_id.clone(),
                                &cwd,
                            )));
                            cwd_state.take_pending_notice(&unset.agent_id);
                            if let Some(pending_cd) =
                                cwd_state.take_pending_cd_result(&unset.agent_id)
                            {
                                let event = Event::ToolError(tau_proto::ToolError {
                                    call_id: pending_cd.invoke.call_id,
                                    tool_name: pending_cd.invoke.tool_name,
                                    tool_type: tau_proto::ToolType::Function,
                                    message: "committed cwd metadata was unset; cwd reverted to the process default"
                                        .to_owned(),
                                    details: None,
                                    display: None,
                                    originator: pending_cd.invoke.originator,
                                });
                                let _ =
                                    tx.send(HarnessInputMessage::emit(with_lock_wait_duration(
                                        event,
                                        pending_cd.lock_wait_duration_seconds,
                                    )));
                            }
                            if let Some(session_id) = cwd_state.take_pending_ready(&unset.agent_id)
                            {
                                let _ = tx.send(HarnessInputMessage::emit(
                                    Event::ExtensionContextReady(ExtensionContextReady {
                                        session_id,
                                        agent_id: unset.agent_id,
                                    }),
                                ));
                            }
                        }
                    }
                    Event::SessionShutdown(_) => {
                        lock_manager.release_all_manual();
                        scheduler.cancel_all_queued();
                        start_agent_owners.clear();
                    }
                    Event::StartAgentAccepted(accepted) => {
                        start_agent_owners.insert(accepted.query_id, accepted.agent_id);
                    }
                    Event::StartAgentResult(result) => {
                        if let Some(agent_id) = start_agent_owners.remove(&result.query_id) {
                            lock_manager.release_agent(&agent_id);
                            scheduler.cancel_agent(&agent_id);
                        }
                    }
                    Event::ActionInvoke(invoke) => {
                        tx.send(HarnessInputMessage::emit(dispatch_action_invoke(
                            invoke,
                            &lock_manager,
                        )))?;
                    }
                    Event::ToolCancelRequest(request) => {
                        if scheduler.cancel_queued_call(&request.target_call_id) {
                            debug!(call_id = %request.target_call_id, "cancellation requested for queued shell work");
                            continue;
                        }
                        let cancel_tx = running_shells
                            .lock()
                            .expect("running shell registry lock poisoned")
                            .get(&request.target_call_id)
                            .cloned();
                        if let Some(cancel_tx) = cancel_tx {
                            debug!(call_id = %request.target_call_id, "shell cancellation requested for running call");
                            if cancel_tx.send(()).is_err() {
                                debug!(call_id = %request.target_call_id, "shell cancellation receiver already gone");
                            }
                        } else if lock_manager.cancel_waiting_call(&request.target_call_id) {
                            debug!(call_id = %request.target_call_id, "cancellation requested for waiting dir-lock call");
                        } else {
                            debug!(call_id = %request.target_call_id, "shell cancellation requested for unknown call");
                        }
                    }
                    Event::UiShellCommand(cmd) => {
                        if let Err(error) =
                            schedule_ui_shell_command(cmd, &scheduler, &tx, config.shell.clone())
                        {
                            let (cmd, message) = *error;
                            send_ui_shell_saturated_failure(cmd, message, &tx);
                        }
                    }
                    _ => {}
                }
            }
            HarnessOutputMessage::Disconnect(_) => break,
            _ => {}
        }
    }

    scheduler.cancel_all_queued();
    drop(scheduler);
    // Drop the sender so the writer thread exits.
    drop(tx);
    writer_handle
        .join()
        .map_err(|_| "writer thread panicked")?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
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
    ToolSpec {
        name: tau_proto::ToolName::new(DIR_LOCK_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Acquire or release an ext-shell directory update lock. Enabled by default; set ext-shell config `dir_lock.enable` to false to opt out. Commands are `update` and `unlock`, and `directory` must be an existing directory. `unlock` normally releases the caller's lock; pass `owner_agent_id` to release an abandoned lock held by another agent."
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
        tags: tool_tags(&["shell:lock"]),
        enabled_by_default,
        background_support: None,
    }
}

fn shell_action_schema() -> tau_actions::ActionSchema {
    tau_actions::ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![tau_actions::ActionCommand {
            name: "/shell-dir-force-unlock".to_owned(),
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
    let removed = lock_manager.force_unlock_overlapping(&dir);
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
    Event::ActionResult(ActionResult {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        output: ActionOutput::Text {
            text: lines.join("\n"),
        },
    })
}

fn action_error(invoke: ActionInvoke, message: String) -> Event {
    Event::ActionError(ActionError {
        invocation_id: invoke.invocation_id,
        action_id: invoke.action_id,
        message,
        details: None,
    })
}

fn rewrite_invoke_for_cwd(
    mut invoke: tau_proto::ToolStarted,
    cwd_state: &CwdState,
    tx: &mpsc::Sender<HarnessInputMessage>,
) -> tau_proto::ToolStarted {
    if invoke.tool_name == CD_TOOL_NAME {
        return invoke;
    }
    let base = cwd_state.get_or_default(&invoke.agent_id);
    let field = match invoke.tool_name.as_str() {
        SHELL_TOOL_NAME | GPT_SHELL_TOOL_NAME => "cwd",
        READ_TOOL_NAME | EDIT_TOOL_NAME | FIND_TOOL_NAME | GREP_TOOL_NAME | LS_TOOL_NAME => "path",
        DIR_LOCK_TOOL_NAME => "directory",
        _ => return invoke,
    };
    let explicit_path = cbor_optional_text(&invoke.arguments, field);
    let Some(path) = explicit_path
        .clone()
        .or_else(|| matches!(field, "path").then(|| ".".to_owned()))
        .or_else(|| (field == "cwd").then(|| base.display().to_string()))
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
        if field == "cwd" && explicit_path.is_some() {
            cwd_state.set_pending_notice(invoke.agent_id.clone(), canonical.clone());
            let _ = tx.send(HarnessInputMessage::emit(Event::AgentMetadataSet(
                tau_proto::AgentMetadataSet {
                    agent_id: invoke.agent_id.clone(),
                    key: cwd_state.key(),
                    value: CborValue::Text(canonical.display().to_string()),
                    inheritable: true,
                },
            )));
        }
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
    (field == "cwd" || field == "directory" || field == "path")
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
    invoke: tau_proto::ToolStarted,
    scheduler: &WorkScheduler,
    tx: &mpsc::Sender<HarnessInputMessage>,
    config: ExtConfig,
    lock_manager: DirLockManager,
    running_shells: Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    cwd_state: CwdState,
) -> Result<(), Box<(tau_proto::ToolStarted, crate::display::ToolFailure)>> {
    let priority = priority_for_tool(&invoke, &config);
    let meta = WorkMeta {
        call_id: Some(invoke.call_id.clone()),
        tool_name: Some(invoke.tool_name.clone()),
        agent_id: Some(invoke.agent_id.clone()),
        queued_bytes: approximate_tool_bytes(&invoke, scheduler.queued_bytes_limit()),
    };
    let tx_for_job = tx.clone();
    let invoke_for_error = invoke.clone();
    scheduler
        .enqueue(priority, meta, move || {
            let invoke = rewrite_invoke_for_cwd(invoke, &cwd_state, &tx_for_job);
            if invoke.tool_name == DIR_LOCK_TOOL_NAME {
                crate::dir_lock::dispatch_dir_lock_tool(
                    invoke,
                    &lock_manager,
                    config.dir_lock.enable,
                    &tx_for_job,
                );
            } else if config.dir_lock.enable && is_dir_lock_update_tool(invoke.tool_name.as_str()) {
                dispatch_locked_tool_invoke(
                    invoke,
                    config.shell,
                    &tx_for_job,
                    &running_shells,
                    &lock_manager,
                    config.enforce_ro_mode,
                    cwd_state.clone(),
                );
            } else {
                dispatch_tool_invoke(
                    invoke,
                    config.shell,
                    &tx_for_job,
                    &running_shells,
                    None,
                    config.enforce_ro_mode,
                    cwd_state.clone(),
                    None,
                );
            }
        })
        .map_err(|error| {
            Box::new((
                invoke_for_error,
                crate::display::ToolFailure::new(error.message),
            ))
        })
}

fn schedule_ui_shell_command(
    cmd: tau_proto::UiShellCommand,
    scheduler: &WorkScheduler,
    tx: &mpsc::Sender<HarnessInputMessage>,
    shell_config: ShellConfig,
) -> Result<(), Box<(tau_proto::UiShellCommand, String)>> {
    let meta = WorkMeta {
        call_id: None,
        tool_name: None,
        agent_id: cmd.target_agent_id.clone(),
        queued_bytes: cmd.command.len(),
    };
    let tx_for_job = tx.clone();
    let cmd_for_error = cmd.clone();
    scheduler
        .enqueue(WorkPriority::User, meta, move || {
            crate::tools::shell::dispatch_user_shell_command(cmd, shell_config, &tx_for_job);
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

fn dispatch_locked_tool_invoke(
    invoke: tau_proto::ToolStarted,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<HarnessInputMessage>,
    running_shells: &Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    lock_manager: &DirLockManager,
    enforce_ro_mode: bool,
    cwd_state: CwdState,
) {
    let cwd = cwd_state.get_or_default(&invoke.agent_id);
    let dirs = match crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
        invoke.tool_name.as_str(),
        &invoke.arguments,
        &cwd,
    ) {
        Ok(Some(dirs)) => crate::dir_lock::normalize_lock_dirs(dirs),
        Ok(None) => {
            dispatch_tool_invoke(
                invoke,
                shell_config,
                tx,
                running_shells,
                None,
                enforce_ro_mode,
                cwd_state.clone(),
                Some(cwd.clone()),
            );
            return;
        }
        Err(error) => {
            send_tool_failure(invoke, error, tx);
            return;
        }
    };

    let lock_wait_started = Instant::now();
    let wait_invoke = invoke.clone();
    let wait_dirs = dirs.clone();
    let wait_tx = tx.clone();
    let guard = match lock_manager.acquire_auto(
        invoke.call_id.clone(),
        invoke.agent_id.clone(),
        dirs,
        move || {
            let _ = wait_tx.send(HarnessInputMessage::emit(
                crate::dir_lock::waiting_progress_event(&wait_invoke, &wait_dirs),
            ));
        },
    ) {
        Ok(guard) => guard,
        Err(crate::dir_lock::LockAcquireError::Cancelled) => {
            let _ = tx.send(HarnessInputMessage::emit(Event::ToolCancelled(
                ToolCancelled {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                },
            )));
            return;
        }
        Err(crate::dir_lock::LockAcquireError::Abandoned(lock)) => {
            send_tool_failure(invoke, lock.tool_failure(), tx);
            return;
        }
        Err(crate::dir_lock::LockAcquireError::SelfConflict { dir }) => {
            send_tool_failure(
                invoke,
                crate::display::ToolFailure::new(format!(
                    "automatic directory lock is outside your manual lock coverage: {}",
                    dir.display()
                )),
                tx,
            );
            return;
        }
    };

    let lock_wait_duration_seconds =
        reported_lock_wait_duration_seconds(lock_wait_started.elapsed());
    dispatch_tool_invoke(
        invoke,
        shell_config,
        tx,
        running_shells,
        lock_wait_duration_seconds,
        enforce_ro_mode,
        cwd_state,
        Some(cwd),
    );
    drop(guard);
}

fn send_ui_shell_saturated_failure(
    cmd: tau_proto::UiShellCommand,
    message: String,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    let _ = tx.send(HarnessInputMessage::emit(Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            target_agent_id: cmd.target_agent_id,
            output: message,
            exit_code: None,
            cancelled: false,
        },
    )));
}

fn send_tool_failure(
    invoke: tau_proto::ToolStarted,
    failure: crate::display::ToolFailure,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    let crate::display::ToolFailure {
        message,
        details,
        display,
    } = failure;
    let _ = tx.send(HarnessInputMessage::emit(Event::ToolError(
        tau_proto::ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            message,
            details: details.map(|details| *details),
            display: Some(*display),
            originator: invoke.originator,
        },
    )));
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
#[allow(clippy::too_many_arguments)]
fn dispatch_tool_invoke(
    invoke: tau_proto::ToolStarted,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<HarnessInputMessage>,
    running_shells: &Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    lock_wait_duration_seconds: Option<u64>,
    enforce_ro_mode: bool,
    cwd_state: CwdState,
    frozen_cwd: Option<PathBuf>,
) {
    if invoke.tool_name == CD_TOOL_NAME {
        let base = cwd_state.get_or_default(&invoke.agent_id);
        let agent_id = invoke.agent_id.clone();
        match crate::tools::cd::target_dir(&invoke.arguments, &base) {
            Ok(path) => match cwd_state.start_pending_cd_result(
                agent_id.clone(),
                path.clone(),
                invoke,
                lock_wait_duration_seconds,
            ) {
                Ok(()) => {
                    cwd_state.set_pending_notice(agent_id.clone(), path.clone());
                    let metadata = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                        agent_id,
                        key: cwd_state.key(),
                        value: CborValue::Text(path.display().to_string()),
                        inheritable: true,
                    });
                    let _ = tx.send(HarnessInputMessage::emit(metadata));
                }
                Err(invoke) => send_tool_failure(
                    *invoke,
                    crate::display::ToolFailure::new(
                        "another cwd change is already pending for this agent",
                    ),
                    tx,
                ),
            },
            Err(failure) => send_tool_failure(invoke, failure, tx),
        }
        return;
    }
    let tool_cwd = frozen_cwd.unwrap_or_else(|| cwd_state.get_or_default(&invoke.agent_id));
    let vcr_config = tau_vcr::VcrConfig::from_env();
    let world = match crate::tools::world::ShellWorld::for_tool_in_dir(
        invoke.tool_name.as_str(),
        invoke.call_id.as_str(),
        &invoke.arguments,
        vcr_config,
        tool_cwd,
    ) {
        Ok(world) => world,
        Err(crate::display::ToolFailure {
            message,
            details,
            display,
        }) => {
            let event = Event::ToolError(tau_proto::ToolError {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                message,
                details: details.map(|details| *details),
                display: Some(*display),
                originator: invoke.originator.clone(),
            });
            let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
            let _ = tx.send(HarnessInputMessage::emit(event));
            return;
        }
    };

    if invoke.tool_name == SHELL_TOOL_NAME || invoke.tool_name == GPT_SHELL_TOOL_NAME {
        dispatch_cancellable_shell_tool(
            invoke,
            shell_config,
            tx,
            running_shells,
            lock_wait_duration_seconds,
            enforce_ro_mode,
            world,
        );
        return;
    }

    if let Some(display) = crate::tools::initial_display(&invoke) {
        let _ = tx.send(HarnessInputMessage::emit(Event::ToolProgress(
            tau_proto::ToolProgress {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                message: None,
                progress: None,
                display: Some(display),
            },
        )));
    }

    let events = execute_tool(invoke, world);
    for event in events {
        let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
        let _ = tx.send(HarnessInputMessage::emit(event));
    }
}

fn dispatch_cancellable_shell_tool(
    invoke: tau_proto::ToolStarted,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<HarnessInputMessage>,
    running_shells: &Arc<Mutex<HashMap<tau_proto::ToolCallId, mpsc::Sender<()>>>>,
    lock_wait_duration_seconds: Option<u64>,
    enforce_ro_mode: bool,
    mut world: crate::tools::world::ShellWorld,
) {
    let (cancel_tx, cancel_rx) = mpsc::channel();
    debug!(
        call_id = %invoke.call_id,
        tool_name = %invoke.tool_name,
        "registering cancellable shell call"
    );
    running_shells
        .lock()
        .expect("running shell registry lock poisoned")
        .insert(invoke.call_id.clone(), cancel_tx);

    let _ = tx.send(HarnessInputMessage::emit(Event::ToolProgress(
        tau_proto::ToolProgress {
            call_id: invoke.call_id.clone(),
            tool_name: invoke.tool_name.clone(),
            message: None,
            progress: None,
            display: Some(crate::tools::shell::initial_display(&invoke.arguments)),
        },
    )));
    let result = crate::tools::shell::run_command_cancellable(
        invoke.call_id.as_str(),
        &invoke.arguments,
        &shell_config,
        enforce_ro_mode,
        Some(cancel_rx),
        &mut world,
    );
    let outcome = match (result, world.finish()) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(failure)) | (Err(failure), Ok(())) | (Err(failure), Err(_)) => Err(failure),
    };
    let event = match outcome {
        Ok(crate::tools::shell::CommandOutcome::Finished(output)) => {
            debug!(call_id = %invoke.call_id, tool_name = %invoke.tool_name, "cancellable shell call finished");
            Event::ToolResult(ToolResult {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                result: output.result,
                kind: ToolResultKind::Final,
                display: Some(output.display),
                originator: invoke.originator.clone(),
            })
        }
        Ok(crate::tools::shell::CommandOutcome::Cancelled) => {
            debug!(call_id = %invoke.call_id, tool_name = %invoke.tool_name, "cancellable shell call cancelled");
            Event::ToolCancelled(ToolCancelled {
                call_id: invoke.call_id.clone(),
                tool_name: invoke.tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
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

    running_shells
        .lock()
        .expect("running shell registry lock poisoned")
        .remove(&invoke.call_id);
    trace!(call_id = %invoke.call_id, "removed shell call from cancellation registry");
    let event = with_lock_wait_duration(event, lock_wait_duration_seconds);
    if tx.send(HarnessInputMessage::emit(event)).is_err() {
        debug!(call_id = %invoke.call_id, "failed to send terminal shell event to harness");
    }
}

fn dispatch_session_started(started: SessionStarted, tx: &mpsc::Sender<HarnessInputMessage>) {
    for event in build_session_started_events(started) {
        let _ = tx.send(HarnessInputMessage::emit(event));
    }
}

fn apply_started_cwd_metadata(
    started: tau_proto::AgentStarted,
    tx: &mpsc::Sender<HarnessInputMessage>,
    cwd_state: &CwdState,
    is_replay: bool,
) {
    for item in started.metadata {
        if item.key == cwd_state.key()
            && let CborValue::Text(path) = item.value
        {
            let cwd = PathBuf::from(path);
            cwd_state.set(started.agent_id.clone(), cwd.clone());
            if !is_replay {
                let _ = tx.send(HarnessInputMessage::emit(cwd_context_event(
                    started.agent_id.clone(),
                    &cwd,
                )));
            }
        }
    }
}

fn dispatch_session_agent_loaded(
    loaded: SessionAgentLoaded,
    tx: &mpsc::Sender<HarnessInputMessage>,
    cwd_state: &CwdState,
) {
    if let Some(cwd) = cwd_state.get(&loaded.agent_id) {
        let _ = tx.send(HarnessInputMessage::emit(cwd_context_event(
            loaded.agent_id.clone(),
            &cwd,
        )));
        let _ = tx.send(HarnessInputMessage::emit(Event::ExtensionContextReady(
            ExtensionContextReady {
                session_id: loaded.session_id,
                agent_id: loaded.agent_id,
            },
        )));
        return;
    }

    let cwd = CwdState::process_default();
    cwd_state.set_pending_ready(loaded.agent_id.clone(), loaded.session_id);
    let _ = tx.send(HarnessInputMessage::emit(Event::AgentMetadataSet(
        tau_proto::AgentMetadataSet {
            agent_id: loaded.agent_id,
            key: cwd_state.key(),
            value: CborValue::Text(cwd.display().to_string()),
            inheritable: true,
        },
    )));
}

fn cwd_context_event(agent_id: tau_proto::AgentId, cwd: &Path) -> Event {
    Event::ExtAgentContextPublish(ExtAgentContextPublish {
        agent_id,
        key: AgentContextKey::new("cwd"),
        value: AgentContextValue(serde_json::Value::String(cwd.display().to_string())),
    })
}

fn cwd_notice_event(agent_id: tau_proto::AgentId, cwd: &Path) -> Event {
    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
        agent_id,
        text: format!("Your working directory changed to {}.", cwd.display()),
        message_class: tau_proto::PromptMessageClass::Internal,
    })
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name,
        READ_TOOL_NAME
            | EDIT_TOOL_NAME
            | APPLY_PATCH_TOOL_NAME
            | GREP_TOOL_NAME
            | FIND_TOOL_NAME
            | LS_TOOL_NAME
            | CD_TOOL_NAME
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
        EDIT_TOOL_NAME | APPLY_PATCH_TOOL_NAME | SHELL_TOOL_NAME | GPT_SHELL_TOOL_NAME
    )
}

#[cfg(any(test, feature = "echo-agent"))]
fn is_echo_tool(name: &str) -> bool {
    name == ECHO_TOOL_NAME
}

#[cfg(not(any(test, feature = "echo-agent")))]
fn is_echo_tool(_name: &str) -> bool {
    false
}

fn build_session_started_events(_started: SessionStarted) -> Vec<Event> {
    let mut events = Vec::new();

    let skill_dirs = session_skill_dirs(std::env::current_dir().ok(), dirs::home_dir());

    let result = tau_skills::load_skills_from_skill_dirs(&skill_dirs);
    push_skill_diagnostic_events(&mut events, result.diagnostics);
    for skill in result.skills {
        let file_path = skill.file_path.canonicalize().unwrap_or(skill.file_path);
        events.push(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: skill.name.into(),
            description: skill.description,
            file_path,
            add_to_prompt: skill.add_to_prompt,
            user_invocable: skill.user_invocable,
            disable_model_invocation: skill.disable_model_invocation,
            argument_hint: skill.argument_hint,
        }));
    }

    for agents_file in discover_session_agents_files() {
        events.push(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: agents_file.file_path,
                content: agents_file.content,
            },
        ));
    }

    events
}

fn shell_cwd_prompt_fragment() -> PromptFragment {
    PromptFragment::new(
        "shell.cwd",
        PromptPriority::new(900),
        PromptContent::new(
            "{{#each agent_context.cwd}}{{#if @first}}Current working directory: \
             {{value}}{{/if}}{{/each}}",
        ),
    )
}

fn push_skill_diagnostic_events(
    events: &mut Vec<Event>,
    diagnostics: Vec<tau_skills::SkillDiagnostic>,
) {
    for diagnostic in diagnostics {
        let (kind, level) = match diagnostic.kind {
            tau_skills::DiagnosticKind::Warning => ("warning", tau_proto::NoticeLevel::Info),
            tau_skills::DiagnosticKind::Collision => ("collision", tau_proto::NoticeLevel::Trace),
            tau_skills::DiagnosticKind::Skipped => ("skipped", tau_proto::NoticeLevel::Warning),
        };
        events.push(Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: format!("skill.{kind}"),
            message: format!(
                "skill {kind}: {}\n{}",
                diagnostic.path.display(),
                diagnostic.message
            ),
            level,
            always_show: matches!(
                level,
                tau_proto::NoticeLevel::Warning | tau_proto::NoticeLevel::Critical
            ),
        }));
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
        skill_dirs.push(user_skill_dir(home.join(".agents").join("skills")));
        skill_dirs.push(user_skill_dir(home.join(".agents.local").join("skills")));
        skill_dirs.push(user_skill_dir(
            home.join(".config").join("agents").join("skills"),
        ));
        skill_dirs.push(user_skill_dir(
            home.join(".config").join("agents.local").join("skills"),
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
    }
}

fn user_skill_dir(path: std::path::PathBuf) -> tau_skills::SkillDir {
    tau_skills::SkillDir {
        path,
        add_to_prompt_by_default: false,
    }
}
