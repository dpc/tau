//! Prepared-image admission through real tool reports and the shared I/O
//! worker.

use std::sync::Condvar;

use tau_core::ToolRouteTarget;
use tau_proto::{ArtifactDescriptor, ArtifactError, ArtifactOp, ToolResultContentPart};

use super::*;
use crate::artifact_store::{ArtifactStore, put_fixture};
use crate::artifact_worker::ArtifactWorker;
use crate::debug_log::DebugEventLog;
use crate::event::HarnessCommand;
use crate::image_verification::ImageVerificationCut;

/// Harness state outlives every controlled I/O worker; its root is removed
/// last.
struct ImageFixture {
    /// Real committed-report and canonical publication pipeline.
    h: Harness,
    /// Loaded transcript runtime owning synthetic tool calls.
    cid: AgentId,
    /// Private temporary storage.
    temp: TempDir,
}

/// Production policy requires durable session/agent authority and a live
/// configured source; fallback selection itself must never start artifact I/O.
#[test]
fn prepared_image_policy_requires_durable_scoped_authority() {
    for mode in [
        crate::HarnessStorageMode::Durable,
        crate::HarnessStorageMode::SessionEphemeral,
        crate::HarnessStorageMode::MemoryOnly,
    ] {
        let temp = TempDir::new().expect("state");
        let mut h =
            echo_harness_with_storage_mode(temp.path().join("state"), mode).expect("harness");
        connect_ready_configured_extension(
            &mut h,
            "shell",
            "policy-shell",
            tau_proto::ClientKind::Tool,
        );
        let cid = ensure_test_user_agent(&mut h);
        // The unit helper advances logical context readiness, not asynchronous
        // physical creation. Artifact requires Prepared, not CreationQueued.
        if let Some(owner) = &h.session_runtime.persistence_owner {
            assert_eq!(
                owner.wait_for_latest_durability_for_test(Duration::from_secs(3)),
                tau_core::DurabilityBarrierOutcome::Durable,
            );
        }
        let target = ToolRouteTarget::Extension(crate::test_connection_id("shell"));
        assert_eq!(
            h.prepared_image_storage_for_tool(&cid, &target, true),
            if mode.is_durable() {
                tau_proto::PreparedImageStorage::Artifact
            } else {
                tau_proto::PreparedImageStorage::Inline
            },
        );
        assert_eq!(
            h.prepared_image_storage_for_tool(&cid, &target, false),
            tau_proto::PreparedImageStorage::Inline
        );
        let (ephemeral, _) = ephemeral_agent(&mut h);
        assert_eq!(
            h.prepared_image_storage_for_tool(&ephemeral, &target, true),
            tau_proto::PreparedImageStorage::Inline
        );
        h.session_runtime
            .agent_store
            .release_managed_agents(Duration::from_secs(3))
            .expect("release");
        assert_eq!(
            h.prepared_image_storage_for_tool(&cid, &target, true),
            tau_proto::PreparedImageStorage::Inline
        );
        assert!(
            h.runtime_io.artifacts.is_none(),
            "policy selection is metadata-only"
        );
    }
}

/// Legacy canonical inline history remains byte-exact and cold-readable. This
/// seeds the pre-reference event form directly, not a current Artifact
/// invocation.
#[test]
fn legacy_inline_image_history_remains_cold_readable_without_artifacts() {
    let mut f = ImageFixture::new();
    dedup::track_image_call(&mut f.h, &f.cid, "legacy-image", true);
    let bytes = dedup::encoded_test_png();
    f.h.publish_for_agent(
        &f.cid,
        Event::ProviderToolResult(dedup::image_result("legacy-image", bytes.clone())),
    );
    let agent_id = f.h.agent_runtime.agent_registry.agents[&f.cid]
        .identity
        .agent_id
        .clone()
        .expect("agent");
    f.h.session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(3))
        .expect("flush");
    let cold = AgentStore::open_lazy(f.h.session_runtime.state_dir.join("agents")).expect("cold");
    assert!(cold.agent_events(agent_id.as_str()).expect("history").iter().any(|record| matches!(
        &record.event, Event::ProviderToolResult(result)
            if matches!(result.provider_content.as_slice(), [ToolResultContentPart::Image(image)] if image.data.as_ref() == bytes)
    )));
    assert!(!f.h.session_runtime.state_dir.join("artifacts").exists());
}

impl ImageFixture {
    /// Creates a durable fixture; only tests select Artifact invocation policy.
    fn new() -> Self {
        let temp = TempDir::new().expect("state");
        let mut h = echo_harness(temp.path().join("state")).expect("harness");
        connect_ready_configured_extension(
            &mut h,
            "shell",
            "configured-shell",
            tau_proto::ClientKind::Tool,
        );
        let cid = ensure_test_user_agent(&mut h);
        Self { h, cid, temp }
    }

