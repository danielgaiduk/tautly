//! Pinned items: constraints and facts that are never summarized or dropped.

pub use hx_tools::MAX_PIN_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    User,
    Config,
    Model,
    AgentsMd,
}

impl PinSource {
    /// The tag a pin is rendered with, so the model can tell who pinned it.
    pub fn label(self) -> &'static str {
        match self {
            PinSource::User => "user",
            PinSource::Config => "config",
            PinSource::Model => "model",
            PinSource::AgentsMd => "AGENTS.md",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub text: String,
    pub source: PinSource,
}

impl Pinned {
    pub fn new(text: impl Into<String>, source: PinSource) -> Self {
        Self {
            text: text.into(),
            source,
        }
    }
}

/// Why [`Context::pin`](crate::Context::pin) refused a pin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinError {
    #[error("a pin must not be empty")]
    Empty,
    /// Over [`MAX_PIN_BYTES`] (trimmed). Refused rather than cut: a constraint cut short
    /// can mean something else.
    #[error("the pin is {bytes} bytes; a pin holds at most {max} bytes")]
    TooLong { bytes: usize, max: usize },
}

/// Add a pin unless an identical text is already pinned.
pub fn add(pinned: &mut Vec<Pinned>, pin: Pinned) {
    if !pinned.iter().any(|p| p.text == pin.text) {
        pinned.push(pin);
    }
}

/// The `<pinned>` block, or None when nothing is pinned.
pub fn render(pinned: &[Pinned]) -> Option<String> {
    render_tagged("pinned", pinned)
}

/// The `<pinned_this_session>` block of the state tail: pins added since the session
/// started, not yet folded into `<pinned>`. None when there are none.
pub fn render_session(pinned: &[Pinned]) -> Option<String> {
    render_tagged("pinned_this_session", pinned)
}

/// One `- [source] text` item per pin. Continuation lines are indented, so a pin cannot
/// start an item of its own (a model pin forging `- [user] ...`), and closing tags of the
/// blocks pins appear in are neutralized, so a pin cannot end its block.
fn render_tagged(tag: &str, pinned: &[Pinned]) -> Option<String> {
    if pinned.is_empty() {
        return None;
    }
    let mut out = format!("<{tag}>\n");
    for p in pinned {
        out.push_str("- [");
        out.push_str(p.source.label());
        out.push_str("] ");
        let text = neutralize(p.text.trim());
        let mut lines = text.lines();
        out.push_str(lines.next().unwrap_or_default());
        out.push('\n');
        for line in lines {
            if !line.is_empty() {
                out.push_str("  ");
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!("</{tag}>"));
    Some(out)
}

/// `text` with every `</pinned` and `</working_state` (any case) written `&lt;/...`.
fn neutralize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("</") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let closes = ["pinned", "working_state"].iter().any(|tag| {
            after
                .as_bytes()
                .get(..tag.len())
                .is_some_and(|b| b.eq_ignore_ascii_case(tag.as_bytes()))
        });
        out.push_str(if closes { "&lt;/" } else { "</" });
        rest = after;
    }
    out.push_str(rest);
    out
}
