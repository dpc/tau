//! Narrow production-scheduler bridge for harness accounting acceptance tests.

use std::io::BufReader;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Weak, mpsc};
use std::thread;

use tau_proto::{HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter};

use super::*;

/// Runs real worker-return, retry-policy, delayed-scheduler, and report-writing
/// paths with a fake finite backend and virtual time. No network work occurs.
///
/// Only the private fixture copy of the model route changes; report correlation
/// and terminal content come from the caller's real harness-owned prompt.
pub fn standalone_compaction_retry_frames(
    mut prompt: tau_proto::AgentPromptCreated,
    failures: u32,
    terminal: tau_proto::ProviderResponseFinishedReport,
) -> Vec<HarnessInputMessage> {
    assert_eq!(
        prompt.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    assert!(failures < u32::MAX);
    assert_eq!(terminal.agent_prompt_id, prompt.agent_prompt_id);
    prompt.model = ModelId::new(
        ProviderName::new(CHATGPT_PROVIDER_NAME),
        ModelName::new("gpt-5.6-sol"),
    );
    let executor: PromptExecutor = Arc::new(move |execution| {
        if execution.job.retry_state.attempts < u64::from(failures) {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("return finite failed attempt");
        } else {
            assert_eq!(execution.job.retry_state.attempts, u64::from(failures));
            execution
                .frame_writer()
                .send_report(HarnessInputMessage::emit_transient(
                    Event::ProviderResponseFinishedReported(terminal.clone()),
                ))
                .expect("emit successful terminal");
        }
    });
    let profiles = profiles_with_chatgpt_auth(OpenAiAuth {
        access_token: "fixture-access".to_owned(),
        refresh_token: String::new(),
        expires_at_ms: u64::MAX,
        account_id: Some("fixture-account".to_owned()),
    });
    let clock = Arc::new(VirtualRetryClock {
        now: Mutex::new(Instant::now()),
        scheduler: Mutex::new(None),
    });
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let (peer, runtime) = UnixStream::pair().expect("fixture socket pair");
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("fixture timeout");
    let runtime_reader = runtime.try_clone().expect("runtime reader");
    let worker = thread::spawn(move || {
        let reload = profiles.clone();
        run_inner_with_executors_and_clock(
            runtime_reader,
            runtime,
            profiles,
            move |_| reload.clone(),
            1,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .map_err(|error| error.to_string())
    });
    let mut writer = HarnessOutputWriter::new(peer.try_clone().expect("fixture writer"));
    writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix: None,
            config: tau_proto::CborValue::Map(Vec::new()),
            instance_name: tau_proto::ExtensionName::parse("retry-fixture").expect("fixture name"),
            state_dir: None,
            secrets: Default::default(),
            absent_optional_secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure fixture");
    writer
        .write_message(&HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(1),
            Event::AgentPromptCreated(prompt),
        ))
        .expect("dispatch fixture");
    writer.flush().expect("flush fixture");
    let mut reader =
        HarnessInputReader::new(BufReader::new(peer.try_clone().expect("fixture reader")));
    let mut frames = Vec::new();
    loop {
        let frame = reader
            .read_message()
            .expect("read fixture frame")
            .expect("terminal before EOF");
        let terminal = if let HarnessInputMessage::Emit(emit) = &frame {
            if let Event::ProviderResponseUpdatedReported(updated) = emit.event.as_ref()
                && let Some(retry) = updated
                    .status
                    .as_ref()
                    .and_then(|status| status.retry.as_ref())
            {
                clock.advance(Duration::from_secs(
                    u64::from(retry.next_retry_delay_secs) + 1,
                ));
            }
            matches!(
                emit.event.as_ref(),
                Event::ProviderResponseFinishedReported(_)
            )
        } else {
            false
        };
        frames.push(frame);
        if terminal {
            break;
        }
    }
    peer.shutdown(Shutdown::Write).expect("fixture EOF");
    worker
        .join()
        .expect("fixture runtime joins")
        .expect("fixture runtime succeeds");
    frames
}

/// Explicit clock keeps even seventy long retry bursts independent of wall
/// time.
struct VirtualRetryClock {
    /// Current monotonic fixture instant.
    now: Mutex<Instant>,
    /// Actor wake channel attached during runtime startup.
    scheduler: Mutex<Option<Weak<SchedulerCommandSender>>>,
}

impl VirtualRetryClock {
    /// Advances the clock and waits for the production actor to observe it.
    fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("virtual time");
        *now = now
            .checked_add(duration)
            .expect("representable virtual time");
        drop(now);
        let scheduler = self
            .scheduler
            .lock()
            .expect("scheduler")
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("live retry scheduler");
        let (acknowledged, ack) = mpsc::sync_channel(0);
        scheduler
            .send(SchedulerCommand::Wake {
                acknowledged: Some(acknowledged),
            })
            .expect("wake scheduler");
        ack.recv_timeout(Duration::from_secs(10))
            .expect("scheduler observed clock");
    }
}

impl RetryClock for VirtualRetryClock {
    fn now(&self) -> Instant {
        *self.now.lock().expect("virtual time")
    }

    fn attach_scheduler(&self, commands: Weak<SchedulerCommandSender>) {
        *self.scheduler.lock().expect("scheduler") = Some(commands);
    }
}
