//! Typed OTLP/OpenInference visualization projection.

use std::collections::BTreeMap;
use std::io as path_std_io;
use std::io::{Read as _, Seek as _, Write as _};

use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{Span, span};
use tau_core::{
    AgentEventParent, AgentJournalSnapshot, PersistedAgentEvent, PersistedAgentEventSeq,
};
use tau_proto::{
    AgentCreator, AgentId, AgentMessageId, AgentOuterTurnId, AgentPromptId, CompactionRequestId,
    CompactionTransactionId, Event, ToolCallId,
};

use super::native::occurrence_json;
use crate::InspectError;
use crate::lossless_json::{event_json, typed_cbor};

/// Domain-preserving key for one explicitly correlated durable operation.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum OperationKey {
    /// One outer agent turn.
    OuterTurn(AgentOuterTurnId),
    /// One provider prompt/response operation.
    Prompt(AgentPromptId),
    /// One tool invocation.
    Tool(ToolCallId),
    /// One inter-agent message.
    Message(AgentMessageId),
    /// One standalone compaction transaction.
    Compaction(CompactionTransactionId),
    /// One manual compaction request.
    CompactionRequest(CompactionRequestId),
}

impl OperationKey {
    /// Returns the OpenInference span kind.
    fn span_kind(&self) -> &'static str {
        match self {
            Self::Prompt(_) => "LLM",
            Self::Tool(_) => "TOOL",
            Self::OuterTurn(_)
            | Self::Message(_)
            | Self::Compaction(_)
            | Self::CompactionRequest(_) => "CHAIN",
        }
    }

    /// Returns the explicit durable ID without erasing its domain in the map
    /// key.
    fn id(&self) -> String {
        match self {
            Self::OuterTurn(id) => id.to_string(),
            Self::Prompt(id) => id.to_string(),
            Self::Tool(id) => id.to_string(),
            Self::Message(id) => id.to_string(),
            Self::Compaction(id) => id.to_string(),
            Self::CompactionRequest(id) => id.to_string(),
        }
    }

    /// Returns the concrete durable-ID domain for collision-free span IDs.
    fn domain_name(&self) -> &'static str {
        match self {
            Self::OuterTurn(_) => "outer_turn",
            Self::Prompt(_) => "prompt",
            Self::Tool(_) => "tool",
            Self::Message(_) => "message",
            Self::Compaction(_) => "compaction",
            Self::CompactionRequest(_) => "compaction_request",
        }
    }
}

/// Semantic role of one occurrence in an explicitly correlated lifecycle.
#[derive(Clone, Copy)]
enum Phase {
    /// Explicit operation start.
    Start,
    /// Intermediate diagnostic or progress fact.
    Auxiliary,
    /// Explicit semantic terminal.
    Terminal,
    /// Self-contained operation represented by one fact.
    Standalone,
}

/// Location of one lifecycle endpoint in anonymous correlation staging.
#[derive(Clone, Copy)]
struct Endpoint {
    /// Byte offset in the anonymous endpoint store.
    offset: u64,
    /// Encoded record byte length.
    length: u64,
}

/// Anonymous staging for every correlated occurrence. Compact operation state
/// retains offsets only for lifecycle endpoints.
struct EndpointStore {
    /// Anonymous file containing encoded correlated occurrences.
    file: std::fs::File,
}

/// Payload stored for one correlated lifecycle endpoint.
#[derive(serde::Deserialize, serde::Serialize)]
enum StagedEndpoint {
    /// Complete durable occurrence needed for raw fallback and terminal output.
    Record(Box<PersistedAgentEvent>),
    /// One compact tool-call item projected from a durable provider response.
    ProviderToolCall {
        /// Authoritative journal sequence of the containing response.
        seq: PersistedAgentEventSeq,
        /// Explicit fold parent of the containing response.
        parent: AgentEventParent,
        /// Durable record timestamp.
        recorded_at: tau_proto::UnixMicros,
        /// Prompt that produced this call.
        agent_prompt_id: AgentPromptId,
        /// Exactly one provider-declared tool call.
        call: Box<tau_proto::ToolCallItem>,
    },
}

