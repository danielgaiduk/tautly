//! Mid-session model switch (spec 7.11.5): `Command::SetModel`.
//!
//! The actor applies a switch between user turns: at once when idle, and when it arrives
//! during a turn it waits in the command queue until the turn ends, so the turn's
//! remaining steps keep the old model and the next user turn starts on the new one.
//!
//! A switch first checks the model (a built-in provider's model must be in the catalog, a
//! compat model needs its `[[providers.compat]]` entry), then builds its provider on the
//! blocking pool (the key lookup may read the keychain). Any failure is an
//! `Error{provider_fatal}` with a hint, and nothing changes. On success it recomputes the
//! capabilities and context window, the tools (whose `edit` schema follows the new
//! model's edit format; a `catalog_overrides` entry for the model wins), the side model
//! (`routing.side_model` wins) and whether USD budgets apply; strips reasoning another
//! provider produced; makes the next step start a cold-cache epoch; logs `ModelSet` and
//! emits `ModelChanged`.

use std::sync::{Arc, PoisonError};

use tautly_provider::{Catalog, ModelId, Provider, ProviderError, Retrying};
use tautly_tools::registry;

use crate::config::Config;
use crate::protocol::{ErrorKind, Event};
use crate::provider_setup::{ProviderFactory, ProviderSetupError, provider_for, provider_label};
use crate::record::SessionRecord;
use crate::routing;
use crate::session::Session;

/// What a switch needs besides the session's state.
pub(crate) struct ModelSetup {
    /// The session's configuration, with `model` kept equal to the session's model (the
    /// side-model rule reads it).
    pub config: Config,
    pub catalog: Arc<Catalog>,
    pub factory: ProviderFactory,
}

impl ModelSetup {
    /// Without a `factory`, providers come from [`provider_for`] with this configuration.
    pub fn new(config: Config, catalog: Arc<Catalog>, factory: Option<ProviderFactory>) -> Self {
        let factory = factory.unwrap_or_else(|| {
            let (cfg, cat) = (config.clone(), Arc::clone(&catalog));
            Arc::new(move |model: &ModelId| provider_for(&cfg, model, Arc::clone(&cat)))
        });
        ModelSetup {
            config,
            catalog,
            factory,
        }
    }

    /// Why `model` cannot be used, before any key lookup.
    fn check(&self, model: &ModelId) -> Result<(), ProviderSetupError> {
        match model.provider() {
            "anthropic" | "openai" | "google" if !self.catalog.contains(model) => {
                Err(ProviderSetupError::UnknownModel {
                    model: model.clone(),
                })
            }
            "anthropic" | "openai" | "google" => Ok(()),
            "compat" => {
                let label = provider_label(model);
                let name = label.strip_prefix("compat:").unwrap_or("");
                if self.config.providers.compat.iter().any(|c| c.name == name) {
                    Ok(())
                } else {
                    Err(ProviderSetupError::NoCompatEntry {
                        name: name.to_string(),
                        model: model.clone(),
                    })
                }
            }
            other => Err(ProviderSetupError::UnknownProvider {
                provider: other.to_string(),
                model: model.clone(),
            }),
        }
    }
}

/// An invalid-request error that names the reasoning (`thinking`, `reasoning`,
/// `signature`): what a provider says when it refuses reasoning carried over from
/// another model of its own.
pub(crate) fn rejects_reasoning(e: &ProviderError) -> bool {
    let ProviderError::InvalidRequest(msg) = e else {
        return false;
    };
    let msg = msg.to_lowercase();
    ["thinking", "reasoning", "signature"]
        .iter()
        .any(|w| msg.contains(w))
}

impl Session {
    /// `Command::SetModel`; see the module docs.
    pub async fn set_model(&mut self, model: ModelId) {
        if model == self.model {
            self.emit(Event::ModelChanged {
                provider: provider_label(&model),
                model,
            })
            .await;
            return;
        }
        let provider = match self.build_provider(&model).await {
            Ok(p) => p,
            Err(e) => {
                self.emit(Event::Error {
                    kind: ErrorKind::ProviderFatal,
                    message: format!("cannot switch to {model}: {e}"),
                    retryable: false,
                    hint: Some(e.hint()),
                })
                .await;
                return;
            }
        };
        let label = provider_label(&model);
        let same_provider = label == provider_label(&self.model);
        let info = self.setup.catalog.get(&model);
        self.provider = Arc::new(Retrying::new(provider));
        self.tools = registry(&info.caps);
        self.specs = self.tools.iter().map(|t| t.spec(&info.caps)).collect();
        self.budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_model(info.price.is_some(), model.as_str());
        self.info = info;
        self.model = model.clone();
        self.setup.config.model = model.clone();
        self.side_model = routing::side_model_id(&self.setup.config, &self.setup.catalog);
        self.context.strip_reasoning(Some(&label));
        // Kept reasoning may still be refused by the new model: the first request may
        // strip it and try again once.
        self.reasoning_retry = same_provider;
        self.context.force_cold_epoch();
        self.log(SessionRecord::ModelSet {
            model: model.clone(),
        })
        .await;
        self.checkpoint().await;
        self.emit(Event::ModelChanged {
            model,
            provider: label,
        })
        .await;
        // An unpriced model with a USD limit is said now.
        self.budget_exhausted().await;
    }

    async fn build_provider(
        &self,
        model: &ModelId,
    ) -> Result<Arc<dyn Provider>, ProviderSetupError> {
        self.setup.check(model)?;
        let factory = Arc::clone(&self.setup.factory);
        let model = model.clone();
        tokio::task::spawn_blocking(move || factory(&model))
            .await
            .unwrap_or_else(|e| {
                Err(ProviderSetupError::Other(format!(
                    "the provider setup stopped: {e}"
                )))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_invalid_requests_naming_the_reasoning_are_rejections() {
        for msg in [
            "messages.1.content.0: Invalid `signature` in `thinking` block",
            "Encrypted content for item rs_1 could not be verified (reasoning)",
        ] {
            assert!(rejects_reasoning(&ProviderError::InvalidRequest(
                msg.into()
            )));
        }
        assert!(!rejects_reasoning(&ProviderError::InvalidRequest(
            "max_tokens too large".into()
        )));
        assert!(!rejects_reasoning(&ProviderError::Other(
            "thinking failed".into()
        )));
    }
}
