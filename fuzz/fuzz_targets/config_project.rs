//! Configuration loading with an untrusted project layer. Input: the user file, `\0`,
//! the project file (`.tautly/config.toml`). Loading never panics, and when both the
//! user layer alone and user plus project load, the project layer:
//! - leaves every user-only setting as the user layer has it;
//! - never raises the permission mode, adds an allow rule or drops a deny rule;
//! - only lowers `max_turns`, `turn_timeout_s`, the budgets and the compaction threshold;
//! - never adds a server to the user's own MCP servers.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tautly_core::{CliOverrides, Config, Mode};

fn strictness(m: Mode) -> u8 {
    match m {
        Mode::Ask => 2,
        Mode::AutoEdit => 1,
        Mode::Yolo => 0,
    }
}

/// `b` is no looser than `a` (None: unlimited).
fn le<T: PartialOrd + Copy>(b: Option<T>, a: Option<T>) -> bool {
    match (b, a) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(b), Some(a)) => b <= a,
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (user, project) = text.split_once('\0').unwrap_or(("", text));
    let home = tempfile::tempdir().unwrap();
    let user_file = home.path().join("config.toml");
    std::fs::write(&user_file, user).unwrap();
    let alone = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".tautly")).unwrap();
    std::fs::write(repo.path().join(".tautly/config.toml"), project).unwrap();

    let base = Config::load_from(Some(&user_file), alone.path(), CliOverrides::default());
    let both = Config::load_from(Some(&user_file), repo.path(), CliOverrides::default());
    let (Ok(base), Ok(both)) = (base, both) else {
        return;
    };
    let (u, p) = (&base.config, &both.config);
    let json = |c: &Config| serde_json::to_value(c).unwrap();
    let (uj, pj) = (json(u), json(p));
    for key in [
        "providers",
        "telemetry",
        "approve_memory",
        "catalog_overrides",
        "routing",
        "evidence",
        "checkpoints",
        "receipts",
        "mcp",
    ] {
        assert_eq!(
            uj.get(key),
            pj.get(key),
            "the project layer changed `{key}`"
        );
    }
    assert!(
        strictness(p.permissions.mode) >= strictness(u.permissions.mode),
        "mode raised from {:?} to {:?}",
        u.permissions.mode,
        p.permissions.mode
    );
    assert_eq!(
        p.permissions.allow, u.permissions.allow,
        "allow rules changed"
    );
    for d in &u.permissions.deny {
        assert!(p.permissions.deny.contains(d), "deny rule {d:?} dropped");
    }
    assert!(p.max_turns <= u.max_turns, "max_turns raised");
    assert!(
        le(p.turn_timeout_s, u.turn_timeout_s),
        "turn_timeout_s raised"
    );
    assert!(
        le(p.compaction_threshold, u.compaction_threshold),
        "compaction threshold raised or compaction turned off"
    );
    for (pb, ub) in [
        (&p.budget.turn, &u.budget.turn),
        (&p.budget.session, &u.budget.session),
    ] {
        assert!(le(pb.max_usd, ub.max_usd), "budget max_usd raised");
        assert!(le(pb.max_tokens, ub.max_tokens), "budget max_tokens raised");
    }
});
