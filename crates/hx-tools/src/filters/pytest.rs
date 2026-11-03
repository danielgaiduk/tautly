//! `pytest` (default and `-q`): the `short test summary info` section names each
//! failure; the matching `____ name ____` block in the FAILURES/ERRORS sections
//! supplies the location and detail.

use super::{Item, Summary, count_pairs, location};

/// A `____ name ____` traceback block.
struct Block<'a> {
    name: String,
    lines: Vec<&'a str>,
}

fn blocks<'a>(lines: &[&'a str]) -> Vec<Block<'a>> {
    let mut out: Vec<Block> = Vec::new();
    let mut open = false;
    for line in lines {
        if let Some(c) = re!(r"^_{2,} (.+?) _{2,}$").captures(line) {
            let name = c[1]
                .trim_start_matches("ERROR at setup of ")
                .trim_start_matches("ERROR at teardown of ")
                .trim_start_matches("ERROR collecting ")
                .to_string();
            out.push(Block {
                name,
                lines: Vec::new(),
            });
            open = true;
        } else if re!(r"^={3,}").is_match(line) {
            open = false;
        } else if open && let Some(b) = out.last_mut() {
            b.lines.push(line);
        }
    }
    out
}

/// The block for a test id like `tests/test_io.py::TestReader::test_reads_header`
/// (headers read `TestReader.test_reads_header`), or a collection error's path.
fn find_block<'a, 'b>(blocks: &'b [Block<'a>], id: &str) -> Option<&'b Block<'a>> {
    let parts: Vec<&str> = id.split("::").skip(1).collect();
    let dotted = parts.join(".");
    let last = parts.last().copied().unwrap_or("");
    [dotted.as_str(), last, id]
        .into_iter()
        .filter(|n| !n.is_empty())
        .find_map(|n| blocks.iter().find(|b| b.name == n))
}

/// Where the failure happened: the last `path.py:line: ...` line of the traceback.
fn block_location(b: &Block) -> Option<String> {
    b.lines.iter().rev().find_map(|l| {
        re!(r"^(\S+\.py):(\d+): ")
            .captures(l)
            .map(|c| location(&c[1], &c[2], None))
    })
}

fn block_message(b: &Block) -> String {
    b.lines
        .iter()
        .find_map(|l| l.strip_prefix("E ").map(|m| m.trim().to_string()))
        .unwrap_or_default()
}

pub(super) fn parse(output: &str) -> Option<Summary> {
    let lines: Vec<&str> = output.lines().collect();
    let blocks = blocks(&lines);

    let mut items: Vec<Item> = Vec::new();
    let (mut n_failed, mut n_errors) = (0, 0);
    let mut in_summary = false;
    for line in &lines {
        if line.contains("short test summary info") {
            in_summary = true;
            continue;
        }
        if !in_summary {
            continue;
        }
        if re!(r"^={3,}").is_match(line) {
            in_summary = false;
            continue;
        }
        let Some(c) = re!(r"^(FAILED|ERROR) (\S+)(?: - (.*))?$").captures(line) else {
            continue;
        };
        if &c[1] == "FAILED" {
            n_failed += 1;
        } else {
            n_errors += 1;
        }
        let id = &c[2];
        let block = find_block(&blocks, id);
        let message = match c.get(3) {
            Some(m) => m.as_str().to_string(),
            None => block.map(block_message).unwrap_or_default(),
        };
        items.push(Item::new(
            id,
            block.and_then(block_location),
            message,
            block
                .map_or(&[][..], |b| b.lines.as_slice())
                .iter()
                .copied(),
        ));
    }
    if items.is_empty() {
        // No summary section (`-r` options): fall back to the tracebacks themselves.
        for b in &blocks {
            n_failed += 1;
            items.push(Item::new(
                &b.name,
                block_location(b),
                block_message(b),
                b.lines.iter().copied(),
            ));
        }
    }

    // `2 failed, 11 passed, 1 warning, 1 error in 0.42s`
    let counts = lines.iter().rev().find_map(|l| {
        let t = l.trim_matches(|c| c == '=' || c == ' ');
        re!(r"^\d+ \w+(?:, \d+ \w+)* in [\d.]+s(?: \(.*\))?$")
            .is_match(t)
            .then(|| count_pairs(t.split(" in ").next().unwrap_or(t)))
    });
    if counts.is_none() && items.is_empty() {
        return None;
    }
    let count = |words: &[&str]| -> Option<u32> {
        counts.as_ref().map(|c| {
            c.iter()
                .filter(|(w, _)| words.contains(&w.as_str()))
                .map(|(_, n)| n)
                .sum()
        })
    };
    Some(Summary {
        tool: "pytest",
        passed: count(&["passed"]),
        failed: count(&["failed"]).unwrap_or(n_failed),
        errors: count(&["error", "errors"]).unwrap_or(n_errors),
        warnings: count(&["warning", "warnings"]).unwrap_or(0),
        items,
    })
}
