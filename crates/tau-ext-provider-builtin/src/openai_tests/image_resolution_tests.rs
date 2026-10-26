//! Artifact responses cross the real provider loop before actual adapter
//! egress.

use std::num::NonZeroU64;

use super::*;
use crate::image_resolution::tests as images;

/// A reference-backed prefix that fits the published cap must reach Chat with
/// resolved image bytes even when its expanded inline JSON exceeds that cap.
/// A genuine canonical overflow must fail before any Artifact RPC or HTTP work.
#[test]
fn chat_summary_admits_canonical_prefix_before_private_image_resolution() {
    use std::io::BufRead as _;

    for fits in [true, false] {
        let mut request = prompt();
        request.context = image_context();
        request.context.blocks.insert(
            0,
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::Message(tau_proto::MessageItem {
                    role: tau_proto::ContextRole::User,
                    content: vec![tau_proto::ContentPart::Text {
                        text: "earlier closed prefix".into(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
            }),
        );
        request.operation = tau_proto::PromptOperation::StandaloneCompaction;
        request.model = model_id("chat", "model");
        request
            .context
            .blocks
            .push(tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::CompactionTrigger],
                },
            ));
        let canonical_bytes =
            tau_provider::local_summary_compaction::historical_prefix_json_bytes(&request.context)
                .expect("canonical bytes");
        let mut expanded = request.context.clone();
        let tau_proto::ContextBlock::ToolResults(results) = &mut expanded.blocks[2] else {
            panic!("closed image round");
        };
        let tau_proto::ToolResultContentPart::ImageReference(reference) =
            &results.items[0].provider_content[0]
        else {
            panic!("reference");
        };
        let mut image = reference.metadata_only_image();
        image.data = images::png().into();
        results.items[0].provider_content[0] = tau_proto::ToolResultContentPart::Image(image);
        assert!(
            tau_provider::local_summary_compaction::historical_prefix_json_bytes(&expanded)
                .expect("expanded bytes")
                > canonical_bytes
        );
        let cap = canonical_bytes.get() - u64::from(!fits);
        let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = fits.then(|| thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("deadline");
            let mut reader = BufReader::new(&mut stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("HTTP header");
                if line == "\r\n" { break; }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().expect("length"));
                }
            }
            let mut body = vec![0; length.expect("body length")];
            reader.read_exact(&mut body).expect("HTTP body");
            let body: serde_json::Value = serde_json::from_slice(&body).expect("request JSON");
            let events = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"image checkpoint\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}", events.len()).expect("response");
            body
        }));

        let mut model = chat_model("model");
        model.input_modalities = vec![tau_proto::InputModality::Image];
        model.tool_result_modalities = vec![tau_proto::InputModality::Image];
        model.local_summary_compaction =
            Some(crate::chat_completions::LocalSummaryCompactionConfig {
                max_input_bytes: NonZeroU64::new(cap),
                ..Default::default()
            });
        let mut profiles = BuiltinProviderProfiles::default();
        profiles.providers.insert(
            ProviderName::new("chat"),
            BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
                base_url: format!("http://{address}/v1"),
                models: vec![model],
                ..Default::default()
            }),
        );
        let prompt_profiles = profiles.clone();
        let input = BlockingInput::default();
        input.push(encode_frames(&[live_event(
            11,
            Event::AgentPromptCreated(request.clone()),
        )]));
        let output = SharedWriter::default();
        let runtime_input = input.clone();
        let runtime_output = output.clone();
        let runtime = thread::spawn(move || {
            run_inner_with_executors(
                runtime_input,
                runtime_output,
                profiles,
                move |_| prompt_profiles.clone(),
                1,
                production_prompt_executor(),
                production_prewarm_executor(),
            )
            .expect("provider")
        });
        let mut seen = HashSet::new();
        let frames = drive_artifacts(&input, &output, &mut seen, |frames| {
            frames.iter().any(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(_))
                )
            })
        });
        input.close();
        runtime.join().expect("runtime");
        let finished = frames
            .iter()
            .find_map(|frame| match input_event(frame) {
                Some(Event::ProviderResponseFinishedReported(finished)) => Some(finished),
                _ => None,
            })
            .expect("terminal");
        if fits {
            assert_eq!(finished.stop_reason, tau_proto::ProviderStopReason::EndTurn);
            assert_eq!(finished.failure_kind, None);
            assert_eq!(seen.len(), 3, "real Open/Read/Close resolution");
            let wire = server
                .expect("server")
                .join()
                .expect("HTTP request")
                .to_string();
            assert!(wire.contains("data:image/png;base64,"));
            assert!(!wire.contains(images::descriptor(&images::png()).key.as_str()));
            assert!(matches!(
                finished.output_items.as_slice(),
                [ContextItem::LocalCompactionNarrative(_)]
            ));
        } else {
            assert_eq!(
                finished.failure_kind,
                Some(tau_proto::ProviderFailureKind::RequestRejected)
            );
            assert!(
                seen.is_empty(),
                "canonical overflow must precede image downloads"
            );
        }
        assert!(
            request.has_image_references(),
            "retained prompt remains canonical"
        );
    }
}

