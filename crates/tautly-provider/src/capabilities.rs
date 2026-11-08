use serde::{Deserialize, Serialize};

/// How a model's reasoning is controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStyle {
    /// It cannot reason.
    None,
    /// A thinking-token budget.
    Budget,
    /// An effort level, which `off` turns off.
    Effort,
    /// Effort-controlled, and thinking cannot be turned off: "off" sends the lowest effort.
    EffortAlways,
}

/// How a model's prompt cache works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStyle {
    /// The request marks breakpoints (Anthropic).
    Explicit,
    /// The provider caches prefixes on its own.
    Automatic,
    /// No prompt cache.
    None,
}

/// How the model sends edits: the `edit` tool's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    /// Replace an exact (tolerantly matched) string.
    StrReplace,
    /// A multi-file patch (`*** Begin Patch`).
    Patch,
    /// Edit by line anchors (`<n>:<hash>`) as `read` prints them.
    Hashline,
}

/// What a model supports, from the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Context window, in tokens.
    pub context_window: u32,
    /// Most output tokens per response.
    pub max_output: u32,
    /// Whether it can call several tools in one response.
    pub parallel_tools: bool,
    /// How its reasoning is controlled.
    pub reasoning: ReasoningStyle,
    /// How its prompt cache works.
    pub cache: CacheStyle,
    /// Whether it accepts images.
    pub images: bool,
    /// The edit format it works best with.
    pub edit_format: EditFormat,
}

impl Capabilities {
    /// Conservative defaults for models missing from the catalog.
    pub fn conservative() -> Self {
        Capabilities {
            context_window: 32_000,
            max_output: 4_000,
            parallel_tools: false,
            reasoning: ReasoningStyle::None,
            cache: CacheStyle::None,
            images: false,
            edit_format: EditFormat::Hashline,
        }
    }
}
