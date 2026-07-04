use crate::ExtensionBuilder;

/// Top-level extension declaration for the shared Tau client runtime.
pub trait TauExtension {
    /// Mutable state shared by this extension's handlers.
    type State;

    /// Returns the protocol client name used in the startup `Hello` frame.
    ///
    /// The runner stores this as a [`tau_proto::ExtensionName`]. The static
    /// string return type keeps simple extensions allocation-free while still
    /// allowing the builder to own the protocol name during startup.
    fn name(&self) -> &'static str;

    /// Returns the protocol client kind used in the startup `Hello` frame.
    fn kind(&self) -> tau_proto::ClientKind {
        tau_proto::ClientKind::Tool
    }

    /// Registers startup declarations and handlers into `builder`.
    fn register(self, builder: &mut ExtensionBuilder<Self::State>);
}

/// Reusable extension component that can register handlers against shared
/// state.
pub trait ExtensionPlugin<State> {
    /// Registers this plugin's startup declarations and handlers into
    /// `builder`.
    fn register(self, builder: &mut ExtensionBuilder<State>);
}
