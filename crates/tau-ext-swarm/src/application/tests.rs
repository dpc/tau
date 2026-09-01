use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use iroh::endpoint as path_iroh_endpoint;
use tau_swarm_api::{
    Agent, AgentActivity, AgentNavigationMode, AgentWorkStatus, ApplicationIncarnationId,
    BlockerId, BlockerPublication, BlockerRevisionNumber, CorrelationId, DeliveryOutcome, Hostname,
    PromptRequest, SessionChange, SessionId, TaskId, TaskInfo, TaskTitle, Timestamp,
};
use tau_swarm_client::{
    Backoff, Connector, ErrorKind, ExpectedPeer, IncomingCommand, SessionTransport,
};
use tau_swarm_client_api::v0::{BlockerAnswerKind, PromptRequest as WirePromptRequest};
use tau_swarm_client_api::{
    AuthenticateRequest, AuthenticateResponse, CURRENT_PROTOCOL_VERSION, Credential, CredentialId,
    DeclareSessionRequest, DeclareSessionResponse, Secret, SubmitChangeRequest,
    SubmitChangeResponse, SubmitSnapshotRequest, SubmitSnapshotResponse,
};
use tau_swarm_iroh::{Credentials, IrohConnector, Server};

use super::*;
use crate::projection::ProjectionLimits;
use crate::tools::{BlockerHistoryLimits, BlockerLifecycle, BlockerRecord};

fn prompt(id: &str, message: &str) -> DeliverPromptRequest {
    DeliverPromptRequest {
        prompt: WirePromptRequest {
            correlation_id: id.into(),
            agent_id: "agent".into(),
            message: message.into(),
        },
    }
}

fn incarnation(byte: u8) -> ApplicationIncarnationId {
    ApplicationIncarnationId::from_bytes([byte; 32])
}

fn application(
    loaded: bool,
) -> (
    Arc<SwarmApplication>,
    mpsc::Receiver<PromptSubmission>,
    mpsc::Receiver<BlockerSubmission>,
) {
    let mut projection = SessionProjection::new(ProjectionLimits {
        history_entries: 16,
        ..ProjectionLimits::unconfigured()
    });
    if loaded {
        projection
            .upsert_agent(Agent {
                id: AgentId::new("agent"),
                name: "Agent".into(),
                activity: AgentActivity::Waiting,
                navigation_mode: AgentNavigationMode::Active,
                watches: BTreeSet::new(),
                work_status: AgentWorkStatus::Unreported,
            })
            .expect("agent publication");
    }
    let (prompt_tx, prompt_rx) = mpsc::channel(4);
    let (blocker_tx, blocker_rx) = mpsc::channel(4);
    let application = SwarmApplication::new(
        SessionIdentity::new(Hostname::new("host"), SessionId::new("session")),
        Arc::new(Mutex::new(projection)),
        Arc::new(Notify::new()),
        prompt_tx,
        blocker_tx,
    )
    .with_command_policy(
        Duration::from_millis(25),
        CommandLimits {
            entries: 8,
            logical_bytes: 16 * 1024,
        },
    );
    (Arc::new(application), prompt_rx, blocker_rx)
}

/// Builds an application whose projection and process-memory history contain
/// the same active blocker, returning the loopback receiver for cut testing.
async fn application_with_blocker(
    command_timeout: Duration,
    command_limits: CommandLimits,
    blocker_history_bytes: usize,
) -> (
    Arc<SwarmApplication>,
    mpsc::Receiver<BlockerSubmission>,
    Arc<StdMutex<Vec<BlockerRecord>>>,
) {
    let (application, _prompts, blockers) = application(true);
    let publication = BlockerPublication {
        blocker_id: BlockerId::new("blocker"),
        revision: BlockerRevisionNumber(1),
        owner: AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        source_timestamp: Timestamp(1),
    };
    application
        .projection
        .lock()
        .await
        .add_blocker(publication)
        .expect("active blocker");
    let history = Arc::new(StdMutex::new(vec![BlockerRecord {
        blocker_id: BlockerId::new("blocker"),
        revision: BlockerRevisionNumber(1),
        owner: AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        lifecycle: BlockerLifecycle::active(),
    }]));
    let application = Arc::new(
        Arc::try_unwrap(application)
            .ok()
            .expect("sole application")
            .with_command_policy(command_timeout, command_limits)
            .with_blocker_history(
                Arc::clone(&history),
                BlockerHistoryLimits {
                    entries: 8,
                    encoded_bytes: blocker_history_bytes,
                },
            ),
    );
    (application, blockers, history)
}

