//! Strict streaming session-reference authority for startup retention.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use tau_proto::{AgentId, Event, SessionId};

use super::{
    PersistedSessionEvent, PersistedSessionEventSeq, SessionMeta, SessionStoreError,
    path_still_names_file, read_cbor_records_from_file, validate_session_event,
};

const MAX_SESSION_MANIFEST_BYTES: u64 = 64 * 1024;

/// Cumulative durable-agent references discovered from surviving canonical
/// sessions.
///
/// Initial capture streams each journal once. Candidate-specific refreshes
/// validate only newly appended suffixes for unchanged sessions while retaining
/// all previously observed references conservatively.
pub struct SessionRetentionReferences {
    /// Canonical session storage root.
    sessions_dir: PathBuf,
    /// Every durable agent observed in a canonical session load event.
    references: HashSet<AgentId>,
    /// Validated journal boundary for each currently canonical session
    /// instance.
    journals: HashMap<SessionId, SessionJournalCursor>,
}

/// Validated boundary for one canonical session journal.
struct SessionJournalCursor {
    /// Identity of the canonical session directory.
    directory_metadata: fs::Metadata,
    /// Identity of the journal file.
    journal_metadata: fs::Metadata,
    /// Validated byte offset at the prior EOF.
    offset: u64,
    /// Next sequence expected after the prior EOF.
    next_seq: PersistedSessionEventSeq,
}

/// Canonical session discovered during one strict namespace scan.
struct CanonicalSession {
    /// Parsed safe session identifier.
    session_id: SessionId,
    /// Identity of the direct child directory.
    directory_metadata: fs::Metadata,
}

impl SessionRetentionReferences {
    /// Captures the initial strict reference authority from all canonical
    /// sessions.
    pub fn capture(sessions_dir: &Path) -> Result<Self, SessionStoreError> {
        let mut references = Self {
            sessions_dir: sessions_dir.to_path_buf(),
            references: HashSet::new(),
            journals: HashMap::new(),
        };
        references.refresh()?;
        Ok(references)
    }

    /// Refreshes canonical sessions and streams only unseen journal suffixes.
    ///
    /// Any namespace, manifest, or journal uncertainty aborts the refresh. The
    /// cumulative reference set never removes an already observed agent.
    pub fn refresh(&mut self) -> Result<(), SessionStoreError> {
        let sessions = canonical_sessions(&self.sessions_dir)?;
        let mut next_journals = HashMap::with_capacity(sessions.len());
        for session in sessions {
            let prior = self.journals.get(&session.session_id).filter(|cursor| {
                same_identity(&cursor.directory_metadata, &session.directory_metadata)
            });
            let cursor =
                scan_session_journal(&self.sessions_dir, &session, prior, &mut self.references)?;
            next_journals.insert(session.session_id, cursor);
        }
        self.journals = next_journals;
        Ok(())
    }

    /// Returns whether any surviving canonical session has ever loaded
    /// `agent_id`.
    #[must_use]
    pub fn contains(&self, agent_id: &AgentId) -> bool {
        self.references.contains(agent_id)
    }
}

fn canonical_sessions(sessions_dir: &Path) -> Result<Vec<CanonicalSession>, SessionStoreError> {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(read_error(sessions_dir, source)),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| read_error(sessions_dir, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| read_error(&entry.path(), source))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(name) else {
            continue;
        };
        let directory_metadata = entry
            .metadata()
            .map_err(|source| read_error(&entry.path(), source))?;
        if !manifest_is_canonical(&entry.path(), &directory_metadata)? {
            continue;
        }
        sessions.push(CanonicalSession {
            session_id,
            directory_metadata,
        });
    }
    Ok(sessions)
}

