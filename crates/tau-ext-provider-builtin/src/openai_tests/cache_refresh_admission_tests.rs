//! Cache-refresh credential admission through the manual runtime and Secret
//! RPC.

use std::cell::Cell;

use tau_client::TauExtensionRunner;

use super::*;

/// Concrete loader keeps focused fixtures independent of runtime closure types.
type TestRuntime =
    ManualExtensionRuntime<ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles>>;

/// Credential-free selected settings with one stable storage identity.
fn profiles(_: Option<&ProviderName>) -> BuiltinProviderProfiles {
    let mut profiles = profiles_with_chatgpt_auth(OpenAiAuth::default());
    profiles.credentials.insert(
        ProviderName::new(CHATGPT_PROVIDER_NAME),
        ProviderCredential::Stored(
            ProviderCredentialReference::new(
                ProviderCredentialIdentity::parse("0123456789abcdef0123456789abcdef")
                    .expect("identity"),
                ProviderCredentialSlot::OAuth,
                None,
            )
            .expect("reference"),
        ),
    );
    profiles
}

/// Installs the real caller-thread-local Secret client and production handlers.
/// Only OAuth transport and provider executors are replaced; no credentials or
/// requests can reach the network.
fn runtime(input: BlockingInput, output: SharedWriter) -> TestRuntime {
    let mut state = crate::tests::observation_test_runtime();
    state.load_prompt_profiles = profiles;
    state.diagnostics.receipt.suppress_oauth_worker = true;
    state.prompt_executor = Arc::new(|_| panic!("unexpected prompt execution"));
    state.prewarm_executor = Arc::new(|_| panic!("unexpected refresh execution"));
    // Reserve the synthetic account's quota fetch without spawning transport.
    // Prompt credential admission may reconcile this same identity, but cannot
    // turn this credential test into a real quota request.
    let Some(PromptBackend::Responses(config)) = resolve_prompt_backend_without_refresh(
        &prompt().model,
        &mut profiles_with_chatgpt_auth(chatgpt_auth()),
        &mut OAuthRefreshRejectionCache::default(),
    ) else {
        panic!("fixture route")
    };
    state.quota.ensure_profile(
        prompt().model.provider.clone(),
        quota_profile_identity(&config),
    );
    state.quota.begin_fetch(&prompt().model.provider);
    let extension = ProviderExtension::new(
        Arc::new(Mutex::new(BuiltinProviderProfiles::default())),
        Some(BuiltinProviderProfiles::default()),
    );
    let mut runtime = TauExtensionRunner::new(extension)
        .start_manual_loop_with_extension_data_state(input, output, move |_, client| {
            state.extension_data_client = Some(client);
            state
        })
        .expect("configured runtime");
    let waker = runtime.waker();
    runtime.state_mut().set_worker_waker(waker);
    runtime
}

/// Runs focused transitions with a live transport and returns flushed frames.
fn with_runtime(test: impl FnOnce(&mut TestRuntime)) -> Vec<HarnessInputMessage> {
    let input = BlockingInput::default();
    input.push(encode_frames(&[]));
    let output = SharedWriter::default();
    let mut runtime = runtime(input.clone(), output.clone());
    test(&mut runtime);
    runtime.state_mut().begin_input_shutdown();
    input.close();
    runtime.finish().expect("finish runtime");
    decode_frames(&output.bytes())
}

/// Enters maintenance through the registered production event callback.
fn request(runtime: &mut TestRuntime) -> (String, Instant) {
    let mut refresh = cache_refresh("pcr-admission");
    refresh.stop_after_millis = NonZeroU32::new(30_000).expect("duration");
    runtime
        .dispatch_one(live_event(11, Event::AgentCacheRefreshRequested(refresh)))
        .expect("dispatch refresh");
    let admission = runtime
        .state()
        .credential_admission
        .admissions
        .back()
        .expect("pending");
    let PendingPromptAdmissionKind::CacheRefresh { deadline, .. } = admission.kind else {
        panic!("refresh owner");
    };
    (admission.request_id.clone().expect("Secret RPC"), deadline)
}

