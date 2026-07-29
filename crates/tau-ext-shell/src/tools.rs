//! Tool registry: dispatches a `ToolStarted` to the right handler.

use std::sync::mpsc;

use tau_proto::{
    CborValue, Event, ToolError, ToolResult, ToolResultKind, ToolUseState, ToolUseStatus,
    cbor_array_field, cbor_text_field,
};

use crate::display::{ToolFailure, ToolOutput};

pub(crate) mod apply_patch;
pub(crate) mod edit;
pub(crate) mod find;
pub(crate) mod grep;
pub(crate) mod ls;
pub(crate) mod read;
pub(crate) mod read_image;
pub(crate) mod shell;
pub(crate) mod workdir;
pub(crate) mod world;

#[cfg(any(test, feature = "echo-agent"))]
pub const ECHO_TOOL_NAME: &str = "echo";
pub const READ_TOOL_NAME: &str = "read";
pub const READ_IMAGE_TOOL_NAME: &str = "read_image";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
pub const SHELL_TOOL_NAME: &str = "shell";
pub const WORKDIR_TOOL_NAME: &str = "workdir";
pub const GPT_SHELL_TOOL_NAME: &str = "gpt_shell";
pub const GREP_TOOL_NAME: &str = "grep";
pub const FIND_TOOL_NAME: &str = "find";
pub const LS_TOOL_NAME: &str = "ls";

/// Provider-facing argument dialect for a shell execution surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellSurface {
    /// Tau's generic `shell` surface.
    Generic,
    /// ChatGPT/Codex-facing `shell_command` surface.
    ChatGpt,
}

impl ShellSurface {
    /// Resolve a known internal shell tool name to its surface.
    pub(crate) fn for_tool_name(tool_name: &str) -> Option<Self> {
        match tool_name {
            SHELL_TOOL_NAME => Some(Self::Generic),
            GPT_SHELL_TOOL_NAME => Some(Self::ChatGpt),
            _ => None,
        }
    }

    /// Return this surface's call-local directory argument.
    pub(crate) const fn directory_argument(self) -> &'static str {
        match self {
            Self::Generic => "cwd",
            Self::ChatGpt => "workdir",
        }
    }
}

/// Execute a tool and return the response event(s).
pub(crate) fn execute_tool(invoke: tau_proto::ToolStarted, world: world::ShellWorld) -> Vec<Event> {
    #[cfg(any(test, feature = "echo-agent"))]
    if invoke.tool_name == ECHO_TOOL_NAME {
        let mut events = Vec::new();
        if let Err(failure) = world.finish() {
            push_failure(&mut events, invoke, failure);
            return events;
        }
        return vec![Event::ToolResult(ToolResult {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            result: invoke.arguments,
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: None,
            originator: invoke.originator.clone(),
        })];
    }

    if invoke.tool_name == WORKDIR_TOOL_NAME {
        unreachable!("workdir is dispatched through the metadata transaction path");
    }

    if invoke.tool_name == READ_TOOL_NAME {
        return wrap_pure(invoke, world, read::read_file);
    }
    if invoke.tool_name == READ_IMAGE_TOOL_NAME {
        return wrap_pure(invoke, world, read_image::read_image);
    }
    if invoke.tool_name == EDIT_TOOL_NAME {
        return wrap_pure(invoke, world, edit::edit_file);
    }
    if invoke.tool_name == APPLY_PATCH_TOOL_NAME {
        return wrap_pure(invoke, world, apply_patch::apply_patch);
    }
    if invoke.tool_name == GREP_TOOL_NAME {
        return wrap_pure(invoke, world, |arguments, _world| grep::run_grep(arguments));
    }
    if invoke.tool_name == FIND_TOOL_NAME {
        return wrap_pure(invoke, world, |arguments, _world| find::run_find(arguments));
    }
    if invoke.tool_name == LS_TOOL_NAME {
        return wrap_pure(invoke, world, ls::run_ls);
    }

    if invoke.tool_name == SHELL_TOOL_NAME || invoke.tool_name == GPT_SHELL_TOOL_NAME {
        unreachable!("shell tools are dispatched through dispatch_cancellable_shell_tool");
    }

    let mut events = Vec::new();
    let finish = world.finish();
    push_failure(
        &mut events,
        invoke,
        finish
            .err()
            .unwrap_or_else(|| ToolFailure::new("unknown tool".to_owned())),
    );
    events
}

