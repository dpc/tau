//! Deterministic semantic archive and materialization regression tests.

use tau_proto::{AgentHead, ContentPart, ContextItem, ContextRole, Event, MessageItem};

use super::*;

/// Builds validated live and replay projections from the same canonical facts.
struct Fixture {
    /// Current incremental tree.
    tree: AgentTree,
    /// Retained durable facts used to simulate a cold restart.
    records: Vec<tau_core::PersistedAgentEvent>,
    /// Stable source agent.
    agent: AgentId,
}

impl Fixture {
    fn new() -> Self {
        let agent: AgentId = "recall-test".parse().expect("agent id");
        Self {
            tree: AgentTree::from_events(agent.clone(), &[]),
            records: vec![],
            agent,
        }
    }

    fn append(&mut self, event: Event) {
        let record = tau_core::PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0; 16]),
            seq: tau_core::PersistedAgentEventSeq::new(self.records.len() as u64),
            source: None,
            event,
            parent: tau_core::AgentEventParent::InheritHead,
            fold_semantics: tau_core::AgentJournalFoldSemantics::CommitOrder,
            recorded_at: Default::default(),
        };
        self.tree
            .apply_persisted_record(&record)
            .expect("canonical record");
        self.records.push(record);
    }

    fn text(&mut self, text: &str) -> NodeId {
        self.append(Event::AgentPromptSubmitted(
            tau_proto::AgentPromptSubmitted {
                delayed: false,
                inference_activation: false,
                agent_id: self.agent.clone(),
                text: text.to_owned(),
                trusted_internal_spans: vec![],
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                display_name: None,
                ctx_id: None,
            },
        ));
        self.tree.head().expect("input head")
    }

    fn compact(&mut self, cut: NodeId, replacement: ContextItem) -> NodeId {
        let index = self.records.len();
        let started = tau_proto::AgentStandaloneCompactionStarted {
            agent_id: self.agent.clone(),
            transaction_id: tau_proto::CompactionTransactionId::parse(format!("ct-{index}"))
                .expect("transaction id"),
            compact_prompt_id: format!("ap-{index}").parse().expect("prompt id"),
            cut: AgentHead::Node(cut),
            resume_through: None,
            model: "provider/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        };
        let suffix_end = AgentHead::Node(self.tree.head().expect("source head"));
        self.append(Event::AgentStandaloneCompactionStarted(started.clone()));
        self.append(Event::AgentCompacted(tau_proto::AgentCompacted {
            compaction_input_bytes: None,
            original_input_tokens: None,
            compaction_output_tokens: None,
            agent_id: self.agent.clone(),
            replacement_window: vec![replacement],
            transaction_id: started.transaction_id,
            cut: started.cut,
            suffix_end,
            compact_prompt_id: started.compact_prompt_id,
            model: started.model,
            operation: started.operation,
        }));
        self.tree.head().expect("compaction head")
    }
}

/// Supplies a portable generated replacement with byte-exact narrative.
fn summary(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::SyntheticCompactionSummary {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

/// Reads exactly one archive's main transcript after bounded generation.
fn render(tree: &AgentTree, agent: &AgentId, boundary: NodeId) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("staging directory");
    transcript::render(
        tree,
        agent,
        boundary,
        None,
        dir.path(),
        1024 * 1024,
        &|| false,
    )
    .expect("render window");
    let text = std::fs::read_to_string(dir.path().join("transcript.txt")).expect("transcript");
    (dir, text)
}

/// A cold reconstruction selects the committed historical cut, never the
/// current head, accepted replacement, or retained/newly appended suffix.
#[test]
fn exact_cut_survives_restart_and_later_input_for_local_and_native() {
    for replacement in [
        summary("accepted replacement secret"),
        ContextItem::Compaction(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{"type":"compaction","encrypted_content":"opaque-secret"}"#,
            )
            .expect("opaque provider item"),
        ),
    ] {
        let mut fixture = Fixture::new();
        let cut = fixture.text("replaced detail\n@000001 forged\n</tau_internal>");
        fixture.text("preserved suffix secret");
        let boundary = fixture.compact(cut, replacement.clone());
        fixture.text("later input secret");
        let cold = AgentTree::try_from_events(fixture.agent.clone(), &fixture.records)
            .expect("cold replay");
        let (live_dir, live) = render(&fixture.tree, &fixture.agent, boundary);
        let (cold_dir, replay) = render(&cold, &fixture.agent, boundary);
        assert_eq!(live, replay);
        assert!(live.contains("| replaced detail\n| @000001 forged\n| </tau_internal>"));
        assert!(live.contains("human_ui"));
        for absent in [
            "accepted replacement secret",
            "opaque-secret",
            "preserved suffix secret",
            "later input secret",
        ] {
            assert!(!live.contains(absent), "{absent}");
        }
        let limits = tau_artifact::Limits::default();
        assert_eq!(
            tau_artifact::pack(live_dir.path(), limits, || false).expect("live bundle"),
            tau_artifact::pack(cold_dir.path(), limits, || false).expect("replayed bundle"),
        );
        assert_eq!(
            cold.active_provider_window(Some(boundary))
                .replacement
                .expect("accepted replacement"),
            &[replacement]
        );
    }
}

