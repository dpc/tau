use crate::extension::ExtensionState;
use crate::harness::Harness;

/// Diagnostic identity for one current ready extension instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalSelfExtension {
    /// Configured name, not an untrusted replacement advertised by the peer.
    pub name: tau_proto::ExtensionName,
    /// Run-local instance identity assigned by the harness.
    pub instance_id: tau_proto::ExtensionInstanceId,
    /// Restart count distinguishes successive connections of the same instance.
    pub restart_attempt: u32,
    /// Reported package metadata; absent for older or uninstrumented peers.
    pub build_identity: Option<tau_proto::BuildIdentity>,
}

impl Harness {
    /// Snapshot current ready instances without probing their processes or
    /// files.
    pub(crate) fn self_extension_info(&self) -> Vec<InternalSelfExtension> {
        let mut extensions: Vec<_> = self
            .extensions
            .entries
            .values()
            .filter(|entry| entry.state == ExtensionState::Ready)
            .map(|entry| InternalSelfExtension {
                name: entry.name.clone(),
                instance_id: entry.instance_id,
                restart_attempt: entry.restart_attempt,
                build_identity: entry.build_identity.clone(),
            })
            .collect();
        extensions.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.instance_id.get().cmp(&b.instance_id.get()))
        });
        extensions
    }
}
