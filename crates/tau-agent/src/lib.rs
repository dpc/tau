//! First-party deterministic agent process.
//!
//! Receives `AgentPromptRequest` from the harness with a fully
//! assembled prompt and returns `AgentPromptResponse`.
//!
//! The current behavior is intentionally simple and command-like:
//!
//! - `read <path>` -> tool call to `fs.read`
//! - `shell <command>` -> tool call to `shell.exec`
//! - anything else -> tool call to `demo.echo`
//!
//! When the harness sends a prompt containing a tool result (from a
//! previous turn), the agent formats and returns it as text.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use tau_proto::{
    AgentPromptResponse, AgentToolCall, CborValue, ClientKind, ContentBlock, ConversationRole,
    Event, EventName, EventReader, EventSelector, EventWriter, LifecycleHello, LifecycleReady,
    LifecycleSubscribe, PROTOCOL_VERSION,
};

/// Runs the agent on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the agent over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = EventReader::new(BufReader::new(reader));
    let mut writer = EventWriter::new(BufWriter::new(writer));
    let mut next_call_number = 1_u64;

    writer.write_event(&Event::LifecycleHello(LifecycleHello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-agent".to_owned(),
        client_kind: ClientKind::Agent,
    }))?;
    writer.write_event(&Event::LifecycleSubscribe(LifecycleSubscribe {
        selectors: vec![
            EventSelector::Exact(EventName::AgentPromptRequest),
            EventSelector::Exact(EventName::LifecycleDisconnect),
        ],
    }))?;
    writer.write_event(&Event::LifecycleReady(LifecycleReady {
        message: Some("agent ready".to_owned()),
    }))?;
    writer.flush()?;

    loop {
        let Some(event) = reader.read_event()? else {
            return Ok(());
        };
        match event {
            Event::AgentPromptRequest(request) => {
                let response = handle_prompt_request(&request, &mut next_call_number);
                writer.write_event(&Event::AgentPromptResponse(response))?;
                writer.flush()?;
            }
            Event::LifecycleDisconnect(_) => return Ok(()),
            _ => {}
        }
    }
}

fn handle_prompt_request(
    request: &tau_proto::AgentPromptRequest,
    next_call_number: &mut u64,
) -> AgentPromptResponse {
    // Look at the last message in the conversation.
    let last_message = request.messages.last();

    // If the last message is a tool result, format it as text.
    if let Some(msg) = last_message {
        if msg.role == ConversationRole::User {
            for block in &msg.content {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    let text = if *is_error {
                        format!("Tool error: {content}")
                    } else {
                        content.clone()
                    };
                    return AgentPromptResponse {
                        turn_id: request.turn_id.clone(),
                        session_id: request.session_id.clone(),
                        text: Some(text),
                        tool_calls: Vec::new(),
                    };
                }
            }
        }
    }

    // Find the last user text message.
    let user_text = request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == ConversationRole::User)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default();

    // Determine which tool to call.
    let call_id = format!("call-{next_call_number}");
    *next_call_number += 1;
    let tool_call = tool_call_for_text(&call_id, &user_text);

    AgentPromptResponse {
        turn_id: request.turn_id.clone(),
        session_id: request.session_id.clone(),
        text: None,
        tool_calls: vec![tool_call],
    }
}

fn tool_call_for_text(call_id: &str, text: &str) -> AgentToolCall {
    if let Some(path) = text.strip_prefix("read ") {
        return AgentToolCall {
            id: call_id.to_owned(),
            name: "fs.read".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.trim().to_owned()),
            )]),
        };
    }
    if let Some(command) = text.strip_prefix("shell ") {
        return AgentToolCall {
            id: call_id.to_owned(),
            name: "shell.exec".to_owned(),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text(command.trim().to_owned()),
            )]),
        };
    }
    AgentToolCall {
        id: call_id.to_owned(),
        name: "demo.echo".to_owned(),
        arguments: CborValue::Text(text.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_routes_to_fs_read_when_requested() {
        let call = tool_call_for_text("call-1", "read Cargo.toml");
        assert_eq!(call.name, "fs.read");
        assert_eq!(
            call.arguments,
            CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("Cargo.toml".to_owned()),
            )])
        );
    }

    #[test]
    fn user_message_routes_to_shell_exec_when_requested() {
        let call = tool_call_for_text("call-1", "shell printf hi");
        assert_eq!(call.name, "shell.exec");
    }
}