impl StagedEndpoint {
    fn record(record: &PersistedAgentEvent) -> Self {
        Self::Record(Box::new(record.clone()))
    }

    fn provider_tool_call(
        record: &PersistedAgentEvent,
        finished: &tau_proto::ProviderResponseFinished,
        call: &tau_proto::ToolCallItem,
    ) -> Self {
        Self::ProviderToolCall {
            seq: record.seq,
            parent: record.parent,
            recorded_at: record.recorded_at,
            agent_prompt_id: finished.agent_prompt_id.clone(),
            call: Box::new(call.clone()),
        }
    }

    fn seq(&self) -> PersistedAgentEventSeq {
        match self {
            Self::Record(record) => record.seq,
            Self::ProviderToolCall { seq, .. } => *seq,
        }
    }

    fn parent(&self) -> AgentEventParent {
        match self {
            Self::Record(record) => record.parent,
            Self::ProviderToolCall { parent, .. } => *parent,
        }
    }

    fn recorded_at(&self) -> tau_proto::UnixMicros {
        match self {
            Self::Record(record) => record.recorded_at,
            Self::ProviderToolCall { recorded_at, .. } => *recorded_at,
        }
    }

    fn to_json(&self, agent_id: &AgentId) -> Result<String, InspectError> {
        match self {
            Self::Record(record) => occurrence_json(agent_id, record),
            Self::ProviderToolCall {
                seq,
                parent,
                recorded_at,
                agent_prompt_id,
                call,
            } => serde_json::to_string(&serde_json::json!({
                "schema": "tau.agent_trace",
                "schema_version": 0,
                "record_type": "provider_tool_call",
                "agent_id": agent_id,
                "seq": seq.get(),
                "recorded_at_unix_micros": recorded_at.get(),
                "parent": parent,
                "agent_prompt_id": agent_prompt_id,
                "call_id": call.call_id,
                "tool_name": call.name,
                "tool_type": call.tool_type,
                "arguments": typed_cbor(&call.arguments),
            }))
            .map_err(json_error),
        }
    }
}

impl EndpointStore {
    fn new() -> Result<Self, InspectError> {
        Ok(Self {
            file: tempfile::tempfile()?,
        })
    }

    fn append(&mut self, endpoint: &StagedEndpoint) -> Result<Endpoint, InspectError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(endpoint, &mut bytes).map_err(projection_error)?;
        let offset = self.file.seek(path_std_io::SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        Ok(Endpoint {
            offset,
            length: bytes.len() as u64,
        })
    }

    fn load(&mut self, endpoint: Endpoint) -> Result<StagedEndpoint, InspectError> {
        self.file
            .seek(path_std_io::SeekFrom::Start(endpoint.offset))?;
        let mut bytes = vec![0; endpoint.length as usize];
        self.file.read_exact(&mut bytes)?;
        ciborium::from_reader(bytes.as_slice()).map_err(projection_error)
    }
}

fn projection_error(error: impl std::fmt::Display) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to stage OTLP lifecycle endpoint: {error}"
    )))
}

/// Compact endpoint locations and timing state for one active operation.
struct OperationState {
    /// First correlated occurrence.
    first: Endpoint,
    /// Explicit start occurrence, when observed.
    start: Option<Endpoint>,
    /// Semantic terminal occurrence, when observed.
    terminal: Option<Endpoint>,
    /// Last timestamp in authoritative sequence order.
    previous_time: tau_proto::UnixMicros,
    /// Whether timestamps decreased anywhere inside this operation.
    decreasing: bool,
}

