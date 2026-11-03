//! The system prompt: fixed text with no per-model or per-turn content.

pub fn system() -> &'static str {
    include_str!("../prompts/system.md")
}
