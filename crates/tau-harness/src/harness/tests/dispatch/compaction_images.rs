//! Generic standalone replacement admission through the shared artifact worker.

use std::sync::mpsc;

use super::super::dedup;
use super::*;
use crate::artifact_store::{ArtifactStore, put_fixture};
use crate::artifact_worker::ArtifactWorker;
use crate::harness::harness_connection_id;
use crate::image_verification::{ImageVerificationCompleted, ImageVerificationCut};

/// A real durable standalone transaction with a new, non-historical image.
struct Fixture {
    /// Harness and its worker outlive the temporary storage.
    h: Harness,
    /// Loaded transaction owner.
    cid: AgentId,
    /// Exact provider terminal.
    response: ProviderResponseFinished,
    /// Prepared object that is not part of the input transcript.
    blob: tau_proto::ArtifactDescriptor,
    /// Isolated state directory, removed last.
    _temp: TempDir,
}

impl Fixture {
    /// Creates a real dispatched compaction and a closed replacement tool
    /// round.
    fn new() -> Self {
        let temp = TempDir::new().expect("state");
        let mut h = quiet_provider_harness(temp.path().join("state")).expect("harness");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(test_session_id("s1"), &role);
        let agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("id");
        finish_test_agent_context_wait(&mut h, &agent_id);
        let index = event_log_count(&h, |event| matches!(event, Event::AgentPromptCreated(_)));
        h.handle_compact_request(
            harness_connection_id(),
            test_session_id("s1"),
            Some(&agent_id),
        );
        let prompt = read_nth_prompt_created(&h, index);
        let bytes = dedup::encoded_test_png();
        let (_, blob) = put_fixture(
            &mut ArtifactStore::new(&h.session_runtime.state_dir),
            "fixture/session",
            "fixture",
            &bytes,
            1,
        );
        let result = dedup::image_result("replacement-image", bytes);
        let image = result.provider_content[0].inline_image().expect("image");
        let mut response = standalone_compaction_success_response(&prompt, "unused");
        response.output_items = vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: result.call_id.clone(),
                name: ToolName::new("read_image"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolResult(tau_proto::ToolResultItem {
                call_id: result.call_id,
                tool_type: tau_proto::ToolType::Function,
                status: tau_proto::ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&result.result),
                presentation: Default::default(),
                provider_content: vec![tau_proto::ToolResultContentPart::ImageReference(
                    tau_proto::ImageReference {
                        blob: blob.clone(),
                        media_type: image.media_type,
                        width: image.width,
                        height: image.height,
                        detail: image.detail,
                    },
                )],
            }),
        ];
        Self {
            h,
            cid,
            response,
            blob,
            _temp: temp,
        }
    }

    /// Exercises authenticated report intake, not just the private completion.
    fn report(&mut self) {
        let source =
            self.h.provider_runtime.pending_prompts[&self.response.agent_prompt_id].clone();
        self.h
            .handle_extension_event(
                source.as_str(),
                TestProtocolItem::Event(Event::ProviderResponseFinishedReported(
                    self.response.clone().into(),
                )),
            )
            .expect("report");
    }

    /// Consumes one completion after physical custody has ended.
    fn complete(&mut self) {
        let completed = next_completion(&mut self.h);
        self.h
            .handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
            .expect("complete");
    }

    /// Counts only the canonical replacement boundary for this prompt.
    fn boundaries(&self) -> usize {
        event_log_count(&self.h, |event| {
            matches!(event,
            Event::AgentCompacted(boundary) if boundary.compact_prompt_id == self.response.agent_prompt_id)
        })
    }

    /// Finds conservative durable protection independently of event
    /// publication.
    fn roots(&self) -> PathBuf {
        self.h
            .session_runtime
            .state_dir
            .join("artifacts/transcript-roots")
            .join(self.response.agent_id.as_str())
            .join("roots.json")
    }
}