/// Writes one standard OTLP JSON `ExportTraceServiceRequest` by streaming each
/// journal and retaining only compact operation IDs and staged endpoint
/// offsets.
pub(super) fn write_json(
    root_agent_id: &AgentId,
    snapshot: &AgentJournalSnapshot,
    output: &mut impl std::io::Write,
) -> Result<(), InspectError> {
    let trace_id = hashed_id(&format!("tau-agent-trace:{root_agent_id}"), 16);
    write!(
        output,
        "{{\"resourceSpans\":[{{\"resource\":{{\"attributes\":[{},{}]}},\
         \"scopeSpans\":[{{\"scope\":{{\"name\":\"tau.agent_trace\",\"version\":\"0\"}},\
         \"spans\":[",
        serde_json::to_string(&string_attr("service.name", "tau")).map_err(json_error)?,
        serde_json::to_string(&string_attr("tau.root_agent_id", root_agent_id.as_str()))
            .map_err(json_error)?,
    )?;
    let mut first_span = true;
    for agent_id in snapshot.agent_ids() {
        let root_span_id = hashed_id(&format!("agent:{agent_id}"), 8);
        let mut endpoints = EndpointStore::new()?;
        let mut records = snapshot.records(agent_id)?;
        let first = records.next().expect("snapshot rejects empty journals")?;
        let mut last = first.clone();
        let mut previous_time = first.recorded_at;
        let mut decreasing = false;
        let mut operations = BTreeMap::new();
        correlate_record(&mut operations, &mut endpoints, &first)?;
        for record in records {
            let record = record?;
            decreasing |= record.recorded_at < previous_time;
            previous_time = record.recorded_at;
            correlate_record(&mut operations, &mut endpoints, &record)?;
            last = record;
        }
        let parent_span_id = creator_agent(&first)
            .filter(|creator| snapshot.contains_agent(creator))
            .map_or_else(Vec::new, |creator| {
                hashed_id(&format!("agent:{creator}"), 8)
            });
        let end = if decreasing {
            first.recorded_at
        } else {
            last.recorded_at
        };
        let mut attributes = vec![
            string_attr("openinference.span.kind", "AGENT"),
            string_attr("tau.timing.quality", "journal_wall_clock"),
            string_attr("tau.agent.id", agent_id.as_str()),
            int_attr("tau.journal.first_seq", first.seq.get()),
            int_attr("tau.journal.last_seq", last.seq.get()),
            string_attr(
                "tau.branch.parent",
                &serde_json::to_string(&first.parent).map_err(json_error)?,
            ),
        ];
        if decreasing {
            attributes.push(bool_attr("tau.incomplete", true));
        }
        write_agent_span(
            output,
            &mut first_span,
            Span {
                trace_id: trace_id.clone(),
                span_id: root_span_id.clone(),
                parent_span_id,
                name: format!("tau.agent {agent_id}"),
                kind: span::SpanKind::Internal.into(),
                start_time_unix_nano: micros_to_nanos(first.recorded_at),
                end_time_unix_nano: micros_to_nanos(end),
                attributes,
                ..Span::default()
            },
            agent_id,
            snapshot.records(agent_id)?,
        )?;
        for (key, state) in operations {
            write_span(
                output,
                &mut first_span,
                &operation_span(
                    &trace_id,
                    &root_span_id,
                    agent_id,
                    &mut endpoints,
                    &key,
                    &state,
                )?,
            )?;
        }
    }
    writeln!(output, "]}}]}}]}}")?;
    Ok(())
}

/// Serializes root-span metadata and streams raw occurrence events into its
/// `events` array without collecting a complete journal.
fn write_agent_span(
    output: &mut impl std::io::Write,
    first: &mut bool,
    span: Span,
    agent_id: &AgentId,
    records: impl Iterator<Item = Result<PersistedAgentEvent, tau_core::AgentStoreError>>,
) -> Result<(), InspectError> {
    if !*first {
        write!(output, ",")?;
    }
    *first = false;
    let mut metadata = serde_json::to_value(span).map_err(json_error)?;
    metadata
        .as_object_mut()
        .expect("serialized span is a JSON object")
        .remove("events");
    let mut prefix = serde_json::to_vec(&metadata).map_err(json_error)?;
    assert_eq!(prefix.pop(), Some(b'}'), "serialized span is a JSON object");
    output.write_all(&prefix)?;
    if prefix.len() != 1 {
        write!(output, ",")?;
    }
    write!(output, "\"events\":[")?;
    let mut first_event = true;
    for record in records {
        if !first_event {
            write!(output, ",")?;
        }
        first_event = false;
        serde_json::to_writer(&mut *output, &raw_event(agent_id, &record?)?).map_err(json_error)?;
    }
    write!(output, "]}}")?;
    Ok(())
}

