//! `tsc`: `file(line,col): error TS2322: message` (plain) or
//! `file:line:col - error TS2322: message` (pretty), with indented continuation lines.

use super::{Item, Summary, location, num};

pub(super) fn parse(output: &str) -> Option<Summary> {
    let mut items: Vec<Item> = Vec::new();
    let mut warnings = 0;
    let mut found: Option<u32> = None;
    // Whether the previous line belongs to the last item (so indented lines continue it).
    let mut open = false;
    for line in output.lines() {
        let plain = re!(r"^(.+?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.*)$").captures(line);
        let pretty = re!(r"^(.+?):(\d+):(\d+) - (error|warning) (TS\d+): (.*)$").captures(line);
        if let Some(c) = plain.or(pretty) {
            if &c[4] == "warning" {
                warnings += 1;
            }
            items.push(Item::new(
                &c[5],
                Some(location(&c[1], &c[2], Some(&c[3]))),
                &c[6],
                Vec::<String>::new(),
            ));
            open = true;
        } else if let Some(c) = re!(r"^error (TS\d+): (.*)$").captures(line) {
            items.push(Item::new(&c[1], None, &c[2], Vec::<String>::new()));
            open = true;
        } else if let Some(c) = re!(r"^Found (\d+) errors?").captures(line) {
            found = Some(num(&c[1]));
            open = false;
        } else if open && line.starts_with(' ') && !line.trim().is_empty() {
            if let Some(last) = items.last_mut()
                && last.detail.len() < super::MAX_DETAIL_LINES
            {
                last.detail.push(line.trim_end().to_string());
            }
        } else {
            open = false;
        }
    }
    if items.is_empty() && found.is_none() {
        return None;
    }
    let errors = items.len() as u32 - warnings;
    Some(Summary {
        tool: "tsc",
        passed: None,
        failed: 0,
        errors: found.unwrap_or(errors),
        warnings,
        items,
    })
}