/// Builds a synthetic valid storage reply, optionally requiring OAuth refresh.
fn reply(request_id: String, due: bool) -> tau_proto::ExtensionDataResult {
    let mut auth = chatgpt_auth();
    auth.expires_at_ms = now_ms().saturating_add(if due { 60_000 } else { 3_600_000 });
    tau_proto::ExtensionDataResult {
        request_id,
        result: tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::ReadFile {
                contents: serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
                    auth,
                ))
                .expect("credential"),
            },
        },
    }
}

/// Returns a fake successful transport rotation for the fixture account.
fn rotated() -> tau_provider_codex::oauth::OAuthTokenRefresh {
    let auth = chatgpt_auth();
    tau_provider_codex::oauth::OAuthTokenRefresh {
        access_token: Some(auth.access_token),
        refresh_token: Some("rotated-refresh".to_owned()),
        expires_at_ms: Some(now_ms().saturating_add(3_600_000)),
        account_id: auth.account_id,
    }
}

/// Checks exact terminal correlation and multiplicity, not just eventual
/// status.
fn assert_terminal(frames: &[HarnessInputMessage], status: tau_proto::ProviderCacheRefreshStatus) {
    let terminals = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderCacheRefreshFinishedReported(terminal)) => Some(terminal),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].refresh_id.as_str(), "pcr-admission");
    assert_eq!(terminals[0].status, status);
}

/// Deadline equality retires a held production Secret correlation; its valid
/// late reply cannot launch a backend or emit a second terminal.
#[test]
fn secret_reply_at_receipt_deadline_is_inert() {
    let frames = with_runtime(|runtime| {
        let (id, deadline) = request(runtime);
        let handle = runtime.handle();
        runtime
            .state_mut()
            .expire_pending_cache_refreshes(deadline, &handle)
            .expect("expire");
        runtime
            .state_mut()
            .handle_extension_data_result(reply(id, false))
            .expect("late reply");
        runtime
            .state_mut()
            .drain_workers_and_start_prompts(&handle)
            .expect("drain");
        assert!(runtime.state().credential_admission.admissions.is_empty());
        assert!(runtime.state().prewarm_supervisor.is_empty());
    });
    assert_terminal(
        &frames,
        tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
    );
}

/// Removing a maintenance consumer at each OAuth/storage stage invalidates its
/// continuations without permitting stale completion to recreate worker
/// authority.
#[test]
fn cancelled_oauth_and_storage_stages_cannot_restart_refresh() {
    for expire in [false, true] {
        for stage in 0..3 {
            let frames = with_runtime(|runtime| {
                let (id, deadline) = request(runtime);
                runtime
                    .state_mut()
                    .handle_extension_data_result(reply(id, true))
                    .expect("read");
                let (key, owner) = runtime
                    .state()
                    .credential_admission
                    .oauth_refreshes
                    .iter()
                    .map(|(key, flight)| (key.clone(), flight.owner))
                    .next()
                    .expect("OAuth flight");
                if 0 < stage {
                    runtime
                        .state_mut()
                        .handle_prompt_oauth_refresh_finished(key.clone(), owner, Ok(rotated()))
                        .expect("OAuth complete");
                }
                let mut rpc = runtime
                    .state()
                    .credential_admission
                    .oauth_rpcs
                    .keys()
                    .next()
                    .cloned();
                if 1 < stage {
                    runtime
                        .state_mut()
                        .handle_extension_data_result(tau_proto::ExtensionDataResult {
                            request_id: rpc.take().expect("CAS"),
                            result: tau_proto::ExtensionDataResultPayload::Ok {
                                value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
                            },
                        })
                        .expect("CAS");
                    rpc = runtime
                        .state()
                        .credential_admission
                        .oauth_rpcs
                        .keys()
                        .next()
                        .cloned();
                }
                let handle = runtime.handle();
                if expire {
                    runtime
                        .state_mut()
                        .expire_pending_cache_refreshes(deadline, &handle)
                        .expect("expire");
                } else {
                    runtime
                        .state_mut()
                        .cancel_pending_cache_refresh(
                            &"pcr-admission".parse().expect("id"),
                            &handle,
                        )
                        .expect("cancel");
                }
                runtime
                    .state_mut()
                    .handle_prompt_oauth_refresh_finished(key, owner, Ok(rotated()))
                    .expect("stale OAuth");
                if let Some(rpc) = rpc {
                    runtime
                        .state_mut()
                        .handle_extension_data_result(reply(rpc, false))
                        .expect("stale storage");
                }
                runtime
                    .state_mut()
                    .drain_workers_and_start_prompts(&handle)
                    .expect("drain");
                assert!(runtime.state().credential_admission.admissions.is_empty());
                assert!(
                    runtime
                        .state()
                        .credential_admission
                        .oauth_refreshes
                        .is_empty()
                );
                assert!(runtime.state().credential_admission.oauth_rpcs.is_empty());
            });
            assert_terminal(
                &frames,
                if expire {
                    tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded
                } else {
                    tau_proto::ProviderCacheRefreshStatus::Cancelled
                },
            );
        }
    }
}