fn write_span(
    output: &mut impl std::io::Write,
    first: &mut bool,
    span: &Span,
) -> Result<(), InspectError> {
    if !*first {
        write!(output, ",")?;
    }
    *first = false;
    serde_json::to_writer(output, span).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to serialize OTLP trace: {error}"
    )))
}

fn correlate_record(
    operations: &mut BTreeMap<OperationKey, OperationState>,
    endpoints: &mut EndpointStore,
    record: &PersistedAgentEvent,
) -> Result<(), InspectError> {
    if let Event::ProviderResponseFinished(finished)
    | Event::ProviderResponseFinishedReported(finished) = &record.event
    {
        for item in &finished.output_items {
            let tau_proto::ContextItem::ToolCall(call) = item else {
                continue;
            };
            let endpoint =
                endpoints.append(&StagedEndpoint::provider_tool_call(record, finished, call))?;
            apply_occurrence(
                operations,
                OperationKey::Tool(call.call_id.clone()),
                Phase::Start,
                endpoint,
                record.recorded_at,
            );
        }
    }
    let occurrences = operation_occurrences(&record.event);
    if occurrences.is_empty() {
        return Ok(());
    }
    for (key, phase) in occurrences {
        let endpoint = endpoints.append(&StagedEndpoint::record(record))?;
        apply_occurrence(operations, key, phase, endpoint, record.recorded_at);
    }
    Ok(())
}

fn apply_occurrence(
    operations: &mut BTreeMap<OperationKey, OperationState>,
    key: OperationKey,
    phase: Phase,
    endpoint: Endpoint,
    recorded_at: tau_proto::UnixMicros,
) {
    operations
        .entry(key)
        .and_modify(|state: &mut OperationState| {
            state.decreasing |= recorded_at < state.previous_time;
            state.previous_time = recorded_at;
            match phase {
                Phase::Start => {
                    state.start.get_or_insert(endpoint);
                }
                Phase::Terminal => state.terminal = Some(endpoint),
                Phase::Standalone => {
                    state.start.get_or_insert(endpoint);
                    state.terminal = Some(endpoint);
                }
                Phase::Auxiliary => {}
            }
        })
        .or_insert_with(|| OperationState {
            first: endpoint,
            start: matches!(phase, Phase::Start | Phase::Standalone).then_some(endpoint),
            terminal: matches!(phase, Phase::Terminal | Phase::Standalone).then_some(endpoint),
            previous_time: recorded_at,
            decreasing: false,
        });
}

