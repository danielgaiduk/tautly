use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    canonical::ModelId,
    capabilities::{CacheStyle, Capabilities, EditFormat, ReasoningStyle},
};

const BUILTIN: &str = include_str!("../catalog/models.toml");

/// Cache lifetime assumed for models with a prompt cache (`explicit` or `automatic`) whose
/// catalog entry sets no `cache_ttl_s`.
pub const DEFAULT_CACHE_TTL_S: u32 = 300;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// Input tokens not read from the cache.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Input tokens read from the cache.
    pub cache_read: f64,
    /// Input tokens written to the cache.
    pub cache_write: f64,
}

/// Size class of a model: side work (summaries, `web_fetch` answers) goes to the
/// provider's `Small` model when the user names none (spec 7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A small, cheap model.
    Small,
    /// Only `Small` has a meaning today (side routing looks for it); `Large` marks a model
    /// as not small, for the catalog's vocabulary.
    Large,
}

/// What the catalog knows about one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// Its capabilities.
    pub caps: Capabilities,
    /// Its price; None when unknown (then USD budgets cannot be enforced).
    pub price: Option<Price>,
    /// How long the provider keeps a prompt-cache entry alive after a request, in seconds;
    /// `None` when the model has no prompt cache.
    pub cache_ttl_s: Option<u32>,
    /// Size class; `None` when the catalog does not say (and for unknown models).
    pub tier: Option<Tier>,
}

/// A catalog TOML document that does not parse.
#[derive(Debug, thiserror::Error)]
#[error("invalid model catalog: {0}")]
pub struct CatalogError(#[from] toml::de::Error);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    context_window: u32,
    max_output: u32,
    parallel_tools: bool,
    reasoning: ReasoningStyle,
    cache: CacheStyle,
    images: bool,
    edit_format: EditFormat,
    price: Option<Price>,
    /// Overrides the default for the cache style (see [`DEFAULT_CACHE_TTL_S`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_ttl_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tier: Option<Tier>,
    /// The largest thinking budget the model takes (Gemini), when documented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking_budget_max: Option<u32>,
}

impl Entry {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            caps: Capabilities {
                context_window: self.context_window,
                max_output: self.max_output,
                parallel_tools: self.parallel_tools,
                reasoning: self.reasoning,
                cache: self.cache,
                images: self.images,
                edit_format: self.edit_format,
            },
            price: self.price,
            cache_ttl_s: self.cache_ttl_s.or(match self.cache {
                CacheStyle::None => None,
                CacheStyle::Explicit | CacheStyle::Automatic => Some(DEFAULT_CACHE_TTL_S),
            }),
            tier: self.tier,
        }
    }
}

/// The model catalog: capabilities and prices by model id (`catalog/models.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    models: BTreeMap<String, Entry>,
}

impl Catalog {
    /// The catalog shipped with Tautly.
    pub fn builtin() -> Catalog {
        Catalog::from_toml(BUILTIN).expect("built-in models.toml is valid")
    }

    /// Parse a catalog from TOML (`[models."provider/model"]` tables).
    pub fn from_toml(s: &str) -> Result<Catalog, CatalogError> {
        Ok(toml::from_str(s)?)
    }

    /// Entries in `overrides` replace same-id entries; others are untouched.
    pub fn merge(&mut self, overrides: Catalog) {
        self.models.extend(overrides.models);
    }

    /// Make `id` edit with `format` (`--edit-format`). A model not in the catalog gets an
    /// entry with the conservative capabilities and no price, as [`get`](Self::get) says.
    pub fn set_edit_format(&mut self, id: &ModelId, format: EditFormat) {
        let entry = self
            .models
            .entry(id.as_str().to_string())
            .or_insert_with(|| {
                let c = Capabilities::conservative();
                Entry {
                    context_window: c.context_window,
                    max_output: c.max_output,
                    parallel_tools: c.parallel_tools,
                    reasoning: c.reasoning,
                    cache: c.cache,
                    images: c.images,
                    edit_format: c.edit_format,
                    price: None,
                    cache_ttl_s: None,
                    tier: None,
                    thinking_budget_max: None,
                }
            });
        entry.edit_format = format;
    }

    /// The largest thinking budget the model takes, when the catalog says
    /// (`thinking_budget_max`); adapters clamp to it.
    pub fn thinking_budget_max(&self, id: &ModelId) -> Option<u32> {
        self.models.get(id.as_str())?.thinking_budget_max
    }

    /// Whether the catalog has an entry for `id`.
    pub fn contains(&self, id: &ModelId) -> bool {
        self.models.contains_key(id.as_str())
    }

    /// Every model id in the catalog, in id order.
    pub fn ids(&self) -> impl Iterator<Item = ModelId> + '_ {
        self.models.keys().map(|k| ModelId::from(k.as_str()))
    }

    /// Unknown models get conservative defaults and no price.
    pub fn get(&self, id: &ModelId) -> ModelInfo {
        self.models.get(id.as_str()).map_or_else(
            || ModelInfo {
                caps: Capabilities::conservative(),
                price: None,
                cache_ttl_s: None,
                tier: None,
            },
            Entry::info,
        )
    }
}
