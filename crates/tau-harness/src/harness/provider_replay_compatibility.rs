//! Account-alias admission under `REQ-best-effort-provider-switching`.

use tau_config::chatgpt_responses_settings::ChatgptResponsesSettings;
use tau_config::settings::BuiltinComponentIdentity;

use super::*;

impl Harness {
    /// Additional exact producing models allowed to replay into this route.
    ///
    /// Only aliases owned by the same configured Tau built-in provider instance
    /// qualify. Current frozen settings interpret historical model IDs;
    /// journals do not record historical profile settings. This permits an
    /// attempt, not a promise that the remote account accepts the original
    /// opaque bytes.
    pub(super) fn compatible_provider_replay_sources(
        &self,
        destination: &ModelId,
    ) -> HashSet<ModelId> {
        let Some(connection) = self.provider_runtime.model_routes.get(destination) else {
            return HashSet::new();
        };
        if self.unique_provider_model_publisher(destination) != Some(connection) {
            return HashSet::new();
        }
        let Some(extension) = self.extensions.entries.get(connection) else {
            return HashSet::new();
        };
        if extension
            .supervised_config
            .as_ref()
            .and_then(|config| config.component)
            != Some(BuiltinComponentIdentity::Provider)
        {
            return HashSet::new();
        }
        let Some(settings) = self
            .config
            .provider_settings_snapshots
            .get(extension.name.as_str())
        else {
            return HashSet::new();
        };
        let route = |model: &ModelId| {
            let bytes = settings.get(&format!("{}.json", model.provider))?;
            ChatgptResponsesSettings::from_private_profile(bytes)
                .map(|settings| settings.uses_lite(model.model.as_str()))
        };
        let Some(destination_lite) = route(destination) else {
            return HashSet::new();
        };
        self.provider_runtime
            .model_routes
            .iter()
            .filter(|(source, owner)| {
                *owner == connection
                    && source.model == destination.model
                    && source.provider != destination.provider
                    && self.unique_provider_model_publisher(source) == Some(connection)
                    && route(source) == Some(destination_lite)
            })
            .map(|(source, _)| source.clone())
            .collect()
    }

    /// Require one publishing connection, not merely the routing map's winner
    /// when multiple configured providers declare the same exact model ID.
    fn unique_provider_model_publisher(&self, model: &ModelId) -> Option<&ConnectionId> {
        let mut publishers =
            self.provider_runtime
                .models_by_extension
                .iter()
                .filter_map(|(connection, models)| {
                    models
                        .iter()
                        .any(|info| &info.id == model)
                        .then_some(connection)
                });
        let publisher = publishers.next()?;
        publishers.next().is_none().then_some(publisher)
    }
}