#[allow(clippy::too_many_lines)]
fn operation_occurrences(event: &Event) -> Vec<(OperationKey, Phase)> {
    let mut occurrences = Vec::new();
    if let Event::AgentStandaloneCompactionStarted(started) = event
        && let tau_proto::StandaloneCompactionTrigger::ManualAgentTool { request_id, .. } =
            &started.trigger
    {
        occurrences.push((
            OperationKey::CompactionRequest(request_id.clone()),
            Phase::Terminal,
        ));
    }
    let occurrence = match event {
        Event::AgentOuterTurnStarted(value) => (
            OperationKey::OuterTurn(value.outer_turn_id.clone()),
            Phase::Start,
        ),
        Event::AgentOuterTurnFinished(value) => (
            OperationKey::OuterTurn(value.outer_turn_id.clone()),
            Phase::Terminal,
        ),
        Event::AgentPromptStarted(value) => (
            OperationKey::Prompt(value.agent_prompt_id.clone()),
            Phase::Start,
        ),
        Event::ProviderPromptSubmitted(value) | Event::ProviderPromptSubmittedReported(value) => (
            OperationKey::Prompt(value.agent_prompt_id.clone()),
            Phase::Auxiliary,
        ),
        Event::ProviderResponseUpdated(value) | Event::ProviderResponseUpdatedReported(value) => (
            OperationKey::Prompt(value.agent_prompt_id.clone()),
            Phase::Auxiliary,
        ),
        Event::ProviderResponseFinished(value) | Event::ProviderResponseFinishedReported(value) => {
            (
                OperationKey::Prompt(value.agent_prompt_id.clone()),
                Phase::Terminal,
            )
        }
        Event::ProviderCacheMissDiagnostic(value)
        | Event::ProviderCacheMissDiagnosticReported(value) => (
            OperationKey::Prompt(value.agent_prompt_id.clone()),
            Phase::Auxiliary,
        ),
        Event::AgentPromptTerminated(value) => (
            OperationKey::Prompt(value.agent_prompt_id.clone()),
            Phase::Terminal,
        ),
        Event::ToolRequest(value) => (OperationKey::Tool(value.call_id.clone()), Phase::Auxiliary),
        Event::ToolStarted(value) => (OperationKey::Tool(value.call_id.clone()), Phase::Start),
        Event::ToolProgress(value) | Event::ToolProgressReported(value) => {
            (OperationKey::Tool(value.call_id.clone()), Phase::Auxiliary)
        }
        Event::ToolResult(value)
        | Event::ToolResultReported(value)
        | Event::ProviderToolResult(value) => (
            OperationKey::Tool(value.call_id.clone()),
            if value.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                Phase::Auxiliary
            } else {
                Phase::Terminal
            },
        ),
        Event::ToolBackgroundResult(value) => {
            (OperationKey::Tool(value.call_id.clone()), Phase::Terminal)
        }
        Event::ToolError(value)
        | Event::ToolErrorReported(value)
        | Event::ProviderToolError(value) => {
            (OperationKey::Tool(value.call_id.clone()), Phase::Terminal)
        }
        Event::ToolBackgroundError(value) => {
            (OperationKey::Tool(value.call_id.clone()), Phase::Terminal)
        }
        Event::ToolRejected(value) => (OperationKey::Tool(value.call_id.clone()), Phase::Terminal),
        Event::ToolCancelled(value) | Event::ToolCancelledReported(value) => {
            (OperationKey::Tool(value.call_id.clone()), Phase::Terminal)
        }
        Event::AgentMessageSent(value) => (
            OperationKey::Message(value.message_id.clone()),
            Phase::Standalone,
        ),
        Event::AgentMessageReceived(value) => (
            OperationKey::Message(value.message_id.clone()),
            Phase::Standalone,
        ),
        Event::AgentStandaloneCompactionStarted(value) => (
            OperationKey::Compaction(value.transaction_id.clone()),
            Phase::Start,
        ),
        Event::AgentStandaloneCompactionFailed(value) => (
            OperationKey::Compaction(value.transaction_id.clone()),
            Phase::Terminal,
        ),
        Event::AgentCompacted(value) => {
            let Some(transaction_id) = &value.transaction_id else {
                return occurrences;
            };
            (
                OperationKey::Compaction(transaction_id.clone()),
                Phase::Terminal,
            )
        }
        Event::AgentManualCompactionRequested(value) => (
            OperationKey::CompactionRequest(value.request_id.clone()),
            Phase::Start,
        ),
        Event::AgentManualCompactionRequestFailed(value) => (
            OperationKey::CompactionRequest(value.request_id.clone()),
            Phase::Terminal,
        ),
        _ => return occurrences,
    };
    occurrences.push(occurrence);
    occurrences
}