/// Supplies a closed historical image round, with no synthetic user image.
fn image_context() -> tau_proto::PromptContext {
    tau_proto::PromptContext {
        blocks: vec![
            tau_proto::ContextBlock::AssistantResponse(tau_proto::AssistantResponseBlock {
                provider_response_id: None,
                backend: None,
                output_items: vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
                    call_id: "image".into(),
                    name: tau_proto::ToolName::new("read_image"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: tau_proto::CborValue::Map(Vec::new()),
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                usage: None,
            }),
            tau_proto::ContextBlock::ToolResults(tau_proto::ToolResultsBlock {
                items: vec![images::result()],
            }),
        ],
    }
}

/// Writes a directed reply without injecting another Configure boundary.
fn push(input: &BlockingInput, message: HarnessOutputMessage) {
    let mut bytes = Vec::new();
    {
        let mut writer = HarnessOutputWriter::new(&mut bytes);
        writer.write_message(&message).expect("reply");
        writer.flush().expect("flush");
    }
    input.push(bytes);
}

/// Answers exact request IDs through the real SDK input reader until a terminal
/// condition is observable. No competing protocol reader is introduced.
fn drive_artifacts(
    input: &BlockingInput,
    output: &SharedWriter,
    seen: &mut HashSet<tau_proto::ArtifactRequestId>,
    done: impl Fn(&[HarnessInputMessage]) -> bool,
) -> Vec<HarnessInputMessage> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let bytes = output.bytes();
        if let Some(frames) = try_decode_frames(&bytes) {
            for frame in &frames {
                if let HarnessInputMessage::ArtifactRequest(request) = frame
                    && seen.insert(request.request_id.clone())
                {
                    push(
                        input,
                        HarnessOutputMessage::ArtifactResult(Box::new(tau_proto::ArtifactResult {
                            request_id: request.request_id.clone(),
                            result: images::artifact_reply(&request.op, &images::png()),
                        })),
                    );
                }
            }
            if done(&frames) {
                return frames;
            }
        }
        assert!(
            Instant::now() < deadline,
            "provider image resolution did not finish"
        );
        output.wait_for_change(bytes.len(), deadline);
    }
}

