use std::io::Write;

/// Typed destination for one provider-to-harness protocol report.
///
/// The ordinary wire implementation preserves the existing encode-and-flush
/// boundary. The provider runtime implements the same boundary with an owned
/// message channel after checking that the typed value can be encoded. The
/// main-loop [`tau_client::ClientHandle`] still owns final frame-size admission
/// after cancellation arbitration.
pub(crate) trait ProviderReportSink {
    /// Submit and flush one complete provider report.
    ///
    /// # Errors
    ///
    /// Returns an error when the report cannot be encoded, admitted, forwarded,
    /// or flushed.
    fn send_report(
        &mut self,
        message: tau_proto::HarnessInputMessage,
    ) -> tau_client::ClientResult<()>;

    /// Submit one report carrying enabled-only sampler phase state.
    fn send_sampled_report(
        &mut self,
        message: tau_proto::HarnessInputMessage,
        observation: Option<crate::output_cost_observation::SamplerObservation>,
    ) -> tau_client::ClientResult<()> {
        let result = self.send_report(message);
        if let Some(observation) = observation {
            observation.finish(if result.is_ok() {
                "direct_written"
            } else {
                "direct_rejected"
            });
        }
        result
    }
}

impl<W: Write> ProviderReportSink for tau_proto::PeerOutputWriter<W> {
    fn send_report(
        &mut self,
        message: tau_proto::HarnessInputMessage,
    ) -> tau_client::ClientResult<()> {
        self.write_message(&message)?;
        self.flush()?;
        Ok(())
    }
}