    /// Creates a real CAS object and a currently owned, explicitly Artifact
    /// call.
    fn image(&mut self, call: &str) -> (ToolResult, ArtifactDescriptor) {
        dedup::track_image_call(&mut self.h, &self.cid, call, true);
        let call_id = ToolCallId::from(call);
        self.h.record_wait_tool_request(&call_id);
        let declaration = self
            .h
            .persisted_tool_call_ref(&self.cid, &call_id)
            .expect("real declaration");
        self.h.record_wait_tool_call_ref(call_id, declaration);
        self.h
            .tool_routing
            .tool_runtime
            .pending_tools
            .get_mut(call)
            .expect("pending")
            .image_storage = tau_proto::PreparedImageStorage::Artifact;
        let owner = self
            .h
            .session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner");
        assert_eq!(
            owner.wait_for_latest_durability_for_test(Duration::from_secs(3)),
            tau_core::DurabilityBarrierOutcome::Durable,
        );
        let bytes = dedup::encoded_test_png();
        let mut store = ArtifactStore::new(&self.h.session_runtime.state_dir);
        let (_, descriptor) = put_fixture(&mut store, "fixture/session", "fixture", &bytes, 1);
        let mut result = dedup::image_result(call, bytes);
        let image = result.provider_content[0]
            .inline_image()
            .expect("inline fixture");
        result.provider_content = vec![ToolResultContentPart::ImageReference(
            tau_proto::ImageReference {
                blob: descriptor.clone(),
                media_type: image.media_type,
                width: image.width,
                height: image.height,
                detail: image.detail,
            },
        )];
        (result, descriptor)
    }

    /// Enters the authenticated, committed peer-report path.
    fn report(&mut self, result: ToolResult) {
        self.h
            .handle_extension_event(
                "shell",
                TestProtocolItem::Event(Event::ToolResultReported(result)),
            )
            .expect("report");
    }

    /// Drives one private image completion while preserving other runtime
    /// commands.
    fn complete(&mut self) {
        let completed = self.next_completion();
        self.h
            .handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
            .expect("image command");
    }

    /// Receives physical completion without consuming its permit or authority.
    fn next_completion(&mut self) -> Box<crate::image_verification::ImageVerificationCompleted> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "image completion missing");
            if let Ok(HarnessEvent::Command(command)) =
                self.h.runtime_io.rx.recv_timeout(Duration::from_millis(50))
            {
                if let HarnessCommand::ImageVerificationCompleted(completed) = command {
                    return completed;
                }
                self.h.handle_harness_command(command).expect("command");
            }
        }
    }

    /// Counts only canonical success, not the producer's transient report.
    fn successes(&self, call: &str) -> usize {
        event_log_events(&self.h)
            .iter()
            .filter(|event| {
                matches!(event,
            Event::ProviderToolResult(result) if result.call_id.as_str() == call)
            })
            .count()
    }

    /// Counts the canonical error that owns ordinary failure settlement.
    fn errors(&self, call: &str) -> usize {
        event_log_events(&self.h)
            .iter()
            .filter(|event| {
                matches!(event,
            Event::ProviderToolError(error) if error.call_id.as_str() == call)
            })
            .count()
    }

    /// Finds the private durable root manifest for this whole transcript.
    fn roots(&self) -> PathBuf {
        let id = self.h.agent_runtime.agent_registry.agents[&self.cid]
            .identity
            .agent_id
            .as_ref()
            .expect("agent id");
        self.h
            .session_runtime
            .state_dir
            .join("artifacts/transcript-roots")
            .join(id.as_str())
            .join("roots.json")
    }
}

/// Shared synthetic filesystem pause; it never owns semantic or transport work.
struct PauseState {
    /// Exact I/O boundary to hold.
    cut: ImageVerificationCut,
    /// Reached/released flags.
    state: Mutex<(bool, bool)>,
    /// Signals test progress and release without sleeps.
    wake: Condvar,
}

/// Test-owned release handle that prevents panic cleanup from stranding I/O.
struct ImagePause(Arc<PauseState>);

impl ImagePause {
    /// Installs a deterministic worker boundary before any image job is queued.
    fn install(fixture: &mut ImageFixture, cut: ImageVerificationCut) -> Self {
        let state = Arc::new(PauseState {
            cut,
            state: Mutex::new((false, false)),
            wake: Condvar::new(),
        });
        let worker_state = state.clone();
        fixture.h.runtime_io.artifacts = Some(
            ArtifactWorker::start_with_image_hook(
                &fixture.h.session_runtime.state_dir,
                fixture.h.runtime_io.tx.clone(),
                Arc::new(move |cut| {
                    if cut != worker_state.cut {
                        return;
                    }
                    let mut state = worker_state.state.lock().expect("pause");
                    state.0 = true;
                    worker_state.wake.notify_all();
                    while !state.1 {
                        state = worker_state.wake.wait(state).expect("pause wait");
                    }
                }),
            )
            .expect("worker"),
        );
        Self(state)
    }

    /// Waits for a positive worker observation, not a guessed delay.
    fn wait(&self) {
        let state = self.0.state.lock().expect("pause");
        let (state, _) = self
            .0
            .wake
            .wait_timeout_while(state, Duration::from_secs(3), |state| !state.0)
            .expect("wait");
        assert!(state.0, "worker did not reach pause");
    }

    /// Releases the worker; calling twice is harmless.
    fn release(&self) {
        self.0.state.lock().expect("pause").1 = true;
        self.0.wake.notify_all();
    }
}

