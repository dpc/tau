//! Exact initial discovery expected by the isolated production-adapter fixture.

/// Render the fixture's sole built-in advertisement, not a second task turn.
pub(super) fn expected_message() -> String {
    let content = "Use when explaining the Tau coding agent, including its built-in self-knowledge, configuration, debugging, source code, community, or where to find Tau help.";
    let item = tau_proto::DiscoveryContextItem {
        identity: "skill:tau-self-knowledge".to_owned(),
        version: blake3::hash(content.as_bytes()).to_hex().to_string(),
        content: content.to_owned(),
    };
    tau_proto::DiscoveryContextDelivery {
        scope: vec![item.reference()],
        items: vec![item],
        updated: Vec::new(),
    }
    .render()
}
