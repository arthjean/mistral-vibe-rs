//! Where a background session title is routed: the configured providers and
//! models, the session's own model active.

use super::*;

/// What a title's model is chosen from: the configured providers and models,
/// with the session's own model active, and the credentials and budgets the
/// call runs with.
#[derive(Clone)]
pub(super) struct TitleRouting {
    pub(super) routing: ModelRouting,
    pub(super) context: BackendContext,
}

impl TitleRouting {
    /// Reference `select_utility_model`, or `None` when the configuration
    /// leaves nothing to run a title on.
    pub(super) fn selection(&self) -> Option<UtilitySelection> {
        utility::select(
            &self.routing,
            self.context.credentials.as_ref(),
            ModelAvailability::global(),
            Some(UtilityFeature::Title),
        )
        .ok()
    }
}

/// The routing a title reads: every configured provider, the driver's own
/// standing in for the entry of the same name, and the driver's model active.
pub(super) fn title_routing(config: &LiveDriverConfig) -> ModelRouting {
    let mut providers = config.providers.clone();
    match providers
        .iter_mut()
        .find(|provider| provider.name == config.provider.name)
    {
        Some(entry) => *entry = config.provider.clone(),
        None => providers.push(config.provider.clone()),
    }
    let mut models = config.models.clone();
    if !models
        .iter()
        .any(|model| model.alias == config.model || model.name == config.model)
    {
        models.push(ModelConfig::new(&config.model, &config.provider.name));
    }
    ModelRouting {
        providers,
        models,
        active_alias: Some(config.model.clone()),
        allowed_models: config.allowed_models.clone(),
        api: config.api,
        utility_models: config.utility_models.clone(),
    }
}