impl Drop for ImagePause {
    fn drop(&mut self) {
        self.release();
    }
}

/// Creates a genuine ephemeral runtime for persistence-authority tests.
fn ephemeral_agent(h: &mut Harness) -> (AgentId, AgentId) {
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("fixture-ui"),
        tau_proto::UiCreateAgent {
            request_id: "ephemeral-image".to_owned(),
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            role: h.config.selected_role.clone(),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: true,
        },
    )
    .expect("ephemeral agent");
    let id = event_log_events(h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral started");
    (h.agent_runtime.agent_registry.agent_routes[&id].clone(), id)
}

/// Root protection must already exist before a reference can enter any
/// canonical terminal or cold replay; ordinary textual artifacts remain outside
/// this path.
#[test]
fn referenced_tool_image_is_rooted_before_canonical_publication() {
    let mut fixture = ImageFixture::new();
    let (result, descriptor) = fixture.image("verified-image");
    let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Retained);
    fixture.report(result.clone());
    pause.wait();
    assert_eq!(fixture.successes("verified-image"), 0);
    assert!(
        fixture
            .h
            .tool_routing
            .tool_runtime
            .images
            .contains("verified-image")
    );
    assert!(
        std::fs::read_to_string(fixture.roots())
            .expect("synced roots")
            .contains(descriptor.key.as_str())
    );
    pause.release();
    fixture.complete();
    assert_eq!(fixture.successes("verified-image"), 1);
    assert!(
        !fixture
            .h
            .tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("verified-image")
    );
    fixture.report(result);
    assert_eq!(fixture.successes("verified-image"), 1, "duplicate report");
    let mut store = ArtifactStore::new(&fixture.h.session_runtime.state_dir);
    store
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("root-aware cleanup");
    assert!(
        store
            .execute(
                "reader",
                "reader",
                ArtifactOp::Stat {
                    key: descriptor.key.clone()
                },
                100
            )
            .is_ok()
    );
    let agent = fixture.h.agent_runtime.agent_registry.agents[&fixture.cid]
        .identity
        .agent_id
        .clone()
        .expect("agent");
    fixture
        .h
        .session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(3))
        .expect("flush history");
    let cold =
        AgentStore::open_lazy(fixture.h.session_runtime.state_dir.join("agents")).expect("cold");
    assert!(cold.agent_events(agent.as_str()).expect("history").iter().any(|record| matches!(
        &record.event, Event::ProviderToolResult(result)
            if matches!(result.provider_content.as_slice(), [ToolResultContentPart::ImageReference(image)] if image.blob == descriptor)
    )));
}

/// Actual media validation rejects a valid digest with false dimensions before
/// establishing roots, and the failure settles only the affected call.
#[test]
fn referenced_tool_image_validates_actual_media_before_rooting() {
    let mut fixture = ImageFixture::new();
    let (mut result, _) = fixture.image("false-dimensions");
    let ToolResultContentPart::ImageReference(image) = &mut result.provider_content[0] else {
        panic!("reference");
    };
    image.width += 1;
    fixture.report(result);
    fixture.complete();
    assert_eq!(fixture.successes("false-dimensions"), 0);
    assert_eq!(fixture.errors("false-dimensions"), 1);
    assert!(!fixture.roots().exists());
    let (valid, _) = fixture.image("next-valid-image");
    fixture.report(valid);
    fixture.complete();
    assert_eq!(fixture.successes("next-valid-image"), 1);
}

/// Corrupt prepared objects must not disclose known-key read authority through
/// generic cleanup notices, late UI delivery, or the diagnostic JSONL mirror.
#[test]
fn corrupt_prepared_object_notice_omits_digest_and_path() {
    let mut fixture = ImageFixture::new();
    let (_, _descriptor) = fixture.image("private-object");
    let hash = blake3::hash(&dedup::encoded_test_png()).to_hex();
    let digest = hash.as_str();
    let path = fixture
        .h
        .session_runtime
        .state_dir
        .join("artifacts/blake3")
        .join(digest)
        .join("meta.json");
    std::fs::write(&path, b"corrupt metadata").expect("corrupt");
    let store = ArtifactStore::new(&fixture.h.session_runtime.state_dir);
    let mut paths = Vec::new();
    assert_eq!(
        store.cleanup_reporting(Some(Duration::from_secs(1)), 100, |path| paths
            .push(path.to_owned())),
        Err(ArtifactError::Integrity)
    );
    assert_eq!(paths, [path.clone()]);
    fixture
        .h
        .handle_harness_command(HarnessCommand::ArtifactCleanupCorruption(paths))
        .expect("notice");
    let notices = event_log_events(&fixture.h).into_iter().filter(|event| {
        matches!(event, Event::HarnessNotice(notice) if notice.message.contains("shared artifact cleanup"))
    }).collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    let encoded = serde_json::to_string(&notices).expect("notice projection");
    assert!(encoded.contains("object metadata"));
    assert!(!encoded.contains(digest));
    assert!(!encoded.contains(&path.display().to_string()));
    let mut log = DebugEventLog::open(&fixture.temp.path().join("diagnostic")).expect("log");
    for notice in &notices {
        log.log_published_event(None, notice, tau_proto::UnixMicros::new(1))
            .expect("debug projection");
    }
    let debug = std::fs::read_to_string(log.path()).expect("debug");
    assert!(!debug.contains(digest));
    assert!(!debug.contains(&path.display().to_string()));
    let observer = connect_test_client(
        &mut fixture.h,
        "late-image-observer",
        tau_proto::ClientKind::Ui,
    );
    fixture
        .h
        .handle_client_event(
            "late-image-observer",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::HARNESS_NOTICE,
                )],
                live_selectors: vec![EventSelector::Prefix("harness.".to_owned())],
            })),
        )
        .expect("subscribe");
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.h.session_runtime.history.has_pending() {
        assert!(Instant::now() < deadline, "history handoff");
        if let Ok(HarnessEvent::Command(command)) = fixture
            .h
            .runtime_io
            .rx
            .recv_timeout(Duration::from_millis(50))
        {
            fixture
                .h
                .handle_harness_command(command)
                .expect("history command");
        }
    }
    let frames = observer.lock().expect("observer");
    let delivered = frames.iter().filter_map(|frame| peel_inner_event(&frame.frame))
        .filter(|event| matches!(event, Event::HarnessNotice(notice) if notice.message.contains("shared artifact cleanup")))
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1);
    assert!(
        !serde_json::to_string(&delivered)
            .expect("delivery")
            .contains(digest)
    );
}

