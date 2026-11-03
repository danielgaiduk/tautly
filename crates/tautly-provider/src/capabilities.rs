use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStyle {
    None,
    Budget,
    Effort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStyle {
    Explicit,
    Automatic,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditFormat {
    StrReplace,
    Patch,
    Hashline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub context_window: u32,
    pub max_output: u32,
    pub parallel_tools: bool,
    pub reasoning: ReasoningStyle,
    pub cache: CacheStyle,
    pub images: bool,
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