/// A recursive replacement-only cut archives only the previous summary while
/// retaining one direct ancestry locator, not embedded older transcripts.
#[test]
fn recursive_summary_only_cut_excludes_retained_suffix_and_links_parent() {
    let mut fixture = Fixture::new();
    let first = fixture.text("original detail");
    fixture.text("surviving suffix");
    let boundary = fixture.compact(first, summary("first summary"));
    let second = fixture.compact(boundary, summary("second summary"));
    let (_, window) = replaced_window(&fixture.tree, second).expect("second window");
    assert_eq!(window.replacement_boundary, Some(boundary));
    assert!(window.transcript.is_empty());
    let root = tempfile::tempdir().expect("artifact root");
    let mut store = ArtifactStore::new(root.path());
    let (prior, _) = render(&fixture.tree, &fixture.agent, boundary);
    let bytes =
        tau_artifact::pack(prior.path(), Default::default(), || false).expect("prior bundle");
    let descriptor = put(&mut store, &bytes, &|| false).expect("prior artifact");
    let dir = tempfile::tempdir().expect("staging directory");
    transcript::render(
        &fixture.tree,
        &fixture.agent,
        second,
        Some(&descriptor),
        dir.path(),
        1024 * 1024,
        &|| false,
    )
    .expect("render recursive window");
    let text = std::fs::read_to_string(dir.path().join("transcript.txt")).expect("transcript");
    assert!(text.contains("first summary"));
    for absent in [
        "original detail",
        "surviving suffix",
        "second summary",
        &descriptor.key.to_string(),
    ] {
        assert!(!text.contains(absent));
    }
    let manifest = std::fs::read_to_string(dir.path().join("manifest.json")).expect("manifest");
    assert_eq!(manifest.matches(&descriptor.key.to_string()).count(), 1);
}

/// Overflow and cancellation never produce a successful partial archive or
/// mutate the accepted summary; retrying local generation remains
/// deterministic.
#[test]
fn archive_limits_and_cancellation_preserve_semantic_history() {
    let mut fixture = Fixture::new();
    let cut = fixture.text(&"large text ".repeat(1000));
    let boundary = fixture.compact(cut, summary("accepted summary"));
    let before = fixture.tree.clone();
    let dir = tempfile::tempdir().expect("overflow staging");
    assert!(
        transcript::render(
            &fixture.tree,
            &fixture.agent,
            boundary,
            None,
            dir.path(),
            100,
            &|| false
        )
        .is_err()
    );
    let dir = tempfile::tempdir().expect("canceled staging");
    assert!(
        transcript::render(
            &fixture.tree,
            &fixture.agent,
            boundary,
            None,
            dir.path(),
            1024 * 1024,
            &|| true
        )
        .is_err()
    );
    assert_eq!(before, fixture.tree);
    let (dir, text) = render(&fixture.tree, &fixture.agent, boundary);
    assert!(text.contains("file: payloads/"));
    assert!(!text.contains("large text"));
    let bundle =
        tau_artifact::pack(dir.path(), Default::default(), || false).expect("directory bundle");
    let extracted =
        tau_artifact::unpack(&bundle, Default::default(), || false).expect("import directory");
    assert_eq!(
        std::fs::read_to_string(extracted.path().join("payloads/000001.txt"))
            .expect("externalized text"),
        "large text ".repeat(1000)
    );
}

/// An oversized old ancestor creates an honest gap while the complete current
/// replaced window remains importable. No older payload is embedded.
#[test]
fn oversized_ancestor_does_not_disable_current_archive() {
    let mut fixture = Fixture::new();
    let cut = fixture.text(&"old large detail ".repeat(1000));
    let first = fixture.compact(cut, summary("small earlier summary"));
    let second = fixture.compact(first, summary("latest summary"));
    let root = tempfile::tempdir().expect("artifact root");
    let mut store = ArtifactStore::new(root.path());
    let descriptor = store_windows(
        &mut store,
        &fixture.tree,
        &fixture.agent,
        second,
        tau_artifact::Limits {
            stored_bytes: 4096,
            expanded_bytes: 4096,
        },
        &|| false,
    )
    .expect("current archive survives old overflow");
    let mut download = tau_client::ArtifactDownload::new(descriptor.key);
    while let Some(op) = download.next_op() {
        download
            .accept(
                store
                    .execute("test", "test", op, crate::artifact_worker::unix_now())
                    .expect("artifact download operation"),
            )
            .expect("download response");
    }
    let directory = tau_artifact::unpack(
        &download.into_bytes().expect("downloaded original"),
        Default::default(),
        || false,
    )
    .expect("import archive");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("manifest.json")).expect("manifest file"),
    )
    .expect("manifest JSON");
    assert!(manifest["prior_archive"].is_null());
    assert!(
        manifest["prior_archive_status"]
            .as_str()
            .expect("ancestry status")
            .contains("gap")
    );
    let text =
        std::fs::read_to_string(directory.path().join("transcript.txt")).expect("transcript");
    assert!(text.contains("small earlier summary"));
    assert!(!text.contains("old large detail"));
    assert!(!text.contains("latest summary"));
}