/// Once the producer has finished, a committed cancel request must still settle
/// harness verification once. Late I/O cannot revive it; completed roots may
/// leak.
#[test]
fn image_admission_cancel_request_settles_before_late_completion() {
    for cut in [
        ImageVerificationCut::Validated,
        ImageVerificationCut::Retained,
    ] {
        let mut fixture = ImageFixture::new();
        let (result, _) = fixture.image("cancel-image");
        let pause = ImagePause::install(&mut fixture, cut);
        fixture.report(result);
        pause.wait();
        fixture
            .h
            .publish_tool_cancel_request_for(&fixture.cid, None, ToolCallId::from("cancel-image"))
            .expect("cancel");
        let cancelled = |h: &Harness| {
            event_log_events(h).iter().filter(|event| matches!(
            event, Event::ToolCancelled(result) if result.call_id.as_str() == "cancel-image"
        )).count()
        };
        assert_eq!(cancelled(&fixture.h), 1, "producer sends no further report");
        assert!(
            !fixture
                .h
                .tool_routing
                .tool_runtime
                .pending_tools
                .contains_key("cancel-image")
        );
        pause.release();
        fixture.complete();
        assert_eq!(fixture.successes("cancel-image"), 0);
        assert_eq!(cancelled(&fixture.h), 1);
        assert_eq!(
            fixture.roots().exists(),
            cut == ImageVerificationCut::Retained
        );
    }
}

/// Persistence pressure retains the verified exact terminal without repeating
/// storage work or prematurely settling the call.
#[test]
fn image_admission_retains_verified_terminal_across_persistence_backpressure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("backpressure-image");
    let validations = Arc::new(AtomicUsize::new(0));
    let observed = validations.clone();
    fixture.h.runtime_io.artifacts = Some(
        ArtifactWorker::start_with_image_hook(
            &fixture.h.session_runtime.state_dir,
            fixture.h.runtime_io.tx.clone(),
            Arc::new(move |cut| {
                if cut == ImageVerificationCut::Validated {
                    observed.fetch_add(1, Ordering::AcqRel);
                }
            }),
        )
        .expect("worker"),
    );
    fixture.report(result);
    reject_semantic_admissions(&fixture.h, 1000);
    fixture.complete();
    assert_eq!(validations.load(Ordering::Acquire), 1);
    assert_eq!(fixture.successes("backpressure-image"), 0);
    assert!(fixture.roots().exists());
    assert!(
        fixture
            .h
            .tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("backpressure-image")
    );
    assert!(
        fixture
            .h
            .tool_routing
            .tool_runtime
            .retained_terminals
            .contains_key("backpressure-image")
    );
    reject_semantic_admissions(&fixture.h, 0);
    fixture.h.retry_retained_tool_terminals(&fixture.cid);
    assert_eq!(fixture.successes("backpressure-image"), 1);
    assert_eq!(validations.load(Ordering::Acquire), 1);
    assert!(
        !fixture
            .h
            .tool_routing
            .tool_runtime
            .pending_tools
            .contains_key("backpressure-image")
    );
}