pub(crate) fn execute_cancellable_tool(
    invoke: tau_proto::ToolStarted,
    world: world::ShellWorld,
    cancel_rx: mpsc::Receiver<()>,
) -> CancellableToolOutcome {
    let result = if invoke.tool_name == GREP_TOOL_NAME {
        grep::run_grep_cancellable(&invoke.arguments, Some(cancel_rx))
    } else if invoke.tool_name == FIND_TOOL_NAME {
        find::run_find_cancellable(&invoke.arguments, Some(&cancel_rx))
    } else {
        return CancellableToolOutcome::Finished(execute_tool(invoke, world));
    };
    let finish = world.finish();
    match (result, finish) {
        (Ok(CancellableToolRun::Finished(output)), Ok(())) => {
            let mut events = Vec::new();
            push_output(&mut events, invoke, *output);
            CancellableToolOutcome::Finished(events)
        }
        (Ok(CancellableToolRun::Cancelled), _) => CancellableToolOutcome::Cancelled,
        (Ok(CancellableToolRun::Finished(_)), Err(failure))
        | (Err(failure), Ok(()))
        | (Err(failure), Err(_)) => {
            let mut events = Vec::new();
            push_failure(&mut events, invoke, failure);
            CancellableToolOutcome::Finished(events)
        }
    }
}

pub(crate) enum CancellableToolOutcome {
    Finished(Vec<Event>),
    Cancelled,
}

pub(crate) enum CancellableToolRun {
    Finished(Box<ToolOutput>),
    Cancelled,
}
/// Common Ok/Err → Result/Error wrapping for tool handlers. The handler's
/// display descriptor and purpose-built failure details are forwarded to the
/// event, then the world is finished so VCR recordings are saved and replays
/// assert all operations were consumed.
fn wrap_pure(
    invoke: tau_proto::ToolStarted,
    mut world: world::ShellWorld,
    handler: impl FnOnce(&CborValue, &mut world::ShellWorld) -> Result<ToolOutput, ToolFailure>,
) -> Vec<Event> {
    let mut events = Vec::new();
    let result = handler(&invoke.arguments, &mut world);
    let finish = world.finish();
    match (result, finish) {
        (Ok(output), Ok(())) => push_output(&mut events, invoke, output),
        (Ok(_), Err(failure)) | (Err(failure), Ok(())) | (Err(failure), Err(_)) => {
            push_failure(&mut events, invoke, failure);
        }
    }
    events
}

fn push_output(events: &mut Vec<Event>, invoke: tau_proto::ToolStarted, output: ToolOutput) {
    let ToolOutput {
        result,
        provider_content,
        display,
    } = output;
    events.push(Event::ToolResult(ToolResult {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result,
        provider_content,
        kind: ToolResultKind::Final,
        display: Some(display),
        originator: invoke.originator.clone(),
    }));
}

fn push_failure(events: &mut Vec<Event>, invoke: tau_proto::ToolStarted, failure: ToolFailure) {
    let ToolFailure {
        message,
        details,
        display,
    } = failure;
    events.push(Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message,
        details: details.map(|details| *details),
        display: Some(*display),
        originator: invoke.originator.clone(),
    }));
}

