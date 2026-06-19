//! Built-in internal tools for `tau-harness`.
//!
//! This crate is the architectural boundary for tools that are bundled with
//! Tau but still should behave like normal tools. Keep as much tool-specific
//! logic here as is practical, especially parsing, per-tool state, and
//! reactions to committed events. When a tool is genuinely tangled with harness
//! state, use [`InternalToolHost`] as a narrow synchronized facade instead of
//! adding harness special cases. Host calls run inside the harness event-log
//! handling loop, so handlers can access the exposed state without races while
//! still driving their work from normal `ToolRequest` / `ToolStarted` / result
//! events.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tau_harness::internal_tools::{InternalSkill, InternalSkillSource};
use tau_harness::{AgentId, AgentToolCall, HarnessError, InternalToolHandler, InternalToolHost};
use tau_proto::{
    BackgroundSupport, CborValue, ContentPart, ContextItem, ContextRole, Event, PromptOriginator,
    ProviderResponseFinished, StartAgentRequest, ToolCallId, ToolError, ToolName, ToolResult,
    ToolResultKind, ToolSpec, ToolStarted, ToolType, ToolUsePayload, ToolUseState, ToolUseStats,
    ToolUseStatus,
};

const SKILL_TOOL_NAME: &str = "skill";
const AGENT_START_TOOL_NAME: &str = "agent_start";
const WAIT_TOOL_NAME: &str = "wait";
const CANCEL_TOOL_NAME: &str = "cancel";
const MESSAGE_TOOL_NAME: &str = "message";
const AGENT_WATCH_TOOL_NAME: &str = "agent_watch";
const SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS: u64 = 5;

/// Return handlers for Tau's built-in harness-process tools.
pub fn builtin_handlers() -> Vec<Arc<dyn InternalToolHandler>> {
    vec![Arc::new(BuiltinTools::default())]
}

#[derive(Default)]
struct BuiltinTools {
    state: Mutex<BuiltinState>,
}

#[derive(Default)]
struct BuiltinState {
    pending_delegates: HashMap<String, PendingDelegate>,
    cancel_requested: HashSet<ToolCallId>,
    in_progress_tool_names: HashMap<ToolCallId, ToolName>,
    next_delegate_query_id: u64,
    agent_watchers: HashMap<String, HashSet<String>>,
}

impl BuiltinState {
    fn remove_agent_watcher(&mut self, agent_id: &str, watcher_id: &str) {
        if let Some(watchers) = self.agent_watchers.get_mut(agent_id) {
            watchers.remove(watcher_id);
            if watchers.is_empty() {
                self.agent_watchers.remove(agent_id);
            }
        }
    }

    fn record_tool_started(&mut self, call_id: ToolCallId, tool_name: ToolName) {
        self.in_progress_tool_names.insert(call_id, tool_name);
    }

    fn record_tool_lifecycle_event(&mut self, event: &Event) {
        match event {
            Event::ToolResult(result) => {
                self.record_tool_finished(&result.call_id);
            }
            Event::ProviderToolResult(result) => {
                if result.kind != ToolResultKind::BackgroundPlaceholder {
                    self.record_tool_finished(&result.call_id);
                }
            }
            Event::ToolError(error) | Event::ProviderToolError(error) => {
                self.record_tool_finished(&error.call_id);
            }
            Event::ToolBackgroundResult(result) => {
                self.record_tool_finished(&result.call_id);
            }
            Event::ToolBackgroundError(error) => {
                self.record_tool_finished(&error.call_id);
            }
            Event::ToolCancelled(cancelled) => {
                self.record_tool_finished(&cancelled.call_id);
            }
            Event::ToolRejected(rejected) => {
                self.record_tool_finished(&rejected.call_id);
            }
            _ => {}
        }
    }

    fn record_tool_finished(&mut self, call_id: &ToolCallId) {
        self.in_progress_tool_names.remove(call_id);
    }

    fn initial_display(&self, call: &AgentToolCall) -> Option<ToolUseState> {
        let (args, status_text, payload) = match call.name.as_str() {
            SKILL_TOOL_NAME => {
                let needles = extract_skill_search_queries(&call.arguments).unwrap_or_default();
                let search_content = extract_optional_bool(&call.arguments, "search_content")
                    .ok()
                    .flatten()
                    .unwrap_or(false);
                let scope = if search_content { " [content]" } else { "" };
                (
                    format!("{}{scope}", needles.join(" ")),
                    tau_proto::PROGRESS_INDICATOR_TEXT,
                    None,
                )
            }
            AGENT_START_TOOL_NAME => {
                let parsed = parse_delegate_args(&call.arguments).ok()?;
                let args = match parsed.role {
                    Some(role) => format!("[{}] +{role}", parsed.task_name),
                    None => format!("[{}]", parsed.task_name),
                };
                (args, tau_proto::PROGRESS_INDICATOR_TEXT, None)
            }
            WAIT_TOOL_NAME => (
                self.wait_initial_display_args(&call.arguments),
                tau_proto::PROGRESS_INDICATOR_TEXT,
                None,
            ),
            MESSAGE_TOOL_NAME => match parse_message_args(&call.arguments) {
                Ok(parsed) => (
                    parsed.recipient_id,
                    tau_proto::PROGRESS_INDICATOR_TEXT,
                    Some(ToolUsePayload::Text {
                        text: parsed.message,
                    }),
                ),
                Err(_) => (String::new(), tau_proto::PROGRESS_INDICATOR_TEXT, None),
            },
            AGENT_WATCH_TOOL_NAME => match parse_agent_watch_args(&call.arguments) {
                Ok(parsed) => (
                    agent_watch_display_args(&parsed),
                    tau_proto::PROGRESS_INDICATOR_TEXT,
                    None,
                ),
                Err(_) => (String::new(), tau_proto::PROGRESS_INDICATOR_TEXT, None),
            },
            CANCEL_TOOL_NAME => match parse_cancel_args(&call.arguments) {
                Ok(target) => (target.to_string(), tau_proto::PROGRESS_INDICATOR_TEXT, None),
                Err(_) => (String::new(), tau_proto::PROGRESS_INDICATOR_TEXT, None),
            },
            _ => return None,
        };
        Some(ToolUseState {
            args,
            status: ToolUseStatus::InProgress,
            status_text: status_text.to_owned(),
            payload,
            ..Default::default()
        })
    }

