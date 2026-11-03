//! Side-model routing (spec 7.5): cheap work that is not the conversation (compaction
//! summaries, `web_fetch` answers, later memory work) goes to a small model.
//!
//! [`side_model_id`] picks the model: `[routing] side_model`, else the catalog's `small`
//! tier model of the main model's provider, else the main model itself. [`SideRunner`] is
//! the [`SideModel`] the tools get: one request with no tools and no reasoning, whose usage
//! it reports through a channel. The session drains that channel at tool boundaries and
//! accounts every request like a main one, but with the side model's id and price (ledger
//! row, `Usage{role: side}`, budgets), never the main model's.
//!
//! A side model that fails for good falls back to the main model for the rest of the
//! session (`Session::side_to_main`). A `SetModel` resolves the side route again for the
//! new main model ([`Side::resolve_with`]), which also ends such a fallback: an explicit
//! switch is a fresh start, and a side model that still fails falls back again, with its
//! one warning.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use tautly_provider::{
    Block, CanonicalRequest, Catalog, Content, Message, ModelId, ModelInfo, Provider,
    ProviderError, ProviderEvent, RawUsage, Retrying, Role, SideModel, Tier,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::provider_setup::{ProviderSetupError, provider_for};
use crate::turn::partial_usage;

/// What one side-model request billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideUsage {
    pub usage: RawUsage,
    /// Some of the numbers are the engine's estimate: the request broke off or was dropped
    /// before the provider said what it billed.
    pub estimated: bool,
}

/// Room for the usage reports waiting for the session to drain them. A tool that asks the
/// side model is a network call, and those run one at a time, so a handful is plenty.
pub(crate) const USAGE_CAPACITY: usize = 64;

/// The provider part of a model id: what comes before the first `/` (so two OpenAI-compatible
/// endpoints, `compat:a` and `compat:b`, are two providers).
fn provider_of(id: &ModelId) -> &str {
    id.as_str().split('/').next().unwrap_or("")
}

/// The model for side work: `routing.side_model`; else the main model if it is itself small;
/// else the first `small`-tier catalog model of the main model's provider; else the main
/// model.
pub fn side_model_id(cfg: &Config, catalog: &Catalog) -> ModelId {
    if let Some(m) = &cfg.routing.side_model {
        return m.clone();
    }
    let main = &cfg.model;
    if catalog.get(main).tier == Some(Tier::Small) {
        return main.clone();
    }
    catalog
        .ids()
        .find(|id| {
            provider_of(id) == provider_of(main) && catalog.get(id).tier == Some(Tier::Small)
        })
        .unwrap_or_else(|| main.clone())
}

/// About four bytes per token, the estimator's rule before calibration.
fn estimate(bytes: usize) -> u64 {
    bytes.div_ceil(4) as u64
}

/// The error says the side model itself is unusable (bad key, not available on the
/// account, request refused): trying it again will not help, so the session falls back to
/// the main model. Transient errors were already retried; a context overflow is about the
/// input, not the model.
pub(crate) fn model_failed(e: &ProviderError) -> bool {
    matches!(
        e,
        ProviderError::Auth(_) | ProviderError::InvalidRequest(_) | ProviderError::Other(_)
    )
}

/// Room kept free for the answer and the instructions when sizing input to a window.
const WINDOW_RESERVE_TOKENS: u32 = 2_500;

/// A [`SideModel`] backed by a provider. Every request's usage goes to `usage`; a request
/// that fails with [`model_failed`] is remembered for the session
/// ([`Side::take_failure`]).
///
/// Usage can be under-reported: a request that fails before any content arrives reports
/// nothing, even if the provider billed for it.
pub struct SideRunner {
    pub provider: Arc<dyn Provider>,
    pub model: ModelId,
    pub usage: mpsc::Sender<SideUsage>,
    window: u32,
    failure: Arc<Mutex<Option<String>>>,
}

impl SideRunner {
    /// `window` is the model's context window in tokens (it sizes [`SideModel::max_input_bytes`]).
    pub fn new(
        provider: Arc<dyn Provider>,
        model: ModelId,
        usage: mpsc::Sender<SideUsage>,
        window: u32,
    ) -> SideRunner {
        SideRunner {
            provider,
            model,
            usage,
            window,
            failure: Arc::default(),
        }
    }
}

/// Reports what a request used, once, whichever way the request ends: complete, failed, or
/// dropped half way (a cancelled call). Nothing is reported for a request that produced
/// nothing.
struct Meter<'a> {
    tx: &'a mpsc::Sender<SideUsage>,
    est_input: u64,
    streamed_bytes: usize,
    reported: Option<RawUsage>,
    settled: bool,
}

impl Meter<'_> {
    fn send(&self, usage: RawUsage, estimated: bool) {
        // Never waits: a full channel means the session stopped draining, and the report
        // is dropped rather than blocking a tool.
        let _ = self.tx.try_send(SideUsage { usage, estimated });
    }

    /// Report the request as it stands. `complete`: the stream ended normally (a response
    /// with no usage at all is then estimated); otherwise only what was streamed counts.
    fn settle(&mut self, complete: bool) {
        if std::mem::replace(&mut self.settled, true) {
            return;
        }
        if complete || self.reported.is_some() || self.streamed_bytes > 0 {
            let (usage, estimated) =
                partial_usage(self.reported, self.est_input, estimate(self.streamed_bytes));
            self.send(usage, estimated);
        }
    }
}