/// A held maintenance Secret must not monopolize the real manual input loop:
/// cancellation and an independently hydrated real prompt finish first.
#[test]
fn held_refresh_secret_does_not_block_cancel_or_real_prompt() {
    let input = BlockingInput::default();
    let mut refresh = cache_refresh("pcr-admission");
    refresh.stop_after_millis = NonZeroU32::new(30_000).expect("duration");
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentCacheRefreshRequested(refresh),
    )]));
    let output = SharedWriter::default();
    let thread_input = input.clone();
    let thread_output = output.clone();
    let worker = thread::spawn(move || {
        let mut runtime = runtime(thread_input, thread_output);
        runtime.state_mut().prompt_concurrency_limit = 1;
        runtime.state_mut().prompt_executor = Arc::new(|execution| {
            let mut writer = execution.frame_writer();
            writer
                .send_report(HarnessInputMessage::emit_transient(
                    Event::ProviderResponseFinishedReported(simple_finished(
                        execution.job.agent_prompt_id,
                        execution.job.prompt.agent_id,
                        execution.job.prompt.originator,
                        "done",
                    )),
                ))
                .expect("prompt finishes");
        });
        run_provider_loop(runtime).expect("manual loop");
    });
    let frames = wait_for_runtime_frames(&output, |frames| !secret_requests(frames).is_empty());
    let held = secret_requests(&frames)[0].clone();
    input.push(encode_frames(&[
        live_event(
            12,
            Event::AgentCacheRefreshCancelRequested(tau_proto::AgentCacheRefreshCancelRequested {
                refresh_id: "pcr-admission".parse().expect("id"),
                reason: tau_proto::ProviderCacheRefreshCancelReason::RealPrompt,
            }),
        ),
        live_event(13, Event::AgentPromptCreated(prompt())),
    ]));
    let frames = wait_for_runtime_frames(&output, |frames| secret_requests(frames).len() == 2);
    let prompt_read = secret_requests(&frames)[1].clone();
    input.push(encode_frames(&[HarnessOutputMessage::ExtensionDataResult(
        Box::new(reply(prompt_read, false)),
    )]));
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(_))
            )
        })
    });
    // Only now release the maintenance reply, after observing prompt
    // completion.
    input.push(encode_frames(&[HarnessOutputMessage::ExtensionDataResult(
        Box::new(reply(held, false)),
    )]));
    input.close();
    worker.join().expect("runtime exits");
    assert_terminal(
        &decode_frames(&output.bytes()),
        tau_proto::ProviderCacheRefreshStatus::Cancelled,
    );
}

/// Returns actual Secret RPC correlations emitted by the SDK writer.
fn secret_requests(frames: &[HarnessInputMessage]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::ExtensionDataRequest(request) => Some(request.request_id.clone()),
            _ => None,
        })
        .collect()
}