    fn wait_initial_display_args(&self, arguments: &CborValue) -> String {
        wait_target_call_id(arguments)
            .and_then(|call_id| self.in_progress_tool_names.get(call_id))
            .map(ToString::to_string)
            .unwrap_or_default()
    }
}

struct PendingDelegate {
    call_id: ToolCallId,
    tool_name: ToolName,
    started_at: Instant,
    self_agent_id: String,
    agent_id: String,
    task_name: String,
    input_stats: ToolUseStats,
}

impl InternalToolHandler for BuiltinTools {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![
            skill_tool_spec(),
            agent_start_tool_spec(),
            wait_tool_spec(),
            cancel_tool_spec(),
            message_tool_spec(),
            agent_watch_tool_spec(),
        ]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        matches!(
            internal_tool_name.as_str(),
            SKILL_TOOL_NAME
                | AGENT_START_TOOL_NAME
                | WAIT_TOOL_NAME
                | CANCEL_TOOL_NAME
                | MESSAGE_TOOL_NAME
                | AGENT_WATCH_TOOL_NAME
        )
    }

    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::ToolStarted(started) => {
                let Some((conversation_id, call, visible_tool_name)) = started_call(host, started)
                else {
                    return Ok(());
                };
                let display = {
                    let mut state = self.state.lock().expect("builtin tool state poisoned");
                    state.record_tool_started(call.id.clone(), visible_tool_name.clone());
                    state.initial_display(&call)
                };
                if let Some(display) = display {
                    host.publish_tool_progress(
                        &conversation_id,
                        call.id.clone(),
                        visible_tool_name.clone(),
                        display,
                    );
                }
                match call.name.as_str() {
                    SKILL_TOOL_NAME => {
                        handle_skill_tool_call(host, &conversation_id, &call, visible_tool_name)
                    }
                    AGENT_START_TOOL_NAME => self.handle_delegate_tool_call(
                        host,
                        &conversation_id,
                        &call,
                        visible_tool_name,
                    ),
                    WAIT_TOOL_NAME => {
                        host.handle_wait_tool_call(&conversation_id, &call, visible_tool_name)
                    }
                    MESSAGE_TOOL_NAME => {
                        handle_message_tool_call(host, &conversation_id, &call, visible_tool_name)
                    }
                    AGENT_WATCH_TOOL_NAME => self.handle_agent_watch_tool_call(
                        host,
                        &conversation_id,
                        &call,
                        visible_tool_name,
                    ),
                    CANCEL_TOOL_NAME => self.handle_cancel_tool_call(
                        host,
                        &conversation_id,
                        &call,
                        visible_tool_name,
                    ),
                    _ => Ok(()),
                }
            }
            Event::StartAgentResult(result) => self.handle_start_agent_result(host, result),
            Event::ProviderResponseFinished(response) => {
                self.handle_agent_response_finished(host, response)
            }
            Event::ToolCancelRequest(request) => {
                self.handle_tool_cancel_request(host, &request.target_call_id)
            }
            Event::StartAgentAccepted(_) => Ok(()),
            _ => {
                self.state
                    .lock()
                    .expect("builtin tool state poisoned")
                    .record_tool_lifecycle_event(event);
                Ok(())
            }
        }
    }
}

impl BuiltinTools {
    fn handle_delegate_tool_call(
        &self,
        host: &mut InternalToolHost<'_>,
        cid: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id = call.id.clone();
        host.ensure_internal_tool_tracking(cid, call, &visible_tool_name);
        let parsed = match parse_delegate_args(&call.arguments) {
            Ok(parsed) => parsed,
            Err(message) => {
                host.finish_tool_with_error(
                    cid,
                    call_id,
                    visible_tool_name,
                    call.tool_type,
                    message,
                    Some(call.arguments.clone()),
                );
                return Ok(());
            }
        };
        let Some(self_agent_id) = host.ensure_agent_id_for_agent(cid) else {
            host.finish_tool_with_error(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                "sender conversation no longer exists".to_owned(),
                Some(call.arguments.clone()),
            );
            return Ok(());
        };
        let query_id = {
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            let query_id = format!("delegate-{}", state.next_delegate_query_id);
            state.next_delegate_query_id += 1;
            query_id
        };
        let input_stats = ToolUseStats::for_text(&parsed.prompt);
        let task_name = parsed.task_name.clone();
        let start_request = StartAgentRequest {
            parent_agent: None,
            query_id: query_id.clone(),
            instruction: delegate_instruction(&self_agent_id, &parsed.prompt),
            role: parsed.role,
            input_stats,
            tool_call_id: Some(call_id.clone()),
            task_name: Some(parsed.task_name),
        };
        let agent_id = match host.enqueue_start_agent_request_without_draining(start_request) {
            Ok(agent_id) => agent_id,
            Err(message) => {
                host.finish_tool_with_error(
                    cid,
                    call_id,
                    visible_tool_name,
                    call.tool_type,
                    message,
                    Some(call.arguments.clone()),
                );
                return Ok(());
            }
        };
        {
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            state
                .agent_watchers
                .entry(agent_id.clone())
                .or_default()
                .insert(self_agent_id.clone());
            state.pending_delegates.insert(
                query_id,
                PendingDelegate {
                    call_id: call_id.clone(),
                    tool_name: visible_tool_name.clone(),
                    started_at: Instant::now(),
                    self_agent_id: self_agent_id.clone(),
                    agent_id: agent_id.clone(),
                    task_name,
                    input_stats,
                },
            );
        }
        host.background_tool_call(
            &call_id,
            CborValue::Text(delegate_background_placeholder(
                &call_id,
                &self_agent_id,
                &agent_id,
            )),
        );
        host.drain_start_agent_requests()
    }

