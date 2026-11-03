//! `jest` (`●` blocks under ` FAIL path` headers) and `vitest` (` FAIL  file > a > b`
//! blocks separated by `⎯⎯⎯` rules).

use super::{Item, Summary, count_pairs, num, split_message};

pub(super) fn parse(output: &str) -> Option<Summary> {
    let lines: Vec<&str> = output.lines().collect();
    let vitest = lines
        .iter()
        .any(|l| re!(r"^\s*(?:Test Files\s|RUN\s+v\d)").is_match(l));
    let (mut items, suite_errors) = if vitest {
        vitest_items(&lines)
    } else {
        jest_items(&lines)
    };
    let mut seen = Vec::new();
    items.retain(|i| {
        let key = (i.name.clone(), i.location.clone());
        let fresh = !seen.contains(&key);
        seen.push(key);
        fresh
    });

    let tests_line = lines.iter().find_map(|l| {
        let c = re!(r"^\s*Tests:?\s+(\d.*)$").captures(l)?;
        Some(count_pairs(&c[1]))
    });
    if tests_line.is_none() && items.is_empty() {
        return None;
    }
    let count = |word: &str| -> Option<u32> {
        tests_line
            .as_ref()
            .map(|c| c.iter().filter(|(w, _)| w == word).map(|(_, n)| n).sum())
    };
    let item_errors = items
        .iter()
        .filter(|i| suite_errors.contains(&i.name))
        .count() as u32;
    // vitest: `Errors  1 error` (unhandled errors); more than found makes `apply` refuse.
    let errors = lines
        .iter()
        .find_map(|l| {
            re!(r"^\s*Errors\s+(\d+) errors?")
                .captures(l)
                .map(|c| num(&c[1]))
        })
        .map_or(item_errors, |n| n.max(item_errors));
    Some(Summary {
        tool: if vitest { "vitest" } else { "jest" },
        passed: count("passed"),
        failed: count("failed").unwrap_or((items.len() as u32).saturating_sub(item_errors)),
        errors,
        warnings: 0,
        items,
    })
}

/// The first stack frame outside `node_modules`: `at fn (file:1:2)` or `at file:1:2`.
fn jest_location(block: &[&str]) -> Option<String> {
    block.iter().find_map(|l| {
        let c = re!(r"^\s*at\s+(?:.+?\s\()?([^\s()]+:\d+:\d+)\)?\s*$").captures(l)?;
        (!c[1].contains("node_modules")).then(|| c[1].to_string())
    })
}

fn jest_items(lines: &[&str]) -> (Vec<Item>, Vec<String>) {
    const SUITE_ERROR: &str = "Test suite failed to run";
    let mut items = Vec::new();
    let mut suite: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(c) = re!(r"^\s*(?:FAIL|PASS)\s+(\S.*)$").captures(line) {
            suite = Some(c[1].trim().to_string());
            i += 1;
            continue;
        }
        let title = re!(r"^  ● (.+)$")
            .captures(line)
            .map(|c| c[1].trim().to_string());
        let Some(title) = title.filter(|t| t != "Console") else {
            i += 1;
            continue;
        };
        let end = lines[i + 1..]
            .iter()
            .position(|l| {
                re!(r"^  ● |^\s*(?:FAIL|PASS)\s|^Test Suites:|^Summary of all failing tests|^Snapshot Summary").is_match(l)
            })
            .map_or(lines.len(), |p| i + 1 + p);
        let block = &lines[i + 1..end];
        let (message, rest) = split_message(block);
        let loc = jest_location(block).or_else(|| suite.clone());
        items.push(Item::new(title, loc, message, rest));
        i = end;
    }
    (items, vec![SUITE_ERROR.to_string()])
}

fn vitest_items(lines: &[&str]) -> (Vec<Item>, Vec<String>) {
    let mut items = Vec::new();
    let mut suite_errors = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(c) = re!(r"^\s*⎯+ (Unhandled Rejection|Unhandled Error|Uncaught Exception) ⎯+$")
            .captures(lines[i])
        {
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('⎯'))
                .map_or(lines.len(), |p| i + 1 + p);
            let block = &lines[i + 1..end];
            let loc = block.iter().find_map(|l| {
                let c = re!(r"^\s*❯ (\S+:\d+:\d+)\s*$").captures(l)?;
                (!c[1].contains("node_modules")).then(|| c[1].to_string())
            });
            let (message, rest) = split_message(block);
            suite_errors.push(c[1].to_string());
            items.push(Item::new(&c[1], loc, message, rest));
            i = end;
            continue;
        }
        let Some(c) = re!(r"^\s*FAIL\s+(\S.*)$").captures(lines[i]) else {
            i += 1;
            continue;
        };
        let header = c[1].trim();
        let end = lines[i + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with('⎯') || re!(r"^\s*FAIL\s").is_match(l))
            .map_or(lines.len(), |p| i + 1 + p);
        let block = &lines[i + 1..end];
        let (file, test) = match header.split_once(" > ") {
            Some((file, test)) => (file, Some(test)),
            None => (header, None),
        };
        let name = match test {
            Some(t) => t.to_string(),
            None => {
                // A suite that failed to load: `FAIL  file [ file ]`.
                let name = re!(r" \[ .* \]$").replace(file, "").to_string();
                suite_errors.push(name.clone());
                name
            }
        };
        let loc = block.iter().find_map(|l| {
            let c = re!(r"^\s*❯ (\S+:\d+:\d+)\s*$").captures(l)?;
            (!c[1].contains("node_modules")).then(|| c[1].to_string())
        });
        let (message, rest) = split_message(block);
        items.push(Item::new(name, loc, message, rest));
        i = end;
    }
    (items, suite_errors)
}