/// Fitting must charge the installed prepared-reference replacement and frozen
/// initialization in their canonical representation, not private decoded bytes.
/// A later oversized suffix must retreat only to the last fitting closed cut.
#[test]
fn prepared_reference_prefix_fitting_counts_canonical_replacement_and_initialization() {
    let mut f = Fixture::new();
    f.report();
    f.complete();
    assert_eq!(f.boundaries(), 1);
    let agent_id = f.response.agent_id.clone();
    f.h.publish_for_agent(
        &f.cid,
        Event::AgentInitializationContextSet(tau_proto::AgentInitializationContextSet {
            discovery_revision: 0,
            discovery_refreshes: Vec::new(),
            discovery_diagnostics: Vec::new(),
            session_id: f.h.session_runtime.current_session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id: "image-byte-fit-init".parse().expect("initialization"),
            agents_message: Some("frozen initialization".repeat(32)),
            effective_skills: Vec::new(),
            agents_files: Vec::new(),
        }),
    );
    super::compaction::append_byte_fit_text(&mut f.h, &f.cid, "closed text prefix".into());
    let safe = f.h.agent_runtime.agent_registry.agents[&f.cid]
        .identity
        .head
        .expect("safe cut");
    super::compaction::append_byte_fit_text(&mut f.h, &f.cid, "later suffix".repeat(1024));
    let incident = f.h.agent_runtime.agent_registry.agents[&f.cid]
        .identity
        .head
        .expect("full cut");
    let tree =
        f.h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("tree");
    let mut canonical = crate::prompt::assemble_prompt_context_prefix_from(
        tree,
        Some(incident),
        tau_proto::AgentHead::Node(safe),
    )
    .expect("prefix")
    .context;
    canonical.blocks.insert(
        0,
        crate::prompt::initialization_agents_context_block(tree).expect("initialization"),
    );
    let canonical_json = serde_json::to_vec(&canonical).expect("canonical JSON");
    assert!(String::from_utf8_lossy(&canonical_json).contains(f.blob.key.as_str()));
    let mut expanded = canonical.clone();
    let mut images = 0;
    for block in &mut expanded.blocks {
        let tau_proto::ContextBlock::UserInput(block) = block else {
            continue;
        };
        for item in &mut block.items {
            let ContextItem::ToolResult(result) = item else {
                continue;
            };
            for part in &mut result.provider_content {
                let tau_proto::ToolResultContentPart::ImageReference(reference) = part else {
                    continue;
                };
                let mut image = reference.metadata_only_image();
                image.data = dedup::encoded_test_png().into();
                *part = tau_proto::ToolResultContentPart::Image(image);
                images += 1;
            }
        }
    }
    assert_eq!(images, 1, "installed replacement remains reference-backed");
    let expanded_bytes = serde_json::to_vec(&expanded).expect("expanded JSON").len();
    assert!(expanded_bytes > canonical_json.len());
    let cap = tau_proto::ByteCount::new(canonical_json.len() as u64);
    assert_eq!(
        f.h.fitting_standalone_compaction_cut(
            agent_id.as_str(),
            tau_proto::AgentHead::Node(incident),
            cap
        ),
        Some(tau_proto::AgentHead::Node(safe)),
    );
    let measurements = crate::prompt::active_prompt_prefix_json_measurements(tree, Some(incident))
        .expect("measurements");
    assert!(measurements.contains(&(safe, cap)));
    f.h.shutdown().expect("shutdown");
}

/// Drives unrelated commands while waiting for exactly one worker completion.
pub(super) fn next_completion(h: &mut Harness) -> Box<ImageVerificationCompleted> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "image completion missing");
        if let Ok(HarnessEvent::Command(command)) =
            h.runtime_io.rx.recv_timeout(Duration::from_millis(50))
        {
            if let HarnessCommand::ImageVerificationCompleted(completed) = command {
                return completed;
            }
            h.handle_harness_command(command).expect("command");
        }
    }
}

