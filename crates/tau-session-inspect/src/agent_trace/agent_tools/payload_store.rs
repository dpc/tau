//! Anonymous payload staging support for compact semantic projection.

use std::io as path_std_io;
use std::io::{Read as _, Seek as _, Write as _};

use serde::Serialize;

use crate::InspectError;

/// Byte location in anonymous projection storage.
#[derive(Clone, Copy)]
pub(super) struct Endpoint {
    /// Starting byte offset.
    offset: u64,
    /// Encoded byte length.
    length: u64,
}

/// Anonymous storage for payload-bearing projected values.
pub(super) struct PayloadStore {
    /// Process-owned delete-on-close file.
    file: std::fs::File,
}

impl PayloadStore {
    /// Creates empty anonymous projection storage.
    pub(super) fn new() -> Result<Self, InspectError> {
        Ok(Self {
            file: tempfile::tempfile()?,
        })
    }

    /// Appends one CBOR-encoded value.
    pub(super) fn append<T: Serialize>(&mut self, value: &T) -> Result<Endpoint, InspectError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).map_err(projection_error)?;
        let offset = self.file.seek(path_std_io::SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        Ok(Endpoint {
            offset,
            length: bytes.len() as u64,
        })
    }

    /// Loads one previously staged value.
    pub(super) fn load<T: serde::de::DeserializeOwned>(
        &mut self,
        endpoint: Endpoint,
    ) -> Result<T, InspectError> {
        self.file
            .seek(path_std_io::SeekFrom::Start(endpoint.offset))?;
        let mut bytes = vec![0; endpoint.length as usize];
        self.file.read_exact(&mut bytes)?;
        ciborium::from_reader(bytes.as_slice()).map_err(projection_error)
    }
}

/// Wraps anonymous staging failures as trace projection errors.
fn projection_error(error: impl std::fmt::Display) -> InspectError {
    InspectError::Trace(crate::AgentTraceError::Projection(format!(
        "failed to stage compact semantic trace: {error}"
    )))
}
