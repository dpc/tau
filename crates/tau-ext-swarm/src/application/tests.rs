use std::collections::BTreeSet;
use std::sync as path_std_sync;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use iroh::endpoint as path_iroh_endpoint;
use tau_swarm_api::{
    Agent, AgentActivity, AgentNavigationMode, AgentWorkStatus, ApplicationIncarnationId,
    CorrelationId, DeliveryOutcome, Hostname, PromptRequest, SessionId,
};
use tau_swarm_client::{
    Backoff, Connector, ErrorKind, ExpectedPeer, IncomingCommand, SessionTransport,
};
use tau_swarm_client_api::v4::{BlockerAnswerKind, PromptRequest as WirePromptRequest};
use tau_swarm_client_api::{
    AuthenticateRequest, AuthenticateResponse, CURRENT_PROTOCOL_VERSION, Credential, CredentialId,
    DeclareSessionRequest, DeclareSessionResponse, Secret, SubmitChangeRequest,
    SubmitChangeResponse, SubmitSnapshotRequest, SubmitSnapshotResponse,
};
use tau_swarm_iroh::{Credentials, IrohConnector, Server};

use super::*;

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
    let mut projection = SessionProjection::new(16);
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
    .with_command_policy(Duration::from_millis(25), 8, 16 * 1024);
    (Arc::new(application), prompt_rx, blocker_rx)
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
            .with_command_policy(Duration::from_millis(25), 8, maximum - 1),
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
        tokio::time::timeout(Duration::from_millis(10), first_prompts.recv())
            .await
            .is_err(),
        "return to first session must not submit the command again"
    );
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

/// Connector whose generations synchronize then fail indeterminately.
#[derive(Clone)]
struct ResnapshotConnector {
    /// Exact declarations observed across connection generations.
    declarations: Arc<std::sync::Mutex<Vec<DeclareSessionRequest>>>,
    /// Exact snapshots observed across connection generations.
    snapshots: Arc<std::sync::Mutex<Vec<SubmitSnapshotRequest>>>,
    /// Releases the first synchronized generation after test mutation.
    disconnect: Arc<Notify>,
}

#[async_trait]
impl Connector for ResnapshotConnector {
    type Transport = ResnapshotTransport;

    async fn connect(&self, _expected_peer: &ExpectedPeer) -> ClientResult<Self::Transport> {
        Ok(ResnapshotTransport {
            declarations: Arc::clone(&self.declarations),
            snapshots: Arc::clone(&self.snapshots),
            disconnect: Arc::clone(&self.disconnect),
        })
    }
}

/// Successful transport that drops after installing each snapshot.
struct ResnapshotTransport {
    /// Cross-generation exact declaration requests.
    declarations: Arc<std::sync::Mutex<Vec<DeclareSessionRequest>>>,
    /// Cross-generation exact snapshot requests.
    snapshots: Arc<std::sync::Mutex<Vec<SubmitSnapshotRequest>>>,
    /// First-generation disconnect gate.
    disconnect: Arc<Notify>,
}

#[async_trait]
impl SessionTransport for ResnapshotTransport {
    async fn authenticate(
        &self,
        _request: AuthenticateRequest,
    ) -> ClientResult<AuthenticateResponse> {
        Ok(AuthenticateResponse::Accepted(CURRENT_PROTOCOL_VERSION))
    }

    async fn declare(
        &self,
        request: DeclareSessionRequest,
    ) -> ClientResult<DeclareSessionResponse> {
        self.declarations
            .lock()
            .expect("declarations")
            .push(request);
        Ok(DeclareSessionResponse::Accepted)
    }

    async fn submit_snapshot(
        &self,
        request: SubmitSnapshotRequest,
    ) -> ClientResult<SubmitSnapshotResponse> {
        self.snapshots.lock().expect("snapshots").push(request);
        Ok(SubmitSnapshotResponse::Accepted)
    }

    async fn submit_change(
        &self,
        _request: SubmitChangeRequest,
    ) -> ClientResult<SubmitChangeResponse> {
        Err(ClientError::transport("generation disconnected"))
    }

    async fn next_command(&self) -> ClientResult<IncomingCommand> {
        if self.snapshots.lock().expect("snapshots").len() == 1 {
            self.disconnect.notified().await;
        } else {
            std::future::pending::<()>().await;
        }
        Err(ClientError::transport("generation disconnected"))
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

/// A transport loss after synchronization establishes a new generation and
/// installs a fresh complete snapshot before serving it.
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
    let snapshots = Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let declarations = Arc::new(path_std_sync::Mutex::new(Vec::new()));
    let disconnect = Arc::new(Notify::new());
    let application_incarnation_id = incarnation(1);
    let client = tau_swarm_client::Client::new(
        Arc::clone(&application),
        application_incarnation_id.clone(),
        ResnapshotConnector {
            declarations: Arc::clone(&declarations),
            snapshots: Arc::clone(&snapshots),
            disconnect: Arc::clone(&disconnect),
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
        while snapshots.lock().expect("snapshots").is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("first synchronized generation");
    application
        .projection
        .lock()
        .await
        .upsert_agent(Agent {
            id: AgentId::new("second"),
            name: "Second".into(),
            activity: AgentActivity::Running,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("new projection head");
    let expected_after = SubmitSnapshotRequest {
        snapshot: application
            .snapshot()
            .await
            .expect("mutated snapshot")
            .snapshot
            .into(),
    };
    disconnect.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while snapshots.lock().expect("snapshots").len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("second synchronized generation");
    {
        let snapshots = snapshots.lock().expect("snapshots");
        assert_eq!(snapshots.as_slice(), [expected_before, expected_after]);
    }
    {
        let declarations = declarations.lock().expect("declarations");
        assert_eq!(declarations.len(), 2);
        assert!(declarations.iter().all(|request| {
            request.application_incarnation_id == application_incarnation_id.clone().into()
        }));
    }
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