/// Head navigation does not invalidate a legitimate in-flight tool round: its
/// result attaches to that original round without selecting it. Ordinary
/// continuation may append new nodes under the user's selected root.
#[test]
fn image_admission_preserves_old_branch_completion_after_navigation() {
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("old-branch-image");
    let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Validated);
    fixture.report(result);
    pause.wait();
    let agent = fixture.h.agent_runtime.agent_registry.agents[&fixture.cid]
        .identity
        .agent_id
        .clone()
        .expect("agent");
    fixture.h.publish_for_agent(
        &fixture.cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert_eq!(
        fixture.h.agent_runtime.agent_registry.agents[&fixture.cid]
            .identity
            .head,
        None
    );
    pause.release();
    fixture.complete();
    assert_eq!(fixture.successes("old-branch-image"), 1);
    let tree = fixture
        .h
        .session_runtime
        .agent_store
        .agent(agent.as_str())
        .expect("tree");
    let result_node = tree
        .terminal_tool_result_node_id(&ToolCallId::from("old-branch-image"))
        .expect("original round completed");
    let selected = fixture.h.agent_runtime.agent_registry.agents[&fixture.cid]
        .identity
        .head
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    assert!(!tree.is_ancestor_head(tau_proto::AgentHead::Node(result_node), selected));
    let round = tree
        .node(result_node)
        .expect("result node")
        .parent_id
        .expect("original round");
    assert!(matches!(&tree.node(round).expect("round").entry,
        AgentEntry::AssistantResponse { output_items, .. }
            if output_items.iter().any(|item| matches!(item,
                ContextItem::ToolCall(call) if call.call_id.as_str() == "old-branch-image"))));
}

/// Storage unavailability and corruption fail one call before any reference
/// becomes canonical, without losing the worker or fabricating replacement
/// pixels.
#[test]
fn image_admission_missing_corrupt_and_wrong_size_fail_nonfatally() {
    for damage in ["missing", "corrupt", "size"] {
        let mut fixture = ImageFixture::new();
        let (mut result, descriptor) = fixture.image("damaged-image");
        let data = fixture
            .h
            .session_runtime
            .state_dir
            .join("artifacts/blake3")
            .join(blake3::hash(&dedup::encoded_test_png()).to_hex().as_str())
            .join("data");
        match damage {
            "missing" => std::fs::remove_file(&data).expect("missing fixture"),
            "corrupt" => {
                let mut bytes = std::fs::read(&data).expect("bytes");
                bytes[0] ^= 1;
                std::fs::write(&data, bytes).expect("corrupt fixture");
            }
            _ => {
                let ToolResultContentPart::ImageReference(image) = &mut result.provider_content[0]
                else {
                    panic!("ref");
                };
                image.blob.size =
                    tau_proto::ArtifactSize::new(descriptor.size.get() + 1).expect("size");
            }
        }
        fixture.report(result);
        fixture.complete();
        assert_eq!(fixture.successes("damaged-image"), 0, "{damage}");
        assert_eq!(fixture.errors("damaged-image"), 1, "{damage}");
        assert!(!fixture.roots().exists(), "{damage}");
        assert!(
            fixture.h.runtime_io.publication.pending_error.is_none(),
            "{damage}"
        );
        assert!(
            !fixture
                .h
                .tool_routing
                .tool_runtime
                .pending_tools
                .contains_key("damaged-image")
        );
    }
}

/// Cleanup winning before CAS protection causes bounded failure, not a durable
/// reference to missing bytes.
#[test]
fn image_admission_cleanup_before_rooting_fails_closed() {
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("gc-image");
    let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Guarded);
    fixture.report(result);
    pause.wait();
    ArtifactStore::new(&fixture.h.session_runtime.state_dir)
        .cleanup(Some(Duration::from_secs(1)), 100)
        .expect("GC before protection");
    pause.release();
    fixture.complete();
    assert_eq!(fixture.successes("gc-image"), 0);
    assert_eq!(fixture.errors("gc-image"), 1);
    assert!(!fixture.roots().exists());
}

/// Session/source/runtime replacement rejects late physical success instead of
/// giving old verification authority to a successor incarnation.
#[test]
fn image_admission_revalidates_session_source_and_runtime_incarnation() {
    for stale in ["session", "source", "runtime"] {
        let mut fixture = ImageFixture::new();
        let (result, _) = fixture.image("stale-image");
        let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Retained);
        fixture.report(result);
        pause.wait();
        match stale {
            "session" => {
                fixture.h.session_runtime.current_session_generation = fixture
                    .h
                    .session_runtime
                    .current_session_generation
                    .saturating_next()
            }
            "source" => {
                fixture
                    .h
                    .extensions
                    .entries
                    .get_mut("shell")
                    .expect("source")
                    .instance_id = tau_proto::ExtensionInstanceId::new(999)
            }
            _ => {
                fixture
                    .h
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(&fixture.cid)
                    .expect("agent")
                    .identity
                    .runtime_incarnation += 1
            }
        }
        pause.release();
        fixture.complete();
        assert_eq!(fixture.successes("stale-image"), 0, "{stale}");
        assert!(
            !fixture
                .h
                .tool_routing
                .tool_runtime
                .images
                .contains("stale-image")
        );
        assert!(
            fixture.roots().exists(),
            "late success is conservative orphan protection"
        );
    }
}

/// Completion retention owns no physical guard. Releasing and re-preparing the
/// exact agent before consumption rejects the old lease even under the same
/// runtime.
#[test]
fn image_admission_completion_cannot_reuse_replaced_durable_lease() {
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("lease-image");
    fixture.report(result);
    let completed = fixture.next_completion();
    let agent = fixture.h.agent_runtime.agent_registry.agents[&fixture.cid]
        .identity
        .agent_id
        .clone()
        .expect("agent");
    fixture
        .h
        .session_runtime
        .agent_store
        .release_managed_agents(Duration::from_secs(2))
        .expect("no guard in completion");
    fixture
        .h
        .session_runtime
        .agent_store
        .prepare_existing_agent(agent.as_str())
        .expect("new lease");
    fixture
        .h
        .handle_harness_command(HarnessCommand::ImageVerificationCompleted(completed))
        .expect("stale completion");
    assert_eq!(fixture.successes("lease-image"), 0);
    assert!(
        !fixture
            .h
            .tool_routing
            .tool_runtime
            .images
            .contains("lease-image")
    );
}