fn operation_span(
    trace_id: &[u8],
    parent_span_id: &[u8],
    agent_id: &AgentId,
    endpoints: &mut EndpointStore,
    key: &OperationKey,
    state: &OperationState,
) -> Result<Span, InspectError> {
    let complete = state.start.is_some() && state.terminal.is_some() && !state.decreasing;
    let start_endpoint = state.start.unwrap_or(state.first);
    let terminal_endpoint = state.terminal.unwrap_or(start_endpoint);
    let start = endpoints.load(start_endpoint)?;
    let terminal = endpoints.load(terminal_endpoint)?;
    let end_time = if complete {
        terminal.recorded_at()
    } else {
        start.recorded_at()
    };
    let mut attributes = vec![
        string_attr("openinference.span.kind", key.span_kind()),
        string_attr("tau.timing.quality", "journal_wall_clock"),
        string_attr("tau.agent.id", agent_id.as_str()),
        string_attr("tau.operation.id", &key.id()),
        int_attr("tau.journal.first_seq", start.seq().get()),
        int_attr("tau.journal.last_seq", terminal.seq().get()),
        string_attr(
            "tau.branch.parent",
            &serde_json::to_string(&start.parent()).map_err(json_error)?,
        ),
        string_attr("input.value", &start.to_json(agent_id)?),
        string_attr(
            "tau.input.scope",
            if matches!(&start, StagedEndpoint::ProviderToolCall { .. }) {
                "durable_provider_tool_call_item"
            } else {
                "lifecycle_metadata_only"
            },
        ),
        string_attr("output.value", &terminal.to_json(agent_id)?),
    ];
    match key {
        OperationKey::OuterTurn(id) => {
            attributes.push(string_attr("tau.agent.outer_turn_id", &id.to_string()));
        }
        OperationKey::Prompt(id) => {
            attributes.push(string_attr("tau.agent.prompt_id", id.as_str()));
            if let (StagedEndpoint::Record(start), StagedEndpoint::Record(terminal)) =
                (&start, &terminal)
            {
                extend_prompt_attributes(&mut attributes, &start.event, &terminal.event);
            }
        }
        OperationKey::Tool(id) => {
            attributes.push(string_attr("tau.tool.call_id", id.as_str()));
            extend_tool_attributes(&mut attributes, &start, &terminal);
        }
        OperationKey::Message(id) => {
            attributes.push(string_attr("tau.agent.message_id", id.as_str()));
        }
        OperationKey::Compaction(id) => {
            attributes.push(string_attr(
                "tau.compaction.transaction_id",
                &id.to_string(),
            ));
        }
        OperationKey::CompactionRequest(id) => {
            attributes.push(string_attr("tau.compaction.request_id", &id.to_string()));
        }
    }
    if !complete {
        attributes.push(bool_attr("tau.incomplete", true));
    }
    Ok(Span {
        trace_id: trace_id.to_vec(),
        span_id: hashed_id(
            &format!("operation:{agent_id}:{}:{}", key.domain_name(), key.id()),
            8,
        ),
        parent_span_id: parent_span_id.to_vec(),
        name: format!("tau.{} {}", key.span_kind(), key.id()),
        kind: span::SpanKind::Internal.into(),
        start_time_unix_nano: micros_to_nanos(start.recorded_at()),
        end_time_unix_nano: micros_to_nanos(end_time),
        attributes,
        ..Span::default()
    })
}

fn extend_prompt_attributes(attributes: &mut Vec<KeyValue>, start: &Event, terminal: &Event) {
    if let Event::AgentPromptStarted(started) = start {
        attributes.push(string_attr("tau.session.id", started.session_id.as_str()));
        attributes.push(string_attr("llm.model_name", &started.model.to_string()));
        if let Ok(params) = serde_json::to_string(&started.model_params) {
            attributes.push(string_attr("llm.invocation_parameters", &params));
        }
    }
    let (Event::ProviderResponseFinished(finished)
    | Event::ProviderResponseFinishedReported(finished)) = terminal
    else {
        return;
    };
    if let Some(usage) = &finished.usage {
        attributes.push(int_attr("llm.token_count.prompt", usage.prompt_sent_tokens));
        attributes.push(int_attr(
            "llm.token_count.completion",
            usage.response_received_tokens,
        ));
        attributes.push(int_attr(
            "llm.token_count.total",
            usage
                .prompt_sent_tokens
                .saturating_add(usage.response_received_tokens),
        ));
        attributes.push(int_attr(
            "llm.token_count.prompt_details.cache_read",
            usage.prompt_cached_tokens,
        ));
    }
    if let Ok(cost) = serde_json::to_string(&finished.estimated_api_cost_increment) {
        attributes.push(string_attr("tau.estimated_api_cost_increment", &cost));
    }
    if let Ok(rates) = serde_json::to_string(&finished.estimated_api_cost_rates) {
        attributes.push(string_attr("tau.estimated_api_cost_rates", &rates));
    }
}

