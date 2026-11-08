//! The `reasoning` setting (spec 4.7): how hard the main model thinks. Resolved against
//! the model's catalog reasoning style into the request's [`Reasoning`] at every step;
//! side-model requests (compaction summaries, memory work, `web_fetch` answers) always
//! go without it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tautly_provider::{Effort, Reasoning, ReasoningStyle};

/// `reasoning = "auto" | "off" | "low" | "medium" | "high"` (default `auto`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    /// `medium` for a model whose catalog entry has a reasoning style, else off.
    #[default]
    Auto,
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningLevel {
    pub const NAMES: &str = "auto, off, low, medium or high";

    /// What a request to a model of reasoning `style` carries: nothing when off or when
    /// the model cannot reason; an effort for effort-style models; the level's token
    /// budget (4k, 12k, 32k; adapters cap it below the output limit) for budget-style
    /// ones.
    pub fn resolve(self, style: ReasoningStyle) -> Option<Reasoning> {
        let effort = match self {
            ReasoningLevel::Off => return None,
            ReasoningLevel::Low => Effort::Low,
            ReasoningLevel::Auto | ReasoningLevel::Medium => Effort::Medium,
            ReasoningLevel::High => Effort::High,
        };
        match style {
            ReasoningStyle::None => None,
            ReasoningStyle::Effort => Some(Reasoning::Effort(effort)),
            ReasoningStyle::Budget => Some(Reasoning::Budget(effort.budget_tokens())),
        }
    }

    /// How much thinking the level asks for, for the project layer's lower-only rule:
    /// `auto` counts as `medium`, which it is for every model that can reason.
    pub fn rank(self) -> u8 {
        match self {
            ReasoningLevel::Off => 0,
            ReasoningLevel::Low => 1,
            ReasoningLevel::Auto | ReasoningLevel::Medium => 2,
            ReasoningLevel::High => 3,
        }
    }
}

impl FromStr for ReasoningLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(ReasoningLevel::Auto),
            "off" => Ok(ReasoningLevel::Off),
            "low" => Ok(ReasoningLevel::Low),
            "medium" => Ok(ReasoningLevel::Medium),
            "high" => Ok(ReasoningLevel::High),
            _ => Err(format!(
                "`{s}` is not a reasoning level (use {})",
                ReasoningLevel::NAMES
            )),
        }
    }
}

impl fmt::Display for ReasoningLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ReasoningLevel::Auto => "auto",
            ReasoningLevel::Off => "off",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High => "high",
        })
    }
}