/// Ephemeral and memory-only harnesses cannot persist prepared images, even if
/// malformed captured state requests Artifact mode.
#[test]
fn image_admission_nondurable_modes_never_start_artifact_io() {
    for (mode, ephemeral_agent) in [
        (crate::HarnessStorageMode::SessionEphemeral, false),
        (crate::HarnessStorageMode::MemoryOnly, false),
        (crate::HarnessStorageMode::Durable, true),
    ] {
        let temp = TempDir::new().expect("state");
        let mut h =
            echo_harness_with_storage_mode(temp.path().join("state"), mode).expect("harness");
        connect_ready_configured_extension(
            &mut h,
            "shell",
            "configured-shell",
            tau_proto::ClientKind::Tool,
        );
        let cid = if ephemeral_agent {
            self::ephemeral_agent(&mut h).0
        } else {
            ensure_test_user_agent(&mut h)
        };
        dedup::track_image_call(&mut h, &cid, "nondurable-image", true);
        h.tool_routing
            .tool_runtime
            .pending_tools
            .get_mut("nondurable-image")
            .expect("pending")
            .image_storage = tau_proto::PreparedImageStorage::Artifact;
        let bytes = dedup::encoded_test_png();
        let mut result = dedup::image_result("nondurable-image", bytes.clone());
        result.provider_content = vec![ToolResultContentPart::ImageReference(
            tau_proto::ImageReference {
                blob: ArtifactDescriptor::new(
                    tau_proto::ArtifactKey::parse(format!(
                        "blake3:{}",
                        blake3::hash(&bytes).to_hex()
                    ))
                    .expect("key"),
                    bytes.len() as u64,
                )
                .expect("descriptor"),
                media_type: tau_proto::ImageMediaType::Png,
                width: 2,
                height: 2,
                detail: tau_proto::ImageDetail::High,
            },
        )];
        h.handle_extension_event(
            "shell",
            TestProtocolItem::Event(Event::ToolResultReported(result)),
        )
        .expect("report");
        assert!(h.runtime_io.artifacts.is_none());
        assert!(!h.session_runtime.state_dir.join("artifacts").exists());
        assert!(!event_log_events(&h).iter().any(|event| matches!(event,
            Event::ProviderToolResult(result) if result.call_id.as_str() == "nondurable-image")));
    }
}

/// Image jobs do not get a second worker budget: queued and unconsumed
/// RPC/image completions together exhaust the existing eight permits.
#[test]
fn image_admission_shares_eight_operation_budget_with_artifact_rpc() {
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("budget-image");
    let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Guarded);
    fixture.report(result);
    pause.wait();
    let session = fixture.h.session_runtime.current_session_id.clone();
    let rpc = |id: &str| tau_proto::ArtifactRequest {
        request_id: id.parse().expect("request"),
        expected_session_id: session.clone(),
        op: ArtifactOp::Available,
    };
    for index in 0..7 {
        fixture
            .h
            .runtime_io
            .artifacts
            .as_mut()
            .expect("worker")
            .submit(
                "fixture".into(),
                crate::test_connection_id("shell"),
                rpc(&format!("queued-{index}")),
            )
            .expect("shared permit");
    }
    assert_eq!(
        fixture
            .h
            .runtime_io
            .artifacts
            .as_mut()
            .expect("worker")
            .submit(
                "fixture".into(),
                crate::test_connection_id("shell"),
                rpc("overflow"),
            ),
        Err(ArtifactError::Busy)
    );
    pause.release();
    let image = fixture.next_completion();
    let mut held = Vec::new();
    while held.len() < 7 {
        if let HarnessEvent::Command(command) = fixture
            .h
            .runtime_io
            .rx
            .recv_timeout(Duration::from_secs(3))
            .expect("completion")
        {
            if matches!(command, HarnessCommand::ArtifactCompleted(_)) {
                held.push(command);
            } else {
                fixture
                    .h
                    .handle_harness_command(command)
                    .expect("other command");
            }
        }
    }
    assert_eq!(
        fixture
            .h
            .runtime_io
            .artifacts
            .as_mut()
            .expect("worker")
            .submit(
                "fixture".into(),
                crate::test_connection_id("shell"),
                rpc("unconsumed"),
            ),
        Err(ArtifactError::Busy)
    );
    fixture
        .h
        .handle_harness_command(HarnessCommand::ImageVerificationCompleted(image))
        .expect("consume image");
    fixture
        .h
        .runtime_io
        .artifacts
        .as_mut()
        .expect("worker")
        .submit(
            "fixture".into(),
            crate::test_connection_id("shell"),
            rpc("recovered"),
        )
        .expect("consumption releases exactly one permit");
    assert_eq!(fixture.successes("budget-image"), 1);
    drop(held);
}