fn manifest_is_canonical(
    session_dir: &Path,
    directory_metadata: &fs::Metadata,
) -> Result<bool, SessionStoreError> {
    let path = session_dir.join("meta.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(read_error(&path, source)),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(open_error(&path, source)),
    };
    let metadata = file
        .metadata()
        .map_err(|source| read_error(&path, source))?;
    if !metadata.is_file() {
        return Ok(false);
    }
    if MAX_SESSION_MANIFEST_BYTES < metadata.len() {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| read_error(&path, source))?;
    if serde_json::from_slice::<SessionMeta>(&bytes).is_err() {
        return Ok(false);
    }
    if !path_still_names_file(&path, &metadata).map_err(|source| read_error(&path, source))?
        || !path_still_names_directory(session_dir, directory_metadata)?
    {
        return Err(read_error(
            &path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "session manifest was replaced during retention inspection",
            ),
        ));
    }
    Ok(true)
}

fn scan_session_journal(
    sessions_dir: &Path,
    session: &CanonicalSession,
    prior: Option<&SessionJournalCursor>,
    references: &mut HashSet<AgentId>,
) -> Result<SessionJournalCursor, SessionStoreError> {
    let path = sessions_dir
        .join(session.session_id.as_str())
        .join("events.cbor");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(&path)
        .map_err(|source| open_error(&path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| read_error(&path, source))?;
    if !metadata.is_file() {
        return Err(open_error(
            &path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "session journal is not a real regular file",
            ),
        ));
    }
    let mut expected_seq = match prior {
        Some(prior)
            if same_identity(&prior.journal_metadata, &metadata)
                && prior.offset <= metadata.len() =>
        {
            file.seek(SeekFrom::Start(prior.offset))
                .map_err(|source| read_error(&path, source))?;
            prior.next_seq
        }
        Some(prior) if same_identity(&prior.journal_metadata, &metadata) => {
            return Err(read_error(
                &path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session journal shrank below the validated retention boundary",
                ),
            ));
        }
        Some(_) => {
            return Err(read_error(
                &path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "session journal identity changed after the validated retention boundary",
                ),
            ));
        }
        _ => PersistedSessionEventSeq::new(0),
    };
    read_cbor_records_from_file(&mut file, &path, |record: PersistedSessionEvent| {
        if record.seq != expected_seq {
            return Err(SessionStoreError::InvalidSequence {
                path: path.clone(),
                expected: expected_seq,
                actual: record.seq,
            });
        }
        expected_seq = expected_seq.next();
        validate_session_event(session.session_id.as_str(), &record.event)?;
        if let Event::SessionAgentLoaded(event) = record.event
            && !event.ephemeral
        {
            references.insert(event.agent_id);
        }
        Ok(())
    })?;
    let new_offset = file
        .stream_position()
        .map_err(|source| read_error(&path, source))?;
    if !path_still_names_file(&path, &metadata).map_err(|source| read_error(&path, source))?
        || !path_still_names_directory(
            &sessions_dir.join(session.session_id.as_str()),
            &session.directory_metadata,
        )?
    {
        return Err(read_error(
            &path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "session journal was replaced during retention inspection",
            ),
        ));
    }
    Ok(SessionJournalCursor {
        directory_metadata: session.directory_metadata.clone(),
        journal_metadata: metadata,
        offset: new_offset,
        next_seq: expected_seq,
    })
}

fn path_still_names_directory(
    path: &Path,
    expected: &fs::Metadata,
) -> Result<bool, SessionStoreError> {
    let current = fs::symlink_metadata(path).map_err(|source| read_error(path, source))?;
    if current.file_type().is_symlink() || !current.is_dir() {
        return Ok(false);
    }
    Ok(same_identity(expected, &current))
}

fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.modified().ok() == right.modified().ok() && left.len() == right.len()
    }
}

fn open_error(path: &Path, source: io::Error) -> SessionStoreError {
    SessionStoreError::Open {
        path: path.to_path_buf(),
        source,
    }
}

fn read_error(path: &Path, source: io::Error) -> SessionStoreError {
    SessionStoreError::Read {
        path: path.to_path_buf(),
        source,
    }
}
