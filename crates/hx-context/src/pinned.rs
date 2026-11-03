//! Pinned items: constraints and facts that are never summarized or dropped.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    User,
    Config,
    Model,
    AgentsMd,
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

/// Add a pin unless an identical text is already pinned.
pub fn add(pinned: &mut Vec<Pinned>, pin: Pinned) {
    if !pinned.iter().any(|p| p.text == pin.text) {
        pinned.push(pin);
    }
}

/// The `<pinned>` block, or None when nothing is pinned.
pub fn render(pinned: &[Pinned]) -> Option<String> {
    if pinned.is_empty() {
        return None;
    }
    let mut out = String::from("<pinned>\n");
    for p in pinned {
        out.push_str("- ");
        out.push_str(p.text.trim());
        out.push('\n');
    }
    out.push_str("</pinned>");
    Some(out)
}