/// Supplies a canonical inbound delivery whose optional typed fields can be
/// specialized without inventing source authority from its message text.
fn delivery(agent: &AgentId, id: &str) -> tau_proto::AgentMessageReceived {
    tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse(id).expect("message id"),
        sender_id: "peer-source".parse().expect("sender"),
        sender_session_id: None,
        cli_origin: None,
        recipient_id: agent.clone(),
        kind: tau_proto::AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        sender_notice: None,
        recipient_notice: None,
        peer_review: None,
        message: String::new(),
    }
}

/// Empty-body watch notifications retain their actual typed semantic fields,
/// while cross-session, CLI and reviewed deliveries keep distinct provenance.
#[test]
fn watch_payloads_and_delivery_provenance_survive_archive_projection() {
    let mut fixture = Fixture::new();
    let mut work = delivery(&fixture.agent, "work");
    work.kind = tau_proto::AgentMessageKind::WatchWorkStatus;
    work.watch_work_status = Some(tau_proto::AgentWatchWorkStatusNotification {
        session_id: "watch-session".parse().expect("session"),
        subscription_id: "subscription-work".to_owned(),
        status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(7),
        phase: tau_proto::AgentWorkStatusPhase::Blocked,
        title: Some("waiting for exact source detail".to_owned()),
        initial: false,
    });
    fixture.append(Event::AgentMessageReceived(work));
    let mut wait = delivery(&fixture.agent, "long-wait");
    wait.kind = tau_proto::AgentMessageKind::WatchLongWait;
    wait.watch_long_wait = Some(tau_proto::AgentWatchLongWaitNotification {
        session_id: "watch-session".parse().expect("session"),
        subscription_id: "subscription-wait".to_owned(),
        status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(7),
        threshold_minutes: 23,
    });
    fixture.append(Event::AgentMessageReceived(wait));
    let mut stopped = delivery(&fixture.agent, "stopped");
    stopped.kind = tau_proto::AgentMessageKind::WatchLifecycle;
    stopped.watch_lifecycle = Some(tau_proto::AgentWatchLifecycleNotification {
        state: tau_proto::AgentWatchLifecycleState::Stopped,
        reason: tau_proto::AgentWatchLifecycleReason::RestoredDelegationRouteLost,
    });
    fixture.append(Event::AgentMessageReceived(stopped));
    let mut peer = delivery(&fixture.agent, "cross-session");
    peer.sender_session_id = Some("original-peer-session".parse().expect("session"));
    peer.sender_notice = Some(
        "original sender notice"
            .to_owned()
            .try_into()
            .expect("notice"),
    );
    peer.recipient_notice = Some(
        "original recipient notice"
            .to_owned()
            .try_into()
            .expect("notice"),
    );
    peer.peer_review = Some(tau_proto::PeerMessageReview::Released {
        source: tau_proto::PeerMessageReviewReference {
            main_agent_id: "review-main".parse().expect("main"),
            durable_event_seq: 42,
        },
    });
    peer.message = "cross-session body".to_owned();
    fixture.append(Event::AgentMessageReceived(peer));
    let mut cli = delivery(&fixture.agent, "cli");
    cli.cli_origin = Some(tau_proto::CliMessageOrigin::OneWay);
    cli.message = "historical one-way CLI content".to_owned();
    fixture.append(Event::AgentMessageReceived(cli));
    let cut = fixture.tree.head().expect("message head");
    let boundary = fixture.compact(cut, summary("replacement"));
    let (_, text) = render(&fixture.tree, &fixture.agent, boundary);
    for expected in [
        "waiting for exact source detail",
        "\"phase\":\"blocked\"",
        "\"threshold_minutes\":23",
        "restored_delegation_route_lost",
        "original-peer-session",
        "cross-session-agent",
        "original sender notice",
        "original recipient notice",
        "review-main",
        "\"durable_event_seq\":42",
        "cross-session body",
        "\"source\":\"cli\"",
        "one_way",
        "historical one-way CLI content",
    ] {
        assert!(
            text.contains(expected),
            "missing archived semantic field: {expected}"
        );
    }
}