/// Interceptors may keep existing argument/web-policy edits, but cannot grant
/// prepared persistence or make admission accept an unsolicited reference.
#[test]
fn tool_started_interception_cannot_change_captured_image_storage() {
    let (_temp, mut h) = dispatch::setup_routed_test_tool_call("policy-image", "owned_tool");
    let call_id = ToolCallId::from("policy-image");
    let cid = h.tool_routing.tool_runtime.tool_agents[&call_id].clone();
    assert_eq!(
        h.tool_routing.tool_runtime.pending_tools[&call_id].image_storage,
        tau_proto::PreparedImageStorage::Inline
    );
    h.tool_routing
        .tool_runtime
        .pending_tools
        .get_mut(&call_id)
        .expect("tool")
        .allows_provider_image = true;
    let interceptor = connect_test_tool(&mut h, "image-policy-interceptor");
    h.handle_extension_event(
        "image-policy-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let started = tau_proto::ToolStarted {
        call_id: call_id.clone(),
        tool_name: ToolName::new("owned_tool"),
        arguments: CborValue::Null,
        agent_id: durable_agent_id_for_conversation(&h, &cid),
        originator: Default::default(),
        invocation_policy: Default::default(),
    };
    h.publish_for_agent(&cid, Event::ToolStarted(started));
    let (Event::ToolStarted(mut replacement), _) = intercepted_payload(&interceptor) else {
        panic!("started");
    };
    replacement.arguments = CborValue::Text("existing argument rewrite".into());
    replacement.invocation_policy.allowed_web_domains = Some(vec!["replacement.example".into()]);
    replacement.invocation_policy.prepared_image_storage =
        tau_proto::PreparedImageStorage::Artifact;
    h.handle_extension_event(
        "image-policy-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ToolStarted(replacement.clone())))),
        })),
    )
    .expect("replacement");
    let observed = event_log_events(&h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::ToolStarted(started) if started.call_id == call_id => Some(started),
            _ => None,
        })
        .expect("started committed");
    assert_eq!(
        observed.invocation_policy.prepared_image_storage,
        tau_proto::PreparedImageStorage::Inline
    );
    assert_eq!(observed.arguments, replacement.arguments);
    assert_eq!(
        observed.invocation_policy.allowed_web_domains,
        replacement.invocation_policy.allowed_web_domains
    );
    assert_eq!(
        h.tool_routing.tool_runtime.pending_tools[&call_id].image_storage,
        tau_proto::PreparedImageStorage::Inline
    );
    let mut result = dedup::image_result(call_id.as_str(), dedup::encoded_test_png());
    result.tool_name = ToolName::new("owned_tool");
    result.provider_content = vec![ToolResultContentPart::ImageReference(
        tau_proto::ImageReference {
            blob: ArtifactDescriptor::new(
                tau_proto::ArtifactKey::parse(format!("blake3:{}", "ab".repeat(32))).expect("key"),
                3,
            )
            .expect("size"),
            media_type: tau_proto::ImageMediaType::Png,
            width: 2,
            height: 2,
            detail: tau_proto::ImageDetail::High,
        },
    )];
    h.handle_extension_event(
        "conn-owner",
        TestProtocolItem::Event(Event::ToolResultReported(result)),
    )
    .expect("unsolicited reference");
    assert!(
        h.runtime_io.artifacts.is_none(),
        "no storage grant through interception"
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(event,
        Event::ProviderToolResult(result) if result.call_id == call_id)));
}

/// A parked report retains its original configured source; replacement of that
/// source before interception finishes cannot start image I/O for its
/// successor.
#[test]
fn intercepted_reference_report_rejects_stale_source_before_io() {
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("intercepted-image");
    let interceptor = connect_test_tool(&mut fixture.h, "image-report-interceptor");
    fixture
        .h
        .handle_extension_event(
            "image-report-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::TOOL_RESULT_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept");
    fixture.report(result);
    assert!(matches!(
        intercepted_payload(&interceptor).0,
        Event::ToolResultReported(_)
    ));
    fixture
        .h
        .extensions
        .entries
        .get_mut("shell")
        .expect("source")
        .instance_id = tau_proto::ExtensionInstanceId::new(999);
    fixture
        .h
        .handle_extension_event(
            "image-report-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("late pass");
    assert!(fixture.h.runtime_io.artifacts.is_none());
    assert!(!fixture.roots().exists());
    assert_eq!(fixture.successes("intercepted-image"), 0);
}

/// An unexpected decoder panic unwinds both lock scopes, reports one bounded
/// failure and leaves the shared worker usable for another image.
#[test]
fn image_admission_worker_panic_settles_without_losing_worker() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut fixture = ImageFixture::new();
    let (result, _) = fixture.image("panic-image");
    let first = Arc::new(AtomicBool::new(true));
    fixture.h.runtime_io.artifacts = Some(
        ArtifactWorker::start_with_image_hook(
            &fixture.h.session_runtime.state_dir,
            fixture.h.runtime_io.tx.clone(),
            Arc::new(move |cut| {
                if cut == ImageVerificationCut::Validated && first.swap(false, Ordering::AcqRel) {
                    panic!("injected verifier failure");
                }
            }),
        )
        .expect("worker"),
    );
    fixture.report(result);
    fixture.complete();
    assert_eq!(fixture.errors("panic-image"), 1);
    assert_eq!(fixture.successes("panic-image"), 0);
    assert!(!fixture.roots().exists());
    let (result, _) = fixture.image("after-panic-image");
    fixture.report(result);
    fixture.complete();
    assert_eq!(fixture.successes("after-panic-image"), 1);
}