/// New replacement refs need no source-image equality and must survive CAS
/// expiry once their exact window becomes canonical.
#[test]
fn compaction_images_verify_and_root_new_replacement_before_boundary() {
    let mut f = Fixture::new();
    f.report();
    assert_eq!(f.boundaries(), 0);
    let completed = next_completion(&mut f.h);
    assert!(completed.outcome.is_ok());
    assert!(f.roots().exists());
    assert_eq!(f.boundaries(), 0);
    f.h.handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
        .expect("complete");
    assert_eq!(f.boundaries(), 1);
    let boundary = event_log_events(&f.h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentCompacted(boundary) => Some(boundary),
            _ => None,
        })
        .expect("boundary");
    assert_eq!(boundary.replacement_window, f.response.output_items);
    let mut store = ArtifactStore::new(&f.h.session_runtime.state_dir);
    store
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("cleanup");
    assert!(
        store
            .execute(
                "fixture/session",
                "fixture",
                tau_proto::ArtifactOp::Stat {
                    key: f.blob.key.clone()
                },
                100
            )
            .is_ok()
    );
    f.h.session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(3))
        .expect("flush history");
    let cold = AgentStore::open_lazy(f.h.session_runtime.state_dir.join("agents")).expect("cold");
    assert!(cold.agent_events(f.response.agent_id.as_str()).expect("history").iter().any(|record|
        matches!(&record.event, Event::AgentCompacted(cold_boundary) if cold_boundary == &boundary)));
}

/// A repeated digest with different per-use metadata must still be decoded and
/// validated for each occurrence; no hash-only verification cache is lawful.
#[test]
fn compaction_images_reject_mismatched_metadata_in_later_occurrence() {
    let mut f = Fixture::new();
    let mut later = f.response.output_items.clone();
    let ContextItem::ToolCall(call) = &mut later[0] else {
        unreachable!()
    };
    call.call_id = "second-image".into();
    let ContextItem::ToolResult(result) = &mut later[1] else {
        unreachable!()
    };
    result.call_id = "second-image".into();
    let tau_proto::ToolResultContentPart::ImageReference(image) = &mut result.provider_content[0]
    else {
        unreachable!()
    };
    image.width += 1;
    f.response.output_items.extend(later);
    f.report();
    f.complete();
    assert_eq!(f.boundaries(), 0);
    assert!(event_log_contains_any_source(&f.h, |event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(_)
    )));
}

/// A missing object rejects only the consuming compact request and never
/// publishes its fetchable descriptor as a canonical boundary.
#[test]
fn compaction_images_missing_object_is_a_safe_terminal_failure() {
    let mut f = Fixture::new();
    ArtifactStore::new(&f.h.session_runtime.state_dir)
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("expire");
    f.report();
    f.complete();
    assert_eq!(f.boundaries(), 0);
    assert!(!f.roots().exists());
    assert!(event_log_contains_any_source(&f.h, |event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(_)
    )));
}

/// Verification completion is not authority after branch navigation, even if
/// durable protection already succeeded.
#[test]
fn compaction_images_recheck_branch_after_verification() {
    let mut f = Fixture::new();
    f.report();
    let completed = next_completion(&mut f.h);
    f.h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&f.cid)
        .expect("agent")
        .identity
        .branch_generation += 1;
    f.h.handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
        .expect("complete");
    assert_eq!(f.boundaries(), 0);
    assert!(event_log_contains_any_source(&f.h, |event| matches!(event,
        Event::AgentStandaloneCompactionFailed(failure)
            if failure.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch)));
}

/// Retained canonical publication owns the verified terminal without another
/// storage operation, duplicate boundary, or duplicate provider accounting.
#[test]
fn compaction_images_backpressure_retains_verified_terminal() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut f = Fixture::new();
    let validations = Arc::new(AtomicUsize::new(0));
    let observed = validations.clone();
    f.h.runtime_io.artifacts = Some(
        ArtifactWorker::start_with_image_hook(
            &f.h.session_runtime.state_dir,
            f.h.runtime_io.tx.clone(),
            Arc::new(move |cut| {
                if cut == ImageVerificationCut::Validated {
                    observed.fetch_add(1, Ordering::AcqRel);
                }
            }),
        )
        .expect("worker"),
    );
    f.report();
    // A duplicate terminal cannot acquire a second worker permit.
    f.report();
    reject_semantic_admissions(&f.h, 1000);
    f.complete();
    assert_eq!(f.boundaries(), 0);
    assert!(f.roots().exists());
    reject_semantic_admissions(&f.h, 0);
    f.h.retry_pending_agent_publications();
    assert_eq!(f.boundaries(), 1);
    assert_eq!(validations.load(Ordering::Acquire), 1);
    f.h.retry_pending_agent_publications();
    assert_eq!(f.boundaries(), 1);
    assert!(
        !f.h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&f.cid)
    );
}