/// Returns one valid custom answer request with a caller-selected command ID.
fn blocker_answer(command_id: &str) -> AnswerBlockerRequest {
    AnswerBlockerRequest {
        command_id: command_id.into(),
        blocker_id: "blocker".into(),
        revision: 1,
        kind: BlockerAnswerKind::Custom,
        response: "answer".into(),
    }
}

/// Builds one active process-memory record for reservation accounting fixtures.
fn active_history_record(id: &str) -> BlockerRecord {
    BlockerRecord {
        blocker_id: BlockerId::new(id),
        revision: BlockerRevisionNumber(1),
        owner: AgentId::new("agent"),
        title: "title".into(),
        description: "description".into(),
        recommended_answer: None,
        task_id: None,
        lifecycle: BlockerLifecycle::active(),
    }
}

/// Asserts that a failed remote transaction restored the exact unreserved
/// active lifecycle rather than leaving a hidden byte reservation behind.
fn assert_unreserved_active(history: &StdMutex<Vec<BlockerRecord>>) {
    assert!(matches!(
        history.lock().expect("history")[0].lifecycle,
        BlockerLifecycle::Active {
            pending_answer_bytes: None
        }
    ));
}

/// Dropping a level-triggered changes wait consumes no revision; a later task
/// metadata mutation remains observable from the same cut.
#[tokio::test]
async fn cancelled_changes_wait_does_not_consume_task_info() {
    let (application, _, _) = application(true);
    let cut = application.snapshot().await.expect("snapshot").revision;
    {
        let mut wait = std::pin::pin!(application.changes_after(cut));
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(
            wait.as_mut().poll(&mut context),
            Poll::Pending,
            "unchanged projection should keep waiting"
        );
    }
    application
        .projection
        .lock()
        .await
        .upsert_task_info(TaskInfo {
            task_id: TaskId::new("task"),
            title: TaskTitle::new("Title").expect("task title"),
            description: None,
        })
        .expect("task metadata");
    application.changed.notify_waiters();
    let batch = application
        .changes_after(cut)
        .await
        .expect("retained task metadata");
    assert!(matches!(
        batch.changes.as_slice(),
        [SessionChange::UpsertTaskInfo(info)] if info.task_id == TaskId::new("task")
    ));
    assert_eq!(batch.revision, PublicationRevision(cut.0 + 1));
}

/// Admission charges the command table, queued/pending loopback copies,
/// detached Tau event, map key, and maximum cached textual result.
#[test]
fn prompt_accounting_covers_all_retained_copies() {
    let request = prompt("id", "body");
    assert_eq!(
        prompt_retained_bytes(&request),
        Ok(4 * 2 + 3 * "agent".len() + 3 * 4 + MAXIMUM_CACHED_RESULT_BYTES)
    );
}

/// One byte below the exact retained footprint rejects before a Tau side
/// effect.
#[tokio::test]
async fn prompt_accounting_rejects_exact_bound_overflow() {
    let (application, mut prompts, _) = application(true);
    let request = prompt("bound", "body");
    let maximum = prompt_retained_bytes(&request).expect("accounting");
    let application = Arc::new(
        Arc::try_unwrap(application)
            .ok()
            .expect("sole application")
            .with_command_policy(
                Duration::from_millis(25),
                CommandLimits {
                    entries: 8,
                    logical_bytes: maximum - 1,
                },
            ),
    );
    assert!(matches!(
        application.deliver_prompt(request).await,
        Ok(DeliverPromptResponse::Rejected(_))
    ));
    assert!(prompts.try_recv().is_err());
}

/// A missing target is an authoritative rejection and never reaches Tau.
#[tokio::test]
async fn rejects_prompt_for_missing_agent_without_submission() {
    let (application, mut prompts, _) = application(false);
    assert!(matches!(
        application.deliver_prompt(prompt("one", "hello")).await,
        Ok(DeliverPromptResponse::Rejected(_))
    ));
    assert!(prompts.try_recv().is_err());
}

