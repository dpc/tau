//! Provider worker report measurement and queue admission.

use super::*;

/// Direct typed report destination for one finite prompt attempt.
pub(super) struct WorkerReportSink<W = ManualRuntimeWaker> {
    /// Channel carrying admitted reports to the provider main loop.
    pub(super) tx: Sender<WorkerMessage>,
    /// Wake handle signaled after the report is queued.
    pub(super) waker: W,
    /// Shared typed-output queue depth for private observations.
    pub(super) worker_output_depth: Option<Arc<WorkerQueueState>>,
    /// Global-cancel generation captured synchronously at dispatch.
    pub(super) cancel_generation: u64,
    /// Prompt identity used for targeted-cancel commit validation.
    pub(super) agent_prompt_id: tau_proto::AgentPromptId,
    /// Exact cooldown probe authority attached to this finite attempt.
    pub(super) cooldown_probe: Option<CooldownProbe>,
}

/// Wake capability used after a typed worker report becomes channel-visible.
pub(super) trait WorkerReportWaker {
    /// Wake the provider main loop.
    fn wake_provider_loop(&self);
}

impl WorkerReportWaker for ManualRuntimeWaker {
    fn wake_provider_loop(&self) {
        self.wake();
    }
}

impl<W: WorkerReportWaker> ProviderReportSink for WorkerReportSink<W> {
    fn send_report(&mut self, message: HarnessInputMessage) -> ClientResult<()> {
        self.send_report_inner(message, None)
    }

    fn send_sampled_report(
        &mut self,
        message: HarnessInputMessage,
        observation: Option<SamplerObservation>,
    ) -> ClientResult<()> {
        self.send_report_inner(message, observation)
    }
}

impl<W: WorkerReportWaker> WorkerReportSink<W> {
    /// Measure, admit, and wake one typed report without changing wire
    /// semantics.
    fn send_report_inner(
        &mut self,
        message: HarnessInputMessage,
        sampler: Option<SamplerObservation>,
    ) -> ClientResult<()> {
        let observation_started =
            output_cost_observation::worker_measurement_start(self.worker_output_depth.as_ref());
        let output = match prepare_worker_report(message) {
            Ok(output) => output,
            Err(error) => {
                if let Some(sampler) = sampler {
                    sampler.finish("worker_measure_rejected");
                }
                return Err(error);
            }
        };
        let provider_correlation = sampler.map(|sampler| sampler.finish("worker_measure_started"));
        let frame_bytes = output.encoded_bytes();
        let send = |output_cost: Option<WorkerOutputObservation>| {
            let admission = output_cost.as_ref().map(WorkerOutputObservation::admission);
            send_observed_worker_output(
                &self.tx,
                WorkerMessage::Output {
                    output,
                    output_cost,
                    cancel_generation: self.cancel_generation,
                    agent_prompt_id: self.agent_prompt_id.clone(),
                    cooldown_probe: self.cooldown_probe.clone(),
                },
                admission,
            )
        };
        let send_result = if let Some(state) = &self.worker_output_depth {
            let _admission = state
                .admission
                .lock()
                .expect("worker output admission lock");
            let output_cost = WorkerOutputObservation::pending(
                Some(state),
                provider_correlation.unwrap_or_else(output_cost_observation::next_correlation),
                observation_started,
                frame_bytes,
            );
            send(output_cost)
        } else {
            send(None)
        };
        if send_result.is_err() {
            return Err(path_std_io::Error::from(path_std_io::ErrorKind::BrokenPipe).into());
        }
        self.waker.wake_provider_loop();
        Ok(())
    }
}

/// Send one output while closing enabled queue ownership on disconnection.
fn send_observed_worker_output(
    tx: &Sender<WorkerMessage>,
    message: WorkerMessage,
    admission: Option<output_cost_observation::WorkerQueueAdmission>,
) -> Result<(), ()> {
    match tx.send(message) {
        Ok(()) => {
            if let Some(admission) = admission {
                admission.admitted();
            }
            Ok(())
        }
        Err(error) => {
            if let Some(admission) = admission {
                admission.rejected();
            }
            if let WorkerMessage::Output {
                output_cost: Some(observation),
                ..
            } = error.0
            {
                observation.finish("queue_closed");
            }
            Err(())
        }
    }
}

/// Check the old worker encoder's validity boundary without moving the final
/// frame-size admission ahead of main-loop cancellation arbitration.
pub(super) fn prepare_worker_report(
    message: HarnessInputMessage,
) -> ClientResult<tau_client::PeerOutput> {
    let output = tau_client::PeerOutput::prepare(message)?;
    if tau_proto::MAX_PROTOCOL_MESSAGE_BYTES < output.encoded_bytes() {
        return Err(path_std_io::Error::new(
            path_std_io::ErrorKind::InvalidData,
            format!(
                "protocol message exceeds {} byte limit",
                tau_proto::MAX_PROTOCOL_MESSAGE_BYTES
            ),
        )
        .into());
    }
    Ok(output)
}