    fn handle_tool_cancel_request(
        &self,
        host: &mut InternalToolHost<'_>,
        target_call_id: &ToolCallId,
    ) -> Result<(), HarnessError> {
        let query_id = self
            .state
            .lock()
            .expect("builtin tool state poisoned")
            .pending_delegates
            .iter()
            .find_map(|(query_id, pending)| {
                (&pending.call_id == target_call_id).then(|| query_id.clone())
            });
        if let Some(query_id) = query_id {
            let _ = host.cancel_start_agent_request(&query_id, target_call_id, true);
        }
        Ok(())
    }

    fn handle_start_agent_result(
        &self,
        host: &mut InternalToolHost<'_>,
        result: &tau_proto::StartAgentResult,
    ) -> Result<(), HarnessError> {
        let Some(pending) = self
            .state
            .lock()
            .expect("builtin tool state poisoned")
            .pending_delegates
            .remove(&result.query_id)
        else {
            return Ok(());
        };
        let duration_seconds = delegate_duration_seconds(pending.started_at.elapsed());
        let delivered_watch_message =
            start_agent_watch_notification_message(&pending.agent_id, result).is_some_and(
                |message| {
                    self.notify_agent_watchers(host, &pending.agent_id, message)
                        .contains(&pending.self_agent_id)
                },
            );
        if let Some(error) = result.error.clone() {
            let display = delegate_final_display(
                &pending.task_name,
                &result.text,
                pending.input_stats,
                ToolUseStatus::Error,
                &error,
            );
            host.finish_prebuilt_tool_error(ToolError {
                call_id: pending.call_id,
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                message: if delivered_watch_message {
                    "Agent response error delivered via agent_watch".to_owned()
                } else {
                    error
                },
                details: delegate_error_details(
                    duration_seconds,
                    Some(&pending.self_agent_id),
                    Some(&pending.agent_id),
                ),
                display: Some(display),
                originator: PromptOriginator::User,
            });
        } else {
            let display = delegate_final_display(
                &pending.task_name,
                &result.text,
                pending.input_stats,
                ToolUseStatus::Success,
                "ok",
            );
            host.finish_prebuilt_tool_result(ToolResult {
                call_id: pending.call_id,
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                result: delegate_result_value(
                    None,
                    duration_seconds,
                    Some(&pending.self_agent_id),
                    Some(&pending.agent_id),
                ),
                kind: ToolResultKind::Final,
                display: Some(display),
                originator: PromptOriginator::User,
            });
        }
        Ok(())
    }
    fn handle_agent_watch_tool_call(
        &self,
        host: &mut InternalToolHost<'_>,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id = call.id.clone();
        host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
        let result = parse_agent_watch_args(&call.arguments).and_then(|parsed| {
            let self_agent_id = host
                .ensure_agent_id_for_agent(conversation_id)
                .ok_or_else(|| "sender conversation no longer exists".to_owned())?;
            if parsed.agent_id == self_agent_id {
                return Err("`agent_id` must identify another agent".to_owned());
            }
            if !host.is_known_agent_id(&parsed.agent_id) {
                return Err(format!("unknown agent: `{}`", parsed.agent_id));
            }
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            if parsed.enable {
                state
                    .agent_watchers
                    .entry(parsed.agent_id.clone())
                    .or_default()
                    .insert(self_agent_id);
                Ok(format!("Watching agent `{}`", parsed.agent_id))
            } else {
                state.remove_agent_watcher(&parsed.agent_id, &self_agent_id);
                Ok(format!("Stopped watching agent `{}`", parsed.agent_id))
            }
        });
        match result {
            Ok(message) => host.finish_tool_with_result(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                None,
            ),
            Err(message) => host.finish_tool_with_error(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }

    fn handle_agent_response_finished(
        &self,
        host: &mut InternalToolHost<'_>,
        response: &ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        let Some(message) = agent_watch_response_should_notify(response) else {
            return Ok(());
        };
        let sender_id = response.agent_id.to_string();
        self.notify_agent_watchers(host, &sender_id, message);
        Ok(())
    }

    fn notify_agent_watchers(
        &self,
        host: &mut InternalToolHost<'_>,
        sender_id: &str,
        message: String,
    ) -> HashSet<String> {
        let watchers = self
            .state
            .lock()
            .expect("builtin tool state poisoned")
            .agent_watchers
            .get(sender_id)
            .cloned()
            .unwrap_or_default();
        let mut delivered_watchers = HashSet::new();
        let mut failed_watchers = Vec::new();
        for watcher_id in watchers {
            if watcher_id == sender_id {
                continue;
            }
            if host
                .publish_agent_watch_response_from_agent_ids(
                    sender_id,
                    watcher_id.clone(),
                    message.clone(),
                )
                .is_err()
            {
                failed_watchers.push(watcher_id);
            } else {
                delivered_watchers.insert(watcher_id);
            }
        }
        if !failed_watchers.is_empty() {
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            if let Some(watchers) = state.agent_watchers.get_mut(sender_id) {
                for watcher_id in failed_watchers {
                    watchers.remove(&watcher_id);
                }
                if watchers.is_empty() {
                    state.agent_watchers.remove(sender_id);
                }
            }
        }
        delivered_watchers
    }
}

fn started_call(
    host: &mut InternalToolHost<'_>,
    started: &ToolStarted,
) -> Option<(AgentId, AgentToolCall, ToolName)> {
    host.internal_started_call(started)
}

const MAX_SKILL_CONTENT_BYTES: usize = 64 * 1024;
const MAX_SKILL_SEARCH_MATCHES: usize = 50;

fn handle_skill_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &AgentId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    let call_id = call.id.clone();
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    match handle_skill_query(host, &call.arguments) {
        Ok((result, display)) => host.finish_tool_with_cbor_result(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            result,
            display,
        ),
        Err((message, display)) => host.finish_tool_with_display_error(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            message,
            None,
            display,
        ),
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn handle_skill_query(
    host: &mut InternalToolHost<'_>,
    arguments: &CborValue,
) -> Result<(CborValue, Option<ToolUseState>), (String, Option<ToolUseState>)> {
    let needles = extract_skill_search_queries(arguments).map_err(|message| {
        (
            message.clone(),
            Some(skill_error_display("search:", &message)),
        )
    })?;
    let search_content = extract_optional_bool(arguments, "search_content")
        .map_err(|message| {
            (
                message.clone(),
                Some(skill_error_display("search:", &message)),
            )
        })?
        .unwrap_or(false);
    let skills = host.discovered_skills();
    let outcome = search_discovered_skills(&skills, &needles, search_content);
    for warning in &outcome.warnings {
        host.emit_info_important(warning);
    }
    if let Some(name) = outcome.auto_load_name.clone() {
        return read_skill_by_name(host, &skills, &name);
    }
    Ok(skill_search_result(&needles, search_content, outcome))
}

#[allow(clippy::result_large_err)]
fn read_skill_by_name(
    host: &mut InternalToolHost<'_>,
    skills: &[InternalSkill],
    name: &str,
) -> Result<(CborValue, Option<ToolUseState>), (String, Option<ToolUseState>)> {
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        let message = format!("unknown skill: {name}");
        return Err((message.clone(), Some(skill_error_display(name, &message))));
    };
    let source_label = skill.source.label();
    let read = read_skill_source_prefix(&skill.source, MAX_SKILL_CONTENT_BYTES).map_err(|e| {
        let message = format!("failed to read skill file: {e}");
        (message.clone(), Some(skill_error_display(name, &message)))
    })?;
    let mut body = skill_body_from_prefix(&read)
        .map_err(|message| (message.clone(), Some(skill_error_display(name, &message))))?;
    if read.truncated {
        host.emit_info_important(&format!(
            "skill too long: {source_label} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while loading {name}",
        ));
        body.push_str(&format!(
            "\n\n[skill content truncated at {MAX_SKILL_CONTENT_BYTES} bytes; file has {} bytes]",
            read.total_bytes
        ));
    }
    let mut display = skill_ok_display(name);
    display.stats = text_stats_for_skill(&body);
    Ok((
        CborValue::Map(vec![
            (
                CborValue::Text("name".to_owned()),
                CborValue::Text(name.to_owned()),
            ),
            (
                CborValue::Text("description".to_owned()),
                CborValue::Text(skill.description.clone()),
            ),
            (CborValue::Text("content".to_owned()), CborValue::Text(body)),
            (
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(read.truncated),
            ),
            (
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer(read.total_bytes.into()),
            ),
        ]),
        Some(display),
    ))
}

struct SkillSearchHit {
    matched_terms: usize,
    matched_fields: Vec<String>,
    name: String,
    description: String,
}
struct SkillSearchOutcome {
    hits: Vec<SkillSearchHit>,
    total_matches: usize,
    truncated: bool,
    auto_load_name: Option<String>,
    warnings: Vec<String>,
}

fn search_discovered_skills(
    skills: &[InternalSkill],
    needles: &[String],
    search_content: bool,
) -> SkillSearchOutcome {
    let mut warnings = Vec::new();
    let mut hits = Vec::new();
    let mut total_matches = 0;
    let mut only_hit_name = None;
    let mut exact_hit_name = None;
    for skill in skills {
        let lower_name = skill.name.to_lowercase();
        let lower_desc = skill.description.to_lowercase();
        let mut body: Option<String> = None;
        let mut matched_fields = Vec::new();
        let mut matched_terms = 0;
        for needle in needles {
            let mut matched = false;
            if lower_name.contains(needle) {
                matched = true;
                push_matched_field(&mut matched_fields, "name");
            }
            if lower_desc.contains(needle) {
                matched = true;
                push_matched_field(&mut matched_fields, "description");
            }
            if search_content {
                let body = body.get_or_insert_with(|| match read_skill_source_prefix(&skill.source, MAX_SKILL_CONTENT_BYTES) {
                    Ok(read) => match skill_body_from_prefix(&read) {
                        Ok(body) => { if read.truncated { warnings.push(format!("skill too long: {} truncated to {MAX_SKILL_CONTENT_BYTES} bytes while content-searching {}", skill.source.label(), skill.name)); } body.to_lowercase() }
                        Err(message) => { warnings.push(format!("skill frontmatter too long: {} while content-searching {}: {message}", skill.source.label(), skill.name)); String::new() }
                    },
                    Err(_) => String::new(),
                });
                if body.contains(needle) {
                    matched = true;
                    push_matched_field(&mut matched_fields, "content");
                }
            }
            if matched {
                matched_terms += 1;
            }
        }
        if matched_terms == 0 {
            continue;
        }
        total_matches += 1;
        only_hit_name = if total_matches == 1 {
            Some(skill.name.clone())
        } else {
            None
        };
        if needles.len() == 1 && skill.name == needles[0] {
            exact_hit_name = Some(skill.name.clone());
        }
        hits.push(SkillSearchHit {
            matched_terms,
            matched_fields,
            name: skill.name.clone(),
            description: tau_skills::truncate_description(&skill.description).into_owned(),
        });
        sort_skill_hits(&mut hits);
        if MAX_SKILL_SEARCH_MATCHES < hits.len() {
            hits.truncate(MAX_SKILL_SEARCH_MATCHES);
        }
    }
    SkillSearchOutcome {
        hits,
        total_matches,
        truncated: MAX_SKILL_SEARCH_MATCHES < total_matches,
        auto_load_name: if total_matches == 1 {
            only_hit_name
        } else {
            exact_hit_name
        },
        warnings,
    }
}

fn skill_search_result(
    needles: &[String],
    search_content: bool,
    outcome: SkillSearchOutcome,
) -> (CborValue, Option<ToolUseState>) {
    let scope_label = if search_content { " [content]" } else { "" };
    let display_args = format!("{}{scope_label}", needles.join(" "));
    let mut display = skill_ok_display(&display_args);
    display.stats = skill_search_stats(&outcome.hits);
    if outcome.truncated {
        display.status_text = format!(
            "ok: showing {} of {} matches",
            outcome.hits.len(),
            outcome.total_matches
        );
    }
    let total_matches = outcome.total_matches;
    let truncated = outcome.truncated;
    let matches = CborValue::Array(
        outcome
            .hits
            .into_iter()
            .map(|hit| {
                CborValue::Map(vec![
                    (
                        CborValue::Text("name".to_owned()),
                        CborValue::Text(hit.name),
                    ),
                    (
                        CborValue::Text("description".to_owned()),
                        CborValue::Text(hit.description),
                    ),
                    (
                        CborValue::Text("matched_terms".to_owned()),
                        CborValue::Integer((hit.matched_terms as u64).into()),
                    ),
                    (
                        CborValue::Text("matched_fields".to_owned()),
                        CborValue::Array(
                            hit.matched_fields
                                .into_iter()
                                .map(CborValue::Text)
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect(),
    );
    (
        CborValue::Map(vec![
            (
                CborValue::Text("queries".to_owned()),
                CborValue::Array(needles.iter().cloned().map(CborValue::Text).collect()),
            ),
            (
                CborValue::Text("search_content".to_owned()),
                CborValue::Bool(search_content),
            ),
            (CborValue::Text("matches".to_owned()), matches),
            (
                CborValue::Text("total_matches".to_owned()),
                CborValue::Integer((total_matches as u64).into()),
            ),
            (
                CborValue::Text("truncated".to_owned()),
                CborValue::Bool(truncated),
            ),
            (
                CborValue::Text("guidance".to_owned()),
                CborValue::Text(skill_search_guidance(total_matches, search_content)),
            ),
        ]),
        Some(display),
    )
}

// Keep this behavior in sync with harness/user_skill_invocation.rs: user
// `/skill` and the model-visible `skill` tool intentionally share bounded
// prefix reads, frontmatter truncation rejection, frontmatter stripping, and
// call-site truncation notes.
struct LimitedTextRead {
    text: String,
    truncated: bool,
    total_bytes: u64,
}
fn skill_body_from_prefix(read: &LimitedTextRead) -> Result<String, String> {
    if read.truncated && tau_skills::has_unclosed_frontmatter(&read.text) {
        return Err(format!(
            "frontmatter closing fence was not found before the {MAX_SKILL_CONTENT_BYTES} byte read limit; file has {} bytes",
            read.total_bytes
        ));
    }
    Ok(tau_skills::strip_frontmatter(&read.text).to_owned())
}
fn read_skill_source_prefix(
    source: &InternalSkillSource,
    max_bytes: usize,
) -> std::io::Result<LimitedTextRead> {
    match source {
        InternalSkillSource::File(path) => read_text_file_prefix(path, max_bytes),
        InternalSkillSource::BuiltIn { content } => {
            Ok(read_text_prefix(content.as_ref(), max_bytes))
        }
    }
}
fn read_text_file_prefix(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<LimitedTextRead> {
    let mut file = std::fs::File::open(path)?;
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = max_bytes < bytes.len();
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok(LimitedTextRead {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
        total_bytes,
    })
}
fn read_text_prefix(text: &str, max_bytes: usize) -> LimitedTextRead {
    let total_bytes = text.len() as u64;
    if text.len() <= max_bytes {
        return LimitedTextRead {
            text: text.to_owned(),
            truncated: false,
            total_bytes,
        };
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    LimitedTextRead {
        text: text[..end].to_owned(),
        truncated: true,
        total_bytes,
    }
}
fn sort_skill_hits(hits: &mut [SkillSearchHit]) {
    hits.sort_by(|a, b| {
        b.matched_terms
            .cmp(&a.matched_terms)
            .then_with(|| a.name.cmp(&b.name))
    });
}
fn skill_ok_display(args: &str) -> ToolUseState {
    ToolUseState {
        args: args.to_owned(),
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}
fn skill_error_display(args: &str, message: &str) -> ToolUseState {
    ToolUseState {
        args: args.to_owned(),
        status: ToolUseStatus::Error,
        status_text: error_chip_text(message),
        ..Default::default()
    }
}
fn text_stats_for_skill(text: &str) -> ToolUseStats {
    if text.is_empty() {
        return ToolUseStats::default();
    }
    ToolUseStats {
        matches: None,
        lines: Some(text.lines().count() as u64),
        bytes: Some(text.len() as u64),
    }
}
fn skill_search_stats(matches: &[SkillSearchHit]) -> ToolUseStats {
    let output = matches
        .iter()
        .map(|hit| format!("{}: {}", hit.name, hit.description))
        .collect::<Vec<_>>()
        .join("\n");
    let mut stats = text_stats_for_skill(&output);
    stats.matches = Some(matches.len() as u64);
    stats
}
fn skill_search_guidance(total_matches: usize, search_content: bool) -> String {
    if total_matches == 0 && search_content {
        return "No skills matched. Try different terms or fewer terms.".to_owned();
    }
    if total_matches == 0 {
        return include_str!("prompts/skill_search_guidance_empty.md").to_owned();
    }
    include_str!("prompts/skill_search_guidance_ambiguous.md").to_owned()
}
fn push_matched_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|existing| existing == field) {
        fields.push(field.to_owned());
    }
}
fn error_chip_text(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}
fn extract_skill_search_queries(arguments: &CborValue) -> Result<Vec<String>, String> {
    let raw = cbor_map_field(arguments, "query")
        .ok_or_else(|| "missing required argument: query".to_owned())?;
    let CborValue::Text(raw_query) = raw else {
        return Err("query must be a string".to_owned());
    };
    let needles = normalized_skill_query_terms(raw_query);
    if needles.is_empty() {
        return Err("query must include at least one non-empty term".to_owned());
    }
    Ok(needles)
}
fn extract_optional_bool(arguments: &CborValue, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = cbor_map_field(arguments, key) else {
        return Ok(None);
    };
    let CborValue::Bool(value) = value else {
        return Err(format!("{key} must be a boolean"));
    };
    Ok(Some(*value))
}
fn cbor_map_field<'a>(arguments: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries.iter().find_map(|(k, v)| match k {
        CborValue::Text(k) if k == key => Some(v),
        _ => None,
    })
}

fn wait_target_call_id(arguments: &CborValue) -> Option<&str> {
    match cbor_map_field(arguments, "tool_call_id") {
        Some(CborValue::Text(id)) if !id.trim().is_empty() => Some(id.as_str()),
        _ => None,
    }
}

fn normalized_skill_query_terms(raw_query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in raw_query.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() || ch == '-' {
            current.push(ch);
        } else {
            push_normalized_skill_term(&mut terms, &mut current);
        }
    }
    push_normalized_skill_term(&mut terms, &mut current);
    terms
}
fn push_normalized_skill_term(terms: &mut Vec<String>, current: &mut String) {
    let term = current.trim_matches('-');
    if !term.is_empty() && !terms.iter().any(|existing| existing == term) {
        terms.push(term.to_owned());
    }
    current.clear();
}

fn handle_message_tool_call(
    host: &mut InternalToolHost<'_>,
    conversation_id: &AgentId,
    call: &AgentToolCall,
    visible_tool_name: ToolName,
) -> Result<(), HarnessError> {
    let call_id = call.id.clone();
    host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
    let result = parse_message_args(&call.arguments).and_then(|parsed| {
        host.publish_agent_message(conversation_id, parsed.recipient_id, parsed.message)
    });
    match result {
        Ok(()) => host.finish_tool_with_result(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            "Message sent".to_owned(),
            None,
        ),
        Err(message) => host.finish_tool_with_error(
            conversation_id,
            call_id,
            visible_tool_name,
            call.tool_type,
            message,
            Some(call.arguments.clone()),
        ),
    }
    Ok(())
}

impl BuiltinTools {
    fn handle_cancel_tool_call(
        &self,
        host: &mut InternalToolHost<'_>,
        conversation_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id = call.id.clone();
        host.ensure_internal_tool_tracking(conversation_id, call, &visible_tool_name);
        let result = parse_cancel_args(&call.arguments).and_then(|target| {
            let mut state = self.state.lock().expect("builtin tool state poisoned");
            if state.cancel_requested.contains(&target) {
                return Err("Tool call already canceled".to_owned());
            }
            if !host.is_running_cancellable_tool_call(&target) {
                if host.is_completed_tool_call(&target) {
                    return Err("Tool call is already done".to_owned());
                }
                return Err("Unknown tool call id".to_owned());
            }
            state.cancel_requested.insert(target.clone());
            drop(state);
            host.publish_tool_cancel_request(target);
            Ok(())
        });
        match result {
            Ok(()) => host.finish_tool_with_result(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Tool cancellation requested".to_owned(),
                None,
            ),
            Err(message) => host.finish_tool_with_error(
                conversation_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }
}

struct MessageArgs {
    recipient_id: String,
    message: String,
}

fn parse_message_args(arguments: &CborValue) -> Result<MessageArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut recipient_id = None;
    let mut message = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "recipient_id" => match v {
                CborValue::Text(text) => recipient_id = Some(text.clone()),
                _ => return Err("`recipient_id` must be a string".to_owned()),
            },
            "message" => match v {
                CborValue::Text(text) => message = Some(text.clone()),
                _ => return Err("`message` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let recipient_id = recipient_id.ok_or_else(|| "`recipient_id` is required".to_owned())?;
    if recipient_id.trim().is_empty() {
        return Err("`recipient_id` must not be empty".to_owned());
    }
    let message = message.ok_or_else(|| "`message` is required".to_owned())?;
    if message.trim().is_empty() {
        return Err("`message` must not be empty".to_owned());
    }
    Ok(MessageArgs {
        recipient_id,
        message,
    })
}
#[derive(Debug)]
struct AgentWatchArgs {
    agent_id: String,
    enable: bool,
}

fn parse_agent_watch_args(arguments: &CborValue) -> Result<AgentWatchArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut agent_id = None;
    let mut enable = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "agent_id" => match v {
                CborValue::Text(text) => agent_id = Some(text.clone()),
                _ => return Err("`agent_id` must be a string".to_owned()),
            },
            "enable" => match v {
                CborValue::Bool(value) => enable = Some(*value),
                _ => return Err("`enable` must be a boolean".to_owned()),
            },
            _ => {}
        }
    }
    let agent_id = agent_id.ok_or_else(|| "`agent_id` is required".to_owned())?;
    if agent_id.trim().is_empty() {
        return Err("`agent_id` must not be empty".to_owned());
    }
    let enable = enable.ok_or_else(|| "`enable` is required".to_owned())?;
    Ok(AgentWatchArgs { agent_id, enable })
}

fn agent_watch_display_args(parsed: &AgentWatchArgs) -> String {
    let action = if parsed.enable { "on" } else { "off" };
    format!("{} {action}", parsed.agent_id)
}

fn agent_watch_response_should_notify(response: &ProviderResponseFinished) -> Option<String> {
    if !response.originator.is_user() || response.stop_reason.requests_tool_calls() {
        return None;
    }
    agent_watch_notification_message(response)
}

fn start_agent_watch_notification_message(
    _agent_id: &str,
    result: &tau_proto::StartAgentResult,
) -> Option<String> {
    if let Some(error) = result.error.as_deref()
        && !error.trim().is_empty()
    {
        return Some(error.to_owned());
    }
    (!result.text.trim().is_empty()).then(|| result.text.clone())
}

fn agent_watch_notification_message(response: &ProviderResponseFinished) -> Option<String> {
    if let Some(error) = response.error.as_deref()
        && !error.trim().is_empty()
    {
        return Some(error.to_owned());
    }
    assistant_text_from_output_items(&response.output_items)
}

fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(assistant_text_from_context_item)
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.trim().is_empty()).then_some(text)
}

fn assistant_text_from_context_item(item: &ContextItem) -> Option<String> {
    let ContextItem::Message(message) = item else {
        return None;
    };
    if message.role != ContextRole::Assistant {
        return None;
    }
    let text = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

#[derive(Debug)]
struct DelegateArgs {
    task_name: String,
    prompt: String,
    role: Option<String>,
}

fn parse_delegate_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut prompt = None;
    let mut task_name = None;
    let mut role = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "prompt" => match v {
                CborValue::Text(text) => prompt = Some(text.clone()),
                _ => return Err("`prompt` must be a string".to_owned()),
            },
            "task_name" => match v {
                CborValue::Text(text) => task_name = Some(text.clone()),
                _ => return Err("`task_name` must be a string".to_owned()),
            },
            "role" => match v {
                CborValue::Text(text) => role = Some(text.clone()),
                _ => return Err("`role` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let prompt = prompt.ok_or_else(|| "missing string argument: prompt".to_owned())?;
    if prompt.trim().is_empty() {
        return Err("`prompt` must not be empty".to_owned());
    }
    let task_name = task_name.ok_or_else(|| "missing string argument: task_name".to_owned())?;
    if task_name.trim().is_empty() {
        return Err("`task_name` must not be empty".to_owned());
    }
    Ok(DelegateArgs {
        task_name,
        prompt,
        role: role.filter(|role| !role.trim().is_empty()),
    })
}

fn delegate_instruction(self_agent_id: &str, prompt: &str) -> String {
    format!(
        include_str!("../../tau-harness/src/harness/prompts/delegate_prefix.md"),
        self_agent_id = self_agent_id,
        prompt = prompt,
    )
}

fn delegate_background_placeholder(
    call_id: &ToolCallId,
    self_agent_id: &str,
    sub_agent_id: &str,
) -> String {
    format!(
        "{}: true\nself_agent_id: {self_agent_id}\nsub_agent_id: {sub_agent_id}\n\nTool call `{call_id}` is running in the background.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn delegate_duration_seconds(elapsed: Duration) -> Option<u64> {
    if Duration::from_secs(SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS) < elapsed {
        Some(elapsed.as_secs_f64().ceil() as u64)
    } else {
        None
    }
}

fn delegate_final_display(
    task_name: &str,
    output: &str,
    input_stats: ToolUseStats,
    status: ToolUseStatus,
    status_text: &str,
) -> ToolUseState {
    let mut info_chips = Vec::new();
    let input_stats_chip = tool_use_stats_chip(input_stats);
    if !input_stats_chip.is_empty() {
        info_chips.push(format!("↘︎{input_stats_chip}"));
    }
    ToolUseState {
        args: format!("[{task_name}]"),
        stats: ToolUseStats::for_text(output),
        info_chips,
        status,
        status_text: status_text.to_owned(),
        ..Default::default()
    }
}

fn tool_use_stats_chip(stats: ToolUseStats) -> String {
    let mut parts = Vec::new();
    if let Some(matches) = stats.matches {
        parts.push(matches.to_string());
    }
    if let Some(lines) = stats.lines {
        parts.push(format!("{lines}L"));
    }
    if let Some(bytes) = stats.bytes {
        parts.push(format_tool_use_bytes(bytes));
    }
    parts.join(", ")
}

fn format_tool_use_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let kb = bytes as f64 / 1024.0;
    if kb < 100.0 {
        format!("{kb:.1}kB")
    } else {
        format!("{kb:.0}kB")
    }
}

fn delegate_result_value(
    text: Option<String>,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> CborValue {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return CborValue::Text(text.unwrap_or_default());
    }
    CborValue::Map(delegate_detail_entries(
        text,
        duration_seconds,
        self_agent_id,
        agent_id,
    ))
}

fn delegate_error_details(
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Option<CborValue> {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return None;
    }
    Some(CborValue::Map(delegate_detail_entries(
        None,
        duration_seconds,
        self_agent_id,
        agent_id,
    )))
}

fn delegate_detail_entries(
    output: Option<String>,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Vec<(CborValue, CborValue)> {
    let mut entries = Vec::new();
    if let Some(self_agent_id) = self_agent_id {
        entries.push((
            CborValue::Text("self_agent_id".to_owned()),
            CborValue::Text(self_agent_id.to_owned()),
        ));
    }
    if let Some(agent_id) = agent_id {
        entries.push((
            CborValue::Text("sub_agent_id".to_owned()),
            CborValue::Text(agent_id.to_owned()),
        ));
    }
    if let Some(duration_seconds) = duration_seconds {
        entries.push((
            CborValue::Text("duration_seconds".to_owned()),
            CborValue::Integer((duration_seconds as i64).into()),
        ));
    }
    if let Some(output) = output {
        entries.push((
            CborValue::Text("output".to_owned()),
            CborValue::Text(output),
        ));
    }
    entries
}

fn parse_cancel_args(arguments: &CborValue) -> Result<ToolCallId, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        if name == "tool_call_id" {
            return match v {
                CborValue::Text(text) if !text.is_empty() => Ok(text.clone().into()),
                CborValue::Text(_) => Err("`tool_call_id` must not be empty".to_owned()),
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Err("`tool_call_id` is required".to_owned())
}

fn skill_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(SKILL_TOOL_NAME),
        model_visible_name: None,
        description: Some("Discover and load short, focused skills. Most available skills are NOT pre-advertised in <available_skills>, so a missing entry there is no reason to skip this tool. If the search resolves to one skill, the full skill is loaded; otherwise matching skill names and descriptions are returned with guidance. Query terms are split on punctuation, lowercased, and deduplicated; hyphenated skill names are preserved. To load a specific ambiguous result, call this tool again with only the exact skill name.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Keywords matched case-insensitively against skill names and descriptions."},"search_content":{"type":"boolean","description":"When true, also search the first 64 KiB of the skill file (except the frontmatter). Default false."}},"required":["query"],"additionalProperties":false})),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn agent_start_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(AGENT_START_TOOL_NAME), model_visible_name: None, description: Some("Start a self-contained sub-task in a new sub-agent. The `prompt` must contain all information the sub-agent needs to complete the task. The sub-agent's responses are delivered asynchronously via `agent_watch` notifications until the caller disables the watch. The instant background placeholder and final result include `self_agent_id` and `sub_agent_id` metadata.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"task_name":{"type":"string","description":"Short user-visible label for the sub-task (a few words)."},"prompt":{"type":"string","description":"Self-contained task for the sub-agent."},"role":{"type":"string","description":"Optional sub-agent role to use."}},"required":["task_name","prompt"],"additionalProperties":false})), format: None, tags: Vec::new(), enabled_by_default: true, background_support: Some(BackgroundSupport::Never), examples: Vec::new() }
}

fn message_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(MESSAGE_TOOL_NAME), model_visible_name: None, description: Some("Send an async message to another agent, or the user. Use recipient_id `user`, or a `sub_agent_id` returned by `agent_start`. Requires `recipient_id` and `message`.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"recipient_id":{"type":"string","description":"Recipient agent_id, or the special value `user`."},"message":{"type":"string","description":"Message body."}},"required":["recipient_id","message"],"additionalProperties":false})), format: None, tags: Vec::new(), enabled_by_default: true, background_support: Some(BackgroundSupport::Never), examples: Vec::new() }
}

fn agent_watch_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(AGENT_WATCH_TOOL_NAME), model_visible_name: None, description: Some("Enable or disable persistent async notifications when another agent produces a response. `agent_start` automatically enables a watch for the started sub-agent; call `agent_watch` with `enable: false` to stop watching. Requires `agent_id` and `enable`.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"agent_id":{"type":"string","description":"Agent id to watch or stop watching."},"enable":{"type":"boolean","description":"True to enable watching, false to disable it."}},"required":["agent_id","enable"],"additionalProperties":false})), format: None, tags: Vec::new(), enabled_by_default: true, background_support: Some(BackgroundSupport::Never), examples: Vec::new() }
}

fn cancel_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(CANCEL_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Request cancellation of a running tool call. Requires `tool_call_id`.".to_owned(),
        ),
        tool_type: ToolType::Function,
        parameters: Some(
            serde_json::json!({"type":"object","properties":{"tool_call_id":{"type":"string","description":"Required tool_call_id to cancel."}},"required":["tool_call_id"],"additionalProperties":false}),
        ),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

fn wait_tool_spec() -> ToolSpec {
    ToolSpec { name: ToolName::new(WAIT_TOOL_NAME), model_visible_name: None, description: Some("Wait for completion of a background tool call with `tool_call_id`, or for the next completed background call in this conversation. When waiting for any call, the result includes an `original_tool_call_id` header identifying the completed call. Already-finished matching results return immediately. Tau will notify you via marked internal messages about background calls completing; `wait({})` consumes one completion and suppresses that completion notice.".to_owned()), tool_type: ToolType::Function, parameters: Some(serde_json::json!({"type":"object","properties":{"tool_call_id":{"type":"string","description":"Optional. When set, wait for this specific background tool call."}},"additionalProperties":false})), format: None, tags: Vec::new(), enabled_by_default: true, background_support: Some(BackgroundSupport::Never), examples: Vec::new() }
}

#[cfg(test)]
mod tests;
