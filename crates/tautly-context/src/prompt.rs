//! The system prompt: fixed text with no per-model or per-turn content.

/// The system prompt (`prompts/system.md`).
pub fn system() -> &'static str {
    include_str!("../prompts/system.md")
}
