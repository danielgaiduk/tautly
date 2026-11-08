//! The shell parser and the permission policy's command matching.
//!
//! Input: a shell string, then optionally `\0` and an allow-rule pattern. Invariants:
//! - `parse_commands` never panics;
//! - the word invariant: the input with `@` replaced by the marker word either is
//!   unparseable or shows the marker in some argv word, assignment or redirect target
//!   (checked when every `@` stands as its own word, so the marker is one; see
//!   `marker_is_a_word`);
//! - `exact_rule` names every single, plain command, and the rule it names matches it;
//! - a command allowed by a rule whose pattern is literal text and `*` holds every
//!   literal fragment of the pattern in its words.
#![no_main]

use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;
use tautly_sandbox::{Effect, Mode, Policy, PolicyConfig, parse_commands};

const MARK: &str = "ZZMARK";

fn policy(allow: Vec<String>) -> Option<Policy> {
    let cfg = PolicyConfig {
        mode: Mode::Ask,
        allow,
        deny: Vec::new(),
    };
    // A root that does not exist: nothing on disk is followed, so runs are repeatable.
    Policy::new(cfg, PathBuf::from("/nonexistent-tautly-fuzz-root")).ok()
}

/// Every `@` stands alone between plain spaces (or the ends), outside any comment,
/// heredoc delimiter or body, arithmetic or keyword position the input could open: then
/// the marker reaches the command (as a word, or inside a quoted or substituted one).
fn marker_is_a_word(src: &str) -> bool {
    if !src.contains('@') || src.contains('#') || src.contains("((") || src.contains("[[") {
        return false;
    }
    if let Some(i) = src.find("<<") {
        // Heredoc bodies start on the line after the operator; its delimiter may be quoted.
        let line_end = src[i..].find('\n').map_or(src.len(), |n| i + n);
        if src[line_end..].contains('@') || src.contains(['\'', '"', '\\']) {
            return false;
        }
    }
    let b = src.as_bytes();
    // Keywords make the next word a name or a list item, not an argument.
    let keyword = |w: &str| {
        matches!(
            w,
            "for" | "case" | "in" | "select" | "function" | "do" | "done" | "then" | "else"
                | "elif" | "fi" | "esac" | "if" | "while" | "until" | "!" | "{" | "}"
                | "time" | "coproc" | "declare" | "typeset" | "local" | "export"
                | "readonly"
                // A test's operands are judged in its syntax tree (arithmetic, `-v`
                // names, substitutions), not as argv: `[ … ]` parses as `[[`.
                | "["
        )
    };
    let words: Vec<&str> = src.split(|c: char| c.is_whitespace()).collect();
    if words.iter().any(|w| keyword(w)) {
        return false;
    }
    // After a file redirect operator the marker is its target; after any other
    // (heredoc, herestring, descriptor duplication) it is no word of the command.
    let file_ops = [">", ">>", "<", "&>", "&>>", ">|", "<>", "2>", "2>>"];
    let after_other_op = words
        .windows(2)
        .any(|w| w[1] == "@" && w[0].contains(['<', '>']) && !file_ops.contains(&w[0]));
    if after_other_op {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| {
        c != b'@' || ((i == 0 || b[i - 1] == b' ') && (i + 1 == b.len() || b[i + 1] == b' '))
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (src, pattern) = text.split_once('\0').unwrap_or((text, ""));
    if src.len() > 4096 {
        return;
    }

    // Word invariant.
    if marker_is_a_word(src) {
        let marked = src.replace('@', MARK);
        if let Ok(cmds) = parse_commands(&marked) {
            let seen = cmds.iter().any(|c| {
                c.argv.iter().any(|w| w.contains(MARK))
                    || c.assignments.iter().any(|w| w.contains(MARK))
                    || c.redirects.iter().any(|r| r.target.contains(MARK))
            });
            // A function definition's body runs later, under its name; with no command
            // at all the input is bare assignments, whose quoted values may hold it.
            let defines = marked.contains("()");
            assert!(
                seen || defines || cmds.is_empty(),
                "{marked:?} parsed as {cmds:?} but hides {MARK}"
            );
        }
    }

    let parsed = parse_commands(src);
    let Some(p) = policy(Vec::new()) else {
        return;
    };
    let effect = Effect::Exec { argv: Vec::new() };

    // exact_rule: names a single plain command and matches it.
    let exact = p.exact_rule("bash", &effect, Some(src));
    if let Some(rule) = &exact {
        assert_eq!(
            p.rule_matches(rule, "bash", &effect, Some(src)).ok(),
            Some(true),
            "exact rule {rule:?} does not match {src:?}"
        );
        // The rule must also load as a configured allow rule.
        assert!(
            policy(vec![rule.clone()]).is_some(),
            "{rule:?} does not load"
        );
    }

    // Literal allow rules: what a command matches, its words hold.
    // `**/` may match nothing at all, slash included: not literal text.
    let pattern_ok = !pattern.is_empty()
        && pattern.len() < 64
        && !pattern.contains("**")
        && pattern
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b" ./*->_".contains(&c));
    let Ok(cmds) = parsed else {
        return;
    };
    // A single command whose words hold no parenthesis (a rule cannot) and no ` > `
    // (it would read as a redirect) always gets its exact rule.
    if let [c] = cmds.as_slice() {
        let words = || {
            c.assignments
                .iter()
                .chain(&c.argv)
                .chain(c.redirects.iter().map(|r| &r.target))
        };
        // A write target must be spelled as its root-relative form: a rule's target
        // pattern matches the spelling and that form both (`> ./x` has no literal rule).
        let normal = |t: &str| {
            t == "/dev/null"
                || (!t.starts_with('/') && t.split('/').all(|p| !matches!(p, "" | "." | "..")))
        };
        let plain = !words().any(|w| w.contains(['(', ')', '\'']) || w.contains(" > "))
            && c.redirects.iter().all(|r| !r.write || normal(&r.target));
        if plain && !c.argv.is_empty() {
            assert!(exact.is_some(), "no exact rule for {src:?} ({c:?})");
        }
    }
    if !pattern_ok {
        return;
    }
    let rule = format!("bash({pattern})");
    if p.rule_matches(&rule, "bash", &effect, Some(src)).ok() != Some(true) {
        return;
    }
    for c in &cmds {
        let words = c
            .assignments
            .iter()
            .chain(&c.argv)
            .cloned()
            .chain(c.redirects.iter().map(|r| r.target.clone()))
            .collect::<Vec<_>>()
            .join(" ");
        for frag in pattern.split(" > ").flat_map(|h| h.split('*')) {
            let frag = frag.trim();
            assert!(
                frag.is_empty() || words.contains(frag),
                "{rule:?} allows {src:?} ({c:?}) without {frag:?}"
            );
        }
    }
});
