#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd as _;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt as _;

use tau_core::{AgentEventParent, AgentStore};
use tau_proto::{AgentCreator, AgentId, AgentStarted, Event, UnixMicros};

use super::*;

fn prepare_fixture() -> (tempfile::TempDir, PreparedAgentTrace) {
    let root = tempfile::tempdir().expect("state root");
    let agent_id = AgentId::parse("agent-stage").expect("agent id");
    let mut store = AgentStore::open_lazy(root.path()).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            AgentEventParent::InheritHead,
            Event::AgentStarted(AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(AgentCreator::User),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            UnixMicros::new(1),
        )
        .expect("creation");
    drop(store);
    let prepared = prepare_agent_trace(
        root.path(),
        &agent_id,
        DescendantSelection::RootOnly,
        AgentTraceFormat::TauJsonl,
    )
    .expect("prepared trace");
    (root, prepared)
}

/// Sensitive staging uses a mode-0600 anonymous file whose procfs descriptor
/// has no live pathname that can survive process termination.
#[test]
#[cfg(target_os = "linux")]
fn prepared_trace_staging_is_private_and_anonymous() {
    let (_root, prepared) = prepare_fixture();
    assert_eq!(
        prepared.file.metadata().expect("metadata").mode() & 0o777,
        0o600
    );
    let descriptor = format!("/proc/self/fd/{}", prepared.file.as_raw_fd());
    let target = std::fs::read_link(descriptor).expect("descriptor target");
    let target = target.to_string_lossy();

    assert!(
        target.contains("(deleted)") || target.contains("/#"),
        "anonymous staging must have no live pathname: {target}"
    );
}

/// A destination write failure is returned without persisting or renaming the
/// anonymous staged artifact.
#[test]
fn prepared_trace_copy_propagates_destination_failure() {
    /// Destination that deterministically rejects every write.
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "consumer exited",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let (_root, mut prepared) = prepare_fixture();
    let error = prepared
        .copy_to(&mut FailingWriter)
        .expect_err("copy must return destination failure");

    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}