/// Cancellation immediately revokes guarded storage work, leaves no canonical
/// replacement, and still permits the original backend accounting correction.
#[test]
fn compaction_images_cancellation_revokes_io_and_preserves_accounting() {
    let mut f = Fixture::new();
    f.response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2,
        stats: Default::default(),
    });
    let pause = Pause::install(&mut f.h);
    f.report();
    pause.wait();
    f.h.handle_cancel_prompt(
        harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(f.response.agent_id.clone()),
            agent_prompt_id: Some(f.response.agent_prompt_id.clone()),
        },
    );
    pause.release();
    f.complete();
    assert_eq!(f.boundaries(), 0);
    assert!(!f.roots().exists());
    assert_eq!(
        event_log_count(&f.h, |event| matches!(event,
        Event::AgentStandaloneCompactionFailed(failure)
            if failure.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled)),
        1
    );
    assert_eq!(
        event_log_count(&f.h, |event| matches!(
            event,
            Event::ProviderStandaloneExecutionAccountingCorrected(_)
        )),
        1
    );
}

/// Runtime, source, session and exact transaction changes cannot transfer a
/// successful old verification to a successor owner.
#[test]
fn compaction_images_reject_stale_verification_authority() {
    for changed in ["session", "runtime", "source", "transaction", "attempt"] {
        let mut f = Fixture::new();
        f.report();
        let completed = next_completion(&mut f.h);
        match changed {
            "session" => {
                f.h.session_runtime.current_session_generation =
                    f.h.session_runtime
                        .current_session_generation
                        .saturating_next()
            }
            "runtime" => {
                f.h.agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(&f.cid)
                    .expect("agent")
                    .identity
                    .runtime_incarnation += 1
            }
            "source" => {
                let source =
                    f.h.provider_runtime.pending_prompts[&f.response.agent_prompt_id].clone();
                f.h.extensions
                    .entries
                    .get_mut(&source)
                    .expect("source")
                    .instance_id = tau_proto::ExtensionInstanceId::new(999);
            }
            "transaction" => {
                let ActivationDispatchState::Running { id, .. } = &mut f
                    .h
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(&f.cid)
                    .expect("agent")
                    .dispatch
                    .activation_dispatch
                else {
                    unreachable!()
                };
                *id = tau_proto::CompactionTransactionId::parse("ct-replacement").expect("id");
            }
            _ => {
                f.h.prompt_coordination
                    .standalone_accounting
                    .highest_retry_attempt
                    .insert(f.response.agent_prompt_id.clone(), 1);
            }
        }
        f.h.handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
            .expect("complete");
        assert_eq!(f.boundaries(), 0, "{changed}");
    }
}

/// Provider disconnect during the harness-owned verification phase must not
/// lose an already reported bill or publish an unverified replacement.
#[test]
fn compaction_images_disconnect_preserves_known_backend_usage() {
    let mut f = Fixture::new();
    f.response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2,
        stats: Default::default(),
    });
    let source = f.h.provider_runtime.pending_prompts[&f.response.agent_prompt_id].clone();
    let pause = Pause::install(&mut f.h);
    f.report();
    pause.wait();
    f.h.handle_disconnect(&source);
    pause.release();
    f.complete();
    assert_eq!(f.boundaries(), 0);
    assert!(!f.roots().exists());
    assert_eq!(
        event_log_count(&f.h, |event| matches!(event,
        Event::ProviderStandaloneExecutionAccounted(accounted)
            if accounted.agent_prompt_id == f.response.agent_prompt_id
                && matches!(accounted.usage, tau_proto::StandaloneExecutionUsage::Known(_)))),
        1
    );
}