fn extend_tool_attributes(
    attributes: &mut Vec<KeyValue>,
    start: &StagedEndpoint,
    terminal: &StagedEndpoint,
) {
    match start {
        StagedEndpoint::Record(record) if matches!(&record.event, Event::ToolStarted(_)) => {
            let Event::ToolStarted(value) = &record.event else {
                unreachable!("guarded tool start")
            };
            attributes.push(string_attr("tool.name", value.tool_name.as_str()));
            if let Ok(arguments) = serde_json::to_string(&typed_cbor(&value.arguments)) {
                attributes.push(string_attr("tool.parameters", &arguments));
            }
        }
        StagedEndpoint::Record(record) if matches!(&record.event, Event::ToolRequest(_)) => {
            let Event::ToolRequest(value) = &record.event else {
                unreachable!("guarded tool request")
            };
            attributes.push(string_attr("tool.name", value.tool_name.as_str()));
            if let Ok(arguments) = serde_json::to_string(&typed_cbor(&value.arguments)) {
                attributes.push(string_attr("tool.parameters", &arguments));
            }
        }
        StagedEndpoint::ProviderToolCall { call, .. } => {
            attributes.push(string_attr("tool.name", call.name.as_str()));
            if let Ok(arguments) = serde_json::to_string(&typed_cbor(&call.arguments)) {
                attributes.push(string_attr("tool.parameters", &arguments));
            }
        }
        _ => {}
    }
    if let StagedEndpoint::Record(terminal) = terminal
        && let Ok(output) = event_json(&terminal.event)
            .and_then(|value| serde_json::to_string(&value).map_err(json_error))
    {
        attributes.push(string_attr("tool.output", &output));
    }
}

fn creator_agent(record: &PersistedAgentEvent) -> Option<&AgentId> {
    let Event::AgentStarted(started) = &record.event else {
        return None;
    };
    let Some(AgentCreator::Agent { agent_id, .. }) = &started.creator else {
        return None;
    };
    Some(agent_id)
}

fn raw_event(
    agent_id: &AgentId,
    record: &PersistedAgentEvent,
) -> Result<span::Event, InspectError> {
    Ok(span::Event {
        time_unix_nano: micros_to_nanos(record.recorded_at),
        name: record.event.name().to_string(),
        attributes: vec![
            int_attr("tau.journal.seq", record.seq.get()),
            string_attr("tau.event.raw", &occurrence_json(agent_id, record)?),
        ],
        ..span::Event::default()
    })
}

fn string_attr(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_owned())),
        }),
        ..KeyValue::default()
    }
}

fn int_attr(key: &str, value: u64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(
                i64::try_from(value).unwrap_or(i64::MAX),
            )),
        }),
        ..KeyValue::default()
    }
}

fn bool_attr(key: &str, value: bool) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        }),
        ..KeyValue::default()
    }
}

fn micros_to_nanos(value: tau_proto::UnixMicros) -> u64 {
    value.get().saturating_mul(1_000)
}

fn hashed_id(seed: &str, length: usize) -> Vec<u8> {
    let hash = |salt: u64| {
        seed.bytes()
            .fold(0xcbf2_9ce4_8422_2325 ^ salt, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
            })
    };
    [
        hash(0).to_be_bytes(),
        hash(0x9e37_79b9_7f4a_7c15).to_be_bytes(),
    ]
    .concat()[..length]
        .to_vec()
}

#[cfg(test)]
mod tests;
