//! Slash commands (spec 7.11.8).

/// A line that starts with `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    /// `/model [id]`: switch, or list the current model and those with a key.
    Model(Option<String>),
    /// `/cost`: the last turn's and the session's receipt.
    Cost,
    /// `/diff [N]`: the session's (or user turn N's) net change.
    Diff(Option<u32>),
    /// `/undo [N]`: show the dry run, ask, then undo.
    Undo(Option<u32>),
    /// `/resume [n|id]`: list the work tree's last sessions, or resume one.
    Resume(Option<String>),
    /// `/compact`: compact the conversation now.
    Compact,
    /// `/pin <text>`: keep `text` in the context.
    Pin(String),
    /// `/unpin <text>`: remove that pin.
    Unpin(String),
    /// `/fork`: branch the conversation into a new session.
    Fork,
    /// `/rewind N`: show the dry run, ask, then rewind files and conversation to step N
    /// as a new branch.
    Rewind(u32),
    /// `/tree`: the project's sessions as a tree.
    Tree,
    /// `/switch <label|id|n>`: show the dry run, ask, then continue that branch.
    Switch(String),
    /// `/label <name>`: rename the current branch.
    Label(String),
    /// `/replay [id]`: play this session (or session id) back from its log.
    Replay(Option<String>),
    /// `/reasoning <level>`: how hard the model thinks from the next request on.
    Reasoning(tautly_core::ReasoningLevel),
    /// `/memory consolidate`: propose merging and pruning project memory.
    MemoryConsolidate,
    /// `/help`: list the commands.
    Help,
    /// Anything else (or a known command with arguments it does not take): the help is
    /// printed.
    Unknown(String),
}

/// What `/help` (and an unknown command) prints.
pub const HELP: &str = "\
commands:
  /model [id]          switch model; without an id, list the current one and those with a key
  /reasoning <level>   how hard the model thinks: auto, off, low, medium or high
  /cost                the last turn's and the session's receipt
  /diff [N]            what the session (or user turn N) changed
  /undo [N]            undo the last turn (or turn N), after showing what it would do
  /resume [n|id]       list this work tree's last sessions, or resume one
  /compact             compact the conversation now
  /pin <text>          keep <text> in the context
  /unpin <text>        remove that pin
  /fork                branch the conversation into a new session
  /rewind N            rewind files and conversation to step N as a new branch
  /tree                this project's sessions as a tree (* marks the current one)
  /switch <label|id|n> continue another branch (n: its line in /tree), files included
  /label <name>        rename the current branch
  /replay [id]         play this session (or another) back from its log; Ctrl+C stops
  /memory consolidate  propose merging and pruning project memory
  /help                this list
keys: Ctrl+C cancels a turn (twice within 2 s exits), clears the line at the prompt;
      Ctrl+D on an empty line exits; Alt+Enter starts a new line";

/// The slash command `line` names, or None when it is a message (no leading `/`).
pub fn parse(line: &str) -> Option<Slash> {
    let line = line.trim();
    let rest = line.strip_prefix('/')?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let arg = (!arg.is_empty()).then(|| arg.to_string());
    let unknown = || Slash::Unknown(line.to_string());
    let number = |a: &Option<String>| match a {
        None => Some(None),
        Some(s) => s.parse::<u32>().ok().filter(|n| *n > 0).map(Some),
    };
    Some(match (name, arg) {
        ("model", a) => Slash::Model(a),
        ("cost", None) => Slash::Cost,
        ("diff", a) => number(&a).map_or_else(unknown, Slash::Diff),
        ("undo", a) => number(&a).map_or_else(unknown, Slash::Undo),
        ("resume", a) => Slash::Resume(a),
        ("compact", None) => Slash::Compact,
        ("pin", Some(t)) => Slash::Pin(t),
        ("unpin", Some(t)) => Slash::Unpin(t),
        ("fork", None) => Slash::Fork,
        ("rewind", Some(a)) => a.parse::<u32>().map_or_else(|_| unknown(), Slash::Rewind),
        ("tree", None) => Slash::Tree,
        ("switch", Some(a)) => Slash::Switch(a),
        ("label", Some(a)) => Slash::Label(a),
        ("replay", a) => Slash::Replay(a),
        ("reasoning", Some(a)) => a.parse().map_or_else(|_| unknown(), Slash::Reasoning),
        ("memory", Some(a)) if a == "consolidate" => Slash::MemoryConsolidate,
        ("help", None) => Slash::Help,
        _ => unknown(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_rejects_bad_arguments() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("/model"), Some(Slash::Model(None)));
        assert_eq!(parse("/model a/b"), Some(Slash::Model(Some("a/b".into()))));
        assert_eq!(parse("/diff 2"), Some(Slash::Diff(Some(2))));
        assert_eq!(parse(" /diff "), Some(Slash::Diff(None)));
        assert_eq!(parse("/undo"), Some(Slash::Undo(None)));
        assert_eq!(
            parse("/pin  use pnpm "),
            Some(Slash::Pin("use pnpm".into()))
        );
        assert_eq!(parse("/memory consolidate"), Some(Slash::MemoryConsolidate));
        assert_eq!(parse("/rewind 3"), Some(Slash::Rewind(3)));
        assert_eq!(parse("/rewind 0"), Some(Slash::Rewind(0)));
        assert_eq!(parse("/tree"), Some(Slash::Tree));
        assert_eq!(parse("/switch try-b"), Some(Slash::Switch("try-b".into())));
        assert_eq!(parse("/label main"), Some(Slash::Label("main".into())));
        assert_eq!(parse("/replay"), Some(Slash::Replay(None)));
        assert_eq!(
            parse("/reasoning high"),
            Some(Slash::Reasoning(tautly_core::ReasoningLevel::High))
        );
        assert_eq!(
            parse("/reasoning off"),
            Some(Slash::Reasoning(tautly_core::ReasoningLevel::Off))
        );
        assert_eq!(
            parse("/replay 01J"),
            Some(Slash::Replay(Some("01J".into())))
        );
        for bad in [
            "/diff x",
            "/diff 0",
            "/rewind",
            "/rewind x",
            "/switch",
            "/label",
            "/tree x",
            "/pin",
            "/memory",
            "/cost now",
            "/reasoning",
            "/reasoning max",
            "/nope",
        ] {
            assert_eq!(parse(bad), Some(Slash::Unknown(bad.into())), "{bad}");
        }
    }
}
