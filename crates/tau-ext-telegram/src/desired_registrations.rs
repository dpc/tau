//! Durable desired Telegram listener registrations.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use tau_proto::AgentId;
#[cfg(not(test))]
use tau_proto::{
    ExtensionDataErrorKind, ExtensionDataPath, ExtensionDataRequestOp, ExtensionDataScope,
    ExtensionDataValue,
};

/// Session-scoped extension-data file containing desired listener agents.
#[cfg(not(test))]
const DESIRED_REGISTRATIONS_PATH: &str = "desired-registrations.cbor";
/// Current strict storage schema.
const DESIRED_REGISTRATIONS_SCHEMA: u32 = 0;

/// Classified failure while replacing one desired-registration snapshot.
#[derive(Debug)]
pub(super) enum DesiredRegistrationStoreError {
    /// Read-back proved that the requested snapshot was not installed.
    Known(String),
    /// Neither the write result nor read-back established the installed
    /// snapshot.
    Indeterminate(String),
}

impl std::fmt::Display for DesiredRegistrationStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known(message) | Self::Indeterminate(message) => formatter.write_str(message),
        }
    }
}

/// Strict versioned representation stored through harness extension-data RPC.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct DesiredRegistrationFile {
    /// Exact schema understood by this extension version.
    schema: u32,
    /// Sorted unique desired agent identities.
    agents: BTreeSet<AgentId>,
}

/// Storage backend for one session's desired Telegram registrations.
pub(super) enum DesiredRegistrationStorage {
    /// Process-local backend used by focused extension state-machine tests.
    Memory(Arc<Mutex<BTreeSet<AgentId>>>),
    /// Harness-owned Session-scope extension-data backend used in production.
    #[cfg(not(test))]
    Rpc(tau_client::ExtensionDataClient),
    /// Deterministic write failure used by ordering tests.
    #[cfg(test)]
    FailWrites,
    /// Deterministic uncertain write outcome used by fail-stop tests.
    #[cfg(test)]
    FailIndeterminate,
}

impl Default for DesiredRegistrationStorage {
    fn default() -> Self {
        Self::Memory(Arc::new(Mutex::new(BTreeSet::new())))
    }
}

impl DesiredRegistrationStorage {
    /// Creates the production Session-scope extension-data backend.
    #[cfg(not(test))]
    pub(super) fn rpc(client: tau_client::ExtensionDataClient) -> Self {
        Self::Rpc(client)
    }

    /// Reads and strictly validates the complete desired-registration set.
    pub(super) fn load(
        &self,
        session_id: &tau_proto::SessionId,
    ) -> Result<BTreeSet<AgentId>, String> {
        let _ = session_id;
        match self {
            Self::Memory(agents) => Ok(agents
                .lock()
                .expect("desired registration memory lock")
                .clone()),
            #[cfg(not(test))]
            Self::Rpc(client) => {
                let contents = match client.request_for_session(
                    ExtensionDataScope::Session,
                    session_id.clone(),
                    ExtensionDataRequestOp::ReadFile {
                        path: ExtensionDataPath::new(DESIRED_REGISTRATIONS_PATH),
                    },
                ) {
                    Ok(ExtensionDataValue::ReadFile { contents }) => contents,
                    Ok(other) => {
                        return Err(format!(
                            "unexpected desired Telegram registration read result: {other:?}"
                        ));
                    }
                    Err(tau_client::ExtensionDataRpcError::Harness {
                        kind: ExtensionDataErrorKind::NotFound,
                        ..
                    }) => return Ok(BTreeSet::new()),
                    Err(error) => {
                        return Err(format!(
                            "reading desired Telegram registrations failed: {error}"
                        ));
                    }
                };
                decode_file(&contents)
            }
            #[cfg(test)]
            Self::FailWrites | Self::FailIndeterminate => Ok(BTreeSet::new()),
        }
    }

    /// Atomically replaces the complete desired-registration set.
    pub(super) fn store(
        &self,
        session_id: &tau_proto::SessionId,
        agents: &BTreeSet<AgentId>,
    ) -> Result<(), DesiredRegistrationStoreError> {
        let _ = session_id;
        match self {
            Self::Memory(stored) => {
                *stored.lock().expect("desired registration memory lock") = agents.clone();
                Ok(())
            }
            #[cfg(not(test))]
            Self::Rpc(client) => {
                let mut contents = Vec::new();
                ciborium::into_writer(
                    &DesiredRegistrationFile {
                        schema: DESIRED_REGISTRATIONS_SCHEMA,
                        agents: agents.clone(),
                    },
                    &mut contents,
                )
                .map_err(|error| {
                    DesiredRegistrationStoreError::Known(format!(
                        "encoding desired Telegram registrations failed: {error}"
                    ))
                })?;
                match client.request_for_session(
                    ExtensionDataScope::Session,
                    session_id.clone(),
                    ExtensionDataRequestOp::WriteFile {
                        path: ExtensionDataPath::new(DESIRED_REGISTRATIONS_PATH),
                        contents,
                    },
                ) {
                    Ok(ExtensionDataValue::WriteFile) => Ok(()),
                    Ok(other) => Err(DesiredRegistrationStoreError::Known(format!(
                        "unexpected desired Telegram registration write result: {other:?}"
                    ))),
                    Err(error) => match self.load(session_id) {
                        Ok(installed) if installed == *agents => {
                            Err(DesiredRegistrationStoreError::Indeterminate(format!(
                                "writing desired Telegram registrations installed the target but \
                                 failed its durability sync: {error}"
                            )))
                        }
                        Ok(_) => Err(DesiredRegistrationStoreError::Known(format!(
                            "writing desired Telegram registrations failed before replacement: {error}"
                        ))),
                        Err(read_error) => {
                            Err(DesiredRegistrationStoreError::Indeterminate(format!(
                                "writing desired Telegram registrations had an indeterminate \
                                 outcome: {error}; read-back failed: {read_error}"
                            )))
                        }
                    },
                }
            }
            #[cfg(test)]
            Self::FailWrites => Err(DesiredRegistrationStoreError::Known(
                "writing desired Telegram registrations failed: injected".to_owned(),
            )),
            #[cfg(test)]
            Self::FailIndeterminate => Err(DesiredRegistrationStoreError::Indeterminate(
                "writing desired Telegram registrations had an indeterminate outcome: injected"
                    .to_owned(),
            )),
        }
    }
}

/// Decodes one exact current-schema desired-registration file.
fn decode_file(contents: &[u8]) -> Result<BTreeSet<AgentId>, String> {
    let mut reader = Cursor::new(contents);
    let file: DesiredRegistrationFile = ciborium::from_reader(&mut reader)
        .map_err(|error| format!("desired Telegram registrations are malformed: {error}"))?;
    if reader.position() != contents.len() as u64 {
        return Err("desired Telegram registrations contain trailing data".to_owned());
    }
    if file.schema != DESIRED_REGISTRATIONS_SCHEMA {
        return Err(format!(
            "desired Telegram registration schema {} is unsupported",
            file.schema
        ));
    }
    Ok(file.agents)
}

#[cfg(test)]
mod tests;
