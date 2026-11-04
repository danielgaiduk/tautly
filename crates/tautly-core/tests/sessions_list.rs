//! `sessions::list`: what `/resume` shows (Task 31).

use std::path::Path;
use std::time::{Duration, SystemTime};

use tautly_core::record::SessionRecord;
use tautly_core::sessions;
use tautly_core::{Command, Ledger};
use tautly_provider::{ModelId, RawUsage};
use tautly_session::{HeaderInit, SessionWriter};
use tempfile::TempDir;

fn session(data: &Path, root: &Path, first: Option<&str>) -> String {
    let mut w =
        SessionWriter::<SessionRecord>::create_in(data, root, HeaderInit::default()).unwrap();
    if let Some(text) = first {
        w.append(&SessionRecord::TurnStart {
            turn: 1,
            text: text.into(),
        })
        .unwrap();
    }
    w.checkpoint().unwrap();
    w.session_id().to_string()
}

#[test]
fn lists_newest_first_with_first_line_cut_and_cost() {
    let (data, work) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let long = "x".repeat(100);
    let a = session(
        data.path(),
        work.path(),
        Some("fix the parser\nsecond line"),
    );
    std::thread::sleep(Duration::from_millis(5));
    let b = session(data.path(), work.path(), Some(&long));
    std::thread::sleep(Duration::from_millis(5));
    let c = session(data.path(), work.path(), None);
    let ledger = Ledger::open(&data.path().join("ledger.sqlite")).unwrap();
    ledger
        .record(
            &a,
            &ModelId::from("anthropic/claude-sonnet-5-5"),
            &RawUsage::default(),
            Some(0.25),
        )
        .unwrap();

    let all = sessions::list_in(data.path(), work.path(), 10);
    let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [c.as_str(), b.as_str(), a.as_str()]);
    assert_eq!(all[2].first_prompt, "fix the parser");
    assert_eq!(all[1].first_prompt.chars().count(), 60);
    assert_eq!(all[0].first_prompt, "");
    assert_eq!(all[2].cost_usd, Some(0.25));
    assert_eq!(all[1].cost_usd, None);
    assert!(all[2].started <= SystemTime::now());
    assert_eq!(sessions::list_in(data.path(), work.path(), 2).len(), 2);
    assert_eq!(sessions::list_in(data.path(), work.path(), 0).len(), 0);
}

#[test]
fn first_message_comes_from_the_command_when_there_is_no_turn_start() {
    let (data, work) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut w =
        SessionWriter::<SessionRecord>::create_in(data.path(), work.path(), HeaderInit::default())
            .unwrap();
    w.append(&SessionRecord::Command(Command::SendMessage {
        text: "  hello there ".into(),
    }))
    .unwrap();
    w.checkpoint().unwrap();
    let all = sessions::list_in(data.path(), work.path(), 5);
    assert_eq!(all[0].first_prompt, "hello there");
}

#[test]
fn another_work_tree_or_an_empty_data_home_lists_nothing() {
    let (data, work, other) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    session(data.path(), work.path(), Some("a"));
    assert!(sessions::list_in(data.path(), other.path(), 5).is_empty());
    assert!(sessions::list_in(&data.path().join("nope"), work.path(), 5).is_empty());
}