/// Exact completed retries replay the cached result without a second Tau side
/// effect, while payload mutation is rejected.
#[tokio::test]
async fn deduplicates_completed_prompt_and_rejects_payload_change() {
    let (application, mut prompts, _) = application(true);
    let task = tokio::spawn({
        let application = Arc::clone(&application);
        async move { application.deliver_prompt(prompt("one", "hello")).await }
    });
    let submission = prompts.recv().await.expect("one submission");
    submission.completion.send(Ok(())).expect("waiting command");
    assert_eq!(
        task.await.expect("task"),
        Ok(DeliverPromptResponse::Accepted)
    );
    assert_eq!(
        application.deliver_prompt(prompt("one", "hello")).await,
        Ok(DeliverPromptResponse::Accepted)
    );
    assert!(matches!(
        application.deliver_prompt(prompt("one", "changed")).await,
        Ok(DeliverPromptResponse::Rejected(_))
    ));
    assert!(matches!(
        application
            .answer_blocker(AnswerBlockerRequest {
                command_id: "one".into(),
                blocker_id: "blocker".into(),
                revision: 1,
                kind: BlockerAnswerKind::Custom,
                response: "answer".into(),
            })
            .await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
    assert!(prompts.try_recv().is_err());
}

/// Process-lifetime command caching partitions identical command IDs by session
/// while still deduplicating when the process returns to the original session.
#[tokio::test]
async fn scopes_shared_command_state_by_session_identity() {
    let (first, mut first_prompts, _) = application(true);
    let (second_prompts_tx, mut second_prompts) = mpsc::channel(4);
    let (second_blockers_tx, _) = mpsc::channel(4);
    let second = SwarmApplication::new(
        SessionIdentity::new(Hostname::new("host"), SessionId::new("other-session")),
        Arc::clone(&first.projection),
        Arc::clone(&first.changed),
        second_prompts_tx,
        second_blockers_tx,
    )
    .with_command_state(Arc::clone(&first.commands), Duration::from_millis(25));
    let request = prompt("same-id", "same payload");

    let first_delivery = first.deliver_prompt(request.clone());
    let first_accept = async {
        first_prompts
            .recv()
            .await
            .expect("first-session submission")
            .completion
            .send(Ok(()))
            .expect("first-session acceptance");
    };
    let (first_result, ()) = tokio::join!(first_delivery, first_accept);
    assert_eq!(
        first_result.expect("first result"),
        DeliverPromptResponse::Accepted
    );

    let second_delivery = second.deliver_prompt(request.clone());
    let second_accept = async {
        second_prompts
            .recv()
            .await
            .expect("second-session submission")
            .completion
            .send(Ok(()))
            .expect("second-session acceptance");
    };
    let (second_result, ()) = tokio::join!(second_delivery, second_accept);
    assert_eq!(
        second_result.expect("second result"),
        DeliverPromptResponse::Accepted
    );

    assert_eq!(
        first
            .deliver_prompt(request)
            .await
            .expect("cached first-session result"),
        DeliverPromptResponse::Accepted
    );
    assert!(
        matches!(
            first_prompts.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ),
        "return to first session must not submit the command again"
    );
}

/// Command-table rejection happens after reservation but rolls the blocker
/// back atomically before returning the admission rejection.
#[tokio::test]
async fn blocker_admission_failure_releases_answer_reservation() {
    let (application, mut blockers, history) = application_with_blocker(
        Duration::from_millis(10),
        CommandLimits {
            entries: 0,
            logical_bytes: 16 * 1024,
        },
        16 * 1024,
    )
    .await;
    assert!(matches!(
        application
            .answer_blocker(blocker_answer("admission"))
            .await,
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
    assert!(blockers.try_recv().is_err());
    assert_unreserved_active(&history);
}

/// Loopback send failure, canonical rejection, indeterminate completion, and
/// timeout all release the pending answer reservation at their transaction cut.
#[tokio::test]
async fn blocker_remote_failure_cuts_release_answer_reservation() {
    let limits = CommandLimits {
        entries: 8,
        logical_bytes: 16 * 1024,
    };

    let (application, blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, 16 * 1024).await;
    drop(blockers);
    assert!(
        application
            .answer_blocker(blocker_answer("send-failure"))
            .await
            .is_err()
    );
    assert_unreserved_active(&history);

    let (application, mut blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, 16 * 1024).await;
    let answer = tokio::spawn({
        let application = Arc::clone(&application);
        async move { application.answer_blocker(blocker_answer("rejected")).await }
    });
    blockers
        .recv()
        .await
        .expect("rejected submission")
        .completion
        .send(Err("rejected".into()))
        .expect("rejection receiver");
    assert!(matches!(
        answer.await.expect("answer task"),
        Ok(AnswerBlockerResponse::Rejected(_))
    ));
    assert_unreserved_active(&history);

    let (application, mut blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, 16 * 1024).await;
    let answer = tokio::spawn({
        let application = Arc::clone(&application);
        async move {
            application
                .answer_blocker(blocker_answer("indeterminate"))
                .await
        }
    });
    drop(blockers.recv().await.expect("indeterminate submission"));
    assert!(answer.await.expect("answer task").is_err());
    assert_unreserved_active(&history);

    let (application, mut blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, 16 * 1024).await;
    let answer = tokio::spawn({
        let application = Arc::clone(&application);
        async move { application.answer_blocker(blocker_answer("timeout")).await }
    });
    let submission = blockers.recv().await.expect("timed-out submission");
    assert!(answer.await.expect("answer task").is_err());
    drop(submission);
    assert_unreserved_active(&history);
}

/// Remote reservation sizes the exact current and prospective serialized owner
/// histories, accepts equality, rejects one byte less, and includes another
/// same-owner reservation without exposing it in either encoding.
#[tokio::test]
async fn blocker_remote_reservation_uses_exact_encoded_history_delta() {
    let request = blocker_answer("accounting");
    let current = vec![active_history_record("blocker")];
    let before = serde_json::to_vec(&current)
        .expect("current owner history encoding")
        .len();
    let mut prospective = current.clone();
    prospective[0].lifecycle.answer(
        request.response.clone(),
        tau_swarm_api::BlockerAnswerKind::Custom,
    );
    let after = serde_json::to_vec(&prospective)
        .expect("prospective owner history encoding")
        .len();
    let additional = after.checked_sub(before).expect("answer grows history");

    let limits = CommandLimits {
        entries: 8,
        logical_bytes: 16 * 1024,
    };
    let (application, _blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, after).await;
    assert_eq!(application.reserve_blocker_answer(&request), Ok(()));
    assert_eq!(
        history.lock().expect("history")[0]
            .lifecycle
            .pending_answer_bytes(),
        additional.max(1)
    );

    let (application, _blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, after - 1).await;
    assert_eq!(
        application.reserve_blocker_answer(&request),
        Err("blocker byte limit is full".into())
    );
    assert_unreserved_active(&history);

    let existing_reservation = 7;
    let mut current = vec![
        active_history_record("blocker"),
        BlockerRecord {
            lifecycle: BlockerLifecycle::Active {
                pending_answer_bytes: NonZeroUsize::new(existing_reservation),
            },
            ..active_history_record("other")
        },
    ];
    let before = serde_json::to_vec(&current)
        .expect("current owner history encoding")
        .len();
    current[0].lifecycle.answer(
        request.response.clone(),
        tau_swarm_api::BlockerAnswerKind::Custom,
    );
    let after = serde_json::to_vec(&current)
        .expect("prospective owner history encoding")
        .len();
    let required = after + existing_reservation;
    let (application, _blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, required).await;
    history.lock().expect("history").push(BlockerRecord {
        lifecycle: BlockerLifecycle::Active {
            pending_answer_bytes: NonZeroUsize::new(existing_reservation),
        },
        ..active_history_record("other")
    });
    let required = before + (after - before) + existing_reservation;
    assert_eq!(required, application.blocker_history_limits.encoded_bytes);
    assert_eq!(application.reserve_blocker_answer(&request), Ok(()));

    let (application, _blockers, history) =
        application_with_blocker(Duration::from_millis(10), limits, required - 1).await;
    history.lock().expect("history").push(BlockerRecord {
        lifecycle: BlockerLifecycle::Active {
            pending_answer_bytes: NonZeroUsize::new(existing_reservation),
        },
        ..active_history_record("other")
    });
    assert_eq!(
        application.reserve_blocker_answer(&request),
        Err("blocker byte limit is full".into())
    );
    assert_unreserved_active(&history);
    assert_eq!(
        history.lock().expect("history")[1]
            .lifecycle
            .pending_answer_bytes(),
        existing_reservation
    );
}

/// The extracted reservation arithmetic locks the otherwise unreachable
/// zero-delta, underflow, and checked-overflow cuts with exact diagnostics.
#[test]
fn blocker_remote_reservation_preserves_checked_arithmetic_cuts() {
    assert_eq!(
        answer_reservation_bytes(10, 10, 0, 10),
        Ok(NonZeroUsize::new(1).expect("nonzero"))
    );
    assert_eq!(
        answer_reservation_bytes(10, 9, 0, usize::MAX),
        Err("blocker byte accounting underflow".into())
    );
    assert_eq!(
        answer_reservation_bytes(usize::MAX, usize::MAX, 1, usize::MAX),
        Err("blocker byte accounting overflow".into())
    );
    assert_eq!(
        answer_reservation_bytes(usize::MAX - 1, usize::MAX, 0, usize::MAX),
        Ok(NonZeroUsize::new(1).expect("nonzero"))
    );
}

/// Canonical acceptance closes the projection and notifies observers while
/// history is still reserved-active, then commits the exhaustive Answered
/// state.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_acceptance_orders_projection_notification_before_history_commit() {
    let limits = CommandLimits {
        entries: 8,
        logical_bytes: 16 * 1024,
    };
    let (application, mut blockers, history) =
        application_with_blocker(Duration::from_secs(30), limits, 16 * 1024).await;
    let notified = application.changed.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let answer = tokio::spawn({
        let application = Arc::clone(&application);
        async move {
            application
                .answer_blocker(blocker_answer("accepted-order"))
                .await
        }
    });
    let submission = blockers.recv().await.expect("answer submission");
    let history_guard = history.lock().expect("hold history commit cut");
    assert!(matches!(
        history_guard[0].lifecycle,
        BlockerLifecycle::Active {
            pending_answer_bytes: Some(_)
        }
    ));
    submission
        .completion
        .send(Ok(()))
        .expect("canonical acceptance");
    notified.await;
    assert!(
        application
            .projection
            .lock()
            .await
            .blocker("blocker", 1)
            .is_none(),
        "projection must close before notification"
    );
    assert!(matches!(
        history_guard[0].lifecycle,
        BlockerLifecycle::Active {
            pending_answer_bytes: Some(_)
        }
    ));
    drop(history_guard);
    assert_eq!(
        answer.await.expect("answer task"),
        Ok(AnswerBlockerResponse::Accepted)
    );
    assert!(matches!(
        history.lock().expect("history")[0].lifecycle,
        BlockerLifecycle::Answered {
            ref answer,
            kind: tau_swarm_api::BlockerAnswerKind::Custom
        } if answer == "answer"
    ));
}

/// An indeterminate timeout is terminal in process memory and exact retry does
/// not submit Tau work again.
#[tokio::test]
async fn caches_indeterminate_prompt_timeout() {
    let (application, mut prompts, _) = application(true);
    let first = application.deliver_prompt(prompt("slow", "hello"));
    let submission = prompts.recv();
    let (result, held) = tokio::join!(first, submission);
    assert_eq!(
        result.expect_err("timeout is indeterminate").kind(),
        ErrorKind::IndeterminateTransport
    );
    let _held = held.expect("one submission keeps completion open");
    assert_eq!(
        application
            .deliver_prompt(prompt("slow", "hello"))
            .await
            .expect_err("cached timeout is indeterminate")
            .kind(),
        ErrorKind::IndeterminateTransport
    );
    assert!(prompts.try_recv().is_err());
}

/// Loss of the Tau submission owner after admission is indeterminate and its
/// exact retry replays that outcome without another side effect.
#[tokio::test]
async fn caches_submission_channel_disconnect() {
    let (application, prompts, _) = application(true);
    drop(prompts);
    let request = prompt("disconnected", "hello");
    assert_eq!(
        application
            .deliver_prompt(request.clone())
            .await
            .expect_err("channel loss is indeterminate")
            .kind(),
        ErrorKind::IndeterminateTransport
    );
    assert_eq!(
        application
            .deliver_prompt(request)
            .await
            .expect_err("cached channel loss")
            .kind(),
        ErrorKind::IndeterminateTransport
    );
}

/// Loss of canonical completion after Tau work was emitted is indeterminate,
/// cached, and never resubmitted on exact retry.
#[tokio::test]
async fn caches_post_admission_completion_loss() {
    let (application, mut prompts, _) = application(true);
    let request = prompt("emitted", "hello");
    let task = tokio::spawn({
        let application = Arc::clone(&application);
        let request = request.clone();
        async move { application.deliver_prompt(request).await }
    });
    let submission = prompts.recv().await.expect("emitted submission");
    drop(submission);
    assert_eq!(
        task.await
            .expect("command task")
            .expect_err("completion loss is indeterminate")
            .kind(),
        ErrorKind::IndeterminateTransport
    );
    assert_eq!(
        application
            .deliver_prompt(request)
            .await
            .expect_err("cached completion loss")
            .kind(),
        ErrorKind::IndeterminateTransport
    );
    assert!(prompts.try_recv().is_err());
}

/// Ensures blocker answers preserve arbitrary body text without allowing an
/// answer to terminate the protocol element early.
#[test]
fn blocker_answer_escapes_only_the_exact_closing_element_in_body() {
    assert_eq!(
        blocker_answer_xml("a&\"", 7, "custom", "before</blocker_answer>after"),
        "<blocker_answer blocker_id=\"a&amp;&quot;\" revision=\"7\" \
answer_kind=\"custom\">\nbefore&lt;/blocker_answer>after\n</blocker_answer>"
    );
}

/// Hermetic connector that fails selected generations before returning a
/// terminal-authentication test transport.
#[derive(Clone)]
struct FakeConnector {
    /// Connection attempts observed across retry generations.
    attempts: Arc<AtomicUsize>,
    /// Number of initial indeterminate failures before transport creation.
    transient_failures: usize,
}

#[async_trait]
impl Connector for FakeConnector {
    type Transport = RejectingTransport;

    async fn connect(&self, expected_peer: &ExpectedPeer) -> ClientResult<Self::Transport> {
        assert_eq!(expected_peer.as_bytes(), b"peer");
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < self.transient_failures {
            Err(ClientError::transport("temporary route failure"))
        } else {
            Ok(RejectingTransport)
        }
    }
}

/// Transport that inspects credentials and rejects authentication.
struct RejectingTransport;

#[async_trait]
impl SessionTransport for RejectingTransport {
    async fn authenticate(
        &self,
        request: AuthenticateRequest,
    ) -> ClientResult<AuthenticateResponse> {
        assert_eq!(request.credential.id.as_str(), "worker");
        assert_eq!(request.credential.secret.expose(), b"secret");
        Ok(AuthenticateResponse::Rejected("credential denied".into()))
    }

    async fn declare(
        &self,
        _request: DeclareSessionRequest,
    ) -> ClientResult<DeclareSessionResponse> {
        unreachable!("authentication rejection is terminal")
    }

    async fn submit_snapshot(
        &self,
        _request: SubmitSnapshotRequest,
    ) -> ClientResult<SubmitSnapshotResponse> {
        unreachable!("authentication rejection is terminal")
    }

    async fn submit_change(
        &self,
        _request: SubmitChangeRequest,
    ) -> ClientResult<SubmitChangeResponse> {
        unreachable!("authentication rejection is terminal")
    }

    async fn next_command(&self) -> ClientResult<IncomingCommand> {
        unreachable!("authentication rejection is terminal")
    }
}

/// Connector whose first generation fails after a live change while later
/// generations stay connected for resnapshot observation.
#[derive(Clone)]
struct ResnapshotConnector {
    /// Complete per-generation publication observations.
    observations: mpsc::UnboundedSender<ResnapshotObservation>,
    /// Allocates the fixture generation that owns each transport.
    generations: Arc<AtomicUsize>,
}

/// Exact client activity observed from one transport generation.
#[derive(Debug)]
enum ResnapshotObservation {
    /// Complete declaration and snapshot that synchronized a generation.
    Snapshot {
        /// Transport generation that submitted this snapshot.
        generation: usize,
        /// Session declaration preceding the observed snapshot.
        declaration: DeclareSessionRequest,
        /// Complete snapshot that synchronized the generation.
        snapshot: SubmitSnapshotRequest,
    },
    /// The generation has entered its live select after synchronizing.
    LiveWait {
        /// Transport generation waiting for commands and changes.
        generation: usize,
    },
    /// Indeterminate submission of the first live projection change.
    Change {
        /// Transport generation that submitted this change.
        generation: usize,
        /// Exact live change request that caused reconnect.
        request: SubmitChangeRequest,
    },
}

#[async_trait]
impl Connector for ResnapshotConnector {
    type Transport = ResnapshotTransport;

    async fn connect(&self, _expected_peer: &ExpectedPeer) -> ClientResult<Self::Transport> {
        Ok(ResnapshotTransport {
            declaration: StdMutex::new(None),
            observations: self.observations.clone(),
            generation: self.generations.fetch_add(1, Ordering::SeqCst),
        })
    }
}

/// Transport that reports its initial synchronization and first live
/// submission.
struct ResnapshotTransport {
    /// Declaration retained until this generation submits its snapshot.
    declaration: StdMutex<Option<DeclareSessionRequest>>,
    /// Sends complete publication observations to the test.
    observations: mpsc::UnboundedSender<ResnapshotObservation>,
    /// Connection generation allocated by the fixture connector.
    generation: usize,
}

#[async_trait]
impl SessionTransport for ResnapshotTransport {
    async fn authenticate(
        &self,
        _request: AuthenticateRequest,
    ) -> ClientResult<AuthenticateResponse> {
        assert_eq!(
            CURRENT_PROTOCOL_VERSION,
            tau_swarm_client_api::ProtocolVersion(0)
        );
        Ok(AuthenticateResponse::Accepted)
    }

    async fn declare(
        &self,
        request: DeclareSessionRequest,
    ) -> ClientResult<DeclareSessionResponse> {
        *self.declaration.lock().expect("declaration") = Some(request);
        Ok(DeclareSessionResponse::Accepted)
    }

    async fn submit_snapshot(
        &self,
        request: SubmitSnapshotRequest,
    ) -> ClientResult<SubmitSnapshotResponse> {
        self.observations
            .send(ResnapshotObservation::Snapshot {
                generation: self.generation,
                declaration: self
                    .declaration
                    .lock()
                    .expect("declaration")
                    .clone()
                    .expect("declare before snapshot"),
                snapshot: request,
            })
            .expect("test receives snapshot");
        Ok(SubmitSnapshotResponse::Accepted)
    }

    async fn submit_change(
        &self,
        request: SubmitChangeRequest,
    ) -> ClientResult<SubmitChangeResponse> {
        self.observations
            .send(ResnapshotObservation::Change {
                generation: self.generation,
                request,
            })
            .expect("test receives live change");
        Err(ClientError::transport("generation disconnected"))
    }

    async fn next_command(&self) -> ClientResult<IncomingCommand> {
        self.observations
            .send(ResnapshotObservation::LiveWait {
                generation: self.generation,
            })
            .expect("test observes live wait");
        std::future::pending::<ClientResult<IncomingCommand>>().await
    }
}

/// The real Swarm client retries indeterminate connection failure, preserves
/// peer pinning and credentials across generations, then stops on authoritative
/// authentication rejection.
#[tokio::test]
async fn swarm_transport_reconnects_only_indeterminate_failures() {
    let (application, _, _) = application(true);
    let attempts = Arc::new(AtomicUsize::new(0));
    let connector = FakeConnector {
        attempts: Arc::clone(&attempts),
        transient_failures: 1,
    };
    let client = tau_swarm_client::Client::new(
        application,
        incarnation(1),
        connector,
        ExpectedPeer::new(b"peer".to_vec()),
        Credential {
            id: CredentialId::new("worker"),
            secret: Secret::new(b"secret"),
        },
        Backoff::new(Duration::from_millis(1), Duration::from_millis(1), 0, 1),
    );
    let error = client.run().await.expect_err("authentication is terminal");
    assert_eq!(error.kind(), ErrorKind::Rejected);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

/// An indeterminate live task-info submission reconnects and converges through
/// a fresh complete snapshot retained by the same extension process.
#[tokio::test]
async fn synchronized_reconnect_installs_fresh_snapshot() {
    let (application, _, _) = application(true);
    let expected_before = SubmitSnapshotRequest {
        snapshot: application
            .snapshot()
            .await
            .expect("initial snapshot")
            .snapshot
            .into(),
    };
    let (observations_tx, mut observations) = mpsc::unbounded_channel();
    let application_incarnation_id = incarnation(1);
    let client = tau_swarm_client::Client::new(
        Arc::clone(&application),
        application_incarnation_id.clone(),
        ResnapshotConnector {
            observations: observations_tx,
            generations: Arc::new(AtomicUsize::new(0)),
        },
        ExpectedPeer::new(b"peer".to_vec()),
        Credential {
            id: CredentialId::new("worker"),
            secret: Secret::new(b"secret"),
        },
        Backoff::new(Duration::from_millis(1), Duration::from_millis(1), 0, 1),
    );
    let task = tokio::spawn(client.run());
    tokio::time::timeout(Duration::from_secs(2), async {
        let ResnapshotObservation::Snapshot {
            generation,
            declaration,
            snapshot,
        } = observations.recv().await.expect("first observation")
        else {
            panic!("first observation must be a snapshot");
        };
        assert_eq!(generation, 0);
        assert_eq!(snapshot, expected_before);
        assert_eq!(
            declaration.application_incarnation_id,
            application_incarnation_id.clone().into()
        );
        assert!(matches!(
            observations.recv().await.expect("first live wait"),
            ResnapshotObservation::LiveWait { generation: 0 }
        ));
        let task_info = TaskInfo {
            task_id: TaskId::new("task"),
            title: TaskTitle::new("Current title").expect("task title"),
            description: None,
        };
        application
            .projection
            .lock()
            .await
            .upsert_task_info(task_info.clone())
            .expect("new projection head");
        let expected_after = SubmitSnapshotRequest {
            snapshot: application
                .snapshot()
                .await
                .expect("mutated snapshot")
                .snapshot
                .into(),
        };
        application.changed.notify_waiters();
        let ResnapshotObservation::Change {
            generation,
            request,
        } = observations
            .recv()
            .await
            .expect("indeterminate live change")
        else {
            panic!("second observation must be a live change");
        };
        assert_eq!(generation, 0);
        assert_eq!(
            request,
            SubmitChangeRequest {
                sequence: 1,
                change: SessionChange::UpsertTaskInfo(task_info).into(),
            }
        );
        let ResnapshotObservation::Snapshot {
            generation,
            declaration,
            snapshot,
        } = observations.recv().await.expect("second snapshot")
        else {
            panic!("third observation must be a snapshot");
        };
        assert_eq!(generation, 1);
        assert_eq!(snapshot, expected_after);
        assert_eq!(
            declaration.application_incarnation_id,
            application_incarnation_id.into()
        );
    })
    .await
    .expect("two synchronized generations");
    task.abort();
    let _ = task.await;
}

/// Exact published 0.2.0 server/core and Iroh crates exercise the successful
/// vertical path through authentication, declaration, snapshot publication,
/// remote prompt dispatch, and canonical Tau acceptance completion.
#[tokio::test]
async fn published_swarm_server_delivers_prompt_through_application_loopback() {
    let server_endpoint = iroh::Endpoint::builder(path_iroh_endpoint::presets::Minimal)
        .bind()
        .await
        .expect("server endpoint");
    let credential = Credential {
        id: CredentialId::new("worker"),
        secret: Secret::new(b"secret"),
    };
    let server = Server::spawn(
        server_endpoint,
        Credentials::single(credential.clone()),
        tau_swarm_core::CoreService::new(()),
    );
    let client_endpoint = iroh::Endpoint::builder(path_iroh_endpoint::presets::Minimal)
        .bind()
        .await
        .expect("client endpoint");
    let connector = IrohConnector::new(client_endpoint.clone(), server.addr());
    let (initial_application, mut prompts, _) = application(true);
    initial_application
        .projection
        .lock()
        .await
        .upsert_task_info(TaskInfo {
            task_id: TaskId::new("restart-ephemeral"),
            title: TaskTitle::new("Lost on restart").expect("task title"),
            description: None,
        })
        .expect("initial task metadata");
    let client = tau_swarm_client::Client::new(
        initial_application,
        incarnation(1),
        connector,
        ExpectedPeer::new(server.addr().id.as_bytes()),
        credential.clone(),
        Backoff::new(Duration::from_millis(1), Duration::from_millis(2), 0, 1),
    );
    let client_task = tokio::spawn(client.run());
    let session = SessionIdentity::new(Hostname::new("host"), SessionId::new("session"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !server.view().snapshot().sessions.iter().any(|view| {
            view.identity == session
                && view.connection == tau_swarm_core::ConnectionState::Synchronized
                && view.agents.contains_key(&AgentId::new("agent"))
                && view
                    .task_info
                    .contains_key(&TaskId::new("restart-ephemeral"))
        }) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("published session");

    let commands = server.commands();
    let dispatch = commands.prompt(
        &session,
        PromptRequest {
            correlation_id: CorrelationId::new("remote"),
            agent_id: AgentId::new("agent"),
            message: "continue".into(),
        },
    );
    let accept = async {
        let submission = prompts.recv().await.expect("Tau prompt submission");
        assert_eq!(submission.agent_id, AgentId::new("agent"));
        assert_eq!(submission.ctx_id, "remote");
        assert_eq!(submission.text, "continue");
        submission
            .completion
            .send(Ok(()))
            .expect("canonical Tau acceptance");
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(dispatch, accept)
    })
    .await
    .expect("bounded remote prompt");
    assert_eq!(outcome.expect("remote dispatch"), DeliveryOutcome::Accepted);

    let ambiguous = PromptRequest {
        correlation_id: CorrelationId::new("ambiguous-before-restart"),
        agent_id: AgentId::new("agent"),
        message: "maybe delivered".into(),
    };
    let dispatch = commands.prompt(&session, ambiguous.clone());
    let lose_result = async {
        let submission = prompts.recv().await.expect("ambiguous Tau submission");
        drop(submission.completion);
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(dispatch, lose_result)
    })
    .await
    .expect("bounded ambiguous prompt");
    assert!(matches!(
        outcome.expect("ambiguous dispatch result"),
        DeliveryOutcome::Indeterminate(_)
    ));

    client_task.abort();
    let _ = client_task.await;
    client_endpoint.close().await;

    let replacement_endpoint = iroh::Endpoint::builder(path_iroh_endpoint::presets::Minimal)
        .bind()
        .await
        .expect("replacement endpoint");
    let replacement_connector = IrohConnector::new(replacement_endpoint.clone(), server.addr());
    let (replacement_application, mut replacement_prompts, _) = application(true);
    let replacement_client = tau_swarm_client::Client::new(
        replacement_application,
        incarnation(2),
        replacement_connector,
        ExpectedPeer::new(server.addr().id.as_bytes()),
        credential,
        Backoff::new(Duration::from_millis(1), Duration::from_millis(2), 0, 1),
    );
    let replacement_task = tokio::spawn(replacement_client.run());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !server.view().snapshot().sessions.iter().any(|view| {
            view.identity == session
                && view.application_incarnation_id == incarnation(2)
                && view.connection == tau_swarm_core::ConnectionState::Synchronized
                && view.task_info.is_empty()
        }) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("replacement incarnation synchronized");

    assert!(matches!(
        commands.prompt(&session, ambiguous).await,
        Err(tau_swarm_iroh::CommandError::Core(
            tau_swarm_core::CoreError::Conflict(_)
        ))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), replacement_prompts.recv())
            .await
            .is_err(),
        "old ambiguous prompt must not enter replacement Tau application"
    );

    replacement_task.abort();
    let _ = replacement_task.await;
    replacement_endpoint.close().await;
    server.shutdown().await.expect("server shutdown");
}
