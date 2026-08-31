use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_client::{
    ClientError, ExtensionBuilder, RawEventContext, TauExtension, TauExtensionRunner,
};
use tau_proto::{Event, EventSelector, HarnessOutputMessage, HarnessOutputWriter, TermBell};
use tau_swarm_api::{
    Agent, AgentActivity, AgentId, AgentNavigationMode, AgentWorkStatus, Hostname, SessionId,
    SessionIdentity,
};
use tau_swarm_client::{Application, ErrorKind};
use tau_swarm_client_api::DeliverPromptRequest;
use tau_swarm_client_api::v0::PromptRequest;
use tokio::sync::{Mutex as TokioMutex, Notify, mpsc as path_tokio_mpsc};

use super::super::*;
use crate::application::{CommandLimits, SwarmApplication};
use crate::projection::{ProjectionLimits, SessionProjection};

/// Extension state that bridges queued Swarm prompts through production
/// detached submission.
struct SaturationState {
    /// Prompt submissions waiting for Tau publication.
    prompts: path_tokio_mpsc::Receiver<PromptSubmission>,
    /// Exact canonical loopbacks retained after admission.
    pending: Arc<Mutex<HashMap<PendingKey, Completion>>>,
    /// Number of queued commands consumed by the trigger.
    count: usize,
}

/// Test extension that invokes the production prompt bridge.
struct SaturationExtension;

impl TauExtension for SaturationExtension {
    type State = SaturationState;

    fn name(&self) -> &'static str {
        "swarm-saturation-test"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.on_raw_live(
            EventSelector::Exact(tau_proto::EventName::TERM_BELL),
            |cx: RawEventContext<'_, SaturationState>| {
                for _ in 0..cx.state.count {
                    let submission = cx
                        .state
                        .prompts
                        .blocking_recv()
                        .ok_or_else(|| ClientError::handler("prompt fixture stopped"))?;
                    submit_loopback(
                        submission.agent_id,
                        submission.text,
                        submission.ctx_id,
                        submission.completion,
                        &cx.handle,
                        &cx.state.pending,
                    );
                }
                Ok(())
            },
        );
    }
}

/// Output sink that blocks the first production internal-prompt frame.
struct PromptSaturationWriter {
    /// Gate released after overload observation.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Announces ownership of the first prompt frame.
    entered: mpsc::Sender<()>,
    /// Prevents repeated announcements.
    announced: bool,
}

impl Write for PromptSaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let needle = b"extension.internal_prompt_submit_request";
        if !self.announced && bytes.windows(needle.len()).any(|window| window == needle) {
            self.announced = true;
            self.entered.send(()).expect("announce blocked writer");
            let (lock, condvar) = &*self.gate;
            let mut blocked = lock.lock().expect("prompt writer gate");
            while *blocked {
                blocked = condvar.wait(blocked).expect("wait prompt writer gate");
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Panic-safe owner that releases the writer and bounds completion waiting.
struct SaturationRunner {
    /// Gate opened before waiting for completion.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Runner completion; watchdog failure deliberately leaves it detached.
    completion: mpsc::Receiver<Result<(), String>>,
}

impl SaturationRunner {
    /// Releases the writer and waits within the cleanup watchdog.
    fn finish(self) -> Result<(), String> {
        self.release();
        self.completion
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "saturation runner cleanup timed out".to_owned())?
    }

    /// Opens the writer gate.
    fn release(&self) {
        let (lock, condvar) = &*self.gate;
        *lock.lock().expect("prompt writer gate") = false;
        condvar.notify_all();
    }
}

impl Drop for SaturationRunner {
    fn drop(&mut self) {
        self.release();
        let _ = self.completion.recv_timeout(Duration::from_secs(30));
    }
}

fn prompt(id: &str) -> DeliverPromptRequest {
    DeliverPromptRequest {
        prompt: PromptRequest {
            correlation_id: id.into(),
            agent_id: "agent".into(),
            message: "body".into(),
        },
    }
}

/// Saturating the real detached FIFO caches an indeterminate result and cleanup
/// remains bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_prompt_saturation_caches_indeterminate_and_cleans_up() {
    const COMMANDS: usize = 96;
    let mut projection = SessionProjection::new(ProjectionLimits {
        history_entries: 16,
        ..ProjectionLimits::unconfigured()
    });
    projection
        .upsert_agent(Agent {
            id: AgentId::new("agent"),
            name: "Agent".into(),
            activity: AgentActivity::Waiting,
            navigation_mode: AgentNavigationMode::Active,
            watches: BTreeSet::new(),
            work_status: AgentWorkStatus::Unreported,
        })
        .expect("agent");
    let (prompt_tx, prompt_rx) = path_tokio_mpsc::channel(COMMANDS);
    let (blocker_tx, _blocker_rx) = path_tokio_mpsc::channel(1);
    let application = Arc::new(
        SwarmApplication::new(
            SessionIdentity::new(Hostname::new("host"), SessionId::new("session")),
            Arc::new(TokioMutex::new(projection)),
            Arc::new(Notify::new()),
            prompt_tx,
            blocker_tx,
        )
        .with_command_policy(
            Duration::from_secs(30),
            CommandLimits {
                entries: COMMANDS,
                logical_bytes: 16 * 1024 * 1024,
            },
        ),
    );
    let (result_tx, result_rx) = mpsc::channel();
    let mut commands = Vec::new();
    for index in 0..COMMANDS {
        let application = Arc::clone(&application);
        let result_tx = result_tx.clone();
        let id = format!("saturated-{index}");
        commands.push(tokio::spawn(async move {
            let result = application.deliver_prompt(prompt(&id)).await;
            result_tx.send((id, result)).expect("result");
        }));
    }
    drop(result_tx);
    let state = SaturationState {
        prompts: prompt_rx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        count: COMMANDS,
    };
    let mut input = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            config: tau_proto::CborValue::Null,
            instance_name: tau_proto::ExtensionName::parse("swarm-saturation-test").expect("name"),
            tool_prefix: None,
            state_dir: None,
            secrets: BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    input_writer
        .write_message(&HarnessOutputMessage::deliver(Event::TermBell(TermBell {})))
        .expect("trigger");
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let writer = PromptSaturationWriter {
        gate: Arc::clone(&gate),
        entered: entered_tx,
        announced: false,
    };
    let (completion_tx, completion_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = TauExtensionRunner::new(SaturationExtension)
            .run(Cursor::new(input), writer, state)
            .map(|_| ())
            .map_err(|error| error.to_string());
        let _ = completion_tx.send(result);
    });
    let runner = SaturationRunner {
        gate,
        completion: completion_rx,
    };
    entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("bounded blocked-writer coordination");
    let (overloaded_id, overloaded_result) = result_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("bounded overload result");
    let error = overloaded_result.expect_err("indeterminate");
    assert_eq!(error.kind(), ErrorKind::IndeterminateTransport);
    assert!(
        error
            .to_string()
            .contains("prompt acceptance became indeterminate")
    );
    let cached = application
        .deliver_prompt(prompt(&overloaded_id))
        .await
        .expect_err("cached indeterminate");
    assert_eq!(cached.kind(), ErrorKind::IndeterminateTransport);
    runner.finish().expect("bridge cleanup");
    for command in commands {
        tokio::time::timeout(Duration::from_secs(30), command)
            .await
            .expect("bounded cleanup")
            .expect("command cleanup");
    }
}
