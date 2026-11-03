//! `eslint` stylish formatter: a file path line, then `line:col  severity  message  rule`
//! rows, then `✖ N problems (E errors, W warnings)`.

use super::{Item, Summary, count_pairs, location};

pub(super) fn parse(output: &str) -> Option<Summary> {
    let mut items: Vec<Item> = Vec::new();
    let (mut errors, mut warnings) = (0, 0);
    let mut file: Option<&str> = None;
    let mut totals: Option<Vec<(String, u32)>> = None;
    for line in output.lines() {
        if let Some(c) =
            re!(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(\S+))?\s*$").captures(line)
        {
            if &c[3] == "error" {
                errors += 1;
            } else {
                warnings += 1;
            }
            let pos = location(&c[1], &c[2], None);
            let loc = match file {
                Some(f) => format!("{f}:{pos}"),
                None => pos,
            };
            let name = c.get(5).map_or(&c[3], |m| m.as_str());
            items.push(Item::new(name, Some(loc), &c[4], Vec::<String>::new()));
        } else if let Some(c) = re!(r"^✖ \d+ problems? \((.*)\)").captures(line) {
            totals = Some(count_pairs(&c[1]));
        } else if !line.starts_with([' ', '✖']) && !line.trim().is_empty() {
            file = Some(line.trim());
        }
    }
    if items.is_empty() && totals.is_none() {
        return None;
    }
    if let Some(t) = &totals {
        let sum = |word: &str| {
            t.iter()
                .filter(|(w, _)| w.starts_with(word))
                .map(|(_, n)| n)
                .sum()
        };
        errors = sum("error");
        warnings = sum("warning");
    }
    Some(Summary {
        tool: "eslint",
        passed: None,
        failed: 0,
        errors,
        warnings,
        items,
    })
}
