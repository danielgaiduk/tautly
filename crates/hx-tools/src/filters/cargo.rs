//! `cargo test|build|check|clippy`: compiler diagnostics (`error[E0308]: ...` /
//! `--> file:line:col`) and libtest failures (`test x ... FAILED`, `---- x stdout ----`).

use std::collections::HashMap;

use super::{Item, Summary, location, num, split_message};

/// Headers that look like diagnostics but only report totals.
fn is_noise(message: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "could not compile",
        "test failed",
        "aborting due to",
        "build failed",
        "some errors have",
        "doctest failed",
    ];
    PREFIXES.iter().any(|p| message.starts_with(p))
        || message.contains(" target failed")
        || message.contains(" targets failed")
        || re!(r"^`[^`]+` \(.*\) generated \d+ warnings?").is_match(message)
}

/// Lint name from the notes rustc and clippy attach to a diagnostic.
fn lint_name(block: &[&str]) -> Option<String> {
    for line in block {
        if let Some(c) = re!(r"#\[(?:warn|deny|forbid)\(([\w:]+)\)\]").captures(line) {
            return Some(c[1].to_string());
        }
    }
    for line in block {
        if let Some(c) = re!(r"rust-clippy/[^/]+/index\.html#(\w+)").captures(line) {
            return Some(format!("clippy::{}", &c[1]));
        }
    }
    None
}

struct Diagnostics {
    errors: Vec<Item>,
    warnings: Vec<Item>,
}

fn diagnostics(lines: &[&str]) -> Diagnostics {
    let mut d = Diagnostics {
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for (i, line) in lines.iter().enumerate() {
        let Some(h) = re!(r"^(error|warning)(?:\[(\w+)\])?: (.+)$").captures(line) else {
            continue;
        };
        let message = &h[3];
        if is_noise(message) {
            continue;
        }
        let is_error = &h[1] == "error";
        let loc = lines
            .get(i + 1)
            .and_then(|l| re!(r"^\s*--> (.+?):(\d+):(\d+)$").captures(l));
        if loc.is_none() && !is_error {
            continue;
        }
        let end = lines[i + 1..]
            .iter()
            .position(|l| l.trim().is_empty())
            .map_or(lines.len(), |p| i + 1 + p);
        let block = &lines[i + 1..end];
        let name = h
            .get(2)
            .map(|c| c.as_str().to_string())
            .or_else(|| lint_name(block))
            .unwrap_or_else(|| h[1].to_string());
        let (loc, detail) = match &loc {
            Some(c) => (Some(location(&c[1], &c[2], Some(&c[3]))), &block[1..]),
            None => (None, block),
        };
        let item = Item::new(name, loc, message, detail);
        let list = if is_error {
            &mut d.errors
        } else {
            &mut d.warnings
        };
        if !list.contains(&item) {
            list.push(item);
        }
    }
    d
}

/// Panic location and message from a `---- name stdout ----` block.
fn failure_item(name: &str, block: &[&str]) -> Item {
    let panic_at = block.iter().position(|l| l.contains("panicked at "));
    if let Some(p) = panic_at {
        let line = block[p];
        // `panicked at src/lib.rs:3:5:` followed by the message.
        if let Some(c) = re!(r"panicked at (.+?):(\d+):(\d+):$").captures(line) {
            let (message, rest) = split_message(&block[p + 1..]);
            return Item::new(
                name,
                Some(location(&c[1], &c[2], Some(&c[3]))),
                message,
                body(&rest),
            );
        }
        // Older: `panicked at 'msg', src/lib.rs:3:5`.
        if let Some(c) = re!(r"panicked at '(.*)', (.+?):(\d+):(\d+)$").captures(line) {
            return Item::new(
                name,
                Some(location(&c[2], &c[3], Some(&c[4]))),
                &c[1],
                body(&block[p + 1..]),
            );
        }
    }
    let (message, rest) = split_message(block);
    Item::new(name, None, message, body(&rest))
}

/// Panic output up to the backtrace or the closing notes.
fn body<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .copied()
        .take_while(|l| !l.starts_with("stack backtrace:") && !l.starts_with("note: "))
        .collect()
}

pub(super) fn parse(output: &str) -> Option<Summary> {
    let lines: Vec<&str> = output.lines().collect();
    let d = diagnostics(&lines);

    // Failure blocks, keyed by test name.
    let mut blocks: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut current: Option<&str> = None;
    for line in &lines {
        if let Some(c) = re!(r"^---- (.+) stdout ----$").captures(line) {
            let name = c.get(1).map_or("", |m| m.as_str());
            current = Some(name);
            blocks.entry(name).or_default();
        } else if line.trim_end() == "failures:" {
            current = None;
        } else if let Some(name) = current {
            blocks.entry(name).or_default().push(line);
        }
    }

    let mut failed_names: Vec<&str> = Vec::new();
    let mut result_lines = 0;
    let (mut passed, mut failed) = (0, 0);
    for line in &lines {
        if let Some(c) = re!(r"^test (.+) \.\.\. FAILED$").captures(line) {
            let name = c.get(1).map_or("", |m| m.as_str());
            if !failed_names.contains(&name) {
                failed_names.push(name);
            }
        } else if let Some(c) =
            re!(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;").captures(line)
        {
            result_lines += 1;
            passed += num(&c[1]);
            failed += num(&c[2]);
        }
    }
    let failures: Vec<Item> = failed_names
        .iter()
        .map(|n| failure_item(n, blocks.get(n).map_or(&[][..], Vec::as_slice)))
        .collect();

    if d.errors.is_empty() && d.warnings.is_empty() && failures.is_empty() && result_lines == 0 {
        return None;
    }
    let failed = if result_lines > 0 {
        failed
    } else {
        failures.len() as u32
    };
    let (errors, warnings) = (d.errors.len() as u32, d.warnings.len() as u32);
    let mut items = d.errors;
    items.extend(failures);
    items.extend(d.warnings);
    Some(Summary {
        tool: "cargo",
        passed: (result_lines > 0).then_some(passed),
        failed,
        errors,
        warnings,
        items,
    })
}