/// An Artifact grant is bound to both original identities. Changing either
/// forces only storage to Inline, and a later interceptor cannot restore the
/// grant by reverting identity. Existing original-call verification stays
/// owned.
#[test]
fn intercepted_identity_edits_cannot_transplant_or_restore_artifact_grant() {
    for edit in ["unchanged", "call", "agent", "chain"] {
        let mut fixture = ImageFixture::new();
        let (_, ephemeral_id) = ephemeral_agent(&mut fixture.h);
        let (result, _) = fixture.image("bound-image");
        let pause = ImagePause::install(&mut fixture, ImageVerificationCut::Retained);
        fixture.report(result);
        pause.wait();
        let original_agent = durable_agent_id_for_conversation(&fixture.h, &fixture.cid);
        let original_call = ToolCallId::from("bound-image");
        let first = connect_test_tool(&mut fixture.h, "image-grant-first");
        fixture
            .h
            .handle_extension_event(
                "image-grant-first",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("first interceptor");
        let second = if edit == "chain" {
            let sink = connect_test_tool(&mut fixture.h, "image-grant-second");
            fixture
                .h
                .handle_extension_event(
                    "image-grant-second",
                    TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                        selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
                        priority: InterceptionPriority::new(1),
                    })),
                )
                .expect("second interceptor");
            Some(sink)
        } else {
            None
        };
        fixture.h.publish_for_agent(
            &fixture.cid,
            Event::ToolStarted(tau_proto::ToolStarted {
                call_id: original_call.clone(),
                tool_name: ToolName::new("read_image"),
                arguments: CborValue::Null,
                agent_id: original_agent.clone(),
                originator: Default::default(),
                invocation_policy: tau_proto::ToolInvocationPolicy {
                    prepared_image_storage: tau_proto::PreparedImageStorage::Artifact,
                    ..Default::default()
                },
            }),
        );
        let (Event::ToolStarted(mut replacement), _) = intercepted_payload(&first) else {
            panic!("started");
        };
        if matches!(edit, "call" | "chain") {
            replacement.call_id = ToolCallId::from("redirected-image");
        }
        if matches!(edit, "agent" | "chain") {
            replacement.agent_id = ephemeral_id.clone();
        }
        replacement.arguments = CborValue::Text("allowed argument edit".into());
        replacement.invocation_policy.allowed_web_domains = Some(vec!["allowed.example".into()]);
        replacement.invocation_policy.prepared_image_storage = if edit == "unchanged" {
            tau_proto::PreparedImageStorage::Inline
        } else {
            tau_proto::PreparedImageStorage::Artifact
        };
        fixture
            .h
            .handle_extension_event(
                "image-grant-first",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(Some(Box::new(Event::ToolStarted(
                        replacement.clone(),
                    )))),
                })),
            )
            .expect("first replacement");
        if let Some(second) = second {
            let (Event::ToolStarted(mut reverted), _) = intercepted_payload(&second) else {
                panic!("chained started");
            };
            assert_eq!(
                reverted.invocation_policy.prepared_image_storage,
                tau_proto::PreparedImageStorage::Inline
            );
            reverted.call_id = original_call.clone();
            reverted.agent_id = original_agent.clone();
            reverted.invocation_policy.prepared_image_storage =
                tau_proto::PreparedImageStorage::Artifact;
            fixture
                .h
                .handle_extension_event(
                    "image-grant-second",
                    TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                        action: InterceptAction::Pass(Some(Box::new(Event::ToolStarted(reverted)))),
                    })),
                )
                .expect("reverted identity");
        }
        let observed = event_log_events(&fixture.h)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                Event::ToolStarted(started) if started.tool_name.as_str() == "read_image" => {
                    Some(started)
                }
                _ => None,
            })
            .expect("final started");
        assert_eq!(
            observed.invocation_policy.prepared_image_storage,
            if edit == "unchanged" {
                tau_proto::PreparedImageStorage::Artifact
            } else {
                tau_proto::PreparedImageStorage::Inline
            },
            "{edit}"
        );
        assert_eq!(observed.arguments, replacement.arguments);
        assert_eq!(
            observed.invocation_policy.allowed_web_domains,
            replacement.invocation_policy.allowed_web_domains
        );
        assert_eq!(
            fixture.h.tool_routing.tool_runtime.pending_tools[&original_call].image_storage,
            tau_proto::PreparedImageStorage::Artifact,
            "identity edits do not change the original call's captured policy"
        );
        pause.release();
        fixture.complete();
        assert_eq!(
            fixture.successes("bound-image"),
            1,
            "original operation remains owned: {edit}"
        );
        assert_eq!(fixture.successes("redirected-image"), 0);
    }
}