pub(crate) fn initial_display(invoke: &tau_proto::ToolStarted) -> Option<ToolUseState> {
    if invoke.tool_name == SHELL_TOOL_NAME || invoke.tool_name == GPT_SHELL_TOOL_NAME {
        return Some(shell::initial_display(
            &invoke.arguments,
            shell::ShellCommandMode::READ_WRITE_HIDDEN,
        ));
    }

    let mode = String::new();
    let args = match invoke.tool_name.as_str() {
        READ_TOOL_NAME => {
            let path = cbor_text_field(&invoke.arguments, "path").unwrap_or_default();
            let ranges = cbor_array_field(&invoke.arguments, "ranges")
                .map(format_requested_read_line_ranges)
                .unwrap_or_else(|| format_requested_read_line_range(&invoke.arguments));
            format!("{path} {ranges}")
        }
        READ_IMAGE_TOOL_NAME => cbor_text_field(&invoke.arguments, "path").unwrap_or_default(),
        EDIT_TOOL_NAME | APPLY_PATCH_TOOL_NAME => {
            let path = cbor_text_field(&invoke.arguments, "path").unwrap_or_default();
            let ranges = cbor_array_field(&invoke.arguments, "edits")
                .map(format_requested_edit_line_ranges)
                .unwrap_or_default();
            if ranges.is_empty() {
                path
            } else {
                format!("{path} {ranges}")
            }
        }
        FIND_TOOL_NAME => {
            let pattern = cbor_text_field(&invoke.arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(&invoke.arguments, "path").unwrap_or_else(|| ".".to_owned());
            format!("{pattern} in {path}")
        }
        GREP_TOOL_NAME => {
            let pattern = cbor_text_field(&invoke.arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(&invoke.arguments, "path").unwrap_or_else(|| ".".to_owned());
            let mut args = format!("{pattern:?} in {path}");
            if let Some(glob) = cbor_text_field(&invoke.arguments, "glob") {
                // Preserve this behavior; the structural alternative is not semantics-neutral
                // here. ast-grep-ignore: push-str-format
                args.push_str(&format!(" [{glob}]"));
            }
            args
        }
        LS_TOOL_NAME => {
            cbor_text_field(&invoke.arguments, "path").unwrap_or_else(|| ".".to_owned())
        }
        _ => return None,
    };
    Some(ToolUseState {
        args,
        mode,
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    })
}

fn format_requested_read_line_ranges(values: &[CborValue]) -> String {
    format_requested_ranges(values, format_requested_read_line_range)
}

fn format_requested_edit_line_ranges(values: &[CborValue]) -> String {
    format_requested_ranges(values, format_requested_edit_line_range)
}

fn format_requested_ranges(values: &[CborValue], format_range: fn(&CborValue) -> String) -> String {
    let ranges: Vec<String> = values
        .iter()
        .map(format_range)
        .filter(|range| !range.is_empty())
        .collect();
    if ranges.is_empty() {
        "..".to_owned()
    } else {
        ranges.join(",")
    }
}

fn format_requested_edit_line_range(arguments: &CborValue) -> String {
    let start_line = positive_usize_field(arguments, "start_line");
    let end_line_exclusive = positive_usize_field(arguments, "end_line_exclusive");
    match (start_line, end_line_exclusive) {
        (None, None) => "..".to_owned(),
        (Some(start), Some(end)) => format!("{start}..<{end}"),
        (Some(start), None) => format!("{start}..<"),
        (None, Some(end)) => format!("?..<{end}"),
    }
}

fn format_requested_read_line_range(arguments: &CborValue) -> String {
    let start_line = positive_usize_field(arguments, "start_line");
    let end_line = positive_usize_field(arguments, "end_line");
    match (start_line, end_line) {
        (None, None) => "..".to_owned(),
        (Some(start), None) => format!("{start}.."),
        (None, Some(end)) => format!("1..{end}"),
        (Some(start), Some(end)) => format!("{start}..{end}"),
    }
}

fn positive_usize_field(arguments: &CborValue, key: &str) -> Option<usize> {
    let value = tau_proto::cbor_int_field(arguments, key)?;
    if value < 1 {
        return None;
    }
    usize::try_from(value).ok()
}
