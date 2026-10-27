use super::*;

/// Runs the real Codex transport and production worker through the logical
/// scheduler, returning the sole final report after all requested attempts.
fn run_failure_accounting(
    operation: tau_proto::PromptOperation,
    events: Vec<serde_json::Value>,
) -> ProviderResponseFinishedReport {
    let expected_attempts = events.len();
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        for event in events {
            let (socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut socket = tungstenite::accept(socket).expect("upgrade");
            let tungstenite::Message::Text(request) = socket.read().expect("request") else {
                panic!("response.create");
            };
            let request: serde_json::Value = serde_json::from_str(&request).expect("JSON");
            assert_eq!(request["type"], "response.create");
            assert!(
                request.get("previous_response_id").is_none(),
                "failed IDs cannot chain"
            );
            socket
                .send(tungstenite::Message::Text(event.to_string().into()))
                .expect("failure");
        }
    });
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    let mut created = prompt();
    created.operation = operation;
    if operation == tau_proto::PromptOperation::StandaloneCompaction {
        created
            .context
            .blocks
            .push(tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::CompactionTrigger],
                },
            ));
    }
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(created),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |mut execution| {
        executor_attempts.fetch_add(1, Ordering::SeqCst);
        let PromptBackend::Responses(config) = &mut execution.job.backend else {
            panic!("Codex route");
        };
        *config = config
            .clone()
            .with_test_base_url(format!("http://{address}/backend-api"));
        production_prompt_executor()(execution);
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            writer,
            profiles,
            move |_| prompt_profiles.clone(),
            1,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("runtime");
    });
    for retry in 1..expected_attempts {
        wait_for_runtime_frames(&output, |frames| {
            frames.iter().any(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseUpdatedReported(update))
                        if update.status.as_ref().and_then(|status| status.retry.as_ref())
                            .is_some_and(|status| status.attempt == retry as u32)
                )
            })
        });
        clock.advance(Duration::from_secs(3600));
    }
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(_))
            )
        })
    });
    input.close();
    runtime.join().expect("runtime join");
    server.join().expect("server join");
    assert_eq!(attempts.load(Ordering::SeqCst), expected_attempts);
    let terminals: Vec<_> = decode_frames(&output.bytes())
        .into_iter()
        .filter_map(|frame| match input_event(&frame) {
            Some(Event::ProviderResponseFinishedReported(finished)) => Some(finished.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one logical terminal");
    let terminal = terminals.into_iter().next().expect("terminal");
    assert_eq!(terminal.stop_reason, ProviderStopReason::Error);
    assert!(terminal.error.is_some());
    assert!(terminal.output_items.is_empty());
    assert_eq!(terminal.provider_attempt.get() as usize, expected_attempts);
    terminal
}

/// Failed and incomplete envelopes retain final accounting through the actual
/// extension report without success, continuation, or intermediate usage sums.
#[test]
fn codex_failure_terminal_reports_final_accounting_once() {
    for kind in ["response.incomplete", "response.failed"] {
        let terminal = run_failure_accounting(
            tau_proto::PromptOperation::Inference,
            vec![serde_json::json!({
                "type": kind,
                "response": {
                    "id": "resp_final_failure", "end_turn": false,
                    "error": {"code": "invalid_prompt"},
                    "incomplete_details": {"reason": "interrupted"},
                    "usage": {"input_tokens": 100, "output_tokens": 7,
                        "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 9}}
                }
            })],
        );
        assert_eq!(
            terminal.failure_kind,
            Some(tau_proto::ProviderFailureKind::RequestRejected)
        );
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("resp_final_failure")
        );
        let usage = terminal.usage.expect("usage");
        assert_eq!(
            (
                usage.prompt_sent_tokens,
                usage.prompt_cached_tokens,
                usage.response_received_tokens
            ),
            (100, 20, 7)
        );
        assert_eq!(usage.cache.expect("cache").write_tokens, Some(9));
    }
}

/// Ordinary retries remain pending and only the final deterministic failure is
/// accounted; a prior attempt's usage/ID must not leak into an absent final
/// one.
#[test]
fn codex_retry_then_failure_does_not_accumulate_or_invent_usage() {
    for final_usage in [
        serde_json::Value::Null,
        serde_json::json!({"output_tokens": 0}),
    ] {
        let terminal = run_failure_accounting(
            tau_proto::PromptOperation::Inference,
            vec![
                serde_json::json!({"type":"response.failed","response":{
                "id":"resp_retry","error":{"code":"server_error"},
                "usage":{"input_tokens":999,"output_tokens":999}}}),
                serde_json::json!({"type":"response.incomplete","response":{
                "incomplete_details":{"reason":"max_output_tokens"},"usage": final_usage}}),
            ],
        );
        assert!(terminal.provider_response_id.is_none());
        assert_eq!(terminal.usage.is_some(), !final_usage.is_null());
        if let Some(usage) = terminal.usage {
            assert_eq!(
                (usage.prompt_sent_tokens, usage.response_received_tokens),
                (0, 0)
            );
        }
    }
}

/// Finite standalone retry exhaustion retains only the last attempt's metadata
/// and preserves the existing exhausted-failure disposition.
#[test]
fn codex_compaction_exhaustion_preserves_last_failure_accounting() {
    for final_has_usage in [false, true] {
        let events = (1..=5)
            .map(|attempt| {
                let mut response = serde_json::json!({
                    "id": format!("resp_retry_{attempt}"), "end_turn": false,
                    "error": {"code": "server_error"},
                    "usage": {"input_tokens": attempt * 10, "output_tokens": attempt}
                });
                if attempt == 5 && !final_has_usage {
                    response["usage"] = serde_json::Value::Null;
                    response["id"] = serde_json::Value::Null;
                }
                serde_json::json!({"type": "response.failed", "response": response})
            })
            .collect();
        let terminal =
            run_failure_accounting(tau_proto::PromptOperation::StandaloneCompaction, events);
        assert_eq!(
            terminal.failure_kind,
            Some(tau_proto::ProviderFailureKind::TransientCompactionExhausted)
        );
        assert_eq!(terminal.usage.is_some(), final_has_usage);
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            final_has_usage.then_some("resp_retry_5")
        );
        if let Some(usage) = terminal.usage {
            assert_eq!(
                (usage.prompt_sent_tokens, usage.response_received_tokens),
                (50, 5)
            );
        }
    }
}