/// A fake local Codex socket observes the real lowered data URLs for ordinary
/// inference, native compact, local-summary fallback and dedicated refresh.
#[test]
fn provider_reference_resolution_reaches_all_codex_submission_paths() {
    for route in ["ordinary-and-prewarm", "native-compact", "local-summary"] {
        let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("deadline");
            let mut socket = tungstenite::accept(stream).expect("upgrade");
            let mut requests = Vec::new();
            for ordinal in 0..if route == "ordinary-and-prewarm" {
                2
            } else {
                1
            } {
                let tungstenite::Message::Text(request) = socket.read().expect("request") else {
                    panic!("JSON request")
                };
                let request: serde_json::Value = serde_json::from_str(&request).expect("JSON");
                let serialized = request.to_string();
                assert!(
                    serialized.contains("data:image/png;base64,"),
                    "{route}: {request}"
                );
                assert!(!serialized.contains(images::descriptor(&images::png()).key.as_str()));
                if ordinal == 1 {
                    assert_eq!(request["generate"], false);
                }
                requests.push(request);
                let item = if route == "native-compact" {
                    serde_json::json!({"type":"compaction", "id":"cmp-reference", "encrypted_content":"opaque"})
                } else {
                    serde_json::json!({"type":"message", "role":"assistant",
                        "content":[{"type":"output_text", "text":"Keep the image result."}]})
                };
                if ordinal == 0 {
                    socket
                        .send(tungstenite::Message::Text(
                            serde_json::json!({
                                "type":"response.output_item.done", "output_index":0, "item":item
                            })
                            .to_string()
                            .into(),
                        ))
                        .expect("item");
                }
                socket.send(tungstenite::Message::Text(serde_json::json!({
                    "type":"response.completed",
                    "response":{"id":format!("resp-{ordinal}"), "usage":{"input_tokens":100,"output_tokens":4}}
                }).to_string().into())).expect("terminal");
            }
            requests
        });

        let mut request = prompt();
        request.context = image_context();
        request.model = model_id(
            CHATGPT_PROVIDER_NAME,
            if route == "local-summary" {
                "gpt-6-luna"
            } else {
                "gpt-5.6-sol"
            },
        );
        if route != "ordinary-and-prewarm" {
            request.operation = tau_proto::PromptOperation::StandaloneCompaction;
            request
                .context
                .blocks
                .push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::CompactionTrigger],
                    },
                ));
        }
        let mut warm = prewarm();
        warm.context = image_context();
        warm.model = Some(request.model.clone());
        let input = BlockingInput::default();
        input.push(encode_frames(&[live_event(
            11,
            Event::AgentPromptCreated(request),
        )]));
        let endpoint = format!("http://{address}/backend-api");
        let warm_endpoint = endpoint.clone();
        let executor: PromptExecutor = Arc::new(move |mut execution| {
            let PromptBackend::Responses(config) = &mut execution.job.backend else {
                panic!("Codex")
            };
            *config = config.clone().with_test_base_url(endpoint.clone());
            production_prompt_executor()(execution);
        });
        let warm_executor: PrewarmExecutor = Arc::new(move |mut execution| {
            execution.config = execution.config.with_test_base_url(warm_endpoint.clone());
            production_prewarm_executor()(execution)
        });
        let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
        let prompt_profiles = profiles.clone();
        let output = SharedWriter::default();
        let runtime_output = output.clone();
        let runtime_input = input.clone();
        let runtime = thread::spawn(move || {
            run_inner_with_executors(
                runtime_input,
                runtime_output,
                profiles,
                move |_| prompt_profiles.clone(),
                1,
                executor,
                warm_executor,
            )
            .expect("provider")
        });
        let mut seen = HashSet::new();
        let frames = drive_artifacts(&input, &output, &mut seen, |frames| {
            frames.iter().any(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(_))
                )
            })
        });
        assert!(frames.iter().any(|frame| matches!(input_event(frame),
            Some(Event::ProviderResponseFinishedReported(finished))
                if finished.stop_reason == tau_proto::ProviderStopReason::EndTurn)));
        if route == "ordinary-and-prewarm" {
            push(
                &input,
                live_event(
                    12,
                    Event::AgentCacheRefreshRequested(tau_proto::AgentCacheRefreshRequested {
                        refresh_id: "pcr-image".parse().expect("refresh"),
                        prompt: warm,
                        stop_after_millis: NonZeroU32::new(5_000).expect("deadline"),
                    }),
                ),
            );
            let frames = drive_artifacts(&input, &output, &mut seen, |frames| {
                frames.iter().any(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderCacheRefreshFinishedReported(_))
                    )
                })
            });
            assert!(frames.iter().any(|frame| matches!(input_event(frame),
                Some(Event::ProviderCacheRefreshFinishedReported(finished))
                    if finished.status == tau_proto::ProviderCacheRefreshStatus::Succeeded)));
        }
        input.close();
        runtime.join().expect("runtime");
        let requests = server.join().expect("server");
        assert_eq!(
            requests.len(),
            if route == "ordinary-and-prewarm" {
                2
            } else {
                1
            }
        );
        assert_eq!(
            seen.len(),
            3,
            "one Open/Read/Close, including the warm cache hit"
        );
    }
}
