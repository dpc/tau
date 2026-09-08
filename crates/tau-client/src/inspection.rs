//! Opt-in bootstrap that selects purpose before constructing runtime state.

use std::io::{BufReader, Read, Write};

use crate::{ClientError, ClientResult, TauExtension, TauExtensionRunner};

#[cfg(test)]
mod tests;

/// An admitted ordinary connection returned by declaration-aware bootstrap.
///
/// Extensions must defer runtime construction, storage, credentials, filesystem
/// discovery, network access and workers until `run`. This is a cooperative
/// process contract, not containment of executable bootstrap code.
pub struct ConfiguredConnection<R, W> {
    /// Input including any bytes buffered beyond initial Configure.
    reader: BufReader<R>,
    /// Protocol output after the single Hello.
    writer: W,
    /// Already received ordinary configuration.
    configure: tau_proto::Configure,
    /// Peer identity and authorities already advertised on this connection.
    hello: tau_proto::Hello,
}

impl<R: Read, W: Write> ConfiguredConnection<R, W> {
    /// Resume any ordinary SDK runner family without a second Hello/Configure.
    ///
    /// Construct extension state inside `runtime`, never before
    /// [`prepare_inspection`]. Returning from inspection does not create a
    /// connection or invoke this closure; ordinary prebuilt-state APIs
    /// alone remain non-inspectable.
    pub fn run<E, T>(
        self,
        extension: E,
        runtime: impl FnOnce(TauExtensionRunner<E>, BufReader<R>, W) -> T,
    ) -> ClientResult<T>
    where
        E: TauExtension,
    {
        if extension.name() != self.hello.client_name.as_str()
            || extension.kind() != self.hello.client_kind
        {
            return Err(ClientError::builder(
                "ordinary continuation changed Hello identity",
            ));
        }
        let builder = TauExtensionRunner {
            extension: Some(extension),
            prepared_builder: None,
            initial_configure: Some(self.configure),
        }
        .into_builder()?;
        let advertised: std::collections::BTreeSet<_> =
            self.hello.capabilities.into_iter().collect();
        let declared: std::collections::BTreeSet<_> =
            builder.peer_capabilities.iter().copied().collect();
        if advertised != declared {
            return Err(ClientError::builder(
                "ordinary continuation changed Hello capabilities",
            ));
        }
        Ok(runtime(
            TauExtensionRunner {
                extension: None,
                prepared_builder: Some(builder),
                initial_configure: None,
            },
            self.reader,
            self.writer,
        ))
    }
}

/// Select inspection or ordinary startup using only protocol I/O and pure data.
///
/// `hello` must describe the same peer as the eventual ordinary runner.
/// Inspection support is set here, not by ordinary runner builders. `declare`
/// receives supplied config but no operational handle; share pure constructors
/// with normal registration and report runtime-only portions as explicit gaps.
/// Inspection supplies neither state paths nor authorized secrets and never
/// dispatches Configure handlers, Ready, subscriptions or lifecycle events.
///
/// Returns `None` after inspection completion or a clean early disconnect.
/// Errors deliberately omit callback diagnostics because config can be secret.
pub fn prepare_inspection<R: Read, W: Write>(
    reader: R,
    writer: W,
    mut hello: tau_proto::Hello,
    declare: impl FnOnce(&tau_proto::Configure) -> ClientResult<tau_proto::InspectionComplete>,
) -> ClientResult<Option<ConfiguredConnection<R, W>>> {
    hello.protocol_version = tau_proto::PROTOCOL_VERSION;
    hello.declaration_inspection = true;
    let mut writer = tau_proto::PeerOutputWriter::new(writer);
    writer.write_message(&tau_proto::HarnessInputMessage::Hello(hello.clone()))?;
    writer.flush()?;
    let mut reader = tau_proto::PeerInputReader::new(reader);
    let configure = match reader.read_message()? {
        Some(tau_proto::HarnessOutputMessage::Configure(configure)) => configure,
        Some(tau_proto::HarnessOutputMessage::Disconnect(_)) | None => return Ok(None),
        Some(_) => return Err(ClientError::handler("expected initial Configure")),
    };
    if configure.purpose.is_runtime() {
        return Ok(Some(ConfiguredConnection {
            reader: reader.into_buffered_inner(),
            writer: writer.into_inner(),
            configure,
            hello,
        }));
    }
    if configure.state_dir.is_some() || !configure.secrets.is_empty() {
        return Err(ClientError::handler(
            "inspection configuration must omit runtime state and secrets",
        ));
    }
    let mut result = declare(&configure).unwrap_or_else(|_| tau_proto::InspectionComplete {
        gaps: vec![tau_proto::InspectionGap::InvalidConfiguration],
        ..Default::default()
    });
    let scope = crate::ToolNameScope::from_configure(&configure);
    result.tools = match result
        .tools
        .into_iter()
        .map(|tool| scope.scope_registration(tool))
        .collect::<ClientResult<Vec<_>>>()
    {
        Ok(tools) => tools,
        Err(_) => {
            result = tau_proto::InspectionComplete {
                gaps: vec![tau_proto::InspectionGap::InvalidConfiguration],
                ..Default::default()
            };
            Vec::new()
        }
    };
    let message = tau_proto::HarnessInputMessage::InspectionComplete(result);
    if crate::encoded_outbound_frame_bytes(&message)? > crate::MAX_OUTBOUND_FRAME_BYTES {
        return Err(ClientError::Overloaded);
    }
    writer.write_message(&message)?;
    writer.flush()?;
    Ok(None)
}
