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