/// Maintenance does not own the prompt FIFO even before cancellation; removing
/// its shared OAuth interest must preserve the live prompt's exact flight.
#[test]
fn refresh_cancellation_preserves_shared_prompt_oauth_owner() {
    let frames = with_runtime(|runtime| {
        let (id, _) = request(runtime);
        let held_reply = reply(id, true);
        // Identical credential bytes, not merely identical provider names,
        // establish the shared generation.
        let payload = held_reply.result.clone();
        runtime
            .state_mut()
            .handle_extension_data_result(held_reply)
            .expect("refresh read");
        runtime
            .dispatch_one(live_event(12, Event::AgentPromptCreated(prompt())))
            .expect("prompt");
        let prompt_read = runtime
            .state()
            .credential_admission
            .admissions
            .back()
            .and_then(|admission| admission.request_id.clone())
            .expect("prompt read");
        runtime
            .state_mut()
            .handle_extension_data_result(tau_proto::ExtensionDataResult {
                request_id: prompt_read,
                result: payload,
            })
            .expect("prompt read");
        let (key, owner) = runtime
            .state()
            .credential_admission
            .oauth_refreshes
            .iter()
            .map(|(key, flight)| (key.clone(), flight.owner))
            .next()
            .expect("flight");
        assert_eq!(
            runtime.state().credential_admission.oauth_refreshes.len(),
            1
        );
        let handle = runtime.handle();
        runtime
            .state_mut()
            .cancel_pending_cache_refresh(&"pcr-admission".parse().expect("id"), &handle)
            .expect("cancel refresh");
        assert!(runtime.state().prompt_oauth_owner_matches(&key, owner));
        runtime
            .state_mut()
            .handle_prompt_oauth_refresh_finished(key.clone(), owner, Ok(rotated()))
            .expect("OAuth");
        let cas = runtime
            .state()
            .credential_admission
            .oauth_rpcs
            .keys()
            .next()
            .cloned()
            .expect("CAS");
        runtime
            .state_mut()
            .handle_extension_data_result(tau_proto::ExtensionDataResult {
                request_id: cas,
                result: tau_proto::ExtensionDataResultPayload::Ok {
                    value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
                },
            })
            .expect("CAS result");
        let reload = runtime
            .state()
            .credential_admission
            .oauth_rpcs
            .keys()
            .next()
            .cloned()
            .expect("reload");
        runtime
            .state_mut()
            .handle_extension_data_result(reply(reload, false))
            .expect("reload result");
        runtime
            .state_mut()
            .drain_prompt_admissions(&handle)
            .expect("prompt admits");
        assert_eq!(runtime.state().prompt_queue.len(), 1);
        assert!(runtime.state().credential_admission.admissions.is_empty());
    });
    assert_terminal(&frames, tau_proto::ProviderCacheRefreshStatus::Cancelled);
}

/// Successful within-budget read/OAuth/CAS/reload must still start
/// authenticated refresh work exactly once; fixing admission must not disable
/// maintenance.
#[test]
fn authenticated_refresh_survives_async_rotation_and_handoff() {
    let frames = with_runtime(|runtime| {
        let (done_tx, done_rx) = mpsc::channel();
        runtime.state_mut().prewarm_executor = Arc::new(move |execution| {
            assert_eq!(
                execution.refresh_id.as_ref().map(|id| id.as_str()),
                Some("pcr-admission")
            );
            done_tx.send(()).expect("started");
            tau_proto::ProviderCacheRefreshStatus::Succeeded
        });
        let (id, deadline) = request(runtime);
        runtime
            .state_mut()
            .handle_extension_data_result(reply(id, true))
            .expect("read");
        let (key, owner) = runtime
            .state()
            .credential_admission
            .oauth_refreshes
            .iter()
            .map(|(key, flight)| (key.clone(), flight.owner))
            .next()
            .expect("flight");
        runtime
            .state_mut()
            .handle_prompt_oauth_refresh_finished(key, owner, Ok(rotated()))
            .expect("OAuth");
        let cas = runtime
            .state()
            .credential_admission
            .oauth_rpcs
            .keys()
            .next()
            .cloned()
            .expect("CAS");
        runtime
            .state_mut()
            .handle_extension_data_result(tau_proto::ExtensionDataResult {
                request_id: cas,
                result: tau_proto::ExtensionDataResultPayload::Ok {
                    value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
                },
            })
            .expect("CAS result");
        let reload = runtime
            .state()
            .credential_admission
            .oauth_rpcs
            .keys()
            .next()
            .cloned()
            .expect("reload");
        runtime
            .state_mut()
            .handle_extension_data_result(reply(reload, false))
            .expect("reload");
        assert!(
            matches!(&runtime.state().credential_admission.admissions[0].kind,
            PendingPromptAdmissionKind::CacheRefresh { deadline: retained, .. } if *retained == deadline)
        );
        let handle = runtime.handle();
        runtime
            .state_mut()
            .drain_prompt_admissions(&handle)
            .expect("handoff");
        assert!(runtime.state().credential_admission.admissions.is_empty());
        // Pending expiry has lost terminal authority at the handoff.
        runtime
            .state_mut()
            .expire_pending_cache_refreshes(deadline, &handle)
            .expect("late timer");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("executor starts");
        let message = runtime
            .state()
            .worker_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("completion");
        runtime
            .state()
            .worker_tx
            .send(message)
            .expect("return completion");
        runtime
            .state_mut()
            .drain_worker_messages(&handle)
            .expect("worker terminal");
        assert!(runtime.state().prewarm_supervisor.is_empty());
    });
    assert_terminal(&frames, tau_proto::ProviderCacheRefreshStatus::Succeeded);
}

