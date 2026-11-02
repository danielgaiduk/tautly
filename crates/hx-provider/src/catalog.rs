use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    canonical::ModelId,
    capabilities::{CacheStyle, Capabilities, EditFormat, ReasoningStyle},
};

const BUILTIN: &str = include_str!("../catalog/models.toml");

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub caps: Capabilities,
    pub price: Option<Price>,
}

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
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    models: BTreeMap<String, Entry>,
}

impl Catalog {
    /// The catalog shipped with hx.
    pub fn builtin() -> Catalog {
        Catalog::from_toml(BUILTIN).expect("built-in models.toml is valid")
    }

    pub fn from_toml(s: &str) -> Result<Catalog, CatalogError> {
        Ok(toml::from_str(s)?)
    }

    /// Entries in `overrides` replace same-id entries; others are untouched.
    pub fn merge(&mut self, overrides: Catalog) {
        self.models.extend(overrides.models);
    }

    pub fn contains(&self, id: &ModelId) -> bool {
        self.models.contains_key(id.as_str())
    }

    /// Unknown models get conservative defaults and no price.
    pub fn get(&self, id: &ModelId) -> ModelInfo {
        self.models.get(id.as_str()).map_or_else(
            || ModelInfo { caps: Capabilities::conservative(), price: None },
            Entry::info,
        )
    }
}