/// Shutdown must settle the retained provider bill before revoking session
/// authority, even when physical verification finished but is not consumed.
#[test]
fn compaction_images_shutdown_preserves_known_usage_before_generation_revocation() {
    let mut f = Fixture::new();
    f.response.usage = Some(tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: 10,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2,
        stats: Default::default(),
    });
    f.report();
    let completed = next_completion(&mut f.h);
    assert!(completed.outcome.is_ok());
    assert_eq!(f.boundaries(), 0);
    f.h.shutdown().expect("shutdown");
    let accounting = event_log_events(&f.h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ProviderStandaloneExecutionAccounted(accounted)
                if accounted.agent_prompt_id == f.response.agent_prompt_id =>
            {
                Some(accounted)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accounting.len(), 1);
    assert!(matches!(&accounting[0].usage,
        tau_proto::StandaloneExecutionUsage::Known(usage)
            if usage.prompt_sent_tokens == 10 && usage.response_received_tokens == 2));
    f.h.handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
        .expect("late completion");
    assert_eq!(f.boundaries(), 0);
    assert_eq!(
        event_log_count(&f.h, |event| matches!(event,
        Event::ProviderStandaloneExecutionAccounted(accounted)
            if accounted.agent_prompt_id == f.response.agent_prompt_id)),
        1
    );
}

/// Duplicate keys consume logical quota per occurrence before any worker job;
/// the physical batch must not turn logical limits into deduplicated limits.
#[test]
fn compaction_images_preflight_rejects_overquota_window_without_io() {
    let mut f = Fixture::new();
    let round = f.response.output_items.clone();
    f.response.output_items.clear();
    for index in 0..17 {
        let mut round = round.clone();
        let id = ToolCallId::from(format!("image-{index}"));
        let ContextItem::ToolCall(call) = &mut round[0] else {
            unreachable!()
        };
        call.call_id = id.clone();
        let ContextItem::ToolResult(result) = &mut round[1] else {
            unreachable!()
        };
        result.call_id = id;
        let tau_proto::ToolResultContentPart::ImageReference(image) =
            &mut result.provider_content[0]
        else {
            unreachable!()
        };
        image.blob.size = tau_proto::ArtifactSize::new(8 * 1024 * 1024).expect("size");
        f.response.output_items.extend(round);
    }
    f.report();
    assert_eq!(f.boundaries(), 0);
    assert!(f.h.runtime_io.artifacts.is_none());
    assert!(!f.roots().exists());
    assert!(event_log_contains_any_source(&f.h, |event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(_)
    )));
}

/// Test-owned physical pause releases automatically even when assertions panic.
struct Pause {
    /// Worker has entered scoped agent custody before CAS access.
    reached: mpsc::Receiver<()>,
    /// Test release; sends are harmless after the worker resumes.
    release: mpsc::Sender<()>,
}

impl Pause {
    /// Installs one deterministic guarded I/O pause.
    fn install(h: &mut Harness) -> Self {
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        h.runtime_io.artifacts = Some(
            ArtifactWorker::start_with_image_hook(
                &h.session_runtime.state_dir,
                h.runtime_io.tx.clone(),
                Arc::new(move |cut| {
                    if cut == ImageVerificationCut::Guarded {
                        reached_tx.send(()).expect("pause observer");
                        release_rx.lock().expect("release").recv().expect("release");
                    }
                }),
            )
            .expect("worker"),
        );
        Self { reached, release }
    }

    /// Waits for actual worker entry, never for a guessed sleep.
    fn wait(&self) {
        self.reached
            .recv_timeout(Duration::from_secs(5))
            .expect("guarded");
    }

    /// Releases scoped custody.
    fn release(&self) {
        let _ = self.release.send(());
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
    }
}
