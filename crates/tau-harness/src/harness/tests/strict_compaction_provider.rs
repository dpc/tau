use super::*;

/// Whether the strict provider remains connected after one reply.
#[derive(Clone, Copy)]
enum ReplyControl {
    /// Wait for the next provider prompt.
    Continue,
    /// Exit after flushing this terminal response.
    Disconnect,
}

/// Named response data produced by the strict provider's prompt policy.
struct StrictProviderReply {
    /// Provider-visible semantic output.
    output_items: Vec<ContextItem>,
    /// Closed provider stop category.
    stop_reason: tau_proto::ProviderStopReason,
    /// Bounded provider error for deliberately rejected compact input.
    error: Option<String>,
    /// Usage that drives the automatic compaction threshold.
    usage: Option<tau_proto::ProviderTokenUsage>,
    /// Connection action after the response is flushed.
    control: ReplyControl,
}

impl StrictProviderReply {
    fn text(text: &str, control: ReplyControl) -> Self {
        Self {
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: text.to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            usage: None,
            control,
        }
    }
}

/// Validates exact call-id and tool-type balance in one provider timeline.
pub(super) fn validate_closed_tool_timeline(
    context: &tau_proto::PromptContext,
) -> Result<(), &'static str> {
    let mut open = std::collections::HashMap::new();
    for item in context.flatten_iter() {
        match item {
            ContextItem::ToolCall(call) => {
                if open.insert(call.call_id, call.tool_type).is_some() {
                    return Err("duplicate tool call");
                }
            }
            ContextItem::ToolResult(result)
                if open.remove(&result.call_id) != Some(result.tool_type) =>
            {
                return Err("orphan or mismatched tool result");
            }
            _ => {}
        }
    }
    if open.is_empty() {
        Ok(())
    } else {
        Err("dangling tool call")
    }
}

fn reply_for_prompt(prompt: &AgentPromptCreated) -> StrictProviderReply {
    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
        return validate_closed_tool_timeline(&prompt.context).map_or_else(
            |error| StrictProviderReply {
                output_items: Vec::new(),
                stop_reason: tau_proto::ProviderStopReason::Error,
                error: Some(error.to_owned()),
                usage: None,
                control: ReplyControl::Continue,
            },
            |()| StrictProviderReply::text("strict compact summary", ReplyControl::Continue),
        );
    }
    let timeline: Vec<_> = prompt.context.flatten_iter().collect();
    if timeline
        .last()
        .is_some_and(|item| matches!(item, ContextItem::ToolResult(_)))
    {
        return StrictProviderReply::text("tool continuation complete", ReplyControl::Continue);
    }
    let is_later_prompt = timeline.iter().rev().any(|item| {
        matches!(
            item,
            ContextItem::Message(message)
                if message.role == ContextRole::User
                    && message.content.iter().any(|part| {
                        matches!(part, ContentPart::Text { text } if text.contains("later"))
                    })
        )
    });
    if is_later_prompt {
        return StrictProviderReply::text("later prompt complete", ReplyControl::Disconnect);
    }
    StrictProviderReply {
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-strict-echo".into(),
            name: ToolName::new("echo"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Text("strict echo".to_owned()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        usage: Some(tau_proto::ProviderTokenUsage {
            model: Some("strict/model".into()),
            prompt_sent_tokens: 100,
            prompt_cached_tokens: 0,
            response_received_tokens: 1,
            stats: Default::default(),
        }),
        control: ReplyControl::Continue,
    }
}

fn write_startup(
    writer: &mut TestInputWriter<BufWriter<UnixStream>>,
) -> Result<(), Box<dyn std::error::Error>> {
    writer.write_frame(&TestProtocolItem::Message(TestMessage::Hello(
        tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-strict-compaction-provider".into(),
            client_kind: tau_proto::ClientKind::Provider,
            capabilities: Default::default(),
        },
    )))?;
    writer.write_frame(&TestProtocolItem::Message(TestMessage::Subscribe(
        Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
        },
    )))?;
    writer.write_frame(&TestProtocolItem::Event(Event::ProviderModelsUpdated(
        tau_proto::ProviderModelsUpdated {
            models: vec![tau_proto::ProviderModelInfo {
                id: "strict/model".into(),
                display_name: Some("Strict compaction".to_owned()),
                tags: Vec::new(),
                supported_tool_types: vec![tau_proto::ToolType::Function],
                input_modalities: Vec::new(),
                tool_result_modalities: Vec::new(),
                supports_parallel_tool_calls: true,
                default_affinity: 0,
                context_window: 10_000,
                efforts: vec![tau_proto::Effort::Off],
                verbosities: vec![tau_proto::Verbosity::Low],
                thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
                supports_compaction: false,
                supports_standalone_compaction: true,
                standalone_compaction_threshold: Some(50),
            }],
        },
    )))?;
    writer.write_frame(&TestProtocolItem::Message(TestMessage::Ready(
        tau_proto::Ready {
            message: Some("strict compaction provider ready".to_owned()),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn run_provider(r: UnixStream, w: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = TestOutputReader::new(BufReader::new(r));
    let mut writer = TestInputWriter::new(BufWriter::new(w));
    write_startup(&mut writer)?;
    while let Some(frame) = reader.read_frame()? {
        let TestProtocolItem::Event(Event::AgentPromptCreated(prompt)) = frame.into_event_frame()
        else {
            continue;
        };
        writer.write_frame(&TestProtocolItem::Event(Event::ProviderPromptSubmitted(
            tau_proto::ProviderPromptSubmitted {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                originator: prompt.originator.clone(),
            },
        )))?;
        let reply = reply_for_prompt(&prompt);
        writer.write_frame(&TestProtocolItem::Event(Event::ProviderResponseFinished(
            ProviderResponseFinished {
                agent_prompt_id: prompt.agent_prompt_id,
                agent_id: prompt.agent_id,
                output_items: reply.output_items,
                stop_reason: reply.stop_reason,
                error: reply.error,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                originator: prompt.originator,
                usage: reply.usage,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            },
        )))?;
        writer.flush()?;
        if matches!(reply.control, ReplyControl::Disconnect) {
            return Ok(());
        }
    }
    Ok(())
}

/// Creates a harness backed by a real provider route that rejects open tool
/// timelines at the standalone-compaction boundary.
pub(super) fn strict_compaction_provider_harness(
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    fn runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        run_provider(r, w).map_err(|error| error.to_string())
    }
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut h = Harness::new_with_provider(
        state_dir,
        dirs,
        runner,
        Vec::new(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        tau_core::SessionPersistenceMode::Durable,
    )?;
    h.agent_id_rng = super::super::deterministic_agent_id_rng();
    h.enable_echo_tool_for_tests();
    Ok(h)
}