impl Drop for Meter<'_> {
    fn drop(&mut self) {
        self.settle(false);
    }
}

#[async_trait]
impl SideModel for SideRunner {
    /// About three bytes per token of the window, less room for the answer and the prompt.
    fn max_input_bytes(&self) -> usize {
        self.window.saturating_sub(WINDOW_RESERVE_TOKENS) as usize * 3
    }

    async fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, ProviderError> {
        let req = CanonicalRequest {
            model: self.model.clone(),
            system: vec![Block::Text(system.to_string())],
            tools: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text(user.to_string())],
            }],
            max_output: max_tokens,
            reasoning: None,
            cache_breakpoints: Vec::new(),
            volatile_tail: false,
        };
        let mut meter = Meter {
            tx: &self.usage,
            est_input: estimate(system.len() + user.len()),
            streamed_bytes: 0,
            reported: None,
            settled: false,
        };
        let mut stream = self.provider.stream(req, CancellationToken::new());
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(ProviderEvent::TextDelta(t)) => {
                    meter.streamed_bytes += t.len();
                    text.push_str(&t);
                }
                Ok(ProviderEvent::Usage(u)) => meter.reported = Some(u),
                // An attempt the retry wrapper discarded: billed, and a request of its own.
                Ok(ProviderEvent::DiscardedUsage(u)) => meter.send(u, false),
                Ok(_) => {}
                Err(e) => {
                    meter.settle(false);
                    if model_failed(&e) {
                        *self.failure.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(e.to_string());
                    }
                    return Err(e);
                }
            }
        }
        meter.settle(true);
        Ok(text.trim().to_string())
    }
}

/// A session's side model: where its requests go, what it costs, and the channel its
/// [`SideRunner`] reports usage on.
pub(crate) struct Side {
    pub provider: Arc<dyn Provider>,
    pub model: ModelId,
    pub info: ModelInfo,
    /// For tools (`ToolCx::with_side`).
    pub runner: Arc<dyn SideModel>,
    pub usage: mpsc::Receiver<SideUsage>,
    failure: Arc<Mutex<Option<String>>>,
}

impl Side {
    pub fn new(provider: Arc<dyn Provider>, model: ModelId, info: ModelInfo) -> Side {
        let (tx, usage) = mpsc::channel(USAGE_CAPACITY);
        let runner = SideRunner::new(
            Arc::clone(&provider),
            model.clone(),
            tx,
            info.caps.context_window,
        );
        let failure = Arc::clone(&runner.failure);
        Side {
            provider,
            model,
            info,
            runner: Arc::new(runner),
            usage,
            failure,
        }
    }

    /// Why a tool's side request failed for good ([`model_failed`]), once.
    pub fn take_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// The side route of a session whose main provider is `main` (already wrapped in
    /// `Retrying`). `provider` serves the side model when the caller supplies one;
    /// otherwise the main provider does if the side model has the same provider, and an
    /// adapter built from the configuration (API key lookup, which may block) when not. If
    /// none can be built the main model does side work, and the second value says so.
    pub fn resolve(
        config: &Config,
        catalog: &Arc<Catalog>,
        main: &Arc<dyn Provider>,
        provider: Option<Arc<dyn Provider>>,
    ) -> (Side, Option<String>) {
        Side::resolve_with(config, catalog, main, provider, |id| {
            provider_for(config, id, Arc::clone(catalog))
        })
    }

    /// [`resolve`](Self::resolve) with `build` making the adapter for a side model of
    /// another provider (`SetModel` passes its provider factory). It may block.
    pub fn resolve_with(
        config: &Config,
        catalog: &Arc<Catalog>,
        main: &Arc<dyn Provider>,
        provider: Option<Arc<dyn Provider>>,
        build: impl FnOnce(&ModelId) -> Result<Arc<dyn Provider>, ProviderSetupError>,
    ) -> (Side, Option<String>) {
        let id = side_model_id(config, catalog);
        let same_as_main = |id: &ModelId| Side::new(Arc::clone(main), id.clone(), catalog.get(id));
        if id == config.model {
            return (same_as_main(&id), None);
        }
        let built = match provider {
            Some(p) => Ok(Arc::new(Retrying::new(p)) as Arc<dyn Provider>),
            None if provider_of(&id) == provider_of(&config.model) => Ok(Arc::clone(main)),
            None => build(&id).map(|p| Arc::new(Retrying::new(p)) as Arc<dyn Provider>),
        };
        match built {
            Ok(p) => (Side::new(p, id.clone(), catalog.get(&id)), None),
            Err(why) => {
                let note = format!(
                    "the side model {id} cannot be used ({why}); the main model {} does \
                     summaries and web_fetch answers instead",
                    config.model
                );
                (same_as_main(&config.model), Some(note))
            }
        }
    }
}