/// Even unresolved, uncancelled maintenance cannot become the prompt FIFO head.
#[test]
fn held_maintenance_is_not_a_prompt_fifo_head() {
    let frames = with_runtime(|runtime| {
        let (held, _) = request(runtime);
        runtime
            .dispatch_one(live_event(12, Event::AgentPromptCreated(prompt())))
            .expect("prompt");
        let read = runtime
            .state()
            .credential_admission
            .admissions
            .back()
            .and_then(|admission| admission.request_id.clone())
            .expect("prompt read");
        runtime
            .state_mut()
            .handle_extension_data_result(reply(read, false))
            .expect("prompt reply");
        let handle = runtime.handle();
        runtime
            .state_mut()
            .drain_prompt_admissions(&handle)
            .expect("prompt admission");
        assert_eq!(runtime.state().prompt_queue.len(), 1);
        assert_eq!(runtime.state().credential_admission.admissions.len(), 1);
        runtime
            .state_mut()
            .handle_session_shutdown(&handle)
            .expect("shutdown");
        runtime
            .state_mut()
            .handle_extension_data_result(reply(held, false))
            .expect("late reply");
        runtime
            .state_mut()
            .drain_prompt_admissions(&handle)
            .expect("late drain");
        assert!(runtime.state().credential_admission.admissions.is_empty());
    });
    assert_terminal(&frames, tau_proto::ProviderCacheRefreshStatus::Cancelled);
}

/// Worker entry uses the same receipt deadline: equality/delayed starts skip
/// the executor, admission consumes budget, and completion remains
/// deadline-first.
#[test]
fn delayed_worker_start_and_completion_keep_receipt_budget() {
    let receipt = Instant::now();
    let deadline = receipt + Duration::from_secs(1);
    for (start, finish, cancel, expected_starts, expected_status) in [
        (
            deadline,
            deadline,
            false,
            0,
            tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
        ),
        (
            deadline + Duration::from_secs(2),
            deadline + Duration::from_secs(2),
            false,
            0,
            tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
        ),
        (
            receipt + Duration::from_millis(900),
            deadline,
            false,
            1,
            tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded,
        ),
        (
            receipt + Duration::from_millis(900),
            receipt + Duration::from_millis(999),
            false,
            1,
            tau_proto::ProviderCacheRefreshStatus::Succeeded,
        ),
        (
            receipt,
            receipt,
            true,
            0,
            tau_proto::ProviderCacheRefreshStatus::Cancelled,
        ),
    ] {
        let Some(PromptBackend::Responses(config)) = resolve_prompt_backend_without_refresh(
            &prompt().model,
            &mut profiles_with_chatgpt_auth(chatgpt_auth()),
            &mut OAuthRefreshRejectionCache::default(),
        ) else {
            panic!("fixture route")
        };
        let starts = Arc::new(AtomicUsize::new(0));
        let worker_starts = starts.clone();
        let executor: PrewarmExecutor = Arc::new(move |_| {
            worker_starts.fetch_add(1, Ordering::SeqCst);
            tau_proto::ProviderCacheRefreshStatus::Succeeded
        });
        let abort = PrewarmAbort::default();
        if cancel {
            abort.cancel();
        }
        let execution = PrewarmExecution {
            runtime: Arc::new(CodexRuntime::new(Arc::new(test_network_policy()))),
            config,
            request: prewarm(),
            refresh_id: Some("pcr-admission".parse().expect("id")),
            debug_provider_requests: false,
            abort,
        };
        let first = Cell::new(true);
        assert_eq!(
            execution.execute_refresh(&executor, deadline, || {
                if first.replace(false) { start } else { finish }
            }),
            expected_status
        );
        assert_eq!(starts.load(Ordering::SeqCst), expected_starts);
    }
}
