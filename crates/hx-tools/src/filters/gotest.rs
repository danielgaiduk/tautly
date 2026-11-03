//! `go test` (verbose or not): `--- FAIL: Name` lines, the indented log lines that
//! belong to each test, compile errors and panics.

use std::collections::HashMap;

use super::{Item, Summary, location};

pub(super) fn parse(output: &str) -> Option<Summary> {
    let mut logs: HashMap<String, Vec<String>> = HashMap::new();
    let mut failed_names: Vec<String> = Vec::new();
    let mut passed: u32 = 0;
    let mut package_lines = 0;
    let mut build_errors: Vec<Item> = Vec::new();
    // Every panic, with the lines that follow it (goroutine traces).
    let mut panics: Vec<(String, Vec<String>)> = Vec::new();
    // The test whose log lines are being read; `None` outside of tests.
    let mut current: Option<String> = None;
    let mut in_panic = false;
    // Unattributed output since the last package result, and the number of failures
    // known at that point: a `FAIL pkg` line that adds none has a cause no test names
    // (`log.Fatal`, `os.Exit`, a setup failure).
    let mut stray: Vec<String> = Vec::new();
    let mut known_at_last_pkg = 0;
    let mut package_errors: Vec<Item> = Vec::new();

    for line in output.lines() {
        if let Some(c) = re!(r"^\s*=== (?:RUN|NAME|CONT|PAUSE)\s+(\S+)").captures(line) {
            current = Some(c[1].to_string());
            in_panic = false;
        } else if let Some(c) = re!(r"^\s*--- (FAIL|PASS|SKIP): (\S+) \(").captures(line) {
            match &c[1] {
                "FAIL" if !failed_names.iter().any(|n| n == &c[2]) => {
                    failed_names.push(c[2].to_string());
                }
                "PASS" => passed += 1,
                _ => {}
            }
            current = Some(c[2].to_string());
            in_panic = false;
        } else if let Some(c) = re!(r"^panic: (.*?)(?: \[recovered\])?$").captures(line) {
            panics.push((c[1].to_string(), Vec::new()));
            current = None;
            in_panic = true;
        } else if let Some(c) = re!(r"^(\S+\.go):(\d+):(\d+): (.*)$").captures(line) {
            build_errors.push(Item::new(
                "build",
                Some(location(&c[1], &c[2], Some(&c[3]))),
                &c[4],
                Vec::<String>::new(),
            ));
            current = None;
        } else if re!(r"^(?:FAIL|PASS|ok)\b").is_match(line) || line.starts_with("exit status") {
            package_lines += 1;
            current = None;
            in_panic = false;
            if line.starts_with("exit status") {
                continue;
            }
            let known = failed_names.len() + build_errors.len() + panics.len();
            if let Some(c) = re!(r"^FAIL\s+(\S+)").captures(line)
                && known == known_at_last_pkg
            {
                package_errors.push(Item::new(
                    "package",
                    None,
                    format!("{} failed", &c[1]),
                    std::mem::take(&mut stray),
                ));
            }
            if re!(r"^(?:FAIL|ok)\s").is_match(line) {
                known_at_last_pkg = known;
                stray.clear();
            }
        } else if line.starts_with([' ', '\t']) && !line.trim().is_empty() {
            if in_panic && let Some((_, detail)) = panics.last_mut() {
                detail.push(line.trim_start().to_string());
            } else if let Some(t) = &current {
                logs.entry(t.clone())
                    .or_default()
                    .push(line.trim_start().to_string());
            }
        } else if in_panic && let Some((_, detail)) = panics.last_mut() {
            detail.push(line.to_string());
        } else {
            current = None;
            stray.push(line.to_string());
        }
    }

    // A parent whose subtests failed has nothing to say beyond them.
    let is_parent = |name: &str| {
        let prefix = format!("{name}/");
        !logs.contains_key(name) && failed_names.iter().any(|n| n.starts_with(&prefix))
    };
    let mut items: Vec<Item> = failed_names
        .iter()
        .filter(|n| !is_parent(n))
        .map(|name| failure_item(name, logs.get(name).map_or(&[][..], Vec::as_slice)))
        .collect();
    let failed = items.len() as u32;
    let mut errors = (build_errors.len() + package_errors.len()) as u32;
    items.extend(build_errors);
    items.extend(package_errors);
    for (message, detail) in panics {
        errors += 1;
        items.push(Item::new("panic", None, message, detail));
    }

    if items.is_empty() && package_lines == 0 {
        return None;
    }
    Some(Summary {
        tool: "go test",
        passed: (passed > 0).then_some(passed),
        failed,
        errors,
        warnings: 0,
        items,
    })
}

/// The first log line `file_test.go:14: message` gives the location and message.
fn failure_item(name: &str, log: &[String]) -> Item {
    match log.split_first() {
        Some((first, rest)) => match re!(r"^(\S+\.go):(\d+): (.*)$").captures(first) {
            Some(c) => Item::new(name, Some(location(&c[1], &c[2], None)), &c[3], rest),
            None => Item::new(name, None, first, rest),
        },
        None => Item::new(name, None, "", Vec::<String>::new()),
    }
}
