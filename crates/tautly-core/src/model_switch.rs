//! Mid-session model switch (spec 7.11.5): `Command::SetModel`.
//!
//! The actor applies a switch between user turns: at once when idle, and when it arrives
//! during a turn it waits in the command queue until the turn ends, so the turn's
//! remaining steps keep the old model and the next user turn starts on the new one.
//!
//! A switch first checks the model's provider (a built-in one, or a compat model with its
//! `[[providers.compat]]` entry), then builds its provider on the blocking pool (the key
//! lookup may read the keychain). The build is raced against shutdown: a `Shutdown` (or
//! the client going away) while a keychain prompt is open abandons the switch, so the
//! session never waits on the prompt to stop. Any failure is an `Error{provider_fatal}`
//! with a hint, and nothing changes. A built-in provider's model that the catalog does not
//! list is used with conservative defaults and a `Warning`, as at start-up (spec 4.3).
//!
//! On success it recomputes the capabilities and context window, the tools (whose `edit`
//! schema follows the new model's edit format; a `catalog_overrides` entry for the model
//! wins; MCP tools are kept), the side route (`routing::Side::resolve_with`:
//! `routing.side_model` wins, else the new model's provider's small tier; a side model of
//! another provider is built with the factory in the same blocking task, and one that
//! cannot be built leaves side work to the new main model, with a `Warning`) and whether
//! USD budgets apply. The recomputed route also ends a fallback of side work to the old
//! main model (`routing.rs`); side usage reported before the switch is accounted first,
//! under the old side model. On a
//! change of provider it strips the reasoning the old one produced and restarts the token
//! estimator's calibration (another tokenizer); makes the next step start a cold-cache
//! epoch; logs `ModelSet` and emits `ModelChanged`.

use std::future::Future;
use std::sync::{Arc, PoisonError};

use tautly_context::TokenEstimator;
use tautly_provider::{Catalog, ModelId, Provider, ProviderError, Retrying};
use tautly_tools::Tool;
use tautly_tools::registry;

use crate::config::Config;
use crate::mcp::MCP_TOOL_PREFIX;
use crate::protocol::{ErrorKind, Event};
use crate::provider_setup::{ProviderFactory, ProviderSetupError, provider_for, provider_label};
use crate::record::SessionRecord;
use crate::routing::Side;
use crate::session::Session;

/// What [`Session::build_provider`] makes: the main provider, the side route, the note.
type Built = (Arc<dyn Provider>, Side, Option<String>);

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
    /// `Command::SetModel`; see the module docs. `stop` resolves when the session is shutting
    /// down: if that happens before the provider is built, the switch is abandoned (the
    /// build finishes on the blocking pool and is dropped).
    pub async fn set_model(&mut self, model: ModelId, stop: impl Future<Output = ()>) {
        if model == self.model {
            self.emit(Event::ModelChanged {
                provider: provider_label(&model),
                model,
            })
            .await;
            return;
        }
        let built = tokio::select! {
            biased;
            built = self.build_provider(&model) => Some(built),
            () = stop => None,
        };
        let (provider, side, side_note) = match built {
            Some(Ok(b)) => b,
            None => {
                self.emit(Event::Error {
                    kind: ErrorKind::ProviderFatal,
                    message: format!("cannot switch to {model}: the session is shutting down"),
                    retryable: false,
                    hint: None,
                })
                .await;
                return;
            }
            Some(Err(e)) => {
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
        if model.provider() != "compat" && !self.setup.catalog.contains(&model) {
            self.warn(format!(
                "{model} is not in the model catalog: using conservative defaults (a {} token \
                 window, no known price); describe it under \
                 [catalog_overrides.models.\"{model}\"]",
                info.caps.context_window
            ))
            .await;
        }
        self.install_model(&model, provider, side, side_note).await;
        self.context.strip_reasoning(Some(&label));
        if !same_provider {
            // The learned ratio is the old provider's tokenizer's.
            self.context.tokens = TokenEstimator::default();
        }
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

    /// Put `model` in place with its `provider` and `side` route: capabilities, window,
    /// tools (MCP tools kept), whether USD budgets apply, and the side model. The context is
    /// left as it is (the reasoning and the estimator are the caller's).
    async fn install_model(
        &mut self,
        model: &ModelId,
        provider: Arc<dyn Provider>,
        side: Side,
        side_note: Option<String>,
    ) {
        if let Some(note) = side_note {
            self.warn(note).await;
        }
        let info = self.setup.catalog.get(model);
        self.provider = provider;
        // MCP tools stay; the next user turn refreshes them as usual.
        let mcp: Vec<Arc<dyn Tool>> = self
            .tools
            .iter()
            .filter(|t| t.name().starts_with(MCP_TOOL_PREFIX))
            .cloned()
            .collect();
        self.tools = registry(&info.caps, mcp);
        self.specs = self.tools.iter().map(|t| t.spec(&info.caps)).collect();
        self.budgets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_model(info.price.is_some(), model.as_str());
        self.info = info;
        self.model = model.clone();
        self.setup.config.model = model.clone();
        // What the old side model was asked is its own spend; then the new route.
        self.drain_usage().await;
        self.side = side;
    }

    /// A resumed session or a fork continues on the model its log ends on (`model`: the
    /// last `ModelSet` replayed, else the header's), whose `ModelSet` and
    /// `ReasoningStripped` records the rebuild already applied to the context. Announced
    /// with `ModelChanged`. When that model cannot be used now (no key, say) the session
    /// stays on its own model, which is logged as a `ModelSet`, with a `Warning`; the next
    /// step start strips another provider's reasoning as after any switch. The provider
    /// build may wait on a keychain prompt: `stop` (a `Shutdown`, or the fork's cancel)
    /// abandons it, and the session stays on its own model the same way.
    pub(crate) async fn restore_model(&mut self, model: ModelId, stop: impl Future<Output = ()>) {
        if model == self.model {
            return;
        }
        let built = tokio::select! {
            biased;
            built = self.build_provider(&model) => built,
            () = stop => Err(ProviderSetupError::Other("the session is shutting down".into())),
        };
        match built {
            Ok((provider, side, side_note)) => {
                self.install_model(&model, provider, side, side_note).await;
                self.emit(Event::ModelChanged {
                    provider: provider_label(&model),
                    model,
                })
                .await;
            }
            Err(e) => {
                let current = self.model.clone();
                self.warn(format!(
                    "this session was on {model}, which cannot be used now ({e}); it \
                     continues on {current}"
                ))
                .await;
                let label = provider_label(&current);
                if self.context.reasoning_provider.as_deref() != Some(label.as_str()) {
                    self.context.tokens = TokenEstimator::default();
                }
                self.context.strip_reasoning(Some(&label));
                self.reasoning_retry = false;
                self.context.force_cold_epoch();
                self.log(SessionRecord::ModelSet { model: current }).await;
                self.checkpoint().await;
            }
        }
    }

    /// The new main provider (wrapped in `Retrying`), the side route for it, and why the
    /// side model could not be used, if so. Both are built on the blocking pool.
    async fn build_provider(&self, model: &ModelId) -> Result<Built, ProviderSetupError> {
        self.setup.check(model)?;
        let factory = Arc::clone(&self.setup.factory);
        let catalog = Arc::clone(&self.setup.catalog);
        let mut config = self.setup.config.clone();
        config.model = model.clone();
        tokio::task::spawn_blocking(move || {
            let main: Arc<dyn Provider> = Arc::new(Retrying::new(factory(&config.model)?));
            let (side, note) = Side::resolve_with(&config, &catalog, &main, None, |id| factory(id));
            Ok((main, side, note))
        })
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
